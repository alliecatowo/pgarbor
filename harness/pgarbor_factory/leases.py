"""SWARM-03: lease catalog loading, static validation, and the lease state ledger.

Lease FAC-02.

Catalog = every ``contracts/leases/*.json`` (format: ``{"crate", "contract_version",
"leases": [...]}``, see contracts/crates/harness.md §Leases) -> ``records.LeaseSpec`` list.

``validate_catalog`` errors (each a dict ``{"kind": ..., ...}`` like dag.validate_leases):
  * everything dag.validate_leases reports (unknown task, glob, overlap, protected path);
  * ``bad_lease_id``: not matching ``^[A-Z][A-Z0-9]{1,9}-\\d{2,3}$``; ``duplicate_lease_id`` across
    files; ``bad_tier``: model_tier not in haiku/sonnet/opus;
  * ``unknown_dependency``: depends_on_leases names a non-existent lease; ``lease_cycle``;
  * ``protected_file``: files matching ``**/lib.rs``, ``**/mod.rs``, ``**/Cargo.toml``,
    ``**/__init__.py``, ``**/__main__.py`` or pgarbor_harness.config.PROTECTED_GLOBS;
  * marker coverage (needs the working tree): for every file of the repo containing a stub
    marker — Rust ``PgError::unimplemented("<ID>"`` or Python
    ``NotImplementedError("<ID>:`` — the file must belong to exactly one lease
    (``unleased_stub_file``) and every marker ID in it must equal that lease's id
    (``marker_lease_mismatch`` with file/line/marker). Use inventory.scan_markers.
  * ``prefix_mismatch``: harness leases must start with HAR-/FAC- and only list files under
    ``harness/**`` or ``tests/cases/README.md``; Rust crate leases must not list harness files.

Ledger (``runs/state/leases.json``, records.locked_json + records.empty_ledger):
  * ``sync_ledger``: add a SPEC_READY entry for every catalog lease not yet present; never
    delete entries (a removed lease is marked in history with note "removed from catalog");
  * ``grant(lease_id, holder, ttl_s)``: SPEC_READY -> SPECULATING, holder, expires_unix =
    now+ttl, attempts += 1, base_sha = HEAD. ``depends_on_leases`` does NOT gate granting:
    speculative work waits only for frozen contracts, never for upstream implementations
    (RFC 003 §2). Grant checks the state and, defensively, that no OTHER SPECULATING lease
    lists an overlapping file;
  * ``renew``, ``release`` (SPECULATING -> SPEC_READY), ``expire_stale(now)`` (all
    SPECULATING with expires_unix < now -> SPEC_READY, note "expired");
  * ``transition(lease_id, to, note)``: enforce records.LEASE_TRANSITIONS, append Transition;
    illegal -> LeaseError.
All ledger mutations happen inside one ``locked_json`` block (atomic, multi-process safe).
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Optional

from pgarbor_harness.config import HARNESS_LEASE_PREFIXES, PROTECTED_GLOBS, paths
from pgarbor_harness.hashing import glob_match

from . import records
from .dag import find_cycle, validate_leases
from .inventory import scan_markers
from .records import LeaseEntry, LeaseSpec, LeaseState


class LeaseError(RuntimeError):
    pass


_LEASE_ID_RE = re.compile(r"^[A-Z][A-Z0-9]{1,9}-\d{2,3}$")
_MODEL_TIERS = ("haiku", "sonnet", "opus")

#: Protected by file name anywhere in the tree (in addition to config.PROTECTED_GLOBS).
_NAME_PROTECTED_GLOBS: tuple[str, ...] = (
    "**/lib.rs", "**/mod.rs", "**/Cargo.toml", "**/__init__.py", "**/__main__.py",
)
_ALL_PROTECTED_GLOBS: tuple[str, ...] = tuple(PROTECTED_GLOBS) + _NAME_PROTECTED_GLOBS

#: Non-``harness/`` files that count as harness files for prefix checks.
_HARNESS_EXTRA_FILES: tuple[str, ...] = ("tests/cases/README.md",)

_REMOVED_NOTE = "removed from catalog"
_EXPIRED_NOTE = "expired"
_LEDGER_FIELDS = ("head_sha", "branch", "holder")


# ---------------------------------------------------------------------------------------------
# Catalog
# ---------------------------------------------------------------------------------------------


def _leases_dir() -> Path:
    """Catalog directory used by grant's overlap check (tests may monkeypatch this)."""
    return paths().leases_dir


