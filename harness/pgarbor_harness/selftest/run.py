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

from dataclasses import dataclass, field
from typing import Any, Iterable, Optional


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


def builtin_fixtures() -> list[tuple[Any, Any]]:
    """[(cases.model.Case, model.Transcript)] hand-built per step 1 (no I/O)."""
    raise NotImplementedError("HAR-28: builtin selftest fixtures")


def run_offline(fixtures: Optional[Iterable[tuple[Any, Any]]] = None) -> SelftestReport:
    """Steps 1-6."""
    raise NotImplementedError("HAR-28: offline selftest")


def run_live(pool: Any, cases: Iterable[Any]) -> SelftestReport:
    """PG-vs-PG + mutants on live recordings."""
    raise NotImplementedError("HAR-28: live selftest")
