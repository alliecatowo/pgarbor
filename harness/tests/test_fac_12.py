"""Tests for FAC-12 (pgarbor_factory.repair): repair ticket generation and repair DAG output."""

import json

from pgarbor_factory.cluster import Cluster
from pgarbor_factory.dag import validate_graph
from pgarbor_factory.records import LeaseSpec
from pgarbor_factory.repair import (
    _common_glob,
    _crate_of,
    generate_tickets,
    score,
    write_repair_dag,
)

STORAGE_FILE = "crates/pgarbor-storage/src/format/superblock.rs"
SQL_FILE = "crates/pgarbor-sql/src/select.rs"


def _graph():
    return {
        "tasks": [
            {"id": "STORE-01", "epic": "storage"},
            {"id": "SQL-01", "epic": "sql"},
        ]
    }


def _specs():
    return [
        LeaseSpec(lease_id="STO-01", crate="pgarbor-storage", task_ids=["STORE-01"],
                  files=[STORAGE_FILE], model_tier="haiku", summary="", spec=""),
        LeaseSpec(lease_id="SQL-01", crate="pgarbor-sql", task_ids=["SQL-01"],
                  files=[SQL_FILE], model_tier="haiku", summary="", spec=""),
    ]


def _cluster(key, outcome, lease_ids, unblock, rep, families):
    return Cluster(
        key=key, outcome=outcome, size=1, critical=outcome in ("CORRUPTION", "CRASH"),
        families=families, sample_ids=[rep], feature_histogram={},
        lease_ids=lease_ids, representative=rep, unblock_estimate=unblock,
    )


def test_critical_storage_ticket_sorts_before_higher_scoring_mismatch():
    storage = _cluster("corr-storage", "CORRUPTION", ["STO-01"], 1, "storage.torn-page",
                       ["storage"])
    mismatch = _cluster("mis-sql", "MISMATCH", ["SQL-01"], 1000, "sql.select-order",
                        ["sql"])
    tickets = generate_tickets([mismatch, storage], _graph(), _specs(), epoch=1)
    assert len(tickets) == 2
    assert tickets[0].critical is True
    assert tickets[0].root_cause == "corr-storage"
    assert tickets[0].id == "REPAIR-02-001"
    assert tickets[1].id == "REPAIR-02-002"
    assert tickets[1].score > tickets[0].score
    assert tickets[0].owned_files == [STORAGE_FILE]
    assert tickets[0].ownership_domain == "pgarbor-storage"
    assert tickets[0].deferred_verification_family == "storage"
    assert tickets[0].deliverable == "Fix root cause: corr-storage"


def test_one_ticket_per_cluster_and_confidence_and_cost():
    clusters = [
        _cluster("build-sql", "BUILD", ["SQL-01"], 2, "sql.a", ["sql"]),
        _cluster("unsup-none", "UNSUPPORTED", [], 1, "types.b", ["types"]),
    ]
    tickets = generate_tickets(clusters, _graph(), _specs(), epoch=1)
    assert len(tickets) == len(clusters)
    by_root = {t.root_cause: t for t in tickets}
    assert by_root["build-sql"].confidence == 0.9
    assert by_root["build-sql"].cost == 1.0
    assert by_root["build-sql"].severity == "BUILD"
    assert by_root["unsup-none"].confidence == 0.3
    assert by_root["unsup-none"].leases == []


def test_write_repair_dag_validates_and_numbers_tickets(tmp_path):
    storage = _cluster("corr-storage", "CORRUPTION", ["STO-01"], 1, "storage.torn-page",
                       ["storage"])
    mismatch = _cluster("mis-sql", "MISMATCH", ["SQL-01"], 5, "sql.select-order", ["sql"])
    tickets = generate_tickets([storage, mismatch], _graph(), _specs(), epoch=1)
    out = tmp_path / "repair_dag.json"
    write_repair_dag(tickets, out, "campaign-x", epoch=1)
    doc = json.loads(out.read_text(encoding="utf-8"))
    assert doc["source_campaign"] == "campaign-x"
    assert doc["epoch"] == 2
    assert [t["id"] for t in doc["tasks"]] == ["REPAIR-02-001", "REPAIR-02-002"]
    errors, stats = validate_graph(doc)
    assert errors == []
    assert stats["tasks"] == 2


def test_interface_drift_adds_amend_node_that_validates(tmp_path):
    drift = _cluster("drift-storage", "MISMATCH", ["STO-01"], 3, "storage.x", ["storage"])
    drift.root_hint = "interface_drift"  # ErrorCluster-style attribute, read via getattr
    tickets = generate_tickets([drift], _graph(), _specs(), epoch=1)
    assert tickets[0].contract_dependencies == ["AMEND-02-001"]
    out = tmp_path / "repair_dag.json"
    write_repair_dag(tickets, out, "campaign-y", epoch=1)
    doc = json.loads(out.read_text(encoding="utf-8"))
    ids = [t["id"] for t in doc["tasks"]]
    assert "AMEND-02-001" in ids
    errors, _ = validate_graph(doc)
    assert errors == []


def test_score_and_glob_helpers():
    assert score("CORRUPTION", 1, 1, 1.0, 1.0) == 1000.0
    assert score("UNKNOWN", 2, 3, 0.5, 2.0) == 1.5
    assert _crate_of(STORAGE_FILE) == "pgarbor-storage"
    assert _common_glob([STORAGE_FILE]) == "crates/pgarbor-storage/src/format/**"
    assert _common_glob([]) == ""