def _catalog_by_id() -> dict[str, LeaseSpec]:
    return {s.lease_id: s for s in load_catalog(_leases_dir())}


def load_catalog(leases_dir: Path) -> list[LeaseSpec]:
    """All lease specs, sorted by lease_id."""
    specs: list[LeaseSpec] = []
    for path in sorted(Path(leases_dir).glob("*.json")):
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as e:
            raise LeaseError(f"{path}: unreadable lease file: {e}") from e
        if not isinstance(doc, dict) or "crate" not in doc:
            raise LeaseError(f"{path}: lease file must be an object with a 'crate' key")
        crate = str(doc["crate"])
        version = str(doc.get("contract_version", "v1"))
        raw_leases = doc.get("leases", [])
        if not isinstance(raw_leases, list):
            raise LeaseError(f"{path}: 'leases' must be a list")
        for raw in raw_leases:
            if not isinstance(raw, dict):
                raise LeaseError(f"{path}: lease entry is not an object")
            try:
                specs.append(LeaseSpec.from_json(crate, version, raw))
            except (KeyError, TypeError, ValueError) as e:
                raise LeaseError(f"{path}: bad lease entry {raw.get('lease_id')!r}: {e!r}") from e
    specs.sort(key=lambda s: s.lease_id)
    return specs


def _is_harness_file(path: str) -> bool:
    return path.startswith("harness/") or path in _HARNESS_EXTRA_FILES


def _prefix_problems(spec: LeaseSpec) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    crate_is_harness = spec.crate == "harness"
    id_is_harness = spec.lease_id.startswith(tuple(HARNESS_LEASE_PREFIXES))
    if crate_is_harness != id_is_harness:
        out.append({"kind": "prefix_mismatch", "lease": spec.lease_id, "crate": spec.crate,
                    "reason": "harness leases need a HAR-/FAC- id and the harness crate"})
    for f in spec.files:
        if _is_harness_file(f) != crate_is_harness:
            out.append({"kind": "prefix_mismatch", "lease": spec.lease_id, "file": f,
                        "crate": spec.crate,
                        "reason": ("harness lease lists a non-harness file" if crate_is_harness
                                   else "Rust crate lease lists a harness file")})
    return out


def validate_catalog(specs: list[LeaseSpec], task_ids: set[str],
                     repo_root: Optional[Path] = None) -> list[dict[str, Any]]:
    """Static + (when repo_root given) marker-coverage validation; [] = valid."""
    errors: list[dict[str, Any]] = []

    # Base checks (unknown task, globs, overlap, protected paths) come from dag.validate_leases.
    doc = {"leases": [{"lease_id": s.lease_id, "task_ids": list(s.task_ids),
                       "files": list(s.files)} for s in specs]}
    dag_errors, _stats = validate_leases([doc], set(task_ids))
    errors.extend(dag_errors)

    counts: dict[str, int] = {}
    crates: dict[str, list[str]] = {}
    edges: dict[str, list[str]] = {}
    for s in specs:
        counts[s.lease_id] = counts.get(s.lease_id, 0) + 1
        crates.setdefault(s.lease_id, []).append(s.crate)
        edges.setdefault(s.lease_id, []).extend(s.depends_on_leases)

    for lid in sorted(counts):
        if counts[lid] > 1:
            errors.append({"kind": "duplicate_lease_id", "lease": lid, "count": counts[lid],
                           "crates": crates[lid]})

    for s in specs:
        if not _LEASE_ID_RE.match(s.lease_id):
            errors.append({"kind": "bad_lease_id", "lease": s.lease_id, "crate": s.crate})
        if s.model_tier not in _MODEL_TIERS:
            errors.append({"kind": "bad_tier", "lease": s.lease_id, "tier": s.model_tier})
        for dep in s.depends_on_leases:
            if dep not in counts:
                errors.append({"kind": "unknown_dependency", "lease": s.lease_id, "dep": dep})
        for f in s.files:
            if any(glob_match(f, p) for p in _ALL_PROTECTED_GLOBS):
                errors.append({"kind": "protected_file", "lease": s.lease_id, "file": f})
        errors.extend(_prefix_problems(s))

    nodes = sorted(counts)
    cycle = find_cycle(nodes, edges)
    if cycle:
        errors.append({"kind": "lease_cycle", "cycle": cycle})

    if repo_root is not None:
        owners = file_owner_map(specs)
        unleased: dict[str, list[str]] = {}
        for m in scan_markers(Path(repo_root)):
            owner = owners.get(m.file)
            if owner is None:
                unleased.setdefault(m.file, []).append(m.lease_id)
            elif m.lease_id != owner:
                errors.append({"kind": "marker_lease_mismatch", "file": m.file, "line": m.line,
                               "marker": m.lease_id, "lease": owner})
        for f in sorted(unleased):
            errors.append({"kind": "unleased_stub_file", "file": f,
                           "markers": sorted(set(unleased[f]))})
    return errors


