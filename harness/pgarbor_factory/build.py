"""Central cargo build/test invocation with timing and compile-error clustering (RFC 003 §5
Epoch 3, §6.1, §7 compile clusters).

Lease FAC-08.

``cargo_build(worktree, target_dir)``: run
``cargo build --workspace --all-targets --offline --message-format=json`` with
``CARGO_TARGET_DIR=<target_dir>`` and ``CARGO_TERM_COLOR=never``; capture stdout (JSON lines)
and stderr to ``<log_dir>/build.stdout.jsonl`` / ``build.stderr.log``; time with
time.monotonic(); also record ``rustc -vV`` and ``cargo -V``.

Parse each JSON line (``json.loads``; skip non-JSON lines): objects with
``"reason": "compiler-message"`` carry ``message`` = rustc diagnostic:
``{"level": "error"|"warning"|..., "message": str, "code": {"code": "E0308"} | null,
"spans": [{"file_name", "line_start", "column_start", "is_primary", "label"}],
"rendered": str, "children": [...]}``; ``"reason": "build-finished"`` has ``"success"``.
Only level "error" (and "error: internal compiler error") count as errors; warnings are
counted per crate but never cluster.

``Diagnostic`` per error: code ("" when null), message, primary span file (repo-relative)/
line, crate (from ``package_id`` / target name of the enclosing message), owning lease
(leases.file_owner_map; "" when the file is unleased e.g. lib.rs -> contract owner).

Clustering (``cluster_errors``) — RFC 003 §7 "one interface-drift cluster": key =
(code, normalized message template) where the template replaces backticked items
(`` `foo::Bar` `` -> `` `?` ``) and numbers; each cluster lists its diagnostics, distinct
files, distinct leases, and ``root_hint``: when >= 3 leases share a cluster whose message
mentions an item defined in a different crate (``cannot find``, ``no method named``,
``mismatched types`` with an external path) -> "interface_drift" (to SWARM-09 /
amendments), else "local". Sorted by (number of leases desc, count desc, code).

``cargo_test(worktree, target_dir, packages, extra_args)``: ``cargo test --offline -p <pkg>
... <extra_args>`` (stable toolchain: libtest JSON output is unstable, so parse the plain
output: lines ``test <name> ... ok|FAILED|ignored`` and each binary's ``test result:``
summary); return per-package TestRun. Timeout per invocation (default 1800 s) -> kill the
process group (``start_new_session=True`` + ``os.killpg``), status "timeout".

Results are written to ``runs/<campaign>/build.json`` (BuildReport.to_json, canonical).
"""

from __future__ import annotations

import json
import os
import re
import signal
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional, Sequence

from pgarbor_harness.model import write_canonical_json


@dataclass
class Diagnostic:
    level: str
    code: str
    message: str
    file: str
    line: int
    crate: str
    lease_id: str
    rendered: str = ""


@dataclass
class ErrorCluster:
    key: str
    code: str
    template: str
    count: int
    files: list[str]
    leases: list[str]
    root_hint: str
    examples: list[Diagnostic] = field(default_factory=list)   #: first 3


@dataclass
class BuildReport:
    ok: bool
    duration_s: float
    rustc: str
    cargo: str
    errors: list[Diagnostic] = field(default_factory=list)
    warnings_by_crate: dict[str, int] = field(default_factory=dict)
    clusters: list[ErrorCluster] = field(default_factory=list)
    log_path: Optional[str] = None

    def to_json(self) -> dict[str, Any]:
        from dataclasses import asdict

        return asdict(self)


@dataclass
class TestRun:
    package: str
    passed: list[str] = field(default_factory=list)
    failed: list[str] = field(default_factory=list)
    ignored: list[str] = field(default_factory=list)
    status: str = "ok"           #: "ok" | "failed" | "timeout" | "build_failed"
    duration_s: float = 0.0
    log_path: Optional[str] = None


