"""SWARM-15: campaign completion and halt rules — stop at milestone or budget, surface
blockers, no infinite no-progress token burn.

Lease FAC-14.

``decide(history, budget)`` where history = evidence packs of this campaign line, oldest
first (records.EvidencePack), returns HaltDecision with exactly one ``action``:

  * ``STOP_THE_LINE``: latest critical.corruption > 0 or critical.crash > 0 in family
    "storage-fault" -> only critical storage repair tickets may be dispatched next epoch;
  * ``MILESTONE_REACHED``: every family named in ``budget.milestone_families`` has
    status RAN, passed == runnable, and UNRUN == 0 for that family (a family with BLOCKED
    items is NOT complete — blockers are listed in ``blockers``);
  * ``BUDGET_EXHAUSTED``: epochs_used >= budget.max_epochs, or cumulative wall time >=
    budget.max_wall_s, or cumulative cpu >= budget.max_cpu_s;
  * ``NO_PROGRESS``: the last ``budget.no_progress_window`` (default 3) campaigns did not
    increase total PASS AND did not reduce the number of distinct failure cluster keys
    (needs clusters per campaign: ``cluster_keys`` list aligned with history) -> escalate:
    the decision lists the persistent cluster keys as ``escalate`` (stronger model /
    architect, RFC 003 §11 "Stalled progress");
  * ``CONTINUE`` otherwise.
Precedence: STOP_THE_LINE > BUDGET_EXHAUSTED > MILESTONE_REACHED > NO_PROGRESS > CONTINUE.
``blockers``: family names with status BLOCKED/SKIPPED_PRECONDITION plus top BLOCKED reasons.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional, Sequence

from .records import EvidencePack, FamilyEvidence


@dataclass
class Budget:
    max_epochs: int = 20
    max_wall_s: Optional[float] = None
    max_cpu_s: Optional[float] = None
    no_progress_window: int = 3
    milestone_families: tuple[str, ...] = ("storage-fault", "differential")


@dataclass
class HaltDecision:
    action: str
    reasons: list[str] = field(default_factory=list)
    blockers: list[str] = field(default_factory=list)
    escalate: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {"action": self.action, "reasons": self.reasons, "blockers": self.blockers,
                "escalate": self.escalate}


ACTIONS: tuple[str, ...] = ("STOP_THE_LINE", "BUDGET_EXHAUSTED", "MILESTONE_REACHED",
                            "NO_PROGRESS", "CONTINUE")

#: Family whose critical storage results trigger STOP_THE_LINE.
_STORAGE_FAMILY = "storage-fault"
#: Family statuses that count as blocked for the ``blockers`` list.
_BLOCKED_STATUSES = ("BLOCKED", "SKIPPED_PRECONDITION")
#: How many blocked families contribute a reason line to ``blockers``.
_TOP_BLOCKED_REASONS = 5


# ---------------------------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------------------------


def _as_pack(item: Any) -> EvidencePack:
    """Accept an EvidencePack, or its JSON dict form (records.EvidencePack.from_json)."""
    if isinstance(item, EvidencePack):
        return item
    if isinstance(item, dict):
        return EvidencePack.from_json(item)
    raise TypeError(f"halt history entries must be EvidencePack or dict, got {type(item).__name__}")


def _num(value: Any) -> float:
    """Lenient numeric read for resource fields; missing or non-finite values count as 0."""
    try:
        v = float(value)
    except (TypeError, ValueError):
        return 0.0
    if v != v or v in (float("inf"), float("-inf")):
        return 0.0
    return v


def _count(counts: Optional[dict[str, Any]], key: str) -> int:
    if not counts:
        return 0
    try:
        return int(counts.get(key, 0) or 0)
    except (TypeError, ValueError):
        return 0


def _family(pack: EvidencePack, name: str) -> Optional[FamilyEvidence]:
    for fam in pack.families:
        if fam.name == name:
            return fam
    return None


def _total_pass(pack: EvidencePack) -> int:
    return _count(pack.totals, "PASS")


# ---------------------------------------------------------------------------------------------
# Rules
# ---------------------------------------------------------------------------------------------


def _stop_the_line_reasons(latest: EvidencePack) -> list[str]:
    """Critical corruption or crash in the latest campaign, from the top-level critical
    counts or the storage-fault family's own counts."""
    reasons: list[str] = []
    corruption = _count(latest.critical, "corruption")
    crash = _count(latest.critical, "crash")
    if corruption > 0:
        reasons.append(f"critical.corruption={corruption} in campaign {latest.campaign_id}")
    if crash > 0:
        reasons.append(f"critical.crash={crash} in campaign {latest.campaign_id}")
    fam = _family(latest, _STORAGE_FAMILY)
    if fam is not None:
        f_corr = _count(fam.counts, "CORRUPTION")
        f_crash = _count(fam.counts, "CRASH")
        if f_corr > 0 or f_crash > 0:
            reasons.append(
                f"{_STORAGE_FAMILY} family CORRUPTION={f_corr} CRASH={f_crash} "
                f"in campaign {latest.campaign_id}")
    return reasons


