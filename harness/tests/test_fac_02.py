"""FAC-02 tests: lease catalog validation and the lease ledger state machine.

Ledger tests use tmp_path ledgers and a synthetic catalog (monkeypatched into
``leases._leases_dir``), so they do not depend on the real contracts/leases files.
"""

import json
from pathlib import Path

import pytest

from pgarbor_factory import inventory, leases
from pgarbor_factory.leases import LeaseError
from pgarbor_factory.records import LeaseSpec, LeaseState
from pgarbor_harness.config import paths

TASKS = {"ORACLE-01", "SWARM-03"}


def _spec(lid, files=(), crate="harness", tier="haiku", deps=()):
    return LeaseSpec(lease_id=lid, crate=crate, task_ids=["SWARM-03"], files=list(files),
                     model_tier=tier, summary="s", spec="x", depends_on_leases=list(deps))


def _lease_dict(lid, files, deps=()):
    return {"lease_id": lid, "task_ids": ["SWARM-03"], "files": list(files),
            "model_tier": "haiku", "summary": "s", "spec": "x", "reads": [],
            "depends_on_leases": list(deps), "acceptance": []}


def _write_catalog(dir_: Path, crate: str, lease_list: list) -> None:
    dir_.mkdir(parents=True, exist_ok=True)
    doc = {"crate": crate, "contract_version": "v1", "leases": lease_list}
    (dir_ / f"{crate}.json").write_text(json.dumps(doc), encoding="utf-8")


@pytest.fixture
def catalog(tmp_path, monkeypatch):
    d = tmp_path / "leases"
    _write_catalog(d, "harness", [
        _lease_dict("FAC-90", ["harness/pgarbor_factory/a.py"]),
        _lease_dict("FAC-91", ["harness/pgarbor_factory/b.py"]),
        _lease_dict("HAR-90", ["harness/pgarbor_harness/c.py"]),
    ])
    monkeypatch.setattr(leases, "_leases_dir", lambda: d)
    return d


@pytest.fixture
def ledger(tmp_path):
    return tmp_path / "state" / "leases.json"


def _kinds(errors):
    return [e["kind"] for e in errors]


# ---------------------------------------------------------------------------------------------
# Catalog validation
# ---------------------------------------------------------------------------------------------


def test_real_catalog_static_checks_clean():
    specs = leases.load_catalog(paths().leases_dir)
    assert specs, "contracts/leases must contain leases"
    task_ids = {t["id"] for t in json.loads(paths().dag.read_text(encoding="utf-8"))["tasks"]}
    assert leases.validate_catalog(specs, task_ids) == []


def test_real_catalog_marker_coverage_clean():
    specs = leases.load_catalog(paths().leases_dir)
    task_ids = {t["id"] for t in json.loads(paths().dag.read_text(encoding="utf-8"))["tasks"]}
    assert leases.validate_catalog(specs, task_ids, repo_root=paths().root) == []


def test_overlap_flagged():
    specs = [_spec("FAC-90", ["harness/x.py"]), _spec("FAC-91", ["harness/x.py"])]
    assert "lease_overlap" in _kinds(leases.validate_catalog(specs, TASKS))


def test_protected_init_py_flagged():
    specs = [_spec("FAC-90", ["harness/pgarbor_factory/__init__.py"])]
    errs = leases.validate_catalog(specs, TASKS)
    assert any(e["kind"] == "protected_file" and e["file"].endswith("__init__.py") for e in errs)


def test_protected_rust_lib_rs_flagged():
    specs = [_spec("STO-90", ["crates/pgarbor-storage/src/lib.rs"], crate="pgarbor-storage")]
    errs = leases.validate_catalog(specs, TASKS)
    assert "protected_file" in _kinds(errs)


def test_protected_contracts_path_flagged():
    specs = [_spec("FAC-90", ["contracts/ARCHITECTURE.md"])]
    assert "protected_file" in _kinds(leases.validate_catalog(specs, TASKS))


