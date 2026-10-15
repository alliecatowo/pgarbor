"""HAR-31 tests: deterministic ORACLE-09 seed corpus generators.

Lease HAR-31. Executed centrally; no oracle container is needed (pure generation + validation).
"""

from __future__ import annotations

import re

import pytest
import yaml

from pgarbor_harness.cases.validate import validate
from pgarbor_harness.config import paths
from pgarbor_harness.generate import seeds
from pgarbor_harness.model import canonical_json

REF_DIR = paths().contracts / "reference" / "pg18"
ID_RE = re.compile(r"^gen-[a-z-]+\.[a-z0-9][a-z0-9_-]*$")


def _rel_path(case: dict) -> str:
    family = case["family"]
    rest = case["id"][len(family) + 1:]
    return f"tests/cases/{family}/{rest}.yaml"


def _all_cases(seed: int) -> list[dict]:
    out: list[dict] = []
    for name in sorted(seeds.GENERATORS):
        out.extend(seeds.GENERATORS[name](seed, REF_DIR))
    return out


@pytest.mark.parametrize("name", sorted(seeds.GENERATORS))
def test_same_seed_is_byte_identical(name: str) -> None:
    gen = seeds.GENERATORS[name]
    first = canonical_json(gen(1, REF_DIR))
    second = canonical_json(gen(1, REF_DIR))
    assert first == second


def test_operator_different_seed_changes_literal_pairs() -> None:
    a = seeds.gen_operators(1, REF_DIR)
    b = seeds.gen_operators(2, REF_DIR)
    sql_a = [step["sql"] for case in a for step in case["steps"]]
    sql_b = [step["sql"] for case in b for step in case["steps"]]
    assert sql_a != sql_b
    assert set(sql_a) != set(sql_b)


@pytest.mark.parametrize("name", sorted(seeds.GENERATORS))
def test_every_generated_case_validates(name: str) -> None:
    cases = seeds.GENERATORS[name](1, REF_DIR)
    assert cases, f"generator {name} produced no cases"
    for case in cases:
        errs = validate(case, _rel_path(case))
        assert errs == [], (case["id"], errs)


def test_ids_unique_and_well_formed() -> None:
    cases = _all_cases(1)
    ids = [c["id"] for c in cases]
    assert len(ids) == len(set(ids))
    for case in cases:
        assert ID_RE.match(case["id"]), case["id"]
        assert case["id"].startswith(case["family"] + ".")
        assert case["id"].endswith("-1")
        assert case["tier"] == "core"
        assert case["provenance"]["source"] == "generated"
        assert case["provenance"]["ref"].startswith("seeds/")
        assert case["provenance"]["ref"].endswith("@1")


def test_ids_change_with_seed() -> None:
    ids1 = {c["id"] for c in _all_cases(1)}
    ids2 = {c["id"] for c in _all_cases(2)}
    assert ids1.isdisjoint(ids2)


def test_yaml_round_trip_preserves_case() -> None:
    for case in _all_cases(3):
        assert yaml.safe_load(yaml.safe_dump(case, sort_keys=False, allow_unicode=True,
                                             width=1000)) == case


def test_operator_pairs_capped_and_null_included() -> None:
    cases = seeds.gen_operators(1, REF_DIR)
    assert cases
    for case in cases:
        assert 1 <= len(case["steps"]) <= 12
    assert any("NULL::" in step["sql"] for case in cases for step in case["steps"])


def test_ddl_dml_uses_multiset_and_ordered_selects() -> None:
    cases = seeds.gen_ddl_dml(1, REF_DIR)
    orders = {step.get("order") for case in cases for step in case["steps"]}
    assert "multiset" in orders
    assert "ordered" in orders or all(
        "ORDER BY" not in step["sql"] for case in cases for step in case["steps"]
    )


def test_write_cases_is_idempotent_and_reports_conflicts(tmp_path) -> None:
    cases = seeds.gen_io(1, REF_DIR)
    assert seeds.write_cases(cases, tmp_path) == []
    # Rewriting identical content is not a conflict.
    assert seeds.write_cases(cases, tmp_path) == []
    # Different content at an existing path is a conflict and is not overwritten.
    changed = [dict(cases[0], title="changed title")]
    conflicts = seeds.write_cases(changed, tmp_path)
    assert len(conflicts) == 1
    written = (tmp_path / cases[0]["family"] / (cases[0]["id"][len(cases[0]["family"]) + 1:]
                                                  + ".yaml")).read_text(encoding="utf-8")
    assert "changed title" not in written
