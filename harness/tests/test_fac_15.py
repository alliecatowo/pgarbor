"""Tests for FAC-15: pgarbor_factory.scheduler (wave dispatch and LPT verification sharding)."""

from __future__ import annotations

import itertools
import random
from typing import Any

import pytest

from pgarbor_factory.records import LeaseEntry, LeaseSpec, LeaseState, Transition
from pgarbor_factory.scheduler import Shard, plan_shards, plan_wave


# ---------------------------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------------------------


def _task(tid: str, contract: tuple[str, ...] = (), integ: tuple[str, ...] = (),
          state: str = "PLANNED") -> dict[str, Any]:
    return {
        "id": tid,
        "title": tid,
        "epic": "test",
        "integration_dependencies": list(integ),
        "contract_dependencies": list(contract),
        "ownership_domain": "store",
        "owned_path_glob": "crates/pgarbor-store/**",
        "deliverable": "impl",
        "deferred_verification_family": "none",
        "state": state,
    }


def _graph(*tasks: dict[str, Any]) -> dict[str, Any]:
    return {"tasks": list(tasks)}


def _spec(lid: str, tasks: list[str], files: list[str], tier: str = "haiku") -> LeaseSpec:
    return LeaseSpec(lease_id=lid, crate="pgarbor-store", task_ids=list(tasks),
                     files=list(files), model_tier=tier, summary="", spec="")


def _entry(lid: str, state: LeaseState, history: tuple[Transition, ...] = ()) -> LeaseEntry:
    return LeaseEntry(lease_id=lid, crate="pgarbor-store", state=state, history=list(history))


def _ledger(*entries: LeaseEntry) -> dict[str, LeaseEntry]:
    return {e.lease_id: e for e in entries}


# ---------------------------------------------------------------------------------------------
# plan_wave
# ---------------------------------------------------------------------------------------------


def test_max_authors_caps_new_grants():
    graph = _graph(_task("STORE-01"), _task("STORE-02"), _task("STORE-03"), _task("STORE-04"))
    specs = [_spec(f"ST-0{i}", [f"STORE-0{i}"], [f"crates/s/f{i}.rs"]) for i in range(1, 5)]
    ledger = _ledger(*(_entry(s.lease_id, LeaseState.SPEC_READY) for s in specs))
    plan = plan_wave(graph, specs, ledger, frozen_contracts=[], max_authors=2)
    assert plan.grants == ["ST-01", "ST-02"]
    assert plan.deferred == {"ST-03": "max_authors", "ST-04": "max_authors"}


def test_already_speculating_leases_count_against_max_authors():
    graph = _graph(_task("STORE-01"), _task("STORE-02"), _task("STORE-09", state="SPECULATING"))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"]),
             _spec("ST-02", ["STORE-02"], ["b.rs"]),
             _spec("ST-09", ["STORE-09"], ["c.rs"])]
    ledger = _ledger(_entry("ST-01", LeaseState.SPEC_READY),
                     _entry("ST-02", LeaseState.SPEC_READY),
                     _entry("ST-09", LeaseState.SPECULATING))
    plan = plan_wave(graph, specs, ledger, frozen_contracts=[], max_authors=2)
    assert plan.grants == ["ST-01"]
    assert plan.deferred == {"ST-02": "max_authors"}


def test_tier_caps_limit_only_listed_tiers():
    graph = _graph(_task("STORE-01"), _task("STORE-02"), _task("STORE-03"), _task("STORE-04"))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"], "haiku"),
             _spec("ST-02", ["STORE-02"], ["b.rs"], "haiku"),
             _spec("ST-03", ["STORE-03"], ["c.rs"], "sonnet"),
             _spec("ST-04", ["STORE-04"], ["d.rs"], "opus")]
    ledger = _ledger(*(_entry(s.lease_id, LeaseState.SPEC_READY) for s in specs))
    plan = plan_wave(graph, specs, ledger, [], max_authors=10,
                     tier_caps={"haiku": 1, "sonnet": 12, "opus": 2})
    assert "ST-03" in plan.grants and "ST-04" in plan.grants
    haiku_grants = [lid for lid in plan.grants if lid in ("ST-01", "ST-02")]
    assert len(haiku_grants) == 1
    other = "ST-02" if haiku_grants == ["ST-01"] else "ST-01"
    assert plan.deferred[other] == "tier_cap:haiku"