def file_owner_map(specs: list[LeaseSpec]) -> dict[str, str]:
    """repo-relative file -> lease_id (overlaps already rejected by validate_catalog)."""
    owners: dict[str, str] = {}
    for s in specs:
        for f in s.files:
            owners.setdefault(f, s.lease_id)
    return owners


# ---------------------------------------------------------------------------------------------
# Ledger
# ---------------------------------------------------------------------------------------------


def _check_doc(doc: Any, ledger_path: Path) -> None:
    if (not isinstance(doc, dict) or doc.get("format") != records.LEDGER_FORMAT
            or not isinstance(doc.get("leases"), dict)):
        raise LeaseError(f"{ledger_path}: not a {records.LEDGER_FORMAT} ledger")


def _entry(doc: dict[str, Any], lease_id: str) -> LeaseEntry:
    raw = doc["leases"].get(lease_id)
    if raw is None:
        raise LeaseError(f"{lease_id}: not in the ledger (run sync_ledger first)")
    return LeaseEntry.from_json(raw)


def _store(doc: dict[str, Any], entry: LeaseEntry) -> LeaseEntry:
    doc["leases"][entry.lease_id] = entry.to_json()
    return entry


def _move(entry: LeaseEntry, to: LeaseState, at_unix: int, note: str) -> None:
    """Apply one state change, enforcing records.LEASE_TRANSITIONS."""
    if to not in records.LEASE_TRANSITIONS[entry.state]:
        raise LeaseError(f"{entry.lease_id}: illegal transition {entry.state.value} -> {to.value}")
    entry.history.append(records.Transition(at_unix=at_unix, from_state=entry.state.value,
                                            to_state=to.value, note=note))
    entry.state = to


def _require_ttl(ttl_s: int) -> int:
    ttl = int(ttl_s)
    if ttl <= 0:
        raise LeaseError(f"ttl_s must be positive, got {ttl_s}")
    return ttl


def _require_holder(entry: LeaseEntry, holder: str, action: str) -> None:
    if entry.state != LeaseState.SPECULATING:
        raise LeaseError(f"{entry.lease_id}: cannot {action} from {entry.state.value}")
    if entry.holder != holder:
        raise LeaseError(f"{entry.lease_id}: {action} by {holder!r} but holder is "
                         f"{entry.holder!r}")


def sync_ledger(ledger_path: Path, specs: list[LeaseSpec]) -> dict[str, LeaseEntry]:
    catalog_ids = {s.lease_id for s in specs}
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        leases = doc["leases"]
        now = records.now_unix()
        for s in specs:
            if s.lease_id not in leases:
                leases[s.lease_id] = LeaseEntry(lease_id=s.lease_id, crate=s.crate).to_json()
        for lid in sorted(leases):
            if lid in catalog_ids:
                continue
            entry = LeaseEntry.from_json(leases[lid])
            if any(t.note == _REMOVED_NOTE for t in entry.history):
                continue
            entry.history.append(records.Transition(at_unix=now, from_state=entry.state.value,
                                                    to_state=entry.state.value,
                                                    note=_REMOVED_NOTE))
            _store(doc, entry)
        return {lid: LeaseEntry.from_json(raw) for lid, raw in sorted(leases.items())}


