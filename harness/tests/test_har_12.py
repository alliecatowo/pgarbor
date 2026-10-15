"""Tests for HAR-12 (ORACLE-04 normalizer registry). Executed centrally, not by the worker."""

from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from pgarbor_harness.compare.normalize import REGISTRY, apply_case_normalizers
from pgarbor_harness.model import (
    ColumnDesc,
    ErrorInfo,
    Notice,
    StatementResult,
    StepObservation,
    Transcript,
)


def _col(name: str, type_oid: int, table_oid: int = 0, attnum: int = 0) -> ColumnDesc:
    return ColumnDesc(name=name, type_oid=type_oid, typmod=-1, format=0,
                      table_oid=table_oid, column_attnum=attnum)


def _fixture() -> Transcript:
    """A transcript exercising every normalizer. Fresh object per call."""
    setup = StepObservation(
        index=0, session="setup", kind="sql",
        results=[StatementResult(
            kind="rows",
            columns=[_col("probe", 26, table_oid=16385)],
            rows=[["16385"]],
            command_tag="SELECT 1",
        )],
    )
    step0 = StepObservation(
        index=0, session="a", kind="sql",
        results=[
            StatementResult(
                kind="rows",
                columns=[
                    _col("pid", 23, table_oid=16385, attnum=1),
                    _col("ts", 1184),
                    _col("d", 1082),
                    _col("u", 2950),
                    _col("x", 28),
                    _col("txid_current", 20),
                    _col("lsn", 3220),
                    _col("o", 26, table_oid=16390),
                ],
                rows=[
                    ["123", "2026-01-01 00:00:00+00", "2026-01-01",
                     "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", "742", "742", "0/16B3748",
                     "16390"],
                    [None, None, None, None, None, None, None, None],
                ],
                command_tag="SELECT 2",
                param_type_oids=[23, 16385],
            ),
            StatementResult(
                kind="error",
                error=ErrorInfo(
                    severity="ERROR", sqlstate="40P01", message="deadlock detected",
                    detail=("Process 4242 waits for ShareLock on transaction 742; "
                            "blocked by process 4243."),
                ),
            ),
            StatementResult(
                kind="rows",
                columns=[_col("QUERY PLAN", 25)],
                rows=[
                    ["Seq Scan on t  (cost=0.00..1.05 rows=5 width=4) "
                     "(actual time=0.010..0.012 rows=5 loops=1)"],
                    ["Planning Time: 0.050 ms"],
                    ["Execution Time: 0.120 ms"],
                ],
                command_tag="EXPLAIN",
            ),
        ],
        notices=[Notice(severity="NOTICE", sqlstate="00000",
                        message="process 99 is still running")],
    )
    return Transcript(case_id="test.fixture", endpoint="target",
                      setup=[setup], steps=[step0])


def _snapshot(t: Transcript) -> str:
    return json.dumps(t.to_json(), sort_keys=True)


def _apply_norm(name: str, t: Transcript) -> None:
    norm = REGISTRY[name]
    if norm.default:
        norm.apply(t)
    else:
        for s in t.steps:
            norm.apply(s)


# ---------------------------------------------------------------------------------------------
# Idempotence and registry shape
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("name", sorted(REGISTRY))
def test_every_normalizer_is_idempotent(name: str) -> None:
    t = _fixture()
    before = _snapshot(t)
    _apply_norm(name, t)
    once = _snapshot(t)
    _apply_norm(name, t)
    twice = _snapshot(t)
    assert twice == once
    assert once != before, f"normalizer {name} did not change the fixture (vacuous test)"


def test_all_normalizers_compose_idempotently() -> None:
    t = _fixture()
    for name in REGISTRY:
        _apply_norm(name, t)
    once = _snapshot(t)
    for name in REGISTRY:
        _apply_norm(name, t)
    assert _snapshot(t) == once


def test_registry_names_and_defaults() -> None:
    assert set(REGISTRY) == {"user_oids", "timestamps", "uuids", "pids", "xids", "lsns",
                             "explain_numbers"}
    defaults = {n for n, v in REGISTRY.items() if v.default}
    assert defaults == {"user_oids"}


# ---------------------------------------------------------------------------------------------
# user_oids
# ---------------------------------------------------------------------------------------------