#: Crate-level standard libraries: items rooted here are never "defined in another workspace crate".
_STD_ROOTS = frozenset({"std", "core", "alloc"})
#: Path roots that always refer to the enclosing crate.
_LOCAL_ROOTS = frozenset({"crate", "self", "super", "Self"})
_BACKTICK_RE = re.compile(r"`[^`]*`")
_PATH_ROOT_RE = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)::")
_NUMBER_RE = re.compile(r"\d+")
_TEST_LINE_RE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)(?:, .*)?$")
_ARGS_BUILD = ("cargo", "build", "--workspace", "--all-targets", "--offline",
               "--message-format=json")


def _crate_name(obj: dict[str, Any]) -> str:
    """Package name of a cargo compiler-message object.

    Handles the PackageIdSpec form ``path+file://...#name@version`` and the older
    ``name version (source)`` form; falls back to the target name.
    """
    pkg_id = str(obj.get("package_id") or "")
    name = ""
    if "#" in pkg_id:
        frag = pkg_id.rsplit("#", 1)[1]
        if "@" in frag:
            name = frag.split("@", 1)[0]
        else:
            # ``path+file:///.../crate-dir#0.1.0`` names the package by its directory.
            name = pkg_id.split("#", 1)[0].rstrip("/").rsplit("/", 1)[-1]
    elif pkg_id:
        name = pkg_id.split(" ", 1)[0]
    if not name:
        target = obj.get("target")
        if isinstance(target, dict):
            name = str(target.get("name") or "")
    return name


def _repo_relative(file_name: str, repo_root: Path) -> str:
    if not file_name:
        return ""
    p = Path(file_name)
    if p.is_absolute():
        for root in (repo_root, repo_root.resolve()):
            try:
                return p.relative_to(root).as_posix()
            except ValueError:
                continue
    return p.as_posix()


def _primary_span(spans: list[Any]) -> Optional[dict[str, Any]]:
    dict_spans = [s for s in spans if isinstance(s, dict)]
    for s in dict_spans:
        if s.get("is_primary"):
            return s
    return dict_spans[0] if dict_spans else None


def parse_cargo_messages(lines: Sequence[str], file_owner: dict[str, str],
                         repo_root: Path) -> tuple[list[Diagnostic], dict[str, int], bool]:
    """(errors, warnings_by_crate, build success flag) from cargo JSON lines.

    Non-JSON lines are skipped. Errors are de-duplicated on (crate, code, message, file, line)
    because ``--all-targets`` compiles the same source in several targets. The success flag is
    True only when a ``build-finished`` message reports ``success: true``.
    """
    root = Path(repo_root)
    errors: list[Diagnostic] = []
    warnings: dict[str, int] = {}
    seen: set[tuple[Any, ...]] = set()
    success = False
    for raw in lines:
        line = raw.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if not isinstance(obj, dict):
            continue
        reason = obj.get("reason")
        if reason == "build-finished":
            success = obj.get("success") is True
            continue
        if reason != "compiler-message":
            continue
        msg = obj.get("message")
        if not isinstance(msg, dict):
            continue
        level = str(msg.get("level") or "")
        message = str(msg.get("message") or "")
        crate = _crate_name(obj)
        span = _primary_span(msg.get("spans") or [])
        file = _repo_relative(str(span.get("file_name") or ""), root) if span else ""
        line_no = int(span.get("line_start") or 0) if span else 0
        if level.startswith("error"):
            code_obj = msg.get("code")
            code = str(code_obj.get("code") or "") if isinstance(code_obj, dict) else ""
            key = ("error", crate, code, message, file, line_no)
            if key in seen:
                continue
            seen.add(key)
            errors.append(Diagnostic(
                level=level,
                code=code,
                message=message,
                file=file,
                line=line_no,
                crate=crate,
                lease_id=file_owner.get(file, "") if file else "",
                rendered=str(msg.get("rendered") or ""),
            ))
        elif level == "warning":
            key = ("warning", crate, message, file, line_no)
            if key in seen:
                continue
            seen.add(key)
            warnings[crate] = warnings.get(crate, 0) + 1
    return errors, warnings, success


def _template(message: str) -> str:
    """Cluster template: backticked items become `?`, digit runs become N."""
    return _NUMBER_RE.sub("N", _BACKTICK_RE.sub("`?`", message))


