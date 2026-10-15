"""Tests for FAC-05 (SWARM-06 submission intake). Executed centrally with pytest."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from pgarbor_factory import gitutil
from pgarbor_factory.leases import LeaseError
from pgarbor_factory.records import LEDGER_FORMAT, LeaseEntry, LeaseState, Submission
from pgarbor_factory.submissions import load_submissions, submit
from pgarbor_harness.model import write_canonical_json

LEASE = "FAC-05"
BRANCH = f"lease/{LEASE}"
LEDGER_REL = Path("runs/state/leases.json")
SUBS_REL = Path("runs/submissions")


def _git(repo: Path, *args: str) -> str:
    cmd = ["git", "-c", "user.name=test", "-c", "user.email=test@example.invalid",
           "-c", "commit.gpgsign=false", *args]
    return subprocess.run(cmd, cwd=str(repo), check=True, capture_output=True,
                          text=True).stdout.strip()


def _commit(repo: Path, message: str) -> str:
    # Factory state under runs/ is never part of a worker's commit.
    _git(repo, "add", "-A", "--", ".", ":(exclude)runs")
    _git(repo, "commit", "-q", "-m", message)
    return _git(repo, "rev-parse", "HEAD")


def _write_ledger(repo: Path, state: LeaseState, base_sha: str) -> None:
    entry = LeaseEntry(lease_id=LEASE, crate="harness", state=state, holder="worker-1",
                       attempts=1, base_sha=base_sha)
    write_canonical_json(repo / LEDGER_REL,
                         {"format": LEDGER_FORMAT, "leases": {LEASE: entry.to_json()}})


@pytest.fixture
def repo(tmp_path: Path):
    """Repo with a base commit (frozen contract) and a lease branch carrying three changes:
    a.txt modified, b.txt added, c.txt deleted. Checked out on the lease branch."""
    r = tmp_path / "repo"
    r.mkdir()
    _git(r, "init", "-q")
    (r / "a.txt").write_text("one\n")
    (r / "c.txt").write_text("keep\n")
    base = _commit(r, "base")
    _git(r, "checkout", "-q", "-b", BRANCH)
    (r / "a.txt").write_text("two\n")
    (r / "b.txt").write_text("new\n")
    (r / "c.txt").unlink()
    _commit(r, "worker work")
    return r, base


def _read_ledger_entry(repo: Path) -> dict:
    doc = json.loads((repo / LEDGER_REL).read_text(encoding="utf-8"))
    return doc["leases"][LEASE]


def test_submit_records_diff_names_and_transitions(repo):
    r, base = repo
    _write_ledger(r, LeaseState.SPECULATING, base)
    head = _git(r, "rev-parse", BRANCH)

    sub = submit(r, LEASE, "worker-1", assumptions=["a1"], behaviors=["b1"],
                 tests_added=["t1"], unresolved=["u1"])

    expected_names = [p for _s, p in gitutil.changed_files(base, head, r)]
    assert expected_names == ["a.txt", "b.txt", "c.txt"]
    assert sub.files_changed == expected_names
    assert sub.base_sha == base
    assert sub.head_sha == head
    assert sub.branch == BRANCH
    assert sub.worker == "worker-1"
    assert sub.assumptions == ["a1"]
    assert sub.implemented_behaviors == ["b1"]
    assert sub.tests_added == ["t1"]
    assert sub.unresolved == ["u1"]
    assert sub.submitted_unix > 0

    on_disk = json.loads((r / SUBS_REL / f"{LEASE}.json").read_text(encoding="utf-8"))
    assert Submission.from_json(on_disk) == sub

    entry = _read_ledger_entry(r)
    assert entry["state"] == LeaseState.PATCH_SUBMITTED.value
    assert entry["head_sha"] == head
    assert entry["branch"] == BRANCH


def test_default_branch_is_lease_branch(repo):
    r, base = repo
    _write_ledger(r, LeaseState.SPECULATING, base)
    sub = submit(r, LEASE, "worker-1")
    assert sub.branch == BRANCH
    assert sub.files_changed == ["a.txt", "b.txt", "c.txt"]


def test_resubmission_archives_previous_record(repo):
    r, base = repo
    _write_ledger(r, LeaseState.SPECULATING, base)
    first = submit(r, LEASE, "worker-1")
    first_head = first.head_sha

    # Lease re-dispatched after a rejection: grant again and let the worker commit more.
    _write_ledger(r, LeaseState.SPECULATING, base)
    (r / "d.txt").write_text("more\n")
    second_head = _commit(r, "worker rework")
    second = submit(r, LEASE, "worker-2")

    archive = r / SUBS_REL / f"{LEASE}.{first_head[:12]}.json"
    assert archive.is_file()
    archived = Submission.from_json(json.loads(archive.read_text(encoding="utf-8")))
    assert archived == first

    current = load_submissions(r)
    assert set(current) == {LEASE}
    assert current[LEASE] == second
    assert current[LEASE].head_sha == second_head
    assert current[LEASE].files_changed == ["a.txt", "b.txt", "c.txt", "d.txt"]


@pytest.mark.parametrize("state", [LeaseState.SPEC_READY, LeaseState.PATCH_SUBMITTED,
                                   LeaseState.REJECTED])
def test_submit_on_non_speculating_lease_raises(repo, state):
    r, base = repo
    _write_ledger(r, state, base)
    with pytest.raises(LeaseError):
        submit(r, LEASE, "worker-1")
    assert not (r / SUBS_REL / f"{LEASE}.json").exists()


def test_submit_unknown_lease_raises(repo):
    r, base = repo
    _write_ledger(r, LeaseState.SPECULATING, base)
    with pytest.raises(LeaseError):
        submit(r, "FAC-99", "worker-1")


def test_load_submissions_empty_when_none(tmp_path: Path):
    assert load_submissions(tmp_path) == {}
