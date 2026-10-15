"""Tests for FAC-18 (pgarbor_factory.amendments): scanning, drift conversion, opening, suspension."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import pytest

from pgarbor_factory import amendments as am
from pgarbor_factory.amendments import Amendment
from pgarbor_factory.records import LeaseSpec

REPO = Path(__file__).resolve().parents[2]


@dataclass
class _Diag:
    message: str
    crate: str


@dataclass
class _Cluster:
    key: str
    code: str
    template: str
    count: int
    files: list[str]
    leases: list[str]
    root_hint: str
    examples: list[_Diag] = field(default_factory=list)


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _amendment_text(status: str, crate: str, leases: str, title: str = "T") -> str:
    return (
        f"Status: {status}\n"
        f"Crate: {crate}\n"
        "Affects: pgarbor-cli\n"
        f"Leases: {leases}\n"
        "\n"
        f"# {title}\n\n## Problem\n\nx\n\n## Proposed change\n\ny\n\n## Affected crates\n\n- a\n"
    )


def test_expand_leases_ranges_and_annotations() -> None:
    assert am._expand_leases("HAR-01..HAR-03, FAC-09 (campaign); pgarbor-cli leases") == [
        "HAR-01", "HAR-02", "HAR-03", "FAC-09",
    ]


def test_scan_parses_existing_harness_amendments() -> None:
    by_path = {a.path: a for a in am.scan(REPO)}

    h1 = by_path["contracts/amendments/harness-01-dag-owned-path-globs.md"]
    assert h1.status == "PROPOSED"
    assert h1.crate == "harness"
    assert h1.affects == ["contracts/dag/task_graph.v1.json"]
    assert len(h1.leases) == 31 + 19
    assert "HAR-01" in h1.leases and "HAR-31" in h1.leases
    assert "FAC-01" in h1.leases and "FAC-19" in h1.leases

    h2 = by_path["contracts/amendments/harness-02-cli-serve-contract.md"]
    assert h2.status == "PROPOSED"
    assert h2.crate == "harness"
    assert h2.affects == ["pgarbor-cli"]
    assert h2.leases == ["HAR-11", "FAC-09"]
    assert h2.title.startswith("harness-02")


def test_scan_reads_status_written_after_title() -> None:
    storage = [a for a in am.scan(REPO) if "pgarbor-storage-01" in a.path]
    assert len(storage) == 1
    assert storage[0].status == "PROPOSED"
    assert storage[0].crate == "pgarbor-storage"
    assert storage[0].leases == []


def test_suspended_excludes_ratified_and_applied(tmp_path: Path) -> None:
    d = tmp_path / "contracts" / "amendments"
    _write(d / "x-01-ratified.md", _amendment_text("RATIFIED", "x", "FAC-01"))
    _write(d / "x-02-applied.md", _amendment_text("APPLIED", "x", "FAC-02"))
    _write(d / "x-03-proposed.md", _amendment_text("PROPOSED", "x", "FAC-03, FAC-04..FAC-05"))
    _write(d / "x-04-rejected.md", _amendment_text("REJECTED", "x", "FAC-06"))

    suspended = am.suspended_leases(tmp_path)
    assert "FAC-01" not in suspended
    assert "FAC-02" not in suspended
    assert suspended["FAC-03"] == "contracts/amendments/x-03-proposed.md"
    assert suspended["FAC-04"] == "contracts/amendments/x-03-proposed.md"
    assert suspended["FAC-05"] == "contracts/amendments/x-03-proposed.md"


def test_drift_three_leases_yields_one_amendment(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PGARBOR_REPO", str(tmp_path))
    _write(tmp_path / "contracts" / "ARCHITECTURE.md", "x\n")
    (tmp_path / "contracts" / "amendments").mkdir(parents=True, exist_ok=True)
    _write(tmp_path / "crates" / "pgarbor-core" / "src" / "value.rs", "pub struct Widget;\n")
    _write(tmp_path / "crates" / "pgarbor-types" / "src" / "a.rs", "use pgarbor_core::Widget;\n")
    _write(tmp_path / "crates" / "pgarbor-types" / "src" / "b.rs", "fn f(w: Widget) {}\n")
    _write(tmp_path / "crates" / "pgarbor-storage" / "src" / "c.rs", "fn g(w: Widget) {}\n")

    specs = [
        LeaseSpec(lease_id="CORE-01", crate="pgarbor-core", task_ids=[],
                  files=["crates/pgarbor-core/src/value.rs"], model_tier="haiku",
                  summary="", spec=""),
        LeaseSpec(lease_id="TYP-01", crate="pgarbor-types", task_ids=[],
                  files=["crates/pgarbor-types/src/a.rs"], model_tier="haiku",
                  summary="", spec=""),
        LeaseSpec(lease_id="TYP-02", crate="pgarbor-types", task_ids=[],
                  files=["crates/pgarbor-types/src/b.rs"], model_tier="haiku",
                  summary="", spec=""),
        LeaseSpec(lease_id="STO-01", crate="pgarbor-storage", task_ids=[],
                  files=["crates/pgarbor-storage/src/c.rs"], model_tier="haiku",
                  summary="", spec=""),
    ]
    clusters = [
        _Cluster(
            key="E0412|cannot find type `?` in this scope",
            code="E0412",
            template="cannot find type `?` in this scope",
            count=3,
            files=["crates/pgarbor-types/src/a.rs", "crates/pgarbor-types/src/b.rs",
                   "crates/pgarbor-storage/src/c.rs"],
            leases=["TYP-01", "TYP-02", "STO-01"],
            root_hint="interface_drift",
            examples=[
                _Diag("cannot find type `pgarbor_core::Widget` in this scope", "pgarbor-types"),
                _Diag("cannot find type `pgarbor_core::Widget` in this scope", "pgarbor-types"),
                _Diag("cannot find type `pgarbor_core::Widget` in this scope", "pgarbor-storage"),
            ],
        ),
        _Cluster(
            key="E0308|mismatched types", code="E0308", template="mismatched types",
            count=1, files=[], leases=["TYP-01"], root_hint="local", examples=[],
        ),
    ]

    proposals = am.drift_from_build(clusters, specs, epoch=3)
    assert len(proposals) == 1
    p = proposals[0]
    assert p.status == "PROPOSED"
    assert p.crate == "pgarbor-core"
    assert p.leases == ["STO-01", "TYP-01", "TYP-02"]
    assert p.affects == ["pgarbor-storage", "pgarbor-types"]
    assert p.ticket == "AMEND-03-001"
    assert "Widget" in p.title


def test_open_amendment_refuses_without_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("PGARBOR_ARCHITECT", raising=False)
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW", raising=False)
    proposal = Amendment(path="", status="PROPOSED", crate="pgarbor-types",
                         affects=["pgarbor-cli"], leases=["TYP-01"], title="Test amendment")
    with pytest.raises(PermissionError):
        am.open_amendment(tmp_path, proposal, "problem", "change")
    assert not (tmp_path / "contracts" / "amendments").exists()


def test_open_amendment_writes_and_round_trips(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PGARBOR_ARCHITECT", "1")
    proposal = Amendment(path="", status="PROPOSED", crate="pgarbor-types",
                         affects=["pgarbor-cli"], leases=["TYP-01", "TYP-02"],
                         ticket="AMEND-03-001", title="Test amendment")
    first = am.open_amendment(tmp_path, proposal, "problem text", "change text")
    second = am.open_amendment(tmp_path, proposal, "problem text", "change text")
    assert first.name == "pgarbor-types-01-test-amendment.md"
    assert second.name == "pgarbor-types-02-test-amendment.md"

    parsed = {a.path: a for a in am.scan(tmp_path)}
    got = parsed["contracts/amendments/pgarbor-types-01-test-amendment.md"]
    assert got.status == "PROPOSED"
    assert got.crate == "pgarbor-types"
    assert got.affects == ["pgarbor-cli"]
    assert got.leases == ["TYP-01", "TYP-02"]
    assert got.ticket == "AMEND-03-001"
    assert got.title == "Test amendment"
    assert "problem text" in first.read_text(encoding="utf-8")


def test_open_amendment_rejects_bad_crate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    bad = Amendment(path="", status="PROPOSED", crate="Bad Crate!", title="x")
    with pytest.raises(am.AmendmentError):
        am.open_amendment(tmp_path, bad, "p", "c")
