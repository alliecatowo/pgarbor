"""Tests for FAC-11 failure clustering (pgarbor_factory.cluster)."""

import json

import pytest

from pgarbor_factory.build import ErrorCluster
from pgarbor_factory.cluster import (
    FailureItem,
    cluster_failures,
    cluster_key,
    from_build_cluster,
    from_case_result,
    write_clusters,
)
from pgarbor_harness.model import CaseResult, Outcome


def _unsupported(i: int, lease: str = "STO-03", steps: int = 2) -> FailureItem:
    return FailureItem(
        family="storage",
        item_id=f"storage.case-{i:03d}",
        outcome="UNSUPPORTED",
        sqlstate="0A000",
        message_template='feature "?" is not supported by PGArbor yet',
        first_keyword="CREATE",
        feature_ids=["F-STORAGE"],
        lease_ids=[lease],
        steps=steps,
    )


def test_unsupported_same_lease_clusters_into_one():
    items = [_unsupported(i) for i in range(5)]
    clusters = cluster_failures(items)
    assert len(clusters) == 1
    c = clusters[0]
    assert c.outcome == "UNSUPPORTED"
    assert c.size == 5
    assert c.lease_ids == ["STO-03"]
    assert c.critical is False
    assert c.families == ["storage"]
    assert len(c.sample_ids) == 5
    assert c.sample_ids == sorted(c.sample_ids)
    assert c.feature_histogram == {"F-STORAGE": 5}


def test_unsupported_lease_from_case_result_signature():
    res = CaseResult(
        case_id="storage.sto03-case",
        family="storage",
        outcome=Outcome.UNSUPPORTED,
        feature_ids=["F-STORAGE"],
        signature={
            "diff_kind": "harness",
            "sqlstate": "0A000",
            "ref_sqlstate": "",
            "message_template": "x",
            "step_kind": "sql",
            "first_keyword": "CREATE",
            "unimplemented_lease": "STO-03",
        },
    )
    item = from_case_result(res, steps=3)
    assert item.lease_ids == ["STO-03"]
    assert item.steps == 3
    assert item.outcome == "UNSUPPORTED"
    clusters = cluster_failures([item, _unsupported(9)])
    assert len(clusters) == 1
    assert clusters[0].lease_ids == ["STO-03"]


def test_from_case_result_rejects_pass():
    res = CaseResult(case_id="smoke.ok", family="smoke", outcome=Outcome.PASS)
    with pytest.raises(ValueError):
        from_case_result(res)


def test_unsupported_without_lease_keys_on_message_template():
    a = FailureItem(family="f", item_id="f.a", outcome="UNSUPPORTED",
                    message_template="tmpl A")
    b = FailureItem(family="f", item_id="f.b", outcome="UNSUPPORTED",
                    message_template="tmpl B")
    c = FailureItem(family="f", item_id="f.c", outcome="UNSUPPORTED",
                    message_template="tmpl A")
    clusters = cluster_failures([a, b, c])
    assert sorted(cl.size for cl in clusters) == [1, 2]


def test_crash_never_merges_with_mismatch():
    crash = FailureItem(family="wire", item_id="wire.x", outcome="CRASH",
                        sqlstate="XX000", message_template="boom", first_keyword="SELECT")
    mismatch = FailureItem(family="wire", item_id="wire.y", outcome="MISMATCH",
                           sqlstate="XX000", ref_sqlstate="XX000",
                           message_template="boom", first_keyword="SELECT")
    clusters = cluster_failures([crash, mismatch])
    assert len(clusters) == 2
    assert {c.outcome for c in clusters} == {"CRASH", "MISMATCH"}
    assert cluster_key(crash) != cluster_key(mismatch)


def test_crash_signatures_are_separate_critical_clusters():
    a = FailureItem(family="storage", item_id="s.1", outcome="CORRUPTION",
                    message_template="page checksum ?")
    b = FailureItem(family="storage", item_id="s.2", outcome="CORRUPTION",
                    message_template="wal gap ?")
    c = FailureItem(family="storage", item_id="s.3", outcome="CORRUPTION",
                    message_template="page checksum ?")
    d = FailureItem(family="storage", item_id="s.4", outcome="MISMATCH",
                    sqlstate="22001", ref_sqlstate="22001", message_template="m")
    clusters = cluster_failures([a, b, c, d])
    assert [c_.critical for c_ in clusters][:2] == [True, True]
    assert clusters[-1].outcome == "MISMATCH"
    crit = [c_ for c_ in clusters if c_.critical]
    assert sorted(c_.size for c_ in crit) == [1, 2]