def test_marker_id_mismatch_and_unleased_file(monkeypatch, tmp_path):
    specs = [_spec("FAC-90", ["harness/pgarbor_factory/a.py"]),
             _spec("FAC-91", ["harness/pgarbor_factory/b.py"])]
    fake = [
        inventory.Marker(file="harness/pgarbor_factory/a.py", line=3, lease_id="FAC-91", what="w"),
        inventory.Marker(file="harness/pgarbor_factory/zz.py", line=1, lease_id="FAC-90", what="w"),
    ]
    monkeypatch.setattr(leases, "scan_markers", lambda root: fake)
    errs = leases.validate_catalog(specs, TASKS, repo_root=tmp_path)
    mismatch = [e for e in errs if e["kind"] == "marker_lease_mismatch"]
    assert mismatch == [{"kind": "marker_lease_mismatch", "file": "harness/pgarbor_factory/a.py",
                         "line": 3, "marker": "FAC-91", "lease": "FAC-90"}]
    unleased = [e for e in errs if e["kind"] == "unleased_stub_file"]
    assert [e["file"] for e in unleased] == ["harness/pgarbor_factory/zz.py"]


def test_marker_check_skipped_without_repo_root(monkeypatch):
    def boom(root):
        raise AssertionError("scan_markers must not run without repo_root")
    monkeypatch.setattr(leases, "scan_markers", boom)
    assert leases.validate_catalog([_spec("FAC-90", ["harness/a.py"])], TASKS) == []


def test_bad_id_tier_dependency_and_cycle():
    specs = [
        _spec("fac-90", ["harness/a.py"], tier="gpt"),
        _spec("FAC-91", ["harness/b.py"], deps=["FAC-99"]),
        _spec("FAC-92", ["harness/c.py"], deps=["FAC-93"]),
        _spec("FAC-93", ["harness/d.py"], deps=["FAC-92"]),
    ]
    kinds = _kinds(leases.validate_catalog(specs, TASKS))
    assert "bad_lease_id" in kinds
    assert "bad_tier" in kinds
    assert "unknown_dependency" in kinds
    assert "lease_cycle" in kinds


def test_duplicate_lease_id_flagged():
    specs = [_spec("FAC-90", ["harness/a.py"]), _spec("FAC-90", ["harness/b.py"])]
    assert "duplicate_lease_id" in _kinds(leases.validate_catalog(specs, TASKS))


def test_prefix_mismatch_flagged():
    specs = [
        _spec("FAC-90", ["crates/pgarbor-storage/src/x.rs"], crate="pgarbor-storage"),
        _spec("PST-90", ["harness/pgarbor_harness/y.py"], crate="pgarbor-storage"),
    ]
    errs = leases.validate_catalog(specs, TASKS)
    assert _kinds(errs).count("prefix_mismatch") >= 2


# ---------------------------------------------------------------------------------------------
# Ledger
# ---------------------------------------------------------------------------------------------


def test_sync_creates_spec_ready_entries(catalog, ledger):
    specs = leases.load_catalog(catalog)
    entries = leases.sync_ledger(ledger, specs)
    assert sorted(entries) == ["FAC-90", "FAC-91", "HAR-90"]
    assert all(e.state == LeaseState.SPEC_READY for e in entries.values())
    assert leases.load_ledger(ledger)["FAC-90"].crate == "harness"


def test_sync_marks_removed_lease_once(catalog, ledger):
    specs = leases.load_catalog(catalog)
    leases.sync_ledger(ledger, specs)
    entries = leases.sync_ledger(ledger, [s for s in specs if s.lease_id != "FAC-91"])
    assert "FAC-91" in entries
    again = leases.sync_ledger(ledger, [s for s in specs if s.lease_id != "FAC-91"])
    notes = [t.note for t in again["FAC-91"].history]
    assert notes.count("removed from catalog") == 1


def test_sync_rejects_foreign_ledger(ledger):
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(json.dumps({"format": "other/1", "leases": {}}), encoding="utf-8")
    with pytest.raises(LeaseError):
        leases.sync_ledger(ledger, [])


