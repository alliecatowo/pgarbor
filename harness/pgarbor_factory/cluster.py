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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable


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


def from_case_result(result: Any, steps: int = 0) -> FailureItem:
    """model.CaseResult -> FailureItem (non-PASS only; PASS raises ValueError)."""
    raise NotImplementedError("FAC-11: case result adapter")


def cluster_key(item: FailureItem) -> str:
    raise NotImplementedError("FAC-11: cluster key")


def cluster_failures(items: Iterable[FailureItem]) -> list[Cluster]:
    raise NotImplementedError("FAC-11: cluster failures")


def write_clusters(clusters: list[Cluster], path: Path) -> None:
    raise NotImplementedError("FAC-11: write clusters")
