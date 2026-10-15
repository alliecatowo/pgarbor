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

import math
from collections import Counter
from dataclasses import dataclass, field
from typing import Any, Iterable, Optional

from .dag import fanout as _dag_fanout
from .dag import speculative_ready as _dag_speculative_ready
from .records import LeaseSpec, LeaseState

#: Task-id prefixes on the RFC 001 north-star path (dispatched first).
VERTICAL_SLICE_PREFIXES: tuple[str, ...] = ("ARCH-", "STORE-", "BRANCH-", "TXN-", "TYPE-",
                                            "CAT-", "SQL-", "PLAN-", "EXEC-", "WIRE-", "CLI-")

# Implementation notes (FAC-15):
#  * ``repair_order`` is the flattened, already-ordered list of repair lease ids (ticket order,
#    critical tickets first); the first occurrence of an id fixes its rank.
#  * A lease's fanout is the maximum dag.fanout over its task_ids.
#  * ``tier_caps`` only constrain tiers that appear in the mapping; other tiers are limited by
#    max_authors alone.
#  * Suspension (RFC 003 §11, see amendments.py): a ledger entry whose most recent history note
#    starting with "suspended:" (and not superseded by a later "resumed:" note) is deferred with
#    reason "suspended". plan_wave has no repo_root, so amendments.suspended_leases is not read
#    here; the ledger history is the only signal.
#  * Shard ``items`` are listed longest first (ties by id); ``estimated_s`` is the sum of
#    durations in that shard.


@dataclass
class WavePlan:
    grants: list[str] = field(default_factory=list)
    deferred: dict[str, str] = field(default_factory=dict)


@dataclass
class Shard:
    index: int
    items: list[str]
    estimated_s: float


def _state_is(entry: Any, state: LeaseState) -> bool:
    return entry.state == state


def _is_suspended(entry: Any) -> bool:
    """Most recent suspension/resume marker in the ledger history decides."""
    for transition in reversed(list(entry.history or [])):
        note = transition.note or ""
        if note.startswith("suspended:"):
            return True
        if note.startswith("resumed:"):
            return False
    return False


def plan_wave(graph: dict[str, Any], lease_specs: list[Any], ledger: dict[str, Any],
              frozen_contracts: Iterable[str], max_authors: int,
              tier_caps: Optional[dict[str, int]] = None,
              repair_order: Optional[list[str]] = None) -> WavePlan:
    if max_authors < 0:
        raise ValueError(f"max_authors must be >= 0, got {max_authors}")
    caps: dict[str, int] = dict(tier_caps or {})
    for tier, cap in caps.items():
        if cap < 0:
            raise ValueError(f"tier cap for {tier!r} must be >= 0, got {cap}")

    specs: dict[str, LeaseSpec] = {}
    for spec in lease_specs:
        if spec.lease_id in specs:
            raise ValueError(f"duplicate lease spec {spec.lease_id}")
        specs[spec.lease_id] = spec

    ready_tasks = set(_dag_speculative_ready(graph, frozen_contracts))
    fan = _dag_fanout(graph)

    repair_rank: dict[str, int] = {}
    for index, lease_id in enumerate(repair_order or []):
        repair_rank.setdefault(lease_id, index)

    # Currently SPECULATING leases count against both caps and their files stay reserved.
    speculating = sorted(lid for lid, entry in ledger.items()
                         if _state_is(entry, LeaseState.SPECULATING))
    total = len(speculating)
    tier_used: Counter[str] = Counter()
    file_in_use: dict[str, str] = {}
    for lid in speculating:
        spec = specs.get(lid)
        if spec is None:
            continue
        tier_used[spec.model_tier] += 1
        for path in spec.files:
            file_in_use.setdefault(path, lid)

    deferred: dict[str, str] = {}
    candidates: list[LeaseSpec] = []
    for lid in sorted(ledger):
        entry = ledger[lid]
        if not _state_is(entry, LeaseState.SPEC_READY):
            continue
        if _is_suspended(entry):
            deferred[lid] = "suspended"
            continue
        spec = specs.get(lid)
        if spec is None:
            deferred[lid] = "no_catalog_spec"
            continue
        if not all(tid in ready_tasks for tid in spec.task_ids):
            deferred[lid] = "task_not_ready"
            continue
        candidates.append(spec)

    def priority(spec: LeaseSpec) -> tuple[int, int, int, int, str]:
        rank = repair_rank.get(spec.lease_id)
        repair_flag = 0 if rank is not None else 1
        slice_flag = 0 if any(tid.startswith(VERTICAL_SLICE_PREFIXES) for tid in spec.task_ids) else 1
        fanout_value = max((fan.get(tid, 0) for tid in spec.task_ids), default=0)
        return (repair_flag, rank if rank is not None else 0, slice_flag, -fanout_value,
                spec.lease_id)

    candidates.sort(key=priority)

    grants: list[str] = []
    for spec in candidates:
        lid = spec.lease_id
        if total >= max_authors:
            deferred[lid] = "max_authors"
            continue
        tier = spec.model_tier
        if tier in caps and tier_used[tier] >= caps[tier]:
            deferred[lid] = f"tier_cap:{tier}"
            continue
        clash = next((file_in_use[path] for path in spec.files if path in file_in_use), None)
        if clash is not None:
            deferred[lid] = f"file_overlap:{clash}"
            continue
        grants.append(lid)
        total += 1
        tier_used[tier] += 1
        for path in spec.files:
            file_in_use[path] = lid

    return WavePlan(grants=grants, deferred=deferred)


