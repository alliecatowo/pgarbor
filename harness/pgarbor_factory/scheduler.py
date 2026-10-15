"""SWARM-02 / SWARM-10: epoch & wave scheduler with budget-aware dispatch and verification
sharding.

Lease FAC-15.

Dispatch (``plan_wave``): choose the leases to grant in the next wave.
  * candidates = ledger entries in SPEC_READY whose lease's task_ids are all in
    dag.speculative_ready(graph, frozen_contracts) — contract deps frozen; integration deps
    ignored (RFC 003 §2);
  * never two leases with overlapping files (catalog already disjoint; defensive);
  * caps: ``max_authors`` total concurrent SPECULATING leases (including already granted),
    and per-tier caps ``tier_caps`` (e.g. {"haiku": 100, "sonnet": 12, "opus": 2});
  * priority order: (1) repair tickets' leases by ticket order (critical first), (2) leases
    on the north-star vertical slice — task ids starting with a ``VERTICAL_SLICE_PREFIXES`` prefix —
    (3) by dag.fanout of their tasks desc, (4) lease_id. Deterministic.
  * output WavePlan{grants: [lease_id], deferred: {lease_id: reason}}.

Verification sharding (``plan_shards``, SWARM-10): split a corpus of runnable items into
``n_shards`` with estimated durations (previous evidence per item, default 1 s) balanced by
LPT (longest processing time first, ties by id) — maximizes new diagnosis per CPU minute;
items whose expected file and engine-relevant inputs are unchanged since a PASS at the same
engine SHA are skipped as ``cached_pass`` only when ``allow_cache`` (never in a trusted
campaign: the campaign passes allow_cache=False).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Iterable, Optional

#: Task-id prefixes on the RFC 001 north-star path (dispatched first).
VERTICAL_SLICE_PREFIXES: tuple[str, ...] = ("ARCH-", "STORE-", "BRANCH-", "TXN-", "TYPE-",
                                            "CAT-", "SQL-", "PLAN-", "EXEC-", "WIRE-", "CLI-")


@dataclass
class WavePlan:
    grants: list[str] = field(default_factory=list)
    deferred: dict[str, str] = field(default_factory=dict)


@dataclass
class Shard:
    index: int
    items: list[str]
    estimated_s: float


def plan_wave(graph: dict[str, Any], lease_specs: list[Any], ledger: dict[str, Any],
              frozen_contracts: Iterable[str], max_authors: int,
              tier_caps: Optional[dict[str, int]] = None,
              repair_order: Optional[list[str]] = None) -> WavePlan:
    raise NotImplementedError("FAC-15: plan dispatch wave")


def plan_shards(items: Iterable[str], durations: dict[str, float], n_shards: int) -> list[Shard]:
    raise NotImplementedError("FAC-15: LPT verification sharding")
