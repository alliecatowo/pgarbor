"""Tests for FAC-14 (SWARM-15): campaign halt rules in pgarbor_factory.halt."""

from __future__ import annotations

import pytest

from pgarbor_factory.halt import ACTIONS, Budget, HaltDecision, decide
from pgarbor_factory.records import COUNT_KEYS, EvidencePack, FamilyEvidence, empty_counts


def _fam(name, *, status="RAN", passed=0, runnable=0, total=None, counts=None, notes=None):
    c = empty_counts()
    if counts:
        c.update(counts)
    return FamilyEvidence(
        name=name, status=status, counts=c,
        total_cataloged=runnable if total is None else total,
        runnable=runnable, passed=passed, notes=list(notes or []))


def _pack(cid, families=(), *, critical=None, epoch=1, wall=0.0, cpu=0.0):
    totals = {k: 0 for k in COUNT_KEYS}
    for fam in families:
        totals["PASS"] += fam.passed
    return EvidencePack(
        campaign_id=cid, epoch=epoch, mode="cathedral",
        git={"sha": "0" * 40, "branch": "main", "dirty": False},
        toolchain={}, platform={}, reference={}, corpus={}, seeds=[1],
        started_unix=0, finished_unix=0, build={"ok": True},
        families=list(families), totals=totals,
        critical=dict({"corruption": 0, "crash": 0}, **(critical or {})),
        resource={"wall_s": wall, "cpu_s": cpu})


def _complete_families(passed_storage=10, passed_diff=20):
    return [
        _fam("storage-fault", passed=passed_storage, runnable=passed_storage,
             counts={"PASS": passed_storage}),
        _fam("differential", passed=passed_diff, runnable=passed_diff,
             counts={"PASS": passed_diff}),
    ]


def _open_families(pass_count=5):
    """Families that are not complete: differential has UNRUN items."""
    return [
        _fam("storage-fault", passed=pass_count, runnable=pass_count,
             counts={"PASS": pass_count}),
        _fam("differential", passed=pass_count, runnable=pass_count,
             counts={"PASS": pass_count, "UNRUN": 7}, total=pass_count + 7),
    ]


# ---------------------------------------------------------------------------------------------
# One test per action
# ---------------------------------------------------------------------------------------------


def test_actions_tuple_is_complete():
    assert ACTIONS == ("STOP_THE_LINE", "BUDGET_EXHAUSTED", "MILESTONE_REACHED",
                       "NO_PROGRESS", "CONTINUE")


def test_stop_the_line_on_critical_corruption():
    hist = [_pack("c1", _open_families(), critical={"corruption": 1})]
    d = decide(hist, Budget())
    assert d.action == "STOP_THE_LINE"
    assert any("corruption=1" in r for r in d.reasons)


def test_stop_the_line_on_storage_fault_crash_family_counts():
    fams = [_fam("storage-fault", passed=3, runnable=4, counts={"PASS": 3, "CRASH": 1})]
    hist = [_pack("c1", fams)]
    d = decide(hist, Budget())
    assert d.action == "STOP_THE_LINE"
    assert any("storage-fault family" in r for r in d.reasons)


def test_stop_the_line_only_looks_at_latest_campaign():
    old = _pack("c1", _open_families(), critical={"corruption": 2})
    latest = _pack("c2", _open_families(), critical={"corruption": 0})
    d = decide([old, latest], Budget(max_epochs=99))
    assert d.action != "STOP_THE_LINE"


def test_budget_exhausted_by_epochs():
    hist = [_pack(f"c{i}", _open_families(i + 1), epoch=i + 1) for i in range(3)]
    d = decide(hist, Budget(max_epochs=3))
    assert d.action == "BUDGET_EXHAUSTED"
    assert any("epochs_used=3" in r for r in d.reasons)


def test_budget_exhausted_by_cumulative_wall_time():
    hist = [_pack("c1", _open_families(), wall=6.0), _pack("c2", _open_families(), wall=5.0, epoch=2)]
    d = decide(hist, Budget(max_wall_s=10.0))
    assert d.action == "BUDGET_EXHAUSTED"
    assert any("wall_s" in r for r in d.reasons)


def test_budget_exhausted_by_cumulative_cpu_time():
    hist = [_pack("c1", _open_families(), cpu=3.0), _pack("c2", _open_families(), cpu=4.0, epoch=2)]
    d = decide(hist, Budget(max_cpu_s=7.0))
    assert d.action == "BUDGET_EXHAUSTED"
    assert any("cpu_s" in r for r in d.reasons)


def test_budget_not_exhausted_below_limits():
    hist = [_pack("c1", _open_families(), wall=1.0, cpu=1.0)]
    d = decide(hist, Budget(max_epochs=5, max_wall_s=10.0, max_cpu_s=10.0))
    assert d.action == "CONTINUE"


def test_milestone_reached_when_all_milestone_families_complete():
    hist = [_pack("c1", _complete_families())]
    d = decide(hist, Budget())
    assert d.action == "MILESTONE_REACHED"
    assert d.blockers == []
    assert d.escalate == []


def test_blocked_family_is_never_milestone_reached():
    fams = [
        _fam("storage-fault", passed=10, runnable=10, counts={"PASS": 10}),
        _fam("differential", passed=10, runnable=10, counts={"PASS": 10, "BLOCKED": 2},
             total=12, notes=["reference container unreachable for 2 cases"]),
    ]
    d = decide([_pack("c1", fams)], Budget())
    assert d.action != "MILESTONE_REACHED"
    assert "differential" in d.blockers
    assert any("reference container unreachable" in b for b in d.blockers)


