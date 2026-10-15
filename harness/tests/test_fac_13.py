"""Tests for pgarbor_factory.report (lease FAC-13)."""

import json

from pgarbor_factory.cluster import Cluster
from pgarbor_factory.records import (
    COUNT_KEYS,
    EvidencePack,
    FamilyEvidence,
    RepairTicket,
    empty_counts,
)
from pgarbor_factory.report import render_json, render_markdown, write_reports


def _counts(**kw: int) -> dict:
    counts = empty_counts()
    for k, v in kw.items():
        counts[k] = v
    return counts


def _family(name: str, passed: int, runnable: int, cataloged: int, **kw: int) -> FamilyEvidence:
    return FamilyEvidence(
        name=name, status="RAN", total_cataloged=cataloged, runnable=runnable,
        passed=passed, counts=_counts(PASS=passed, **kw),
    )


def _pack(families=None, critical=None, dirty=False, halt=None) -> EvidencePack:
    if families is None:
        families = [
            _family("differential", 5, 8, 10, MISMATCH=2, UNSUPPORTED=1, UNRUN=2),
            _family("regress", 0, 0, 3, BLOCKED=3),
        ]
    return EvidencePack(
        campaign_id="c-test-001",
        epoch=1,
        mode="seams",
        git={"sha": "abc123", "branch": "main", "dirty": dirty},
        toolchain={"rustc": "rustc 1.90.0", "cargo": "cargo 1.90.0"},
        platform={"os": "linux", "kernel": "6.x", "arch": "x86_64", "python": "3.12"},
        reference={"image_digest": "sha256:deadbeef", "server_version": "18.6",
                   "fingerprint_sha256": "ffee"},
        corpus={"cases": "aa", "expected": None},
        seeds=[1, 2],
        started_unix=100,
        finished_unix=200,
        build={"ok": True, "duration_s": 1.5, "error_clusters": [], "log": "x"},
        families=families,
        totals={},
        critical=critical if critical is not None else {"corruption": 0, "crash": 0},
        halt=halt if halt is not None else {},
    )


def _clusters(n: int) -> list:
    return [
        Cluster(key=f"k{i}", outcome="MISMATCH", size=i + 1, critical=False,
                families=["differential"], sample_ids=[f"case-{i}"],
                feature_histogram={}, lease_ids=["FAC-01"], representative=f"case-{i}",
                unblock_estimate=0)
        for i in range(n)
    ]


def _tickets(n: int) -> list:
    return [
        RepairTicket(
            id=f"T-{i:03d}", title=f"ticket {i}", root_cause="rc", severity="MISMATCH",
            expected_unblocked=1, dependency_fanout=0, confidence=0.5, cost=1.0,
            score=float(n - i), critical=False, repro=[], expected_behavior="e",
            actual_behavior="a", owned_files=[], leases=["FAC-01"],
        )
        for i in range(n)
    ]


def _header_cells(md: str) -> list:
    for line in md.splitlines():
        if line.startswith("| Family |"):
            return [c.strip() for c in line.strip().strip("|").split("|")]
    raise AssertionError("no family table header")


def test_markdown_family_table_has_every_count_column_except_pass():
    md = render_markdown(_pack(), [], [])
    header = _header_cells(md)
    for key in COUNT_KEYS:
        if key == "PASS":
            continue
        assert key in header, key
    assert "PASS" not in header
    assert header[:5] == ["Family", "Status", "Passed", "Runnable", "Cataloged"]


def test_zero_counts_are_printed_not_blank():
    md = render_markdown(_pack(), [], [])
    header = _header_cells(md)
    row = next(l for l in md.splitlines() if l.startswith("| regress |"))
    cells = [c.strip() for c in row.strip().strip("|").split("|")]
    assert len(cells) == len(header)
    assert all(c != "" for c in cells)
    assert cells[header.index("CRASH")] == "0"


def test_banner_when_corruption_present():
    md = render_markdown(_pack(critical={"corruption": 2, "crash": 0}), [], [])
    assert "> **STOP THE LINE:** 2 corruption, 0 crash failures" in md


def test_banner_when_crash_present():
    md = render_markdown(_pack(critical={"corruption": 0, "crash": 3}), [], [])
    assert "> **STOP THE LINE:** 0 corruption, 3 crash failures" in md


def test_no_banner_when_critical_is_zero():
    md = render_markdown(_pack(), [], [])
    assert "STOP THE LINE" not in md


def test_no_percentage_figure_anywhere():
    md = render_markdown(_pack(), _clusters(3), _tickets(3))
    assert "%" not in md


