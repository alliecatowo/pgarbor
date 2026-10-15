"""Tests for HAR-14 (compare.classify: outcome precedence, locate, cluster signature).

Executed centrally (needs HAR-08 sqltext for the signature helpers).
"""

from __future__ import annotations

from pgarbor_harness.cases.model import Case
from pgarbor_harness.compare.classify import classify, locate
from pgarbor_harness.model import (
    Diff,
    ErrorInfo,
    Outcome,
    StatementResult,
    StepObservation,
    Transcript,
)


def _case(sql_steps: list[dict] | None = None, setup: str | None = None) -> Case:
    steps = sql_steps if sql_steps is not None else [{"sql": "select 1"}]
    d = {"id": "smoke.har14", "title": "har14", "family": "smoke",
         "feature_ids": ["F-1"], "steps": steps}
    if setup is not None:
        d["setup"] = setup
    return Case.from_dict(d, path="tests/cases/smoke/har14.yaml", sha256="0" * 64)


def _err(sqlstate: str, message: str = "boom", detail: str | None = None) -> ErrorInfo:
    return ErrorInfo(severity="ERROR", sqlstate=sqlstate, message=message, detail=detail)


def _err_result(sqlstate: str, message: str = "boom", detail: str | None = None) -> StatementResult:
    return StatementResult(kind="error", error=_err(sqlstate, message, detail))


def _rows_result(rows: list[list[str]] | None = None) -> StatementResult:
    return StatementResult(kind="rows", rows=rows if rows is not None else [["1"]],
                           command_tag="SELECT 1")


def _step(index: int, results: list[StatementResult] | None = None, *,
          session: str = "a", kind: str = "sql", **flags) -> StepObservation:
    return StepObservation(index=index, session=session, kind=kind,
                           results=results if results is not None else [], **flags)


def _tx(steps: list[StepObservation], endpoint: str, setup: list[StepObservation] | None = None
        ) -> Transcript:
    return Transcript(case_id="smoke.har14", endpoint=endpoint,
                      setup=setup if setup is not None else [], steps=steps)


def _diff(path: str, kind: str = "sqlstate", ref=None, tgt=None) -> Diff:
    return Diff(path=path, kind=kind, reference=ref, target=tgt)


# ---------------------------------------------------------------------------------------------
# locate
# ---------------------------------------------------------------------------------------------


def test_locate_step_result_and_nested_path():
    assert locate(_diff("steps[3].results[0].rows[1][2]")) == ("steps", 3, 0)


def test_locate_setup_without_result():
    assert locate(_diff("setup[0]")) == ("setup", 0, None)


def test_locate_step_level_path():
    assert locate(_diff("steps[7]", kind="missing_step")) == ("steps", 7, None)


def test_locate_setup_result():
    assert locate(_diff("setup[0].results[2].columns[1]")) == ("setup", 0, 2)


def test_locate_rejects_unknown_prefix():
    try:
        locate(_diff("rows[0]"))
    except ValueError:
        return
    raise AssertionError("locate must reject paths that are not setup/steps")


# ---------------------------------------------------------------------------------------------
# Rule 5 / diffs never PASS
# ---------------------------------------------------------------------------------------------


def test_rule5_clean_transcripts_and_no_diffs_pass():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result()])], "target")
    res = classify(_case(), ref, tgt, [])
    assert res.outcome is Outcome.PASS
    assert res.outcome.is_pass
    assert res.first_divergence is None
    assert res.diffs == []
    assert res.reason == ""
    assert res.case_id == "smoke.har14"
    assert res.family == "smoke"
    assert res.feature_ids == ["F-1"]
    assert res.signature["diff_kind"] == ""