def grant(ledger_path: Path, lease_id: str, holder: str, ttl_s: int, base_sha: str) -> LeaseEntry:
    ttl = _require_ttl(ttl_s)
    catalog = _catalog_by_id()
    spec = catalog.get(lease_id)
    if spec is None:
        raise LeaseError(f"{lease_id}: not in the lease catalog")
    now = records.now_unix()
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        entry = _entry(doc, lease_id)
        if entry.state != LeaseState.SPEC_READY:
            raise LeaseError(f"{lease_id}: cannot grant from {entry.state.value}")
        mine = set(spec.files)
        for other_id, raw in sorted(doc["leases"].items()):
            if other_id == lease_id:
                continue
            other = LeaseEntry.from_json(raw)
            if other.state != LeaseState.SPECULATING:
                continue
            other_spec = catalog.get(other_id)
            if other_spec is not None and mine.intersection(other_spec.files):
                raise LeaseError(f"{lease_id}: files overlap speculating lease {other_id}")
        _move(entry, LeaseState.SPECULATING, now, f"granted to {holder}")
        entry.holder = holder
        entry.expires_unix = now + ttl
        entry.attempts += 1
        entry.base_sha = base_sha
        return _store(doc, entry)


def renew(ledger_path: Path, lease_id: str, holder: str, ttl_s: int) -> LeaseEntry:
    ttl = _require_ttl(ttl_s)
    now = records.now_unix()
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        entry = _entry(doc, lease_id)
        _require_holder(entry, holder, "renew")
        if entry.expires_unix is not None and entry.expires_unix < now:
            raise LeaseError(f"{lease_id}: lease already expired; it must be re-granted")
        entry.expires_unix = now + ttl
        return _store(doc, entry)


def release(ledger_path: Path, lease_id: str, holder: str, note: str = "") -> LeaseEntry:
    now = records.now_unix()
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        entry = _entry(doc, lease_id)
        _require_holder(entry, holder, "release")
        _move(entry, LeaseState.SPEC_READY, now, note or "released")
        entry.holder = None
        entry.expires_unix = None
        return _store(doc, entry)


def expire_stale(ledger_path: Path, now_unix: int) -> list[str]:
    expired: list[str] = []
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        for lid in sorted(doc["leases"]):
            entry = LeaseEntry.from_json(doc["leases"][lid])
            if (entry.state == LeaseState.SPECULATING and entry.expires_unix is not None
                    and entry.expires_unix < now_unix):
                _move(entry, LeaseState.SPEC_READY, now_unix, _EXPIRED_NOTE)
                entry.holder = None
                entry.expires_unix = None
                _store(doc, entry)
                expired.append(lid)
    return expired


def transition(ledger_path: Path, lease_id: str, to: LeaseState, note: str = "",
               **fields: Any) -> LeaseEntry:
    """Generic guarded transition; ``fields`` may set head_sha/branch/holder."""
    try:
        target = LeaseState(to)
    except ValueError as e:
        raise LeaseError(f"{lease_id}: unknown lease state {to!r}") from e
    if target == LeaseState.SPECULATING:
        raise LeaseError(f"{lease_id}: use grant() to enter SPECULATING")
    unknown = sorted(set(fields) - set(_LEDGER_FIELDS))
    if unknown:
        raise LeaseError(f"{lease_id}: transition cannot set {unknown}")
    now = records.now_unix()
    with records.locked_json(Path(ledger_path), records.empty_ledger()) as doc:
        _check_doc(doc, Path(ledger_path))
        entry = _entry(doc, lease_id)
        _move(entry, target, now, note)
        if target == LeaseState.SPEC_READY:
            entry.holder = None
            entry.expires_unix = None
        for key, value in fields.items():
            setattr(entry, key, value)
        return _store(doc, entry)


def load_ledger(ledger_path: Path) -> dict[str, LeaseEntry]:
    """Unlocked snapshot read."""
    path = Path(ledger_path)
    doc = records.read_json(path, None)
    if doc is None:
        return {}
    _check_doc(doc, path)
    return {lid: LeaseEntry.from_json(raw) for lid, raw in sorted(doc["leases"].items())}
