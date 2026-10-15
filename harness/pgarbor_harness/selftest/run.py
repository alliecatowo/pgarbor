"""ORACLE-13: verifier self-test. A harness that cannot flag a broken engine is not trusted.

Lease HAR-28.

``run_offline(fixtures)`` (no container needed; runs in CI and in pytest):
  1. ``fixtures``: list of (Case, reference Transcript) — by default the built-in fixtures
     from ``builtin_fixtures()``: hand-constructed transcripts covering rows (ordered and
     multiset, with duplicates, NULLs and empty strings), errors with sqlstate, a command,
     notices, a transaction block (txn_status T), typmod'd columns.
  2. identity: compare(reference, deepcopy(reference)) must be PASS for every fixture;
  3. every observation mutant applicable to a fixture must classify as
     mutants.EXPECTED_OUTCOME[name]; every mutant must be applicable to >= 1 fixture
     (otherwise the fixture set is too weak -> selftest FAIL "mutant never applicable");
  4. every comparator mutant must fail to detect at least one of its kill-list mutants on
     some fixture (proves the real comparator is strictly stronger);
  5. invariants: no Outcome other than PASS has is_pass; classify never returns PASS when
     diffs is non-empty (checked over all mutant runs).
  6. schema: the three example cases under tests/cases validate with cases.validate AND the
     JSON Schema subset interpreter accepts the schema file itself (no unknown keywords).

``run_live(pool, cases)`` (needs the oracle): PG-vs-PG — record each case twice through
runner.executor against PostgresTarget, compare the two -> every case must PASS (otherwise
the case or a normalizer is wrong: report INVALID_CASE candidates); then apply each
observation mutant to the second transcript and assert detection as above.

Returns SelftestReport; ``ok`` False on any failed check. The CLI exits 1 when not ok; the
factory refuses to start a campaign whose selftest is not ok (pgarbor_factory.campaign).
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Optional

import yaml

from ..cases.model import Case
from ..cases.validate import load_schema, validate
from ..compare import typed as _typed
from ..compare.classify import classify
from ..config import paths
from ..model import (ColumnDesc, ErrorInfo, Notice, Outcome, StatementResult, StepObservation,
                     Transcript)
from .mutants import COMPARATOR_KILLS, EXPECTED_OUTCOME
from . import mutants as _mutants


@dataclass
class SelftestCheck:
    name: str
    ok: bool
    detail: str = ""


@dataclass
class SelftestReport:
    ok: bool
    checks: list[SelftestCheck] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {"ok": self.ok, "checks": [{"name": c.name, "ok": c.ok, "detail": c.detail}
                                          for c in self.checks]}


#: Example case files whose validity is part of the offline self-test (step 6).
EXAMPLE_CASES: tuple[str, ...] = (
    "tests/cases/smoke/create-insert-select.yaml",
    "tests/cases/errors/division-by-zero.yaml",
    "tests/cases/errors/syntax-errors.yaml",
)

#: Keywords of the JSON Schema subset interpreted by cases.validate (its module docstring).
_SCHEMA_KEYWORDS: frozenset[str] = frozenset({
    "type", "properties", "required", "additionalProperties", "items", "enum", "const",
    "pattern", "minLength", "maxLength", "minItems", "maxItems", "uniqueItems", "minimum",
    "oneOf", "not", "$ref", "default", "description", "title", "$schema", "$id", "$defs",
})

#: (case, reference transcript, observed transcript) triples the sweeps run over.
_Sample = tuple[Case, Transcript, Transcript]


def _exc_text(exc: BaseException) -> str:
    return f"{type(exc).__name__}: {exc}"[:300]


def _report(checks: list[SelftestCheck]) -> SelftestReport:
    return SelftestReport(ok=bool(checks) and all(c.ok for c in checks), checks=checks)


# ---------------------------------------------------------------------------------------------
# Built-in fixtures (step 1). Hand-constructed; no I/O.
# ---------------------------------------------------------------------------------------------

_FX_TABLE_OID = 16385


def _fx_case(case_id: str, steps: list[dict[str, Any]]) -> Case:
    return Case.from_dict({"id": case_id, "title": case_id, "family": "selftest",
                           "feature_ids": ["selftest"], "steps": steps},
                          path=f"selftest/{case_id}.yaml")


def _fx_transcript(case_id: str, steps: list[StepObservation]) -> Transcript:
    return Transcript(case_id=case_id, endpoint="reference", setup=[], steps=steps)


def _fx_step(index: int, results: Iterable[StatementResult] = (),
             notices: Iterable[Notice] = (), txn: str = "I",
             **extra: Any) -> StepObservation:
    return StepObservation(index=index, session="a", kind="sql", results=list(results),
                           notices=list(notices), txn_status=txn, **extra)


def _fx_col(name: str, type_oid: int, typmod: int = -1, table_oid: int = 0,
            attnum: int = 0) -> ColumnDesc:
    return ColumnDesc(name=name, type_oid=type_oid, typmod=typmod, format=0,
                      table_oid=table_oid, column_attnum=attnum)


def _fx_rows(columns: list[ColumnDesc], rows: list[list[Any]], tag: str) -> StatementResult:
    return StatementResult(kind="rows", columns=columns, rows=rows, command_tag=tag)


def _fx_command(tag: str) -> StatementResult:
    return StatementResult(kind="command", command_tag=tag)


def _fx_error(sqlstate: str, message: str, position: Optional[str] = None) -> StatementResult:
    return StatementResult(kind="error", error=ErrorInfo(
        severity="ERROR", sqlstate=sqlstate, message=message, position=position))


def builtin_fixtures() -> list[tuple[Any, Any]]:
    """[(cases.model.Case, model.Transcript)] hand-built per step 1 (no I/O)."""
    fixtures: list[tuple[Any, Any]] = []

    # A: ordered rows with >= 2 distinct rows, a NULL, an empty string, table-origin columns.
    a_id = "selftest.ordered-rows"
    fixtures.append((
        _fx_case(a_id, [{"sql": "SELECT id, name FROM t ORDER BY id", "order": "ordered"}]),
        _fx_transcript(a_id, [_fx_step(0, [_fx_rows(
            [_fx_col("id", 23, table_oid=_FX_TABLE_OID, attnum=1),
             _fx_col("name", 25, table_oid=_FX_TABLE_OID, attnum=2)],
            [["1", "a"], ["2", None], ["3", ""]], "SELECT 3")])]),
    ))

    # B: multiset rows with a duplicate row, a NULL and an empty string (the only fixture
    # with duplicate rows, so removing it makes drop_duplicate_row never applicable).
    b_id = "selftest.multiset-dup"
    fixtures.append((
        _fx_case(b_id, [{"sql": "SELECT grp, val FROM t", "order": "multiset"}]),
        _fx_transcript(b_id, [_fx_step(0, [_fx_rows(
            [_fx_col("grp", 23), _fx_col("val", 25)],
            [["1", "x"], ["1", "x"], ["2", ""], [None, "y"]], "SELECT 4")])]),
    ))

    # C: typmod'd column numeric(6,2) (typmod 393222), ordered.
    c_id = "selftest.typmod-numeric"
    fixtures.append((
        _fx_case(c_id, [{"sql": "SELECT amount FROM n62", "order": "ordered"}]),
        _fx_transcript(c_id, [_fx_step(0, [_fx_rows(
            [_fx_col("amount", 1700, typmod=393222)],
            [["1.50"], ["2.00"]], "SELECT 2")])]),
    ))

    # D: an error with sqlstate and position.
    d_id = "selftest.error-sqlstate"
    fixtures.append((
        _fx_case(d_id, [{"sql": "SELECT * FROM missing_t"}]),
        _fx_transcript(d_id, [_fx_step(0, [_fx_error(
            "42P01", 'relation "missing_t" does not exist', position="15")])]),
    ))

    # E: transaction block: BEGIN (T), INSERT command (T), failing statement (E).
    e_id = "selftest.txn-command"
    fixtures.append((
        _fx_case(e_id, [{"sql": "BEGIN"}, {"sql": "INSERT INTO t VALUES (1)"},
                        {"sql": "SELECT 1/0"}]),
        _fx_transcript(e_id, [
            _fx_step(0, [_fx_command("BEGIN")], txn="T"),
            _fx_step(1, [_fx_command("INSERT 0 1")], txn="T"),
            _fx_step(2, [_fx_error("22012", "division by zero")], txn="E"),
        ]),
    ))

    # F: a step with a notice.
    f_id = "selftest.notices"
    fixtures.append((
        _fx_case(f_id, [{"sql": "CREATE TABLE IF NOT EXISTS t (a int)"}]),
        _fx_transcript(f_id, [_fx_step(
            0, [_fx_command("CREATE TABLE")],
            notices=[Notice(severity="NOTICE", sqlstate="42P07",
                            message='relation "t" already exists, skipping')])]),
    ))
    return fixtures


# ---------------------------------------------------------------------------------------------
# Sweeps shared by run_offline and run_live
# ---------------------------------------------------------------------------------------------


def _identity_check(case: Case, ref: Transcript, tgt: Transcript, name: str,
                    checks: list[SelftestCheck]) -> None:
    try:
        diffs = _typed.compare_transcripts(case, ref, tgt)
        res = classify(case, ref, tgt, diffs)
    except Exception as exc:  # a broken comparator/classifier is a self-test failure
        checks.append(SelftestCheck(name, False, _exc_text(exc)))
        return
    if res.outcome is Outcome.PASS and not diffs:
        checks.append(SelftestCheck(name, True, "identical"))
    else:
        hint = (" (INVALID_CASE candidate: case or normalizer is nondeterministic)"
                if name.startswith("live:") else "")
        checks.append(SelftestCheck(
            name, False,
            f"{res.outcome.value} {res.reason}: {_typed.render_diff(diffs, limit=3)}{hint}"))


def _mutant_sweep(samples: list[_Sample], prefix: str, checks: list[SelftestCheck]) -> None:
    registry = _mutants.observation_mutants()
    unknown = sorted(set(registry) ^ set(EXPECTED_OUTCOME))
    checks.append(SelftestCheck(
        f"{prefix}mutants:registry", not unknown,
        ("names differ: " + ", ".join(unknown)) if unknown
        else f"{len(registry)} observation mutants"))
    for name, expected in EXPECTED_OUTCOME.items():
        fn: Optional[Callable[[Transcript], Optional[Transcript]]] = registry.get(name)
        if fn is None:
            checks.append(SelftestCheck(f"{prefix}mutant:{name}", False, "no implementation"))
            continue
        applicable = 0
        failures: list[str] = []
        for case, ref, tgt in samples:
            try:
                mutated = fn(copy.deepcopy(tgt))
                if mutated is None:
                    continue
                applicable += 1
                diffs = _typed.compare_transcripts(case, ref, mutated)
                res = classify(case, ref, mutated, diffs)
            except Exception as exc:
                failures.append(f"{case.id}: {_exc_text(exc)}")
                continue
            if res.outcome is not expected:
                failures.append(f"{case.id}: got {res.outcome.value} ({res.reason}), "
                                f"expected {expected.value}")
            if diffs and res.outcome.is_pass:
                failures.append(f"{case.id}: classified PASS with {len(diffs)} diff(s)")
        if applicable == 0:
            checks.append(SelftestCheck(f"{prefix}mutant:{name}", False,
                                        "mutant never applicable"))
        else:
            checks.append(SelftestCheck(
                f"{prefix}mutant:{name}", not failures,
                f"applicable to {applicable} sample(s)" if not failures
                else "; ".join(failures[:5])))


def _comparator_sweep(samples: list[_Sample], prefix: str,
                      checks: list[SelftestCheck]) -> None:
    registry = _mutants.observation_mutants()
    for cname, kills in COMPARATOR_KILLS.items():
        witnesses: list[str] = []
        try:
            weak = _mutants.comparator_mutant(cname)
            for oname in kills:
                fn = registry[oname]
                for case, ref, tgt in samples:
                    mutated = fn(copy.deepcopy(tgt))
                    if mutated is None:
                        continue
                    if not _typed.compare_transcripts(case, ref, mutated):
                        continue  # the real comparator must flag it
                    if not weak(case, ref, mutated):
                        witnesses.append(f"{oname} on {case.id}")
        except Exception as exc:
            checks.append(SelftestCheck(f"{prefix}comparator:{cname}", False,
                                        _exc_text(exc)))
            continue
        checks.append(SelftestCheck(
            f"{prefix}comparator:{cname}", bool(witnesses),
            "; ".join(witnesses[:3]) if witnesses else
            "no sample where the real comparator flags a kill mutant and this one passes"))


def _invariant_check(checks: list[SelftestCheck]) -> None:
    bad = [o.value for o in Outcome if o.is_pass != (o is Outcome.PASS)]
    checks.append(SelftestCheck(
        "invariant:is_pass", not bad,
        ("is_pass wrong for: " + ", ".join(bad)) if bad else "only PASS is_pass"))


# ---------------------------------------------------------------------------------------------
# Step 6: schema
# ---------------------------------------------------------------------------------------------


def _walk_schema(node: Any, path: str, errs: list[str]) -> None:
    """Every schema keyword must be one the cases.validate interpreter supports."""
    if not isinstance(node, dict):
        errs.append(f"{path}: schema node must be an object")
        return
    for key, val in node.items():
        here = f"{path}.{key}"
        if key not in _SCHEMA_KEYWORDS:
            errs.append(f"{here}: unknown keyword")
        elif key in ("properties", "$defs"):
            if not isinstance(val, dict):
                errs.append(f"{here}: must be an object")
                continue
            for name, sub in val.items():
                _walk_schema(sub, f"{here}.{name}", errs)
        elif key in ("items", "not"):
            _walk_schema(val, here, errs)
        elif key == "oneOf":
            if not isinstance(val, list):
                errs.append(f"{here}: must be an array")
                continue
            for i, sub in enumerate(val):
                _walk_schema(sub, f"{here}[{i}]", errs)
        elif key == "additionalProperties" and not isinstance(val, bool):
            errs.append(f"{here}: must be a boolean")
        elif key == "$ref" and not (isinstance(val, str) and val.startswith("#/")):
            errs.append(f"{here}: only local #/ pointers are supported")


def _schema_checks(checks: list[SelftestCheck]) -> None:
    try:
        schema = load_schema()
    except Exception as exc:
        checks.append(SelftestCheck("schema:load", False, _exc_text(exc)))
        return
    errs: list[str] = []
    _walk_schema(schema, "$", errs)
    checks.append(SelftestCheck("schema:keywords", not errs,
                                "; ".join(errs[:5]) or "all keywords supported"))
    root = paths().root
    for rel in EXAMPLE_CASES:
        name = f"schema:example:{rel}"
        path = root / rel
        try:
            doc = yaml.safe_load(path.read_bytes().decode("utf-8"))
            problems = validate(doc, rel, schema)
        except Exception as exc:
            checks.append(SelftestCheck(name, False, _exc_text(exc)))
            continue
        checks.append(SelftestCheck(name, not problems,
                                    "; ".join(problems[:5]) or "valid"))


# ---------------------------------------------------------------------------------------------
# Entry points
# ---------------------------------------------------------------------------------------------


def run_offline(fixtures: Optional[Iterable[tuple[Any, Any]]] = None) -> SelftestReport:
    """Steps 1-6."""
    fx = list(builtin_fixtures() if fixtures is None else fixtures)
    checks: list[SelftestCheck] = [
        SelftestCheck("offline:fixtures", bool(fx), f"{len(fx)} fixture(s)")]
    samples: list[_Sample] = [(case, ref, copy.deepcopy(ref)) for case, ref in fx]
    for case, ref, tgt in samples:
        _identity_check(case, ref, tgt, f"offline:identity:{case.id}", checks)
    _mutant_sweep(samples, "offline:", checks)
    _comparator_sweep(samples, "offline:", checks)
    _invariant_check(checks)
    _schema_checks(checks)
    return _report(checks)


def run_live(pool: Any, cases: Iterable[Any]) -> SelftestReport:
    """PG-vs-PG + mutants on live recordings."""
    from ..runner.executor import execute_case
    from ..runner.targets import PostgresTarget

    checks: list[SelftestCheck] = []
    case_list = list(cases)
    samples: list[_Sample] = []
    target = PostgresTarget(pool)
    try:
        target.start()
    except Exception as exc:
        checks.append(SelftestCheck("live:start", False, _exc_text(exc)))
        return _report(checks)
    try:
        for case in case_list:
            try:
                ref = execute_case(case, target, "reference")
                tgt = execute_case(case, target, "target")
            except Exception as exc:
                checks.append(SelftestCheck(f"live:{case.id}", False,
                                            "execution failed: " + _exc_text(exc)))
                continue
            samples.append((case, ref, tgt))
    finally:
        try:
            target.stop()
        except Exception as exc:
            checks.append(SelftestCheck("live:stop", False, _exc_text(exc)))
    checks.append(SelftestCheck("live:cases", bool(samples),
                                f"{len(samples)} of {len(case_list)} case(s) executed"))
    for case, ref, tgt in samples:
        _identity_check(case, ref, tgt, f"live:{case.id}", checks)
    _mutant_sweep(samples, "live:", checks)
    _comparator_sweep(samples, "live:", checks)
    _invariant_check(checks)
    return _report(checks)