def test_critical_clusters_sort_first_even_when_smaller():
    mismatches = [
        FailureItem(family="smoke", item_id=f"smoke.m{i}", outcome="MISMATCH",
                    diff_kind="column_type", first_keyword="SELECT")
        for i in range(6)
    ]
    crash = FailureItem(family="smoke", item_id="smoke.c", outcome="CRASH",
                        message_template="boom")
    clusters = cluster_failures(mismatches + [crash])
    assert clusters[0].outcome == "CRASH"
    assert clusters[0].critical is True
    assert clusters[1].size == 6


def test_mismatch_with_error_clusters_by_sqlstate_and_template():
    items = [
        FailureItem(family="sql", item_id=f"sql.{i}", outcome="MISMATCH",
                    sqlstate="42P01", ref_sqlstate="42P01",
                    message_template='relation "?" does not exist', first_keyword="SELECT")
        for i in range(3)
    ]
    items.append(FailureItem(family="sql", item_id="sql.other", outcome="MISMATCH",
                             sqlstate="42703", ref_sqlstate="42P01",
                             message_template='column "?" does not exist',
                             first_keyword="SELECT"))
    clusters = cluster_failures(items)
    assert [c.size for c in clusters] == [3, 1]


def test_mismatch_without_error_clusters_by_diff_kind_and_keyword():
    items = [
        FailureItem(family="sel", item_id=f"sel.{i}", outcome="MISMATCH",
                    diff_kind="column_type", first_keyword="SELECT") for i in range(4)
    ]
    items.append(FailureItem(family="sel", item_id="sel.ins", outcome="MISMATCH",
                             diff_kind="column_type", first_keyword="INSERT"))
    clusters = cluster_failures(items)
    assert sorted(c.size for c in clusters) == [1, 4]


def test_representative_is_shortest_case():
    items = [
        FailureItem(family="f", item_id="f.long", outcome="MISMATCH", diff_kind="rows",
                    first_keyword="SELECT", steps=9),
        FailureItem(family="f", item_id="f.short", outcome="MISMATCH", diff_kind="rows",
                    first_keyword="SELECT", steps=2),
    ]
    (c,) = cluster_failures(items)
    assert c.representative == "f.short"


def test_unblock_estimate_counts_proxy_matches_in_other_clusters():
    cluster_items = [_unsupported(i) for i in range(2)]
    # Other cluster: same first keyword and feature, different lease.
    other = FailureItem(family="storage", item_id="storage.other", outcome="UNSUPPORTED",
                        message_template="other", first_keyword="CREATE",
                        feature_ids=["F-STORAGE"], lease_ids=["STO-04"], steps=1)
    unrelated = FailureItem(family="storage", item_id="storage.unrel", outcome="MISMATCH",
                            diff_kind="rows", first_keyword="SELECT",
                            feature_ids=["F-OTHER"])
    clusters = cluster_failures(cluster_items + [other, unrelated])
    sto03 = next(c for c in clusters if c.lease_ids == ["STO-03"])
    # 2 distinct items in the cluster + 1 proxy match in another cluster.
    assert sto03.unblock_estimate == 3


def test_build_cluster_passes_through():
    ec = ErrorCluster(key="E0308|?", code="E0308", template="mismatched types ?", count=4,
                      files=["crates/pgarbor-storage/src/lib.rs"], leases=["STO-01"],
                      root_hint="", examples=[])
    (c,) = cluster_failures([from_build_cluster(ec)])
    assert c.outcome == "BUILD"
    assert c.critical is False
    assert c.lease_ids == ["STO-01"]
    assert json.loads(c.key) == ["BUILD", "E0308", "mismatched types ?"]


def test_key_is_separator_safe():
    a = FailureItem(family="f", item_id="a", outcome="UNSUPPORTED", message_template="x|y")
    b = FailureItem(family="f", item_id="b", outcome="UNSUPPORTED", message_template="x|y")
    assert cluster_key(a) == cluster_key(b)
    c = FailureItem(family="f", item_id="c", outcome="UNSUPPORTED", message_template="x")
    assert cluster_key(c) != cluster_key(a)
    # Separator ambiguity: ("a|b", "c") must not collide with ("a", "b|c").
    d = FailureItem(family="f", item_id="d", outcome="MISMATCH", sqlstate="a|b",
                    ref_sqlstate="c", message_template="")
    e = FailureItem(family="f", item_id="e", outcome="MISMATCH", sqlstate="a",
                    ref_sqlstate="b|c", message_template="")
    assert cluster_key(d) != cluster_key(e)


def test_write_clusters_roundtrip(tmp_path):
    clusters = cluster_failures([_unsupported(0), _unsupported(1)])
    out = tmp_path / "clusters.json"
    write_clusters(clusters, out)
    data = json.loads(out.read_text(encoding="utf-8"))
    assert isinstance(data, list) and len(data) == 1
    assert data[0]["size"] == 2
    assert data[0]["lease_ids"] == ["STO-03"]
    assert data[0]["outcome"] == "UNSUPPORTED"