def _budget_reasons(packs: Sequence[EvidencePack], budget: Budget) -> list[str]:
    reasons: list[str] = []
    epochs_used = len({p.epoch for p in packs})
    if epochs_used >= budget.max_epochs:
        reasons.append(f"epochs_used={epochs_used} >= max_epochs={budget.max_epochs}")
    if budget.max_wall_s is not None:
        wall = sum(_num((p.resource or {}).get("wall_s", 0)) for p in packs)
        if wall >= budget.max_wall_s:
            reasons.append(f"cumulative wall_s={wall:.1f} >= max_wall_s={budget.max_wall_s}")
    if budget.max_cpu_s is not None:
        cpu = sum(_num((p.resource or {}).get("cpu_s", 0)) for p in packs)
        if cpu >= budget.max_cpu_s:
            reasons.append(f"cumulative cpu_s={cpu:.1f} >= max_cpu_s={budget.max_cpu_s}")
    return reasons


def _milestone_reasons(latest: EvidencePack, budget: Budget) -> Optional[list[str]]:
    """Return the per-family reasons when every milestone family is complete, else None.

    A family is complete only when it RAN, has runnable items, passed == runnable, and has
    no UNRUN and no BLOCKED items. An empty milestone list never completes.
    """
    families = tuple(budget.milestone_families)
    if not families:
        return None
    reasons: list[str] = []
    for name in families:
        fam = _family(latest, name)
        if fam is None or fam.status != "RAN" or fam.runnable <= 0:
            return None
        if fam.passed != fam.runnable:
            return None
        if _count(fam.counts, "UNRUN") != 0 or _count(fam.counts, "BLOCKED") != 0:
            return None
        reasons.append(f"{name}: {fam.passed}/{fam.runnable} passed, UNRUN=0, BLOCKED=0")
    return reasons


def _blockers(latest: EvidencePack) -> list[str]:
    """Family names that are blocked (by status or by BLOCKED items), then the reasons of
    the most-blocked families."""
    names: list[str] = []
    for fam in latest.families:
        if fam.status in _BLOCKED_STATUSES or _count(fam.counts, "BLOCKED") > 0:
            if fam.name not in names:
                names.append(fam.name)
    blocked_fams = [f for f in latest.families
                    if _count(f.counts, "BLOCKED") > 0 or f.status == "BLOCKED"]
    blocked_fams.sort(key=lambda f: -_count(f.counts, "BLOCKED"))
    reasons: list[str] = []
    for fam in blocked_fams[:_TOP_BLOCKED_REASONS]:
        blocked = _count(fam.counts, "BLOCKED")
        if fam.notes:
            for note in fam.notes:
                reasons.append(f"{fam.name}: {note}")
        else:
            reasons.append(f"{fam.name}: {blocked} blocked item(s), status {fam.status}")
    return names + reasons


def _no_progress(packs: Sequence[EvidencePack], cluster_keys: Optional[Sequence[Sequence[str]]],
                 window: int) -> Optional[tuple[list[str], list[str]]]:
    """Return (reasons, persistent_cluster_keys) when the last ``window`` campaigns made no
    progress, else None.

    The window is the last ``window`` campaigns. Progress means the latest campaign's PASS
    total exceeds the window's first campaign, or its distinct cluster-key count is lower.
    A window below 2 cannot show progress or its absence, so the rule is off.
    """
    if cluster_keys is None or window < 2 or len(packs) < window:
        return None
    start = len(packs) - window
    start_pass = _total_pass(packs[start])
    latest_pass = _total_pass(packs[-1])
    start_keys = {str(k) for k in cluster_keys[start]}
    latest_keys = {str(k) for k in cluster_keys[-1]}
    if latest_pass > start_pass or len(latest_keys) < len(start_keys):
        return None
    persistent = set.intersection(*(
        {str(k) for k in cluster_keys[i]} for i in range(start, len(packs))))
    reasons = [
        f"last {window} campaigns: PASS {start_pass} -> {latest_pass}",
        f"distinct cluster keys {len(start_keys)} -> {len(latest_keys)}",
    ]
    return reasons, sorted(persistent)


# ---------------------------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------------------------


def decide(history: Sequence[Any], budget: Budget,
           cluster_keys: Optional[Sequence[Sequence[str]]] = None) -> HaltDecision:
    packs = [_as_pack(item) for item in history]
    if cluster_keys is not None and len(cluster_keys) != len(packs):
        raise ValueError(
            f"cluster_keys must align with history: {len(cluster_keys)} != {len(packs)}")
    if not packs:
        return HaltDecision(action="CONTINUE", reasons=["no evidence packs yet"])

    latest = packs[-1]
    blockers = _blockers(latest)

    stl = _stop_the_line_reasons(latest)
    if stl:
        return HaltDecision(action="STOP_THE_LINE", reasons=stl, blockers=blockers)

    budget_reasons = _budget_reasons(packs, budget)
    if budget_reasons:
        return HaltDecision(action="BUDGET_EXHAUSTED", reasons=budget_reasons,
                            blockers=blockers)

    milestone = _milestone_reasons(latest, budget)
    if milestone is not None:
        return HaltDecision(action="MILESTONE_REACHED", reasons=milestone, blockers=blockers)

    stalled = _no_progress(packs, cluster_keys, budget.no_progress_window)
    if stalled is not None:
        reasons, persistent = stalled
        return HaltDecision(action="NO_PROGRESS", reasons=reasons, blockers=blockers,
                            escalate=persistent)

    return HaltDecision(action="CONTINUE", reasons=["no halt condition met"], blockers=blockers)