def _has_external_item(d: Diagnostic) -> bool:
    """True when the message names a path rooted in a different (non-std) crate."""
    own = d.crate.replace("-", "_")
    for item in _BACKTICK_RE.findall(d.message):
        m = _PATH_ROOT_RE.search(item)
        if m is None:
            continue
        root = m.group(1)
        if root in _LOCAL_ROOTS or root in _STD_ROOTS or root == own:
            continue
        # Module and crate paths are lowercase; type paths like `Vec::new` are not crate roots.
        if root[0].islower():
            return True
    return False


def cluster_errors(errors: list[Diagnostic]) -> list[ErrorCluster]:
    """Group errors by (code, normalized message template). Warnings are never passed here.

    ``root_hint`` is "interface_drift" when at least three distinct leases share the cluster and
    one of its messages names an item rooted in another crate; otherwise "local".
    """
    groups: dict[tuple[str, str], list[Diagnostic]] = {}
    for d in errors:
        groups.setdefault((d.code, _template(d.message)), []).append(d)
    clusters: list[ErrorCluster] = []
    for (code, template), members in groups.items():
        leases = sorted({d.lease_id for d in members if d.lease_id})
        files = sorted({d.file for d in members if d.file})
        drift = len(leases) >= 3 and any(_has_external_item(d) for d in members)
        clusters.append(ErrorCluster(
            key=f"{code or 'no-code'}: {template}",
            code=code,
            template=template,
            count=len(members),
            files=files,
            leases=leases,
            root_hint="interface_drift" if drift else "local",
            examples=members[:3],
        ))
    clusters.sort(key=lambda c: (-len(c.leases), -c.count, c.code, c.key))
    return clusters


def _cargo_env(target_dir: Path) -> dict[str, str]:
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_TERM_COLOR"] = "never"
    return env


def _tool_version(tool: str, args: Sequence[str], cwd: Path, env: dict[str, str]) -> str:
    try:
        proc = subprocess.run([tool, *args], cwd=cwd, env=env, capture_output=True, check=False)
    except OSError as exc:
        return f"unavailable: {exc}"
    out = proc.stdout.decode("utf-8", "replace").strip()
    if proc.returncode != 0 or not out:
        err = proc.stderr.decode("utf-8", "replace").strip()
        return f"unavailable: exit {proc.returncode}: {err}"
    return out


def cargo_build(worktree: Path, target_dir: Path, file_owner: dict[str, str],
                log_dir: Path) -> BuildReport:
    """Central ``cargo build`` of the whole workspace; writes logs and ``build.json``.

    ``ok`` requires both a ``build-finished`` message with ``success: true`` and exit status 0.
    When cargo fails without producing a parsable diagnostic, a synthetic error carrying the
    first stderr error line is added so the failure is never reported without a reason.
    """
    worktree = Path(worktree)
    target_dir = Path(target_dir)
    log_dir = Path(log_dir)
    log_dir.mkdir(parents=True, exist_ok=True)
    env = _cargo_env(target_dir)
    rustc = _tool_version("rustc", ["-vV"], worktree, env)
    cargo = _tool_version("cargo", ["-V"], worktree, env)

    started = time.monotonic()
    try:
        proc = subprocess.run(list(_ARGS_BUILD), cwd=worktree, env=env,
                              capture_output=True, check=False)
        stdout, stderr, rc = proc.stdout, proc.stderr, proc.returncode
    except OSError as exc:
        stdout, stderr, rc = b"", f"failed to start cargo build: {exc}\n".encode(), -1
    duration = time.monotonic() - started

    stdout_path = log_dir / "build.stdout.jsonl"
    stderr_path = log_dir / "build.stderr.log"
    stdout_path.write_bytes(stdout)
    stderr_path.write_bytes(stderr)

    errors, warnings, success = parse_cargo_messages(
        stdout.decode("utf-8", "replace").splitlines(), file_owner, worktree)
    ok = success and rc == 0
    if not ok and not errors:
        err_lines = [ln.strip() for ln in stderr.decode("utf-8", "replace").splitlines()
                     if ln.strip()]
        message = next((ln for ln in err_lines if ln.startswith("error")),
                       err_lines[0] if err_lines else f"cargo build exited with status {rc}")
        errors.append(Diagnostic(level="error", code="", message=message, file="", line=0,
                                 crate="", lease_id=""))
    report = BuildReport(
        ok=ok,
        duration_s=round(duration, 3),
        rustc=rustc,
        cargo=cargo,
        errors=errors,
        warnings_by_crate=warnings,
        clusters=cluster_errors(errors),
        log_path=str(stdout_path),
    )
    write_canonical_json(log_dir / "build.json", report.to_json())
    return report


