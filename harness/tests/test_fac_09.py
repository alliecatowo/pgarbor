"""FAC-09: campaign orchestration tests (every phase function monkeypatched)."""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from pgarbor_factory import build, campaign, evidence
from pgarbor_factory.guard import GuardReport, GuardViolation
from pgarbor_factory.records import EvidencePack, FamilyEvidence, empty_counts


@pytest.fixture
def cfg(tmp_path: Path) -> campaign.CampaignConfig:
    return campaign.CampaignConfig(repo_root=tmp_path)


def _patch_all_ok(monkeypatch, calls: list[str]) -> None:
    monkeypatch.setattr(campaign, "_head_sha", lambda root: "abcdef0123456789")
    monkeypatch.setattr(campaign, "_phase_guard", lambda cfg: GuardReport(True))

    def inv(cfg, ctx):
        calls.append("inventory")

    def bld(cfg, ctx):
        calls.append("build")
        ctx.build_ok = True
        ctx.build_dict = {"ok": True, "duration_s": 1.0, "error_clusters": 0, "log": ""}

    def oracle(cfg, ctx):
        calls.append("oracle")

    monkeypatch.setattr(campaign, "_phase_inventory", inv)
    monkeypatch.setattr(campaign, "_phase_build", bld)
    monkeypatch.setattr(campaign, "_phase_oracle", oracle)
    for key, names in (("storage_fault", ["storage-fault"]), ("differential", ["differential"]),
                       ("regress", list(campaign._REGRESS_FAMILIES)),
                       ("isolation", ["isolation"]), ("wire", ["wire"]),
                       ("clients", ["clients"])):
        def phase(cfg, ctx, key=key, names=names):
            calls.append(key)
            return [campaign._make_family(n, "RAN", ["PASS", "PASS"], 2) for n in names]

        monkeypatch.setattr(campaign, f"_phase_{key}", phase)


def _capture_finalize(monkeypatch) -> list:
    captured: list = []

    def fin(cfg, ctx, families, started, t0, ru0):
        captured.extend(families)
        return campaign.CampaignResult(ctx.campaign_id, ctx.run_dir, 0)

    monkeypatch.setattr(campaign, "_finalize", fin)
    return captured


def test_campaign_id_format():
    cid = campaign._new_campaign_id("0123456789abcdef", 0)
    assert cid == "19700101T000000Z-01234567"


def test_guard_refusal_exits_3_and_runs_nothing(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)
    bad = GuardReport(False, [GuardViolation("selftest_failed", "", "x")])
    monkeypatch.setattr(campaign, "_phase_guard", lambda cfg: bad)

    def boom(*a, **k):
        raise AssertionError("must not run after guard refusal")

    monkeypatch.setattr(campaign, "_phase_build", boom)
    monkeypatch.setattr(campaign, "_phase_inventory", boom)
    monkeypatch.setattr(campaign, "_finalize", boom)
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 3
    assert res.evidence_path is None
    assert calls == []
    assert (res.run_dir / "campaign.json").is_file()
    g = json.loads((res.run_dir / "guard.json").read_text())
    assert g["ok"] is False and g["violations"][0]["kind"] == "selftest_failed"


