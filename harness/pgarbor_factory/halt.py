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


def decide(history: Sequence[Any], budget: Budget,
           cluster_keys: Optional[Sequence[Sequence[str]]] = None) -> HaltDecision:
    raise NotImplementedError("FAC-14: halt decision")