def test_user_oids_same_oid_same_canonical_across_setup_and_steps() -> None:
    t = _fixture()
    REGISTRY["user_oids"].apply(t)
    # 16385 first appears as the setup probe column's oid-typed value and then as a table_oid
    # in the step; both spellings must resolve to k=1.
    probe_col = t.setup[0].results[0].columns[0]
    assert probe_col.table_oid == 1_000_000_001
    assert t.setup[0].results[0].rows[0][0] == "U1"
    pid_col = t.steps[0].results[0].columns[0]
    assert pid_col.table_oid == 1_000_000_001


def test_user_oids_different_oids_get_different_values() -> None:
    t = _fixture()
    REGISTRY["user_oids"].apply(t)
    # 16390 (second distinct OID, first seen in step 0's table_oid of column "o") is k=2.
    o_col = t.steps[0].results[0].columns[7]
    assert o_col.table_oid == 1_000_000_002
    assert t.steps[0].results[0].rows[0][7] == "U2"
    assert o_col.table_oid != 1_000_000_001


def test_user_oids_acceptance_pair() -> None:
    t = _fixture()
    REGISTRY["user_oids"].apply(t)
    assert t.setup[0].results[0].columns[0].table_oid == 1000000001
    assert t.setup[0].results[0].rows[0][0] == "U1"


def test_user_oids_param_type_oids_canonicalized_and_builtin_kept() -> None:
    t = _fixture()
    REGISTRY["user_oids"].apply(t)
    res = t.steps[0].results[0]
    # 23 is built-in and untouched; 16385 is k=1 in the same transcript.
    assert res.param_type_oids == [23, 1_000_000_001]
    assert res.columns[0].type_oid == 23


def test_oids_below_16384_untouched() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(
            kind="rows",
            columns=[_col("c", 26, table_oid=0), _col("d", 26, table_oid=16383)],
            rows=[["5", "16383"], ["0", "1"]],
            param_type_oids=[26, 16383],
        )],
    )])
    before = _snapshot(t)
    REGISTRY["user_oids"].apply(t)
    assert _snapshot(t) == before


def test_type_oid_23_never_modified() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(
            kind="rows",
            columns=[_col("i", 23), _col("j", 23, table_oid=16400)],
            rows=[["1", "2"]],
            param_type_oids=[23],
        )],
    )])
    REGISTRY["user_oids"].apply(t)
    res = t.steps[0].results[0]
    assert res.columns[0].type_oid == 23
    assert res.columns[1].type_oid == 23
    assert res.param_type_oids == [23]
    # int4 values in non-oid columns are data, never OIDs.
    assert res.rows[0] == ["1", "2"]


def test_user_oids_skips_non_oid_text_and_values_above_uint32() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(
            kind="rows",
            columns=[_col("o", 26), _col("n", 25, table_oid=0)],
            rows=[["16385", "16385"], ["4294967296", "abc"]],
        )],
    )])
    REGISTRY["user_oids"].apply(t)
    rows = t.steps[0].results[0].rows
    assert rows[0] == ["U1", "16385"]  # text column of type 25 is never an oid value
    assert rows[1] == ["4294967296", "abc"]  # above uint32 is not an OID


def test_user_oids_canonical_range_is_not_renumbered() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(
            kind="rows",
            columns=[_col("c", 26, table_oid=1_000_000_001)],
            rows=[["U1"]],
        )],
    )])
    REGISTRY["user_oids"].apply(t)
    assert t.steps[0].results[0].columns[0].table_oid == 1_000_000_001
    assert t.steps[0].results[0].rows[0][0] == "U1"


def test_user_oids_rejects_nothing_on_null_rows() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(kind="rows", columns=[_col("o", 26)], rows=[[None]])],
    )])
    REGISTRY["user_oids"].apply(t)
    assert t.steps[0].results[0].rows[0][0] is None


# ---------------------------------------------------------------------------------------------
# Step normalizers (targeted behavior)
# ---------------------------------------------------------------------------------------------


def test_timestamps_mask_only_time_columns_and_keep_null() -> None:
    t = _fixture()
    REGISTRY["timestamps"].apply(t.steps[0])
    res = t.steps[0].results[0]
    assert res.rows[0][1] == "<timestamp>"
    assert res.rows[0][2] == "<timestamp>"
    assert res.rows[0][0] == "123"  # int4 pid is not a timestamp
    assert res.rows[1][1] is None
    assert res.rows[1][2] is None


