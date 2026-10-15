"""OID data requires provenance before numeric differences may be ignored."""
from copy import deepcopy
from pgarbor_harness.cases.model import Case
from pgarbor_harness.compare.normalize import apply_case_normalizers
from pgarbor_harness.compare.typed import compare_transcripts
from pgarbor_harness.model import ColumnDesc, StatementResult, StepObservation, Transcript


def sample(value="20000", table=0):
    case = Case.from_dict(dict(id="probe.oid", title="OID", family="probe", feature_ids=[],
                               steps=[{"sql": "SELECT 20000::oid"}]))
    tr = Transcript(case.id, "reference", steps=[StepObservation(0, "a", "sql", results=[
        StatementResult("rows", columns=[ColumnDesc("oid", 26, table_oid=table)],
                        rows=[[value]], command_tag="SELECT 1")])])
    return case, tr


def test_wrong_literal_oid_cannot_pass():
    case, ref = sample()
    _, target = sample("30000")
    assert compare_transcripts(case, ref, target)


def test_stored_oid_is_data_even_when_equal_to_descriptor_identity():
    case, ref = sample(table=20000)
    _, target = sample("30000", table=30000)
    assert compare_transcripts(case, ref, target)


def test_descriptor_renumbering_remains_accepted():
    case, ref = sample("42", table=20000)
    _, target = sample("42", table=30000)
    assert compare_transcripts(case, ref, target) == []


def test_normalization_is_idempotent():
    case, tr = sample(table=20000)
    apply_case_normalizers(case, tr)
    once = deepcopy(tr)
    apply_case_normalizers(case, tr)
    assert tr == once


def test_legacy_normalized_oid_reference_blocks_before_target_execution(tmp_path):
    from pgarbor_harness.model import ExpectedFile, ReferenceIdentity, Outcome, expected_path
    from pgarbor_harness.runner.differential import run_case
    case, legacy = sample("U1")
    reference = ExpectedFile(case.id, case.sha256, ReferenceIdentity("image", "version", "fp"), legacy)
    reference.save(expected_path(tmp_path / "expected", case.id))
    result = run_case(case, object(), tmp_path / "expected", tmp_path / "out", reruns=0)
    assert result.outcome is Outcome.BLOCKED
    assert result.reason == "reference_normalization_stale"