# ---------------------------------------------------------------------------------------------
# Verification sharding
# ---------------------------------------------------------------------------------------------

_EPS = 1e-9


def _sum_sq_delta(la: float, lb: float, d: float) -> float:
    """Change in la^2 + lb^2 when la drops by d and lb rises by d (d may be negative)."""
    return (la - d) ** 2 + (lb + d) ** 2 - la ** 2 - lb ** 2


def _find_improvement(members: list[list[str]], loads: list[float],
                      dur: dict[str, float]) -> Optional[tuple[int, int, str, Optional[str]]]:
    """First move (x: a->b) or swap (x in a, y in b) that strictly lowers the sum of squared
    shard loads. Scan order is fixed (shard index, then item order), so the result is
    deterministic. A move or swap that lowers the sum of squares never raises the larger of
    the two loads it touches, so the LPT makespan bound is preserved."""
    n = len(members)
    for a in range(n):
        for b in range(n):
            if a == b:
                continue
            la, lb = loads[a], loads[b]
            for x in members[a]:
                if _sum_sq_delta(la, lb, dur[x]) < -_EPS:
                    return (a, b, x, None)
            for x in members[a]:
                for y in members[b]:
                    if _sum_sq_delta(la, lb, dur[x] - dur[y]) < -_EPS:
                        return (a, b, x, y)
    return None


def plan_shards(items: Iterable[str], durations: dict[str, float], n_shards: int) -> list[Shard]:
    if n_shards < 1:
        raise ValueError(f"n_shards must be >= 1, got {n_shards}")
    ids = list(items)
    seen: set[str] = set()
    dur: dict[str, float] = {}
    for item in ids:
        if item in seen:
            raise ValueError(f"duplicate verification item {item!r}")
        seen.add(item)
        value = float(durations.get(item, 1.0))
        if not math.isfinite(value) or value < 0:
            raise ValueError(f"duration for {item!r} must be finite and >= 0, got {value}")
        dur[item] = value

    # LPT: longest first, each item to the currently lightest shard (ties: lowest index).
    members: list[list[str]] = [[] for _ in range(n_shards)]
    loads: list[float] = [0.0] * n_shards
    for item in sorted(ids, key=lambda i: (-dur[i], i)):
        k = min(range(n_shards), key=lambda j: (loads[j], j))
        members[k].append(item)
        loads[k] += dur[item]

    # Deterministic local refinement of the LPT result (moves and swaps), see _find_improvement.
    while True:
        step = _find_improvement(members, loads, dur)
        if step is None:
            break
        a, b, x, y = step
        members[a].remove(x)
        loads[a] -= dur[x]
        members[b].append(x)
        loads[b] += dur[x]
        if y is not None:
            members[b].remove(y)
            loads[b] -= dur[y]
            members[a].append(y)
            loads[a] += dur[y]

    shards: list[Shard] = []
    for k in range(n_shards):
        ordered = sorted(members[k], key=lambda i: (-dur[i], i))
        shards.append(Shard(index=k, items=ordered,
                            estimated_s=float(sum(dur[i] for i in ordered))))
    return shards
