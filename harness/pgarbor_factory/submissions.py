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

import os
from pathlib import Path
from typing import Optional, Sequence

from pgarbor_harness.model import write_canonical_json

from .gitutil import GitError, changed_files, rev_parse
from .leases import LeaseError, load_ledger, transition
from .records import LeaseState, Submission, now_unix, read_json


def _ledger_path(repo_root: Path) -> Path:
    return repo_root / "runs" / "state" / "leases.json"


def _submissions_dir(repo_root: Path) -> Path:
    return repo_root / "runs" / "submissions"


def submit(repo_root: Path, lease_id: str, worker: str, branch: Optional[str] = None,
           assumptions: Sequence[str] = (), behaviors: Sequence[str] = (),
           tests_added: Sequence[str] = (), unresolved: Sequence[str] = ()) -> Submission:
    # CONTRACT (signature frozen)
    ledger_path = _ledger_path(repo_root)
    entry = load_ledger(ledger_path).get(lease_id)
    if entry is None:
        raise LeaseError(f"{lease_id}: no ledger entry; the lease was never granted")
    if entry.state is not LeaseState.SPECULATING:
        raise LeaseError(
            f"{lease_id}: cannot submit in state {entry.state.value}; expected SPECULATING")
    if not entry.base_sha:
        raise LeaseError(f"{lease_id}: ledger entry has no base_sha")

    branch_name = branch if branch is not None else f"lease/{lease_id}"
    try:
        head = rev_parse(f"refs/heads/{branch_name}", repo_root)
    except GitError as exc:
        raise LeaseError(f"{lease_id}: branch {branch_name} not found: {exc}") from exc
    files = [path for _status, path in changed_files(entry.base_sha, head, repo_root)]

    sub = Submission(
        lease_id=lease_id,
        worker=worker,
        base_sha=entry.base_sha,
        branch=branch_name,
        head_sha=head,
        files_changed=files,
        assumptions=list(assumptions),
        implemented_behaviors=list(behaviors),
        tests_added=list(tests_added),
        unresolved=list(unresolved),
        submitted_unix=now_unix(),
    )

    sub_dir = _submissions_dir(repo_root)
    current = sub_dir / f"{lease_id}.json"
    if current.exists():
        # Resubmission: keep the previous record under its head_sha.
        prev = Submission.from_json(read_json(current))
        os.replace(current, sub_dir / f"{lease_id}.{prev.head_sha[:12]}.json")
    write_canonical_json(current, sub.to_json())

    transition(ledger_path, lease_id, LeaseState.PATCH_SUBMITTED,
               note=f"submitted by {worker}", head_sha=head, branch=branch_name)
    return sub


def load_submissions(repo_root: Path) -> dict[str, Submission]:
    """Current (non-archived) submission per lease id."""
    # CONTRACT (signature frozen)
    sub_dir = _submissions_dir(repo_root)
    out: dict[str, Submission] = {}
    if not sub_dir.is_dir():
        return out
    for path in sorted(sub_dir.glob("*.json")):
        name = path.name
        # Current records are "<LEASE-ID>.json" (lease ids contain no dots). Archives are
        # "<LEASE-ID>.<sha12>.json" and temp files start with a dot; both are skipped.
        if name.startswith(".") or name.count(".") != 1:
            continue
        sub = Submission.from_json(read_json(path))
        out[sub.lease_id] = sub
    return out
