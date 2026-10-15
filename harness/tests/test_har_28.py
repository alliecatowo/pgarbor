"""HAR-28 (ORACLE-13): verifier self-test tests.

Offline tests run in CI. The live test needs the PG18 oracle (marked ``oracle``).
"""

from __future__ import annotations

import pytest

from pgarbor_harness.compare import typed
from pgarbor_harness.selftest import run
from pgarbor_harness.selftest.mutants import EXPECTED_OUTCOME


def _failed(report: run.SelftestReport) -> list[run.SelftestCheck]:
    return [c for c in report.checks if not c.ok]


def test_builtin_fixtures_ok() -> None:
    report = run.run_offline()
    assert report.ok, [(c.name, c.detail) for c in _failed(report)]


def test_builtin_fixture_ids_unique() -> None:
    ids = [case.id for case, _ in run.builtin_fixtures()]
    assert len(ids) == len(set(ids))


def test_every_mutant_checked_offline() -> None:
    report = run.run_offline()
    names = {c.name for c in report.checks}
    for mutant in EXPECTED_OUTCOME:
        assert f"offline:mutant:{mutant}" in names


def test_broken_comparator_fails_selftest(monkeypatch: pytest.MonkeyPatch) -> None:
    # A comparator that never reports a difference must be caught.
    monkeypatch.setattr(typed, "compare_transcripts", lambda case, ref, tgt: [])
    report = run.run_offline()
    assert not report.ok


def test_removing_duplicate_fixture_reports_never_applicable() -> None:
    fixtures = [(case, ref) for case, ref in run.builtin_fixtures()
                if case.id != "selftest.multiset-dup"]
    report = run.run_offline(fixtures)
    assert not report.ok
    details = [c.detail for c in report.checks
               if c.name == "offline:mutant:drop_duplicate_row"]
    assert details == ["mutant never applicable"]


def test_schema_walker_rejects_unknown_keyword() -> None:
    errs: list[str] = []
    run._walk_schema({"type": "object", "frobnicate": True}, "$", errs)
    assert errs and "frobnicate" in errs[0]


def test_schema_walker_accepts_case_schema() -> None:
    from pgarbor_harness.cases.validate import load_schema

    errs: list[str] = []
    run._walk_schema(load_schema(), "$", errs)
    assert errs == []


def test_example_cases_validate() -> None:
    report = run.run_offline()
    example_checks = [c for c in report.checks if c.name.startswith("schema:example:")]
    assert len(example_checks) == len(run.EXAMPLE_CASES)
    assert all(c.ok for c in example_checks), [(c.name, c.detail) for c in example_checks]


@pytest.mark.oracle
def test_live_pg_vs_pg_all_pass() -> None:
    from pgarbor_harness.cases.loader import load_corpus
    from pgarbor_harness.config import paths
    from pgarbor_harness.oracle.pool import OraclePool

    p = paths()
    corpus = load_corpus(p.cases, p.root)
    cases = [c for c in corpus.cases if c.family == "smoke"]
    pool = OraclePool()
    pool.start()
    try:
        report = run.run_live(pool, cases)
    finally:
        pool.close()
    assert report.ok, [(c.name, c.detail) for c in _failed(report)]