def test_diffs_non_empty_never_pass():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result()])], "target")
    # Even a divergence that looks like an identical result must not be PASS.
    diffs = [_diff("steps[0].results[0].command_tag", kind="command_tag", ref="SELECT 1",
                   tgt="SELECT 1")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is not Outcome.PASS
    assert not res.outcome.is_pass
    assert res.first_divergence is diffs[0]
    assert res.diffs == diffs


# ---------------------------------------------------------------------------------------------
# Rule 1 HARNESS_ERROR
# ---------------------------------------------------------------------------------------------


def test_rule1_harness_error_regardless_of_diffs():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, harness_error="exploded " + "x" * 200)], "target")
    diffs = [_diff("steps[0]", kind="missing_step")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.HARNESS_ERROR
    assert res.reason == "harness:" + ("exploded " + "x" * 200)[:80]
    assert res.signature["diff_kind"] == "harness"


def test_rule1_harness_error_in_setup():
    ref = _tx([], "reference")
    tgt = _tx([], "target", setup=[_step(0, harness_error="setup broke")])
    res = classify(_case(), ref, tgt, [])
    assert res.outcome is Outcome.HARNESS_ERROR


def test_rule1_outranks_corruption_and_crash():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("XX001"), _err_result("XX000")], harness_error="h",
                     connection_lost=True)], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")],
                   crash_evidence="corrupt page")
    assert res.outcome is Outcome.HARNESS_ERROR


# ---------------------------------------------------------------------------------------------
# Rule 2 CORRUPTION
# ---------------------------------------------------------------------------------------------


def test_rule2_xx001_without_matching_reference_is_corruption():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("XX001", "invalid page")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")])
    assert res.outcome is Outcome.CORRUPTION
    assert res.signature["diff_kind"] == "corruption"
    assert res.signature["sqlstate"] == "XX001"


def test_rule2_xx002_without_matching_reference_is_corruption():
    ref = _tx([_step(0, [_err_result("42P01")])], "reference")
    tgt = _tx([_step(0, [_err_result("XX002")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")])
    assert res.outcome is Outcome.CORRUPTION


def test_rule2_xx001_matching_reference_error_is_not_corruption():
    ref = _tx([_step(0, [_err_result("XX001", "same")])], "reference")
    tgt = _tx([_step(0, [_err_result("XX001", "same")])], "target")
    diffs = [_diff("steps[0].results[0].error.detail", kind="error_field")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.MISMATCH


def test_rule2_corruption_evidence_text_is_corruption():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result()])], "target")
    res = classify(_case(), ref, tgt, [], crash_evidence="Heap CORRUPT detected")
    assert res.outcome is Outcome.CORRUPTION


def test_rule2_outranks_crash():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("XX001"), _err_result("XX000")], connection_lost=True)],
              "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")],
                   crash_evidence="panic")
    assert res.outcome is Outcome.CORRUPTION


# ---------------------------------------------------------------------------------------------
# Rule 3 CRASH
# ---------------------------------------------------------------------------------------------


def test_rule3_connection_lost_without_reference_loss_is_crash():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [], connection_lost=True)], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0]", kind="connection")])
    assert res.outcome is Outcome.CRASH
    assert res.signature["diff_kind"] == "crash"


def test_rule3_connection_lost_mirrored_by_reference_is_not_crash():
    ref = _tx([_step(0, [], connection_lost=True)], "reference")
    tgt = _tx([_step(0, [], connection_lost=True)], "target")
    res = classify(_case(), ref, tgt, [])
    assert res.outcome is Outcome.PASS


def test_rule3_xx000_without_reference_xx000_is_crash():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("XX000", "invariant failed")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")])
    assert res.outcome is Outcome.CRASH


def test_rule3_xx000_mirrored_by_reference_is_not_crash():
    ref = _tx([_step(0, [_err_result("XX000")])], "reference")
    tgt = _tx([_step(0, [_err_result("XX000")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].error.detail",
                                             kind="error_field")])
    assert res.outcome is Outcome.MISMATCH


def test_rule3_crash_evidence_is_crash():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result()])], "target")
    res = classify(_case(), ref, tgt, [], crash_evidence="thread panicked at src/lib.rs")
    assert res.outcome is Outcome.CRASH