def test_guard_exception_is_harness_error(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def raising(cfg):
        raise RuntimeError("cannot check")

    monkeypatch.setattr(campaign, "_phase_guard", raising)
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 4 and calls == []


def test_build_failure_skips_sql_families_with_unrun(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def bld(cfg, ctx):
        ctx.build_ok = False
        ctx.build_report = SimpleNamespace(errors=[SimpleNamespace(crate="pgarbor-sql")])

    monkeypatch.setattr(campaign, "_phase_build", bld)
    monkeypatch.setattr(campaign, "_catalog", lambda cfg, key: 7)
    fams = _capture_finalize(monkeypatch)
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 0
    by = {f.name: f for f in fams}
    for name in ("differential", "regress-official", "regress-differential", "isolation",
                 "wire", "clients"):
        f = by[name]
        assert f.status == "SKIPPED_PRECONDITION"
        assert f.counts["UNRUN"] == 7 and f.total_cataloged == 7
        assert f.runnable == 0 and f.passed == 0
    # storage crates compiled, so the fault suite still runs
    assert by["storage-fault"].status == "RAN"
    assert "differential" not in calls and "oracle" not in calls
    assert "storage_fault" in calls


def test_build_failure_in_storage_crate_skips_storage_too(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def bld(cfg, ctx):
        ctx.build_ok = False
        ctx.build_report = SimpleNamespace(errors=[SimpleNamespace(crate="pgarbor-storage")])

    monkeypatch.setattr(campaign, "_phase_build", bld)
    monkeypatch.setattr(campaign, "_catalog", lambda cfg, key: 3)
    fams = _capture_finalize(monkeypatch)
    campaign.run_campaign(cfg)
    by = {f.name: f for f in fams}
    assert by["storage-fault"].status == "SKIPPED_PRECONDITION"
    assert by["storage-fault"].counts["UNRUN"] == 3


def test_phase_exception_becomes_harness_error_and_later_phases_run(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def bad(cfg, ctx):
        raise RuntimeError("kaboom")

    monkeypatch.setattr(campaign, "_phase_differential", bad)
    monkeypatch.setattr(campaign, "_phase_inventory", bad)
    monkeypatch.setattr(campaign, "_catalog", lambda cfg, key: 5)
    fams = _capture_finalize(monkeypatch)
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 0
    by = {f.name: f for f in fams}
    assert by["differential"].status == "HARNESS_ERROR"
    assert "kaboom" in by["differential"].notes[0]
    assert by["differential"].counts["UNRUN"] == 5
    for later in ("regress-official", "isolation", "wire", "clients"):
        assert by[later].status == "RAN"
    errs = json.loads((res.run_dir / "phase_errors.json").read_text())["errors"]
    assert {e["phase"] for e in errs} == {"inventory", "differential"}


def test_harness_error_without_catalog_is_visible(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def bad(cfg, ctx):
        raise ValueError("x")

    monkeypatch.setattr(campaign, "_phase_wire", bad)
    monkeypatch.setattr(campaign, "_catalog", lambda cfg, key: 0)
    fams = _capture_finalize(monkeypatch)
    campaign.run_campaign(cfg)
    wire = next(f for f in fams if f.name == "wire")
    assert wire.status == "HARNESS_ERROR" and wire.counts["HARNESS_ERROR"] == 1


def test_family_selection(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)
    cfg.families = ("wire", "regress")
    fams = _capture_finalize(monkeypatch)
    campaign.run_campaign(cfg)
    assert [f.name for f in fams] == ["regress-official", "regress-differential", "wire"]
    assert "oracle" not in calls


def test_make_family_denominators():
    f = campaign._make_family("x", "RAN", ["PASS", "MISMATCH", "BLOCKED", "INVALID_CASE"], 10)
    assert f.counts["UNRUN"] == 6
    assert f.total_cataloged == 10 and f.runnable == 2 and f.passed == 1
    assert sum(f.counts.values()) == f.total_cataloged
    g = campaign._make_family("y", "RAN", ["PASS"] * 3, 1)   # more results than cataloged
    assert g.total_cataloged == 3 and g.counts["UNRUN"] == 0


def test_storage_fault_failure_mapping(cfg, monkeypatch, tmp_path):
    seen = {}

    def fake_test(worktree, target_dir, packages, extra_args=(), timeout_s=1800.0,
                  log_dir=None):
        import os
        seen["seeds"] = os.environ.get("PGARBOR_FAULT_SEEDS")
        seen["packages"] = list(packages)
        seen["extra"] = list(extra_args)
        return [
            build.TestRun("pgarbor-fault", passed=["a", "b"], failed=["crash_recovery_3"]),
            build.TestRun("pgarbor-storage", passed=["c"], failed=["page_layout"],
                          ignored=["slow"]),
            build.TestRun("pgarbor-branches", status="timeout"),
        ]

    monkeypatch.setattr(build, "cargo_test", fake_test)
    monkeypatch.setattr(campaign, "_head_sha", lambda r: "abcdef0123456789")
    run_dir = tmp_path / "runs" / "x"
    run_dir.mkdir(parents=True)
    ctx = campaign._Ctx(tmp_path, campaign.config.Paths(root=tmp_path), run_dir, "x", "abcdef01")
    cluster_mod = campaign.cluster
    monkeypatch.setattr(cluster_mod, "FailureItem", lambda **kw: SimpleNamespace(**kw))
    cfg.fault_seeds = [7, 8]
    (fam,) = campaign._phase_storage_fault(cfg, ctx)
    assert seen["seeds"] == "7,8"
    assert seen["packages"] == ["pgarbor-fault", "pgarbor-storage", "pgarbor-branches"]
    assert seen["extra"] == ["--", "--include-ignored"]
    assert fam.counts["PASS"] == 3
    assert fam.counts["CORRUPTION"] == 1      # crash_recovery_3
    assert fam.counts["MISMATCH"] == 1        # page_layout
    assert fam.counts["HANG"] == 1
    assert fam.counts["UNRUN"] == 1           # ignored
    import os
    assert "PGARBOR_FAULT_SEEDS" not in os.environ or os.environ["PGARBOR_FAULT_SEEDS"] != "7,8"
    assert {f.outcome for f in ctx.failures} == {"CORRUPTION", "MISMATCH", "HANG"}


def test_clients_without_fixtures_is_blocked(cfg, tmp_path):
    ctx = campaign._Ctx(tmp_path, campaign.config.Paths(root=tmp_path), tmp_path / "r", "x",
                        "abcdef01")
    (fam,) = campaign._phase_clients(cfg, ctx)
    assert fam.status == "BLOCKED" and "no fixtures" in fam.notes


def test_parse_client_lines():
    out = 'noise\n{"test": "a", "status": "pass"}\n{"test": "b", "status": "fail", "detail": "d"}\n{bad'
    assert campaign._parse_client_lines(out) == [("a", "pass", ""), ("b", "fail", "d")]


def test_dsn_quotes_credentials():
    ep = SimpleNamespace(user="u@x", password="p w", host="127.0.0.1", port=5, dbname="db")
    assert campaign._dsn(ep) == "postgresql://u%40x:p%20w@127.0.0.1:5/db"


def test_run_family_unknown(cfg):
    with pytest.raises(ValueError):
        campaign.run_family(cfg, "nope")


def test_finalize_with_stubbed_modules_writes_valid_evidence(cfg, monkeypatch, tmp_path):
    """Orchestration of phase 10 with FAC-10..14 replaced by light fakes."""
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)
    seen = {}

    def build_pack(repo_root, cid, epoch, mode, reference, seeds, started, finished, build,
                   families, halt=None, resource=None):
        seen["resource"] = resource
        return EvidencePack(
            campaign_id=cid, epoch=epoch, mode=mode, git={"sha": "x"}, toolchain={},
            platform={}, reference=reference, corpus={}, seeds=seeds, started_unix=started,
            finished_unix=finished, build=build, families=families, totals=empty_counts(),
            critical={"corruption": 0, "crash": 0}, halt={}, resource=resource or {})

    def save_pack(pack, run_dir, repo_root):
        p = run_dir / "evidence.json"
        pack.save(p)
        return p

    monkeypatch.setattr(evidence, "build_pack", build_pack)
    monkeypatch.setattr(evidence, "save_pack", save_pack)
    monkeypatch.setattr(campaign.cluster, "cluster_failures", lambda items: [])
    monkeypatch.setattr(campaign.cluster, "write_clusters", lambda c, p: p.write_text("[]"))
    monkeypatch.setattr(campaign.repair, "generate_tickets", lambda *a: [])
    monkeypatch.setattr(campaign.repair, "write_repair_dag", lambda *a: None)
    monkeypatch.setattr(campaign.leases, "load_catalog", lambda d: [])

    def decide(history, budget, keys=None):
        return SimpleNamespace(to_json=lambda: {"action": "CONTINUE", "reasons": [],
                                                "blockers": [], "escalate": []})

    monkeypatch.setattr(campaign.halt, "decide", decide)
    monkeypatch.setattr(campaign.report, "write_reports",
                        lambda pack, c, t, d: (d / "report.md", d / "report.json"))
    dag = tmp_path / "contracts" / "dag"
    dag.mkdir(parents=True)
    (dag / "task_graph.v1.json").write_text('{"tasks": []}')
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 0
    assert res.evidence_path is not None and res.evidence_path.is_file()
    pack = EvidencePack.from_json(json.loads(res.evidence_path.read_text()))
    assert pack.halt["action"] == "CONTINUE"
    assert {"cpu_s", "wall_s", "max_rss_mb"} <= set(seen["resource"])
    assert pack.reference["fingerprint_sha256"] == "unverified"
    assert res.repair_dag_path is not None


def test_finalize_build_pack_failure_exits_4(cfg, monkeypatch):
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)

    def bad(*a, **k):
        raise RuntimeError("no pack")

    monkeypatch.setattr(evidence, "build_pack", bad)
    res = campaign.run_campaign(cfg)
    assert res.exit_code == 4 and res.evidence_path is None


def test_real_evidence_modules_roundtrip(cfg, monkeypatch):
    """Acceptance: evidence.json validates via EvidencePack.from_json (needs FAC-10..14)."""
    calls: list[str] = []
    _patch_all_ok(monkeypatch, calls)
    res = campaign.run_campaign(cfg)
    if res.exit_code == 4 and any("NotImplementedError" in str(e)
                                  for e in json.loads(
                                      (res.run_dir / "phase_errors.json").read_text())["errors"]):
        pytest.skip("evidence modules not integrated yet")
    assert res.exit_code == 0
    assert res.evidence_path is not None
    EvidencePack.from_json(json.loads(res.evidence_path.read_text()))