def test_illegal_transition_spec_ready_to_verified(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    with pytest.raises(LeaseError):
        leases.transition(ledger, "FAC-90", LeaseState.VERIFIED, "skip")
    assert leases.load_ledger(ledger)["FAC-90"].state == LeaseState.SPEC_READY


def test_transition_rejects_unknown_fields_and_speculating(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    with pytest.raises(LeaseError):
        leases.transition(ledger, "FAC-90", LeaseState.PATCH_SUBMITTED, "x", attempts=9)
    with pytest.raises(LeaseError):
        leases.transition(ledger, "FAC-90", LeaseState.SPECULATING, "x")


def test_full_legal_path_to_verified(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    leases.grant(ledger, "FAC-90", "w1", 600, "base")
    leases.transition(ledger, "FAC-90", LeaseState.PATCH_SUBMITTED, "submitted",
                      head_sha="deadbeef", branch="lease/FAC-90")
    leases.transition(ledger, "FAC-90", LeaseState.ASSEMBLED, "assembled")
    leases.transition(ledger, "FAC-90", LeaseState.CAMPAIGN_PASS, "pass")
    entry = leases.transition(ledger, "FAC-90", LeaseState.VERIFIED, "verified")
    assert entry.state == LeaseState.VERIFIED
    assert entry.head_sha == "deadbeef" and entry.branch == "lease/FAC-90"
    assert [t.to_state for t in entry.history][-1] == "VERIFIED"


def test_grant_renew_release_cycle(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    entry = leases.grant(ledger, "FAC-90", "w1", 600, "basesha")
    assert entry.state == LeaseState.SPECULATING
    assert entry.holder == "w1"
    assert entry.attempts == 1
    assert entry.base_sha == "basesha"
    assert entry.expires_unix is not None and entry.expires_unix > 0
    with pytest.raises(LeaseError):
        leases.grant(ledger, "FAC-90", "w2", 600, "basesha")
    with pytest.raises(LeaseError):
        leases.renew(ledger, "FAC-90", "w2", 600)
    renewed = leases.renew(ledger, "FAC-90", "w1", 1200)
    assert renewed.expires_unix >= entry.expires_unix
    released = leases.release(ledger, "FAC-90", "w1", "done early")
    assert released.state == LeaseState.SPEC_READY
    assert released.holder is None and released.expires_unix is None
    assert released.history[-1].note == "done early"
    regranted = leases.grant(ledger, "FAC-90", "w2", 600, "basesha2")
    assert regranted.attempts == 2


def test_grant_rejects_unknown_lease_and_bad_ttl(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    with pytest.raises(LeaseError):
        leases.grant(ledger, "FAC-99", "w1", 600, "base")
    with pytest.raises(LeaseError):
        leases.grant(ledger, "FAC-90", "w1", 0, "base")
    assert leases.load_ledger(ledger)["FAC-90"].state == LeaseState.SPEC_READY


def test_grant_refuses_overlapping_speculating_lease(catalog, ledger):
    # Two leases claiming the same file: grant must refuse the second while the first is live.
    _write_catalog(catalog, "harness", [
        _lease_dict("FAC-90", ["harness/shared.py"]),
        _lease_dict("FAC-91", ["harness/shared.py"]),
    ])
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    leases.grant(ledger, "FAC-90", "w1", 600, "base")
    with pytest.raises(LeaseError):
        leases.grant(ledger, "FAC-91", "w2", 600, "base")
    assert leases.load_ledger(ledger)["FAC-91"].state == LeaseState.SPEC_READY


def test_expire_stale_returns_lease_to_spec_ready(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    entry = leases.grant(ledger, "FAC-90", "w1", 100, "base")
    leases.grant(ledger, "FAC-91", "w2", 100000, "base")
    assert leases.expire_stale(ledger, entry.expires_unix) == []
    expired = leases.expire_stale(ledger, entry.expires_unix + 1)
    assert expired == ["FAC-90"]
    after = leases.load_ledger(ledger)
    assert after["FAC-90"].state == LeaseState.SPEC_READY
    assert after["FAC-90"].holder is None
    assert after["FAC-90"].history[-1].note == "expired"
    assert after["FAC-91"].state == LeaseState.SPECULATING


def test_renew_after_expiry_refused(catalog, ledger):
    leases.sync_ledger(ledger, leases.load_catalog(catalog))
    leases.grant(ledger, "FAC-90", "w1", 600, "base")
    doc = json.loads(ledger.read_text(encoding="utf-8"))
    doc["leases"]["FAC-90"]["expires_unix"] = 1
    ledger.write_text(json.dumps(doc), encoding="utf-8")
    with pytest.raises(LeaseError):
        leases.renew(ledger, "FAC-90", "w1", 600)


def test_load_ledger_missing_is_empty(tmp_path):
    assert leases.load_ledger(tmp_path / "absent" / "leases.json") == {}
