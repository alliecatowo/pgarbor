"""SWARM-06: speculative implementation submission intake.

Lease FAC-05.

A worker finishes by committing on branch ``lease/<LEASE-ID>`` and calling
``submit(...)`` (or the CLI ``python -m pgarbor_factory submit --lease X --worker W
[--assumption ...]... [--behavior ...]... [--unresolved ...]...``). ``submit``:
  1. resolves head_sha of the branch and base_sha = ledger entry's base_sha;
  2. computes files_changed = gitutil.changed_files(base_sha, head_sha) paths;
  3. writes ``runs/submissions/<LEASE-ID>.json`` (records.Submission, canonical JSON); a
     resubmission overwrites it and keeps the previous file as
     ``<LEASE-ID>.<previous head_sha[:12]>.json``;
  4. ledger transition SPECULATING -> PATCH_SUBMITTED (leases.transition) with head_sha and
     branch.
No tests are required or run (RFC 003: worker verification is right-shifted). Claims are
stored verbatim; they never affect pass/fail.
"""

from __future__ import annotations

from pathlib import Path
from typing import Optional, Sequence

from .records import Submission


def submit(repo_root: Path, lease_id: str, worker: str, branch: Optional[str] = None,
           assumptions: Sequence[str] = (), behaviors: Sequence[str] = (),
           tests_added: Sequence[str] = (), unresolved: Sequence[str] = ()) -> Submission:
    raise NotImplementedError("FAC-05: record submission")


def load_submissions(repo_root: Path) -> dict[str, Submission]:
    """Current (non-archived) submission per lease id."""
    raise NotImplementedError("FAC-05: load submissions")
