"""SWARM-13: evidence pack builder and immutable evidence ledger.

Lease FAC-10.

``build_pack(...)`` assembles records.EvidencePack from campaign outputs:
  * git: gitutil.head_sha / current_branch / is_dirty (a dirty tree is allowed but recorded;
    reports print a warning banner);
  * toolchain: ``rustc -vV`` and ``cargo -V`` stdout verbatim;
  * platform: platform.system(), platform.release(), platform.machine(),
    platform.python_version();
  * reference: from the oracle fingerprint (image digest, server_version, fingerprint sha);
  * corpus: pgarbor_harness.hashing.tree_sha256 over cases (tests/cases/**/*.yaml), expected
    (tests/expected/**), verifier (config.VERIFIER_GLOBS), protected (config.PROTECTED_GLOBS),
    plus sha256 of canonical regress / isolation manifests (None if unavailable);
  * families: FamilyEvidence per phase; ``totals`` = per-key sums; ``critical`` =
    {"corruption": totals CORRUPTION, "crash": totals CRASH};
  * denominators per family: total_cataloged = sum(counts.values()), runnable = total minus
    (BLOCKED + INVALID_CASE + UNRUN), passed = PASS.

``save_pack(pack, run_dir)`` writes ``evidence.json`` with mode 0o444 after writing (immutable
by convention) and appends ``{"campaign_id", "git_sha", "path", "sha256", "totals"}`` to
``runs/state/evidence_ledger.jsonl``; updates ``runs/state/last_evidence.json`` pointer.
Refuses to overwrite an existing evidence.json (FileExistsError).

``family_from_results(name, results, cataloged_ids)``: build FamilyEvidence from
model.CaseResult-like objects (anything with ``.outcome``) — items in ``cataloged_ids`` with
no result count as UNRUN.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import subprocess
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from pgarbor_harness.config import PROTECTED_GLOBS, VERIFIER_GLOBS, Paths
from pgarbor_harness.hashing import matches_any, sha256_bytes, sha256_file
from pgarbor_harness.model import canonical_json, write_canonical_json

from . import gitutil
from .records import COUNT_KEYS, EvidencePack, FamilyEvidence, empty_counts, read_json

#: Oracle reference keys every pack must carry (model.ReferenceIdentity fields).
REFERENCE_KEYS: tuple[str, ...] = ("image_digest", "server_version", "fingerprint_sha256")

#: Campaign modes (records.EvidencePack.mode).
MODES: tuple[str, ...] = ("cathedral", "seams", "conventional")

#: Family-level statuses (records.FamilyEvidence.status).
FAMILY_STATUSES: tuple[str, ...] = ("RAN", "SKIPPED_PRECONDITION", "BLOCKED", "HARNESS_ERROR")

#: Count keys subtracted from total_cataloged to obtain the runnable denominator.
_NON_RUNNABLE_KEYS: tuple[str, ...] = ("BLOCKED", "INVALID_CASE", "UNRUN")

#: Corpus tree patterns (repo-relative).
CASES_GLOBS: tuple[str, ...] = ("tests/cases/**/*.yaml",)
EXPECTED_GLOBS: tuple[str, ...] = ("tests/expected/**",)

#: Ledger and pointer locations, relative to ``runs/state``.
LEDGER_NAME = "evidence_ledger.jsonl"
POINTER_NAME = "last_evidence.json"

_TOOL_TIMEOUT_S = 60.0


# ---------------------------------------------------------------------------------------------
# Family evidence
# ---------------------------------------------------------------------------------------------


def _outcome_key(result: Any) -> str:
    """Status column name for one CaseResult-like object (its ``.outcome``)."""
    try:
        outcome = result.outcome
    except AttributeError as e:
        raise TypeError(f"result {result!r} has no .outcome") from e
    key = outcome.value if isinstance(outcome, Enum) else str(outcome)
    if key not in COUNT_KEYS or key == "UNRUN":
        raise ValueError(f"result outcome {key!r} is not a countable status")
    return key


def _finalize_family(fam: FamilyEvidence) -> FamilyEvidence:
    """Validate a family and recompute its three denominators from its counts (RFC 002 §8).

    Returns a new FamilyEvidence; the input is not modified.
    """
    if fam.status not in FAMILY_STATUSES:
        raise ValueError(f"family {fam.name!r}: bad status {fam.status!r}")
    unknown = sorted(set(fam.counts) - set(COUNT_KEYS))
    if unknown:
        raise ValueError(f"family {fam.name!r}: unknown count keys {unknown}")
    counts = {k: int(fam.counts.get(k, 0)) for k in COUNT_KEYS}
    if any(v < 0 for v in counts.values()):
        raise ValueError(f"family {fam.name!r}: negative count")
    total = sum(counts.values())
    runnable = total - sum(counts[k] for k in _NON_RUNNABLE_KEYS)
    return FamilyEvidence(
        name=fam.name, status=fam.status, duration_s=float(fam.duration_s), counts=counts,
        total_cataloged=total, runnable=runnable, passed=counts["PASS"],
        artifacts=list(fam.artifacts), notes=list(fam.notes),
    )


def family_from_results(name: str, results: Iterable[Any], cataloged_ids: Iterable[str],
                        duration_s: float = 0.0, status: str = "RAN") -> FamilyEvidence:
    """Count one family. Each cataloged id with no result counts as UNRUN.

    Results carrying ``case_id`` are de-duplicated (the last result for an id wins, so
    retries are not double counted). Results without ``case_id`` are each counted once.
    Results whose id is not cataloged are still counted and noted.
    """
    if status not in FAMILY_STATUSES:
        raise ValueError(f"family {name!r}: bad status {status!r}")
    cataloged = list(dict.fromkeys(str(c) for c in cataloged_ids))
    cataloged_set = set(cataloged)

    by_id: dict[str, str] = {}
    anonymous: list[str] = []
    for r in results:
        key = _outcome_key(r)
        cid = getattr(r, "case_id", None)
        if cid is None:
            anonymous.append(key)
        else:
            by_id[str(cid)] = key

    counts = empty_counts()
    for key in by_id.values():
        counts[key] += 1
    for key in anonymous:
        counts[key] += 1
    counts["UNRUN"] += sum(1 for c in cataloged if c not in by_id)

    notes: list[str] = []
    stray = sum(1 for c in by_id if c not in cataloged_set)
    if stray:
        notes.append(f"{stray} result(s) with case ids not in the cataloged set")

    return _finalize_family(FamilyEvidence(
        name=name, status=status, duration_s=float(duration_s), counts=counts, notes=notes,
    ))


# ---------------------------------------------------------------------------------------------
# Corpus, toolchain and manifest hashing
# ---------------------------------------------------------------------------------------------


def _keep(rel: str, patterns: tuple[str, ...]) -> bool:
    parts = rel.split("/")
    if "__pycache__" in parts or ".git" in parts or rel.endswith(".pyc"):
        return False
    return matches_any(rel, patterns)


def _tree_files(root: Path, patterns: tuple[str, ...]) -> dict[str, str]:
    """{repo-relative path: sha256} for files matching any pattern, sorted by path.

    Same selection as hashing.list_tree, but only the literal directory prefix of each glob
    is walked (``target/`` and ``.claude/worktrees`` are never touched for ``tests/**``).
    """
    rels: set[str] = set()
    for pat in patterns:
        prefix: list[str] = []
        for seg in pat.split("/"):
            if any(ch in seg for ch in "*?"):
                break
            prefix.append(seg)
        start = root.joinpath(*prefix) if prefix else root
        if start.is_file():
            rel = start.relative_to(root).as_posix()
            if _keep(rel, patterns):
                rels.add(rel)
        elif start.is_dir():
            for dirpath, dirnames, filenames in os.walk(start):
                dirnames[:] = [d for d in dirnames if d not in ("__pycache__", ".git")]
                for fn in filenames:
                    full = Path(dirpath) / fn
                    if not full.is_file():
                        continue
                    rel = full.relative_to(root).as_posix()
                    if _keep(rel, patterns):
                        rels.add(rel)
    return {rel: sha256_file(root / rel) for rel in sorted(rels)}


def _tree_sha(root: Path, patterns: Iterable[str]) -> str:
    """hashing.tree_sha256 algorithm (byte-exact) restricted to the glob prefixes."""
    files = _tree_files(root, tuple(patterns))
    h = hashlib.sha256()
    for rel, digest in files.items():
        h.update(f"{rel}\0{digest}\n".encode("utf-8"))
    return h.hexdigest()


def _tool_stdout(argv: list[str]) -> str:
    try:
        p = subprocess.run(argv, capture_output=True, text=True, check=False,
                           timeout=_TOOL_TIMEOUT_S)
    except FileNotFoundError as e:
        raise RuntimeError(f"toolchain: {argv[0]} not found on PATH") from e
    except subprocess.TimeoutExpired as e:
        raise RuntimeError(f"toolchain: {' '.join(argv)} timed out") from e
    if p.returncode != 0:
        raise RuntimeError(
            f"toolchain: {' '.join(argv)} exited {p.returncode}: {p.stderr.strip()}")
    return p.stdout


def _toolchain_versions() -> dict[str, str]:
    """``rustc -vV`` and ``cargo -V`` stdout, verbatim. Missing tools are an error: a pack
    without its toolchain identity would not be reproducible."""
    return {"rustc": _tool_stdout(["rustc", "-vV"]), "cargo": _tool_stdout(["cargo", "-V"])}


def _manifest_sha(build: Callable[[Path], dict[str, Any]], directory: Path) -> Optional[str]:
    """sha256 of the canonical manifest document, or None when it cannot be built here
    (source tree absent, or the builder is still unimplemented)."""
    if not directory.is_dir():
        return None
    try:
        doc = build(directory)
    except (NotImplementedError, OSError):
        return None
    return sha256_bytes(canonical_json(doc).encode("utf-8"))


def _manifest_hashes(paths: Paths) -> tuple[Optional[str], Optional[str]]:
    """(regress manifest sha, isolation manifest sha); None when unavailable."""
    try:
        from pgarbor_harness.regress.manifest import build_manifest as regress_build
    except ImportError:
        regress_sha = None
    else:
        regress_sha = _manifest_sha(regress_build, paths.pg_regress_dir)
    try:
        from pgarbor_harness.isolation.spec_parser import build_manifest as iso_build
    except ImportError:
        isolation_sha = None
    else:
        isolation_sha = _manifest_sha(iso_build, paths.pg_isolation_dir)
    return regress_sha, isolation_sha


# ---------------------------------------------------------------------------------------------
# Pack
# ---------------------------------------------------------------------------------------------


def build_pack(repo_root: Path, campaign_id: str, epoch: int, mode: str,
               reference: dict[str, str], seeds: list[int], started_unix: int,
               finished_unix: int, build: dict[str, Any], families: list[FamilyEvidence],
               halt: Optional[dict[str, Any]] = None,
               resource: Optional[dict[str, Any]] = None) -> EvidencePack:
    """Assemble the evidence pack. Family denominators are recomputed from counts, so the
    invariant ``runnable == total - BLOCKED - INVALID_CASE - UNRUN`` always holds."""
    if mode not in MODES:
        raise ValueError(f"bad campaign mode {mode!r}")
    missing = [k for k in REFERENCE_KEYS if k not in reference]
    if missing:
        raise ValueError(f"reference identity missing keys {missing}")
    if finished_unix < started_unix:
        raise ValueError("finished_unix precedes started_unix")
    names = [f.name for f in families]
    if len(set(names)) != len(names):
        raise ValueError(f"duplicate family names in {names}")

    root = Path(repo_root).resolve()
    paths = Paths(root=root)

    git_info = {
        "sha": gitutil.head_sha(root),
        "branch": gitutil.current_branch(root),
        "dirty": gitutil.is_dirty(root),
    }
    regress_sha, isolation_sha = _manifest_hashes(paths)
    corpus: dict[str, Optional[str]] = {
        "cases": _tree_sha(root, CASES_GLOBS),
        "expected": _tree_sha(root, EXPECTED_GLOBS),
        "verifier": _tree_sha(root, VERIFIER_GLOBS),
        "protected": _tree_sha(root, PROTECTED_GLOBS),
        "regress_manifest": regress_sha,
        "isolation_manifest": isolation_sha,
    }

    fams = [_finalize_family(f) for f in families]
    totals = empty_counts()
    for f in fams:
        for k in COUNT_KEYS:
            totals[k] += f.counts[k]

    return EvidencePack(
        campaign_id=campaign_id,
        epoch=int(epoch),
        mode=mode,
        git=git_info,
        toolchain=_toolchain_versions(),
        platform={
            "os": platform.system(),
            "kernel": platform.release(),
            "arch": platform.machine(),
            "python": platform.python_version(),
        },
        reference={k: str(v) for k, v in reference.items()},
        corpus=corpus,
        seeds=[int(s) for s in seeds],
        started_unix=int(started_unix),
        finished_unix=int(finished_unix),
        build=dict(build),
        families=fams,
        totals=totals,
        critical={"corruption": totals["CORRUPTION"], "crash": totals["CRASH"]},
        halt=dict(halt or {}),
        resource=dict(resource or {}),
    )


# ---------------------------------------------------------------------------------------------
# Save, ledger, pointer
# ---------------------------------------------------------------------------------------------


def _rel_or_abs(p: Path, root: Path) -> str:
    try:
        return p.relative_to(root).as_posix()
    except ValueError:
        return p.as_posix()


def _append_ledger(ledger: Path, entry: dict[str, Any]) -> None:
    import fcntl

    ledger.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(entry, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
    with open(ledger, "a", encoding="utf-8", newline="\n") as f:
        fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            f.write(line)
            f.flush()
            os.fsync(f.fileno())
        finally:
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)


def save_pack(pack: EvidencePack, run_dir: Path, repo_root: Path) -> Path:
    """Write ``run_dir/evidence.json`` exclusively (FileExistsError if it already exists),
    make it read-only (0o444), append the ledger line and update the last-evidence pointer.
    """
    root = Path(repo_root).resolve()
    paths = Paths(root=root)
    run_dir = Path(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    evidence_path = (run_dir / "evidence.json").resolve()

    data = canonical_json(pack.to_json()).encode("utf-8")
    digest = sha256_bytes(data)

    try:
        fd = os.open(evidence_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o444)
    except FileExistsError as e:
        raise FileExistsError(
            f"{evidence_path} already exists; evidence packs are immutable") from e
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
    except BaseException:
        try:
            os.unlink(evidence_path)
        except OSError:
            pass
        raise
    os.chmod(evidence_path, 0o444)

    rel = _rel_or_abs(evidence_path, root)
    _append_ledger(paths.state / LEDGER_NAME, {
        "campaign_id": pack.campaign_id,
        "git_sha": pack.git.get("sha"),
        "path": rel,
        "sha256": digest,
        "totals": dict(pack.totals),
    })
    write_canonical_json(paths.state / POINTER_NAME, {
        "campaign_id": pack.campaign_id,
        "path": rel,
        "sha256": digest,
    })
    return evidence_path


def load_last(repo_root: Path) -> Optional[EvidencePack]:
    """Previous campaign's pack via runs/state/last_evidence.json (None if none).

    Raises ValueError when the pack's bytes no longer match the sha256 recorded in the
    pointer (the pack was altered after it was saved)."""
    root = Path(repo_root).resolve()
    pointer = read_json(Paths(root=root).state / POINTER_NAME, None)
    if pointer is None:
        return None
    target = root / pointer["path"]
    data = target.read_bytes()
    if sha256_bytes(data) != pointer["sha256"]:
        raise ValueError(f"{target}: sha256 does not match the last-evidence pointer")
    return EvidencePack.from_json(json.loads(data.decode("utf-8")))