def test_tier_cap_counts_existing_speculating_leases():
    graph = _graph(_task("STORE-01"), _task("STORE-02"), _task("STORE-09", state="SPECULATING"))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"], "opus"),
             _spec("ST-02", ["STORE-02"], ["b.rs"], "opus"),
             _spec("ST-09", ["STORE-09"], ["c.rs"], "opus")]
    ledger = _ledger(_entry("ST-01", LeaseState.SPEC_READY),
                     _entry("ST-02", LeaseState.SPEC_READY),
                     _entry("ST-09", LeaseState.SPECULATING))
    plan = plan_wave(graph, specs, ledger, [], max_authors=10, tier_caps={"opus": 2})
    assert plan.grants == ["ST-01"]
    assert plan.deferred["ST-02"] == "tier_cap:opus"


def test_repair_order_first():
    # ST-02 is a plain lease with a large fanout; ST-05 is a repair lease with none.
    graph = _graph(_task("STORE-02"),
                   _task("STORE-03", integ=("STORE-02",)),
                   _task("STORE-04", integ=("STORE-02",)),
                   _task("STORE-05"))
    specs = [_spec("ST-02", ["STORE-02"], ["a.rs"]),
             _spec("ST-05", ["STORE-05"], ["b.rs"])]
    ledger = _ledger(_entry("ST-02", LeaseState.SPEC_READY),
                     _entry("ST-05", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, [], max_authors=1, repair_order=["ST-05"])
    assert plan.grants == ["ST-05"]
    assert plan.deferred == {"ST-02": "max_authors"}


def test_vertical_slice_beats_fanout_without_repair():
    graph = _graph(_task("DOC-01"),
                   _task("DOC-02", integ=("DOC-01",)),
                   _task("DOC-03", integ=("DOC-01",)),
                   _task("ARCH-01"))
    specs = [_spec("DO-01", ["DOC-01"], ["a.rs"]),
             _spec("AR-01", ["ARCH-01"], ["b.rs"])]
    ledger = _ledger(_entry("DO-01", LeaseState.SPEC_READY),
                     _entry("AR-01", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, [], max_authors=1)
    assert plan.grants == ["AR-01"]


def test_fanout_orders_non_slice_leases_and_lease_id_breaks_ties():
    graph = _graph(_task("DOC-01"), _task("DOC-02", integ=("DOC-01",)),
                   _task("DOC-03"), _task("DOC-04"))
    specs = [_spec("ZZ-01", ["DOC-03"], ["a.rs"]),
             _spec("AA-01", ["DOC-04"], ["b.rs"]),
             _spec("MM-01", ["DOC-01"], ["c.rs"])]
    ledger = _ledger(*(_entry(s.lease_id, LeaseState.SPEC_READY) for s in specs))
    plan = plan_wave(graph, specs, ledger, [], max_authors=3)
    # MM-01 has the highest fanout; the other two tie at 0 and sort by lease id.
    assert plan.grants == ["MM-01", "AA-01", "ZZ-01"]


def test_integration_dependencies_do_not_block_grant():
    graph = _graph(_task("STORE-01"),
                   _task("STORE-02", integ=("STORE-01",)))  # STORE-01 not verified
    specs = [_spec("ST-02", ["STORE-02"], ["a.rs"])]
    ledger = _ledger(_entry("ST-02", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, frozen_contracts=[], max_authors=5)
    assert plan.grants == ["ST-02"]


def test_unfrozen_contract_dependency_blocks_grant():
    graph = _graph(_task("ARCH-01"), _task("STORE-02", contract=("ARCH-01",)))
    specs = [_spec("ST-02", ["STORE-02"], ["a.rs"])]
    ledger = _ledger(_entry("ST-02", LeaseState.SPEC_READY))
    blocked = plan_wave(graph, specs, ledger, frozen_contracts=[], max_authors=5)
    assert blocked.grants == []
    assert "ST-02" in blocked.deferred
    granted = plan_wave(graph, specs, ledger, frozen_contracts=["ARCH-01"], max_authors=5)
    assert granted.grants == ["ST-02"]


def test_overlapping_files_never_granted_together():
    graph = _graph(_task("STORE-01"), _task("STORE-02"))
    specs = [_spec("ST-01", ["STORE-01"], ["shared.rs"]),
             _spec("ST-02", ["STORE-02"], ["shared.rs", "other.rs"])]
    ledger = _ledger(_entry("ST-01", LeaseState.SPEC_READY),
                     _entry("ST-02", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, [], max_authors=5)
    assert plan.grants == ["ST-01"]
    assert plan.deferred == {"ST-02": "file_overlap:ST-01"}


def test_file_overlap_with_speculating_lease_defers():
    graph = _graph(_task("STORE-01"), _task("STORE-09", state="SPECULATING"))
    specs = [_spec("ST-01", ["STORE-01"], ["shared.rs"]),
             _spec("ST-09", ["STORE-09"], ["shared.rs"])]
    ledger = _ledger(_entry("ST-01", LeaseState.SPEC_READY),
                     _entry("ST-09", LeaseState.SPECULATING))
    plan = plan_wave(graph, specs, ledger, [], max_authors=5)
    assert plan.grants == []
    assert plan.deferred == {"ST-01": "file_overlap:ST-09"}


def test_suspended_leases_are_deferred():
    graph = _graph(_task("STORE-01"), _task("STORE-02"))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"]), _spec("ST-02", ["STORE-02"], ["b.rs"])]
    suspended_history = (Transition(at_unix=1, from_state="SPEC_READY", to_state="SPEC_READY",
                                    note="suspended: AMEND-02-001"),)
    ledger = _ledger(_entry("ST-01", LeaseState.SPEC_READY, suspended_history),
                     _entry("ST-02", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, [], max_authors=5)
    assert plan.grants == ["ST-02"]
    assert plan.deferred == {"ST-01": "suspended"}


def test_non_spec_ready_entries_are_not_candidates():
    graph = _graph(_task("STORE-01"), _task("STORE-02"))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"]), _spec("ST-02", ["STORE-02"], ["b.rs"])]
    ledger = _ledger(_entry("ST-01", LeaseState.REJECTED),
                     _entry("ST-02", LeaseState.SPEC_READY))
    plan = plan_wave(graph, specs, ledger, [], max_authors=5)
    assert plan.grants == ["ST-02"]
    assert "ST-01" not in plan.deferred


def test_plan_wave_is_deterministic_across_input_order():
    graph = _graph(_task("STORE-01"), _task("STORE-02"), _task("STORE-03"),
                   _task("DOC-01"), _task("DOC-02", integ=("DOC-01",)))
    specs = [_spec("ST-01", ["STORE-01"], ["a.rs"]),
             _spec("ST-02", ["STORE-02"], ["b.rs"]),
             _spec("ST-03", ["STORE-03"], ["c.rs"]),
             _spec("DO-01", ["DOC-01"], ["d.rs"]),
             _spec("DO-02", ["DOC-02"], ["e.rs"])]
    entries = [_entry(s.lease_id, LeaseState.SPEC_READY) for s in specs]
    results = set()
    for order in itertools.islice(itertools.permutations(range(len(specs))), 24):
        spec_order = [specs[i] for i in order]
        ledger = _ledger(*(entries[i] for i in order))
        plan = plan_wave(graph, spec_order, ledger, [], max_authors=2,
                         repair_order=["DO-02"])
        results.add((tuple(plan.grants), tuple(sorted(plan.deferred.items()))))
    assert len(results) == 1
    grants, _ = results.pop()
    assert grants[0] == "DO-02"


def test_grants_never_exceed_max_authors_even_with_many_candidates():
    rng = random.Random(7)
    tasks = [_task(f"STORE-{i:02d}") for i in range(30)]
    graph = _graph(*tasks)
    specs = [_spec(f"ST-{i:02d}", [f"STORE-{i:02d}"], [f"f{i}.rs"],
                   rng.choice(["haiku", "sonnet", "opus"])) for i in range(30)]
    ledger = _ledger(*(_entry(s.lease_id, LeaseState.SPEC_READY) for s in specs))
    for cap in (0, 1, 4, 7):
        plan = plan_wave(graph, specs, ledger, [], max_authors=cap,
                         tier_caps={"opus": 1, "sonnet": 3})
        assert len(plan.grants) <= cap
        assert sum(1 for s in specs if s.model_tier == "opus"
                   and s.lease_id in plan.grants) <= 1


def test_negative_max_authors_is_rejected():
    with pytest.raises(ValueError):
        plan_wave(_graph(), [], {}, [], max_authors=-1)


# ---------------------------------------------------------------------------------------------
# plan_shards
# ---------------------------------------------------------------------------------------------


def _durations(values: list[float]) -> dict[str, float]:
    return {f"item{i}": v for i, v in enumerate(values)}


def test_plan_shards_balances_5_4_3_3_3_into_9_and_9():
    durations = _durations([5, 4, 3, 3, 3])
    shards = plan_shards(durations.keys(), durations, 2)
    assert [s.estimated_s for s in shards] == [9.0, 9.0]
    assert sorted(i for s in shards for i in s.items) == sorted(durations)


def test_plan_shards_is_deterministic_and_order_independent():
    durations = _durations([2.5, 1.0, 7.0, 3.0, 3.0, 0.5, 4.0, 6.0])
    ids = list(durations)
    reference = plan_shards(ids, durations, 3)
    for rotation in range(len(ids)):
        shuffled = ids[rotation:] + ids[:rotation]
        assert plan_shards(shuffled, durations, 3) == reference


def test_plan_shards_defaults_missing_durations_to_one_second():
    shards = plan_shards(["a", "b", "c", "d"], {"a": 10.0}, 2)
    total = sum(s.estimated_s for s in shards)
    assert total == pytest.approx(13.0)
    # 'a' (10 s) is alone on one shard; the three 1 s items fill the other.
    by_size = sorted(shards, key=lambda s: s.estimated_s)
    assert by_size[0].estimated_s == pytest.approx(3.0)
    assert by_size[1].estimated_s == pytest.approx(10.0)


def test_plan_shards_covers_every_item_exactly_once():
    rng = random.Random(3)
    ids = [f"case-{i:04d}" for i in range(200)]
    durations = {i: rng.uniform(0.1, 9.0) for i in ids}
    shards = plan_shards(ids, durations, 8)
    assert [s.index for s in shards] == list(range(8))
    flat = [i for s in shards for i in s.items]
    assert sorted(flat) == sorted(ids)
    assert len(flat) == len(set(flat))
    for s in shards:
        assert s.estimated_s == pytest.approx(sum(durations[i] for i in s.items))


def test_plan_shards_handles_empty_input():
    shards = plan_shards([], {}, 3)
    assert shards == [Shard(index=0, items=[], estimated_s=0.0),
                      Shard(index=1, items=[], estimated_s=0.0),
                      Shard(index=2, items=[], estimated_s=0.0)]


@pytest.mark.parametrize("bad", [0, -2])
def test_plan_shards_rejects_non_positive_shard_count(bad: int):
    with pytest.raises(ValueError):
        plan_shards(["a"], {}, bad)


def test_plan_shards_rejects_duplicate_items_and_bad_durations():
    with pytest.raises(ValueError):
        plan_shards(["a", "a"], {}, 2)
    with pytest.raises(ValueError):
        plan_shards(["a"], {"a": -1.0}, 2)
    with pytest.raises(ValueError):
        plan_shards(["a"], {"a": float("nan")}, 2)
