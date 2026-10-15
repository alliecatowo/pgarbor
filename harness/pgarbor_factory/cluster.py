"""SWARM-11: failure clustering by root cause (not one ticket per assertion).

Lease FAC-11.

Input: failures from a campaign — model.CaseResult objects (differential), regress/isolation
results (adapted to ``FailureItem``), compile ErrorClusters (build.ErrorCluster) and storage
fault failures.

``FailureItem`` normalization:
  family, item_id, outcome, sqlstate, ref_sqlstate, diff_kind, message_template,
  first_keyword, feature_ids, files (engine files implicated: from compile spans, or none),
  lease_ids (owners of those files, or the signature's unimplemented_lease).

Cluster key (``cluster_key``), most specific evidence first:
  * CORRUPTION/CRASH/HANG: (outcome, family, message_template or "") — each distinct crash
    signature is its own critical cluster;
  * UNSUPPORTED: ("UNSUPPORTED", unimplemented_lease or message_template of the 0A000
    message). ``PgError::unimplemented(lease, what)`` (crates/pgarbor-common/src/error.rs)
    produces message ``"<what> is not supported by PGArbor yet"`` and DETAIL
    ``"pgarbor-unimplemented: <LEASE-ID>"``; compare.classify copies that lease id into
    ``CaseResult.signature["unimplemented_lease"]`` so clusters map directly to leases
    (``lease_ids = [that id]``). Lease id syntax: ``[A-Z][A-Z0-9]{1,9}-\\d{2,3}``;
  * MISMATCH/UNEXPECTED_ACCEPTANCE with an error on either side: (outcome, sqlstate,
    ref_sqlstate, message_template);
  * other MISMATCH: (outcome, diff_kind, first_keyword) — e.g. all ``column_type`` diffs on
    SELECT form one coercion cluster;
  * compile clusters pass through with key ("BUILD", code, template).

Each Cluster: key, outcome, size, families, sample item ids (first 10 sorted), feature_ids
histogram, lease_ids (sorted), representative (the item with the shortest case — fewest
steps — for minimization), ``unblock_estimate`` = number of distinct items in the cluster
plus items in OTHER clusters whose cases share the representative's first failing
first_keyword+feature (cheap proxy, documented as heuristic).

Output sorted by (critical first, size desc, key). ``write_clusters`` -> ``clusters.json``.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Iterable, Optional

from pgarbor_harness.model import CaseResult, Outcome, write_canonical_json

from .build import ErrorCluster

#: Lease id syntax (ARCHITECTURE / compare.classify): ``[A-Z][A-Z0-9]{1,9}-\d{2,3}``.
_LEASE_RE = re.compile(r"[A-Z][A-Z0-9]{1,9}-\d{2,3}")

#: Outcomes whose clusters are critical (stop the line): Outcome.is_critical.
_CRITICAL = frozenset(o.value for o in Outcome if o.is_critical)

#: Outcomes keyed by (outcome, family, message_template): each crash signature is its own cluster.
_SIGNATURE_OUTCOMES = frozenset({"CORRUPTION", "CRASH", "HANG"})


@dataclass
class FailureItem:
    family: str
    item_id: str
    outcome: str
    sqlstate: str = ""
    ref_sqlstate: str = ""
    diff_kind: str = ""
    message_template: str = ""
    first_keyword: str = ""
    feature_ids: list[str] = field(default_factory=list)
    files: list[str] = field(default_factory=list)
    lease_ids: list[str] = field(default_factory=list)
    steps: int = 0


@dataclass
class Cluster:
    key: str
    outcome: str
    size: int
    critical: bool
    families: list[str]
    sample_ids: list[str]
    feature_histogram: dict[str, int]
    lease_ids: list[str]
    representative: str
    unblock_estimate: int


def _unimplemented_lease(item: FailureItem) -> str:
    """The lease id owning an unimplemented feature, if the item carries one."""
    for lid in item.lease_ids:
        if _LEASE_RE.fullmatch(lid):
            return lid
    return ""


def from_case_result(result: CaseResult, steps: int = 0) -> FailureItem:
    """model.CaseResult -> FailureItem (non-PASS only; PASS raises ValueError).

    Signature fields come from ``CaseResult.signature`` (filled by compare.classify); the
    unimplemented lease id, when present, becomes the item's only ``lease_ids`` entry.
    """
    outcome = Outcome(result.outcome)
    if outcome is Outcome.PASS:
        raise ValueError(f"FAC-11: PASS result is not a failure: {result.case_id}")
    sig = result.signature or {}
    diff_kind = str(sig.get("diff_kind", ""))
    if not diff_kind and result.first_divergence is not None:
        diff_kind = result.first_divergence.kind
    lease = str(sig.get("unimplemented_lease", ""))
    return FailureItem(
        family=result.family,
        item_id=result.case_id,
        outcome=outcome.value,
        sqlstate=str(sig.get("sqlstate", "")),
        ref_sqlstate=str(sig.get("ref_sqlstate", "")),
        diff_kind=diff_kind,
        message_template=str(sig.get("message_template", "")),
        first_keyword=str(sig.get("first_keyword", "")),
        feature_ids=list(result.feature_ids),
        files=[],
        lease_ids=[lease] if _LEASE_RE.fullmatch(lease) else [],
        steps=steps,
    )


def from_build_cluster(cluster: ErrorCluster) -> FailureItem:
    """build.ErrorCluster -> FailureItem with outcome "BUILD" (one item per compile cluster).

    The cluster key is then ("BUILD", code, template), so compile clusters pass through
    unchanged. ``files`` and ``lease_ids`` are the cluster's owners.
    """
    return FailureItem(
        family="build",
        item_id=cluster.key,
        outcome="BUILD",
        sqlstate=cluster.code,
        message_template=cluster.template,
        files=list(cluster.files),
        lease_ids=list(cluster.leases),
    )


def cluster_key(item: FailureItem) -> str:
    """Root-cause key (see module docstring). Returned as a canonical JSON array string so
    that components containing separators cannot collide."""
    outcome = item.outcome
    if outcome in _SIGNATURE_OUTCOMES:
        parts = [outcome, item.family, item.message_template]
    elif outcome == "UNSUPPORTED":
        parts = ["UNSUPPORTED", _unimplemented_lease(item) or item.message_template]
    elif outcome == "BUILD":
        parts = ["BUILD", item.sqlstate, item.message_template]
    elif item.sqlstate or item.ref_sqlstate:
        parts = [outcome, item.sqlstate, item.ref_sqlstate, item.message_template]
    else:
        parts = [outcome, item.diff_kind, item.first_keyword]
    return json.dumps(parts, ensure_ascii=False)


def _ident(item: FailureItem) -> tuple[str, str]:
    return (item.family, item.item_id)


def cluster_failures(items: Iterable[FailureItem]) -> list[Cluster]:
    """Group failures into root-cause clusters, sorted (critical first, size desc, key).

    Items are de-duplicated by (family, item_id) for sizes and samples; the first occurrence
    wins. ``unblock_estimate`` is the heuristic described in the module docstring: the cluster's
    distinct items plus the other-cluster items sharing the representative's first_keyword and
    its first feature id (or the first_keyword alone when the representative has no features).
    """
    buckets: dict[str, dict[tuple[str, str], FailureItem]] = {}
    all_items: dict[tuple[str, str], FailureItem] = {}
    for item in items:
        key = cluster_key(item)
        ident = _ident(item)
        buckets.setdefault(key, {}).setdefault(ident, item)
        all_items.setdefault(ident, item)

    # (first_keyword, feature or None) -> identities of items matching that proxy.
    # The None feature slot indexes every item with that first_keyword.
    proxy_index: dict[tuple[str, Optional[str]], set[tuple[str, str]]] = {}
    for ident, item in all_items.items():
        proxy_index.setdefault((item.first_keyword, None), set()).add(ident)
        for feat in item.feature_ids:
            proxy_index.setdefault((item.first_keyword, feat), set()).add(ident)

    clusters: list[Cluster] = []
    for key, members in buckets.items():
        uniq = list(members.values())
        member_ids = set(members.keys())
        outcome = uniq[0].outcome
        rep = min(uniq, key=lambda it: (it.steps, it.item_id, it.family))

        feature_counts: Counter[str] = Counter()
        lease_set: set[str] = set()
        family_set: set[str] = set()
        for it in uniq:
            feature_counts.update(set(it.feature_ids))
            lease_set.update(it.lease_ids)
            family_set.add(it.family)

        feat = rep.feature_ids[0] if rep.feature_ids else None
        proxy = proxy_index.get((rep.first_keyword, feat), set())
        unblock = len(member_ids) + len(proxy - member_ids)

        clusters.append(Cluster(
            key=key,
            outcome=outcome,
            size=len(uniq),
            critical=outcome in _CRITICAL,
            families=sorted(family_set),
            sample_ids=sorted(it.item_id for it in uniq)[:10],
            feature_histogram=dict(sorted(feature_counts.items())),
            lease_ids=sorted(lease_set),
            representative=rep.item_id,
            unblock_estimate=unblock,
        ))

    clusters.sort(key=lambda c: (not c.critical, -c.size, c.key))
    return clusters


def write_clusters(clusters: list[Cluster], path: Path) -> None:
    """Atomically write the clusters as a canonical JSON list to ``path`` (clusters.json)."""
    write_canonical_json(path, [asdict(c) for c in clusters])