def _parse_test_output(text: str) -> tuple[list[str], list[str], list[str], bool]:
    """(passed, failed, ignored, ran) from plain libtest output.

    ``ran`` is True when at least one test verdict or ``test result:`` summary was seen.
    """
    passed: list[str] = []
    failed: list[str] = []
    ignored: list[str] = []
    ran = False
    for raw in text.splitlines():
        line = raw.rstrip()
        if line.startswith("test result:"):
            ran = True
            continue
        m = _TEST_LINE_RE.match(line)
        if m is None:
            continue
        ran = True
        name, verdict = m.group(1), m.group(2)
        if verdict == "ok":
            passed.append(name)
        elif verdict == "FAILED":
            failed.append(name)
        else:
            ignored.append(name)
    return passed, failed, ignored, ran


def _kill_group(proc: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(proc.pid, signal.SIGKILL)  # the child leads its own session (pgid == pid)
    except OSError:
        pass


def _safe_name(package: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]", "_", package)


def _run_one_test(cwd: Path, env: dict[str, str], argv: list[str], package: str,
                  timeout_s: float, log_path: Optional[Path]) -> TestRun:
    started = time.monotonic()
    timed_out = False
    try:
        proc = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, start_new_session=True)
    except OSError as exc:
        out_bytes = f"failed to start cargo test: {exc}\n".encode()
        rc = -1
    else:
        try:
            out_bytes, _ = proc.communicate(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_group(proc)
            out_bytes, _ = proc.communicate()
        rc = proc.returncode
    duration = time.monotonic() - started

    text = out_bytes.decode("utf-8", "replace")
    if log_path is not None:
        log_path.write_text(text, encoding="utf-8")
    passed, failed, ignored, ran = _parse_test_output(text)
    if timed_out:
        status = "timeout"
    elif rc == 0:
        status = "ok"
    elif ran:
        status = "failed"
    else:
        status = "build_failed"
    return TestRun(
        package=package,
        passed=passed,
        failed=failed,
        ignored=ignored,
        status=status,
        duration_s=round(duration, 3),
        log_path=str(log_path) if log_path is not None else None,
    )


def cargo_test(worktree: Path, target_dir: Path, packages: Sequence[str],
               extra_args: Sequence[str] = (), timeout_s: float = 1800.0,
               log_dir: Optional[Path] = None) -> list[TestRun]:
    """Run ``cargo test --offline -p <pkg> <extra_args>`` once per package.

    One invocation per package keeps attribution exact (plain output has no package
    names) and gives each package its own timeout. Pass ``--no-fail-fast`` in ``extra_args``
    to keep running test binaries after a failure. Empty ``packages`` is a caller error.
    """
    if not packages:
        raise ValueError("cargo_test: at least one package is required")
    worktree = Path(worktree)
    target_dir = Path(target_dir)
    log_root: Optional[Path] = None
    if log_dir is not None:
        log_root = Path(log_dir)
        log_root.mkdir(parents=True, exist_ok=True)
    env = _cargo_env(target_dir)
    runs: list[TestRun] = []
    for package in packages:
        argv = ["cargo", "test", "--offline", "-p", package, *extra_args]
        log_path = log_root / f"test.{_safe_name(package)}.log" if log_root is not None else None
        runs.append(_run_one_test(worktree, env, argv, package, timeout_s, log_path))
    return runs