def test_uuids_mask_uuid_column() -> None:
    t = _fixture()
    REGISTRY["uuids"].apply(t.steps[0])
    res = t.steps[0].results[0]
    assert res.rows[0][3] == "<uuid>"
    assert res.rows[1][3] is None


def test_pids_mask_pid_column_and_redact_process_messages() -> None:
    t = _fixture()
    REGISTRY["pids"].apply(t.steps[0])
    res = t.steps[0].results[0]
    assert res.rows[0][0] == "<pid>"
    assert res.rows[1][0] is None
    err = t.steps[0].results[1].error
    assert err is not None
    assert err.detail == ("Process <pid> waits for ShareLock on transaction 742; "
                          "blocked by process <pid>.")
    assert t.steps[0].notices[0].message == "process <pid> is still running"


def test_xids_mask_xid_types_and_named_columns() -> None:
    t = _fixture()
    REGISTRY["xids"].apply(t.steps[0])
    res = t.steps[0].results[0]
    assert res.rows[0][4] == "<xid>"  # xid type 28
    assert res.rows[0][5] == "<xid>"  # txid_current
    assert res.rows[0][6] == "0/16B3748"  # pg_lsn is not an xid


def test_lsns_mask_pg_lsn() -> None:
    t = _fixture()
    REGISTRY["lsns"].apply(t.steps[0])
    res = t.steps[0].results[0]
    assert res.rows[0][6] == "<lsn>"
    assert res.rows[1][6] is None


def test_explain_numbers_mask_estimates_but_not_actual_rows() -> None:
    t = _fixture()
    REGISTRY["explain_numbers"].apply(t.steps[0])
    rows = t.steps[0].results[2].rows
    assert rows[0][0] == ("Seq Scan on t  (cost=? rows=? width=?) "
                          "(actual time=? rows=5 loops=1)")
    assert rows[1][0] == "Planning Time: ? ms"
    assert rows[2][0] == "Execution Time: ? ms"


def test_explain_numbers_only_touches_query_plan_column() -> None:
    t = Transcript(case_id="t", endpoint="target", steps=[StepObservation(
        index=0, session="a", kind="sql",
        results=[StatementResult(
            kind="rows", columns=[_col("text", 25)],
            rows=[["cost=0.00..1.05 rows=5 width=4"]],
        )],
    )])
    before = _snapshot(t)
    REGISTRY["explain_numbers"].apply(t.steps[0])
    assert _snapshot(t) == before


# ---------------------------------------------------------------------------------------------
# apply_case_normalizers
# ---------------------------------------------------------------------------------------------


def _case(*normalize_lists: list[str]) -> SimpleNamespace:
    return SimpleNamespace(steps=[SimpleNamespace(normalize=list(n)) for n in normalize_lists])


def test_apply_case_normalizers_scopes_step_normalizers_and_returns_same_object() -> None:
    t = _fixture()
    t.steps.append(StepObservation(index=1, session="a", kind="sql"))
    case = _case(["timestamps"], [])
    out = apply_case_normalizers(case, t)
    assert out is t
    # Step 0 timestamps masked; setup gets only defaults (timestamp column absent there).
    assert t.steps[0].results[0].rows[0][1] == "<timestamp>"
    # Default user_oids applied to setup and steps.
    assert t.setup[0].results[0].rows[0][0] == "U1"
    assert t.steps[0].results[0].columns[0].table_oid == 1_000_000_001


def test_apply_case_normalizers_does_not_apply_step_normalizers_to_setup() -> None:
    t = _fixture()
    t.setup[0].results[0].columns.append(_col("ts", 1184))
    t.setup[0].results[0].rows[0].append("2026-01-01 00:00:00+00")
    case = _case(["timestamps"])
    apply_case_normalizers(case, t)
    assert t.setup[0].results[0].rows[0][1] == "2026-01-01 00:00:00+00"


def test_apply_case_normalizers_unknown_name_raises_keyerror_without_mutation() -> None:
    t = _fixture()
    before = _snapshot(t)
    with pytest.raises(KeyError):
        apply_case_normalizers(_case(["timestamps", "no_such_normalizer"]), t)
    assert _snapshot(t) == before


def test_apply_case_normalizers_is_idempotent() -> None:
    case = _case(["timestamps", "pids", "explain_numbers"])
    t = _fixture()
    apply_case_normalizers(case, t)
    once = _snapshot(t)
    apply_case_normalizers(case, t)
    assert _snapshot(t) == once