def test_rule3_outranks_hang():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [], timed_out=True, connection_lost=True)], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0]", kind="timeout")])
    assert res.outcome is Outcome.CRASH


# ---------------------------------------------------------------------------------------------
# Rule 4 HANG
# ---------------------------------------------------------------------------------------------


def test_rule4_timeout_is_hang():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [], timed_out=True)], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0]", kind="timeout")])
    assert res.outcome is Outcome.HANG
    assert res.reason == "hang:timeout"
    assert res.signature["diff_kind"] == "hang"


def test_rule4_timeout_mirrored_by_reference_is_not_hang():
    ref = _tx([_step(0, [], timed_out=True)], "reference")
    tgt = _tx([_step(0, [], timed_out=True)], "target")
    res = classify(_case(), ref, tgt, [])
    assert res.outcome is Outcome.PASS


# ---------------------------------------------------------------------------------------------
# Rule 6 first divergence
# ---------------------------------------------------------------------------------------------


def test_rule6a_target_0a000_vs_reference_rows_is_unsupported():
    ref = _tx([_step(0, [_rows_result([["1"]])])], "reference")
    tgt = _tx([_step(0, [_err_result("0A000", "ON CONFLICT is not supported by PGArbor yet",
                                     detail="pgarbor-unimplemented: EXEC-13")])], "target")
    diffs = [_diff("steps[0].results[0].sqlstate", ref="", tgt="0A000")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.reason == "first_divergence"
    assert not res.outcome.is_pass
    assert res.signature["unimplemented_lease"] == "EXEC-13"
    assert res.signature["sqlstate"] == "0A000"
    assert res.signature["ref_sqlstate"] == ""
    assert res.signature["diff_kind"] == "sqlstate"
    assert res.signature["step_kind"] == "sql"
    assert res.signature["first_keyword"] == "SELECT"


def test_rule6a_identical_reference_0a000_is_not_unsupported():
    msg = "feature not supported"
    ref = _tx([_step(0, [_err_result("0A000", msg)])], "reference")
    tgt = _tx([_step(0, [_err_result("0A000", msg, detail="pgarbor-unimplemented: EXEC-13")])],
              "target")
    diffs = [_diff("steps[0].results[0].error.detail", kind="error_field")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.MISMATCH


def test_rule6b_reference_error_target_rows_is_unexpected_acceptance():
    ref = _tx([_step(0, [_err_result("42P01", 'relation "t" does not exist')])], "reference")
    tgt = _tx([_step(0, [_rows_result([["1"]])])], "target")
    diffs = [_diff("steps[0].results[0].result_kind", kind="result_kind")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.UNEXPECTED_ACCEPTANCE
    assert res.signature["ref_sqlstate"] == "42P01"
    assert res.signature["sqlstate"] == ""
    # message template comes from the reference error for UNEXPECTED_ACCEPTANCE
    assert res.signature["message_template"] == 'relation "?" does not exist'


def test_rule6b_target_error_is_not_unexpected_acceptance():
    ref = _tx([_step(0, [_err_result("42P01")])], "reference")
    tgt = _tx([_step(0, [_err_result("42703")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate", kind="sqlstate")])
    assert res.outcome is Outcome.MISMATCH


def test_rule6c_plain_value_difference_is_mismatch():
    ref = _tx([_step(0, [_rows_result([["1"]])])], "reference")
    tgt = _tx([_step(0, [_rows_result([["2"]])])], "target")
    diffs = [_diff("steps[0].results[0].rows[0][0]", kind="value", ref="1", tgt="2")]
    res = classify(_case(), ref, tgt, diffs)
    assert res.outcome is Outcome.MISMATCH
    assert res.reason == "first_divergence"
    assert res.first_divergence is diffs[0]
    assert res.signature["diff_kind"] == "value"
    assert res.signature["unimplemented_lease"] == ""


def test_rule6c_step_level_divergence_is_mismatch():
    ref = _tx([_step(0, [_rows_result()]), _step(1, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result()])], "target")
    res = classify(_case([{"sql": "select 1"}, {"sql": "select 2"}]), ref, tgt,
                   [_diff("steps[1]", kind="missing_step")])
    assert res.outcome is Outcome.MISMATCH
    assert res.signature["step_kind"] == "sql"
    assert res.signature["sqlstate"] == ""


def test_unimplemented_detail_must_match_exact_shape():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("0A000", "x", detail="pgarbor-unimplemented: exec-13")])],
              "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")])
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.signature["unimplemented_lease"] == ""


