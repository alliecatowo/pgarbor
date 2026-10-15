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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional, Sequence


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


def parse_cargo_messages(lines: Sequence[str], file_owner: dict[str, str],
                         repo_root: Path) -> tuple[list[Diagnostic], dict[str, int], bool]:
    """(errors, warnings_by_crate, build success flag) from cargo JSON lines."""
    raise NotImplementedError("FAC-08: parse cargo json messages")


def cluster_errors(errors: list[Diagnostic]) -> list[ErrorCluster]:
    raise NotImplementedError("FAC-08: compile error clustering")


def cargo_build(worktree: Path, target_dir: Path, file_owner: dict[str, str],
                log_dir: Path) -> BuildReport:
    raise NotImplementedError("FAC-08: central cargo build")


def cargo_test(worktree: Path, target_dir: Path, packages: Sequence[str],
               extra_args: Sequence[str] = (), timeout_s: float = 1800.0,
               log_dir: Optional[Path] = None) -> list[TestRun]:
    raise NotImplementedError("FAC-08: central cargo test")
