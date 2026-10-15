import pytest
from pgarbor_harness.model import Outcome
from pgarbor_harness.sqllogictest.runner import SltFileResult, _finalize, _run_records
from pgarbor_harness.sqllogictest.parser import parse
from pgarbor_harness.isolation.tester import run_suite


@pytest.mark.parametrize("text", ["", "halt\n", "skipif pgarbor\nstatement ok\nSELECT 1\n\n"])
def test_zero_executed_slt_never_passes(text):
    result = SltFileResult("probe.slt", Outcome.PASS)
    _run_records(parse(text, "probe.slt"), object(), result)
    assert _finalize(result).outcome is Outcome.BLOCKED


@pytest.mark.parametrize("only", [[], ["typo"]])
def test_empty_or_unknown_isolation_selection_errors(tmp_path, only):
    (tmp_path / "isolation_schedule").write_text("test: known\n")
    with pytest.raises(ValueError): run_suite(tmp_path, object(), tmp_path / "out", only=only)


def test_isolation_host_permission_failure_is_environment_blocked(tmp_path):
    from pgarbor_harness.isolation.tester import _run_one
    (tmp_path / "specs").mkdir()
    (tmp_path / "specs" / "known.spec").write_text('session "s"\nstep "one" { SELECT 1; }\n')
    class Target:
        def provision(self, name): raise PermissionError("socket denied")
    result = _run_one("known", tmp_path, Target(), tmp_path)
    assert result.outcome is Outcome.BLOCKED
    assert result.reason.startswith("environment_permission:")