def test_0a000_without_detail_has_no_lease():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_err_result("0A000", "not supported")])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].sqlstate")])
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.signature["unimplemented_lease"] == ""


# ---------------------------------------------------------------------------------------------
# Signature
# ---------------------------------------------------------------------------------------------


def test_signature_values_are_all_strings_and_complete():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result([["2"]])])], "target")
    res = classify(_case(), ref, tgt, [_diff("steps[0].results[0].rows[0][0]", kind="value")])
    assert set(res.signature) == {"diff_kind", "sqlstate", "ref_sqlstate", "message_template",
                                  "step_kind", "first_keyword", "unimplemented_lease"}
    assert all(isinstance(v, str) for v in res.signature.values())


def test_signature_setup_step_kind_and_setup_sql_keyword():
    ref = _tx([], "reference", setup=[_step(0, [_rows_result()], session="setup")])
    tgt = _tx([], "target", setup=[_step(0, [_err_result("42P01", 'table "x" missing')],
                                         session="setup")])
    res = classify(_case(setup="create table x(a int)"), ref, tgt,
                   [_diff("setup[0].results[0].sqlstate")])
    assert res.signature["step_kind"] == "setup"
    assert res.signature["first_keyword"] == "CREATE"
    assert res.signature["sqlstate"] == "42P01"
    assert res.signature["message_template"] == 'table "?" missing'


def test_signature_first_keyword_picks_divergent_statement():
    case = _case([{"sql": "create table t(a int); insert into t values (1); select a from t"}])
    ref = _tx([_step(0, [StatementResult(kind="command", command_tag="CREATE TABLE"),
                         StatementResult(kind="command", command_tag="INSERT 0 1"),
                         _rows_result()])], "reference")
    tgt = _tx([_step(0, [StatementResult(kind="command", command_tag="CREATE TABLE"),
                         _err_result("0A000", "x", detail="pgarbor-unimplemented: EXEC-13"),
                         _rows_result()])], "target")
    res = classify(case, ref, tgt, [_diff("steps[0].results[1].sqlstate")])
    assert res.outcome is Outcome.UNSUPPORTED
    assert res.signature["first_keyword"] == "INSERT"


def test_signature_wait_step_uses_async_sql():
    case = _case([
        {"sql": "select pg_sleep(0)", "async": True, "session": "a"},
        {"wait": {}, "session": "a"},
    ])
    ref = _tx([_step(0, [_rows_result()]), _step(1, [_rows_result()], kind="wait")], "reference")
    tgt = _tx([_step(0, [_rows_result()]),
               _step(1, [_err_result("0A000", "x", detail="pgarbor-unimplemented: EXEC-13")],
                     kind="wait")], "target")
    res = classify(case, ref, tgt, [_diff("steps[1].results[0].sqlstate")])
    assert res.signature["step_kind"] == "wait"
    assert res.signature["first_keyword"] == "SELECT"


def test_classify_never_raises_on_unparseable_diff_path():
    ref = _tx([_step(0, [_rows_result()])], "reference")
    tgt = _tx([_step(0, [_rows_result([["2"]])])], "target")
    res = classify(_case(), ref, tgt, [_diff("weird", kind="value")])
    assert res.outcome is Outcome.MISMATCH
    assert res.signature["sqlstate"] == ""
    assert res.signature["step_kind"] == ""