def test_skipped_precondition_family_blocks_milestone_and_is_listed():
    fams = _complete_families()
    fams[1] = _fam("differential", status="SKIPPED_PRECONDITION", total=30)
    d = decide([_pack("c1", fams)], Budget())
    assert d.action != "MILESTONE_REACHED"
    assert "differential" in d.blockers


def test_unrun_items_block_milestone():
    d = decide([_pack("c1", _open_families())], Budget())
    assert d.action == "CONTINUE"


def test_missing_milestone_family_is_not_reached():
    d = decide([_pack("c1", [_fam("storage-fault", passed=4, runnable=4, counts={"PASS": 4})])],
               Budget())
    assert d.action != "MILESTONE_REACHED"


def test_empty_milestone_list_is_never_reached():
    d = decide([_pack("c1", _complete_families())], Budget(milestone_families=()))
    assert d.action == "CONTINUE"


def test_no_progress_three_equal_campaigns_escalates_persistent_keys():
    keys = [["k-a", "k-b"], ["k-a", "k-b"], ["k-a", "k-b"]]
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(3)]
    d = decide(hist, Budget(), cluster_keys=keys)
    assert d.action == "NO_PROGRESS"
    assert d.escalate == ["k-a", "k-b"]


def test_no_progress_escalates_only_keys_persistent_across_window():
    keys = [["k-old", "k-a"], ["k-a", "k-b"], ["k-a", "k-c"]]
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(3)]
    d = decide(hist, Budget(), cluster_keys=keys)
    assert d.action == "NO_PROGRESS"
    assert d.escalate == ["k-a"]


def test_no_progress_not_triggered_when_pass_increases():
    keys = [["k-a"], ["k-a"], ["k-a"]]
    hist = [_pack("c0", _open_families(5), epoch=1), _pack("c1", _open_families(5), epoch=1),
            _pack("c2", _open_families(6), epoch=1)]
    d = decide(hist, Budget(), cluster_keys=keys)
    assert d.action == "CONTINUE"


def test_no_progress_not_triggered_when_cluster_count_drops():
    keys = [["k-a", "k-b"], ["k-a", "k-b"], ["k-a"]]
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(3)]
    d = decide(hist, Budget(), cluster_keys=keys)
    assert d.action == "CONTINUE"


def test_no_progress_needs_cluster_keys():
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(3)]
    d = decide(hist, Budget())
    assert d.action == "CONTINUE"


def test_no_progress_needs_full_window():
    keys = [["k-a"], ["k-a"]]
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(2)]
    d = decide(hist, Budget(no_progress_window=3), cluster_keys=keys)
    assert d.action == "CONTINUE"


def test_continue_on_empty_history():
    d = decide([], Budget())
    assert d.action == "CONTINUE"
    assert d.blockers == [] and d.escalate == []


def test_continue_default_when_nothing_triggers():
    d = decide([_pack("c1", _open_families())], Budget())
    assert d.action == "CONTINUE"


def test_misaligned_cluster_keys_raise():
    hist = [_pack("c1", _open_families()), _pack("c2", _open_families())]
    with pytest.raises(ValueError):
        decide(hist, Budget(), cluster_keys=[["k-a"]])


def test_bad_history_entry_type_raises():
    with pytest.raises(TypeError):
        decide([object()], Budget())


def test_dict_history_entries_are_accepted():
    hist = [_pack("c1", _complete_families()).to_json()]
    d = decide(hist, Budget())
    assert d.action == "MILESTONE_REACHED"


def test_blockers_lists_skipped_family_without_halting():
    fams = [
        _fam("storage-fault", status="SKIPPED_PRECONDITION", total=5),
        _fam("differential", passed=3, runnable=3, counts={"PASS": 3}),
    ]
    d = decide([_pack("c1", fams)], Budget(milestone_families=("differential",)))
    assert d.action == "MILESTONE_REACHED"
    assert "storage-fault" in d.blockers


# ---------------------------------------------------------------------------------------------
# Precedence
# ---------------------------------------------------------------------------------------------


def test_precedence_corruption_with_budget_exhausted_is_stop_the_line():
    hist = [_pack("c1", _open_families(), critical={"corruption": 1}, epoch=1),
            _pack("c2", _open_families(), critical={"corruption": 1}, epoch=2)]
    d = decide(hist, Budget(max_epochs=1))
    assert d.action == "STOP_THE_LINE"


def test_precedence_stop_the_line_over_milestone():
    fams = _complete_families()
    d = decide([_pack("c1", fams, critical={"crash": 1})], Budget())
    assert d.action == "STOP_THE_LINE"


def test_precedence_budget_over_milestone():
    d = decide([_pack("c1", _complete_families(), epoch=1)], Budget(max_epochs=1))
    assert d.action == "BUDGET_EXHAUSTED"


def test_precedence_budget_over_no_progress():
    keys = [["k-a"], ["k-a"], ["k-a"]]
    hist = [_pack(f"c{i}", _open_families(5), epoch=1) for i in range(3)]
    d = decide(hist, Budget(max_epochs=1), cluster_keys=keys)
    assert d.action == "BUDGET_EXHAUSTED"


def test_precedence_milestone_over_no_progress():
    keys = [["k-a"], ["k-a"], ["k-a"]]
    hist = [_pack(f"c{i}", _complete_families(), epoch=1) for i in range(3)]
    d = decide(hist, Budget(), cluster_keys=keys)
    assert d.action == "MILESTONE_REACHED"


def test_decision_to_json_shape():
    d = HaltDecision(action="CONTINUE", reasons=["r"], blockers=["b"], escalate=["e"])
    assert d.to_json() == {"action": "CONTINUE", "reasons": ["r"], "blockers": ["b"],
                           "escalate": ["e"]}