def test_unsupported_blocked_unrun_never_counted_as_passed():
    fams = [_family("only", 0, 4, 6, UNSUPPORTED=2, BLOCKED=1, UNRUN=3)]
    data = render_json(_pack(families=fams), [], [])
    assert data["totals"]["passed"] == 0
    assert data["totals"]["counts"]["PASS"] == 0
    assert data["totals"]["counts"]["UNSUPPORTED"] == 2
    assert data["totals"]["fail"] == 0
    md = render_markdown(_pack(families=fams), [], [])
    assert "| UNSUPPORTED | 2 |" in md
    assert "| UNRUN | 3 |" in md


def test_fail_total_is_sum_of_failure_statuses():
    fams = [_family("f", 1, 5, 7, MISMATCH=1, CRASH=1, HANG=1, CORRUPTION=0,
                    UNEXPECTED_ACCEPTANCE=1, FLAKY=2)]
    data = render_json(_pack(families=fams), [], [])
    assert data["totals"]["fail"] == 4
    md = render_markdown(_pack(families=fams), [], [])
    assert "| FLAKY | 2 |" in md
    assert "| FAIL (MISMATCH + CRASH + HANG + CORRUPTION + UNEXPECTED_ACCEPTANCE) | 4 |" in md


def test_dirty_marker_in_header():
    md = render_markdown(_pack(dirty=True), [], [])
    assert "git abc123 (dirty!)" in md
    clean = render_markdown(_pack(dirty=False), [], [])
    assert "git abc123 (dirty!)" not in clean
    assert "git abc123" in clean


def test_section_order_matches_contract():
    md = render_markdown(_pack(critical={"corruption": 1, "crash": 0}), _clusters(1),
                         _tickets(1))
    order = [
        "# PGArbor campaign", "STOP THE LINE", "## Families", "## Totals",
        "## Top failure clusters", "## Next-epoch repair tickets", "## Halt decision",
        "## Provenance",
    ]
    positions = [md.index(s) for s in order]
    assert positions == sorted(positions)


def test_pipe_is_escaped_in_table_cells():
    fams = [_family("a|b", 0, 0, 0)]
    md = render_markdown(_pack(families=fams), [], [])
    assert "a\\|b" in md


def test_json_families_in_evidence_order_and_fields():
    fams = [_family("zeta", 1, 1, 1), _family("alpha", 0, 0, 2), _family("mid", 0, 0, 0)]
    data = render_json(_pack(families=fams), [], [])
    assert [f["name"] for f in data["families"]] == ["zeta", "alpha", "mid"]
    assert set(data["families"][0]) == {
        "name", "status", "total_cataloged", "runnable", "passed", "counts"}
    assert set(data["families"][0]["counts"]) == set(COUNT_KEYS)
    assert data["campaign_id"] == "c-test-001"
    assert data["git_sha"] == "abc123"
    assert data["dirty"] is False


def test_json_top_clusters_and_tickets_are_capped():
    data = render_json(_pack(), _clusters(25), _tickets(60))
    assert len(data["top_clusters"]) == 20
    assert data["top_clusters"][0] == {"key": "k0", "outcome": "MISMATCH", "size": 1,
                                       "leases": ["FAC-01"]}
    assert len(data["tickets"]) == 50
    assert data["tickets"][0] == {"id": "T-000", "score": 60.0}


def test_markdown_caps_clusters_and_tickets():
    md = render_markdown(_pack(), _clusters(25), _tickets(60))
    assert "| k19 |" in md
    assert "| k20 |" not in md
    assert "| T-049 |" in md
    assert "| T-050 |" not in md


def test_halt_section_renders_decision():
    halt = {"action": "STOP_THE_LINE", "reasons": ["corruption"], "blockers": [],
            "escalate": ["storage owner"]}
    md = render_markdown(_pack(halt=halt), [], [])
    assert "- **action:** STOP_THE_LINE" in md
    assert "- **blockers:** none" in md
    data = render_json(_pack(halt=halt), [], [])
    assert data["halt"] == halt


def test_write_reports_creates_both_files(tmp_path):
    md_path, json_path = write_reports(_pack(), _clusters(2), _tickets(2), tmp_path / "run")
    assert md_path.name == "report.md" and json_path.name == "report.json"
    assert md_path.read_text(encoding="utf-8").startswith("# PGArbor campaign c-test-001")
    loaded = json.loads(json_path.read_text(encoding="utf-8"))
    assert loaded["campaign_id"] == "c-test-001"
    assert loaded["totals"]["passed"] == 5
