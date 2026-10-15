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

from pathlib import Path
from typing import Any, Optional

from .records import LeaseEntry, LeaseSpec, LeaseState


class LeaseError(RuntimeError):
    pass


def load_catalog(leases_dir: Path) -> list[LeaseSpec]:
    """All lease specs, sorted by lease_id."""
    raise NotImplementedError("FAC-02: load lease catalog")


def validate_catalog(specs: list[LeaseSpec], task_ids: set[str],
                     repo_root: Optional[Path] = None) -> list[dict[str, Any]]:
    """Static + (when repo_root given) marker-coverage validation; [] = valid."""
    raise NotImplementedError("FAC-02: validate lease catalog")


def file_owner_map(specs: list[LeaseSpec]) -> dict[str, str]:
    """repo-relative file -> lease_id (overlaps already rejected by validate_catalog)."""
    raise NotImplementedError("FAC-02: file owner map")


def sync_ledger(ledger_path: Path, specs: list[LeaseSpec]) -> dict[str, LeaseEntry]:
    raise NotImplementedError("FAC-02: sync ledger with catalog")


def grant(ledger_path: Path, lease_id: str, holder: str, ttl_s: int, base_sha: str) -> LeaseEntry:
    raise NotImplementedError("FAC-02: grant lease")


def renew(ledger_path: Path, lease_id: str, holder: str, ttl_s: int) -> LeaseEntry:
    raise NotImplementedError("FAC-02: renew lease")


def release(ledger_path: Path, lease_id: str, holder: str, note: str = "") -> LeaseEntry:
    raise NotImplementedError("FAC-02: release lease")


def expire_stale(ledger_path: Path, now_unix: int) -> list[str]:
    raise NotImplementedError("FAC-02: expire stale leases")


def transition(ledger_path: Path, lease_id: str, to: LeaseState, note: str = "",
               **fields: Any) -> LeaseEntry:
    """Generic guarded transition; ``fields`` may set head_sha/branch/holder."""
    raise NotImplementedError("FAC-02: guarded state transition")


def load_ledger(ledger_path: Path) -> dict[str, LeaseEntry]:
    """Unlocked snapshot read."""
    raise NotImplementedError("FAC-02: read ledger")
