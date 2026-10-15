"""SWARM-12: next-epoch repair ticket generation from frozen campaign clusters.

Lease FAC-12.

One ticket per cluster (never per failing item), records.RepairTicket fields:
  * id ``REPAIR-<epoch+1 two digits>-<NNN>`` numbered in final priority order;
  * severity = cluster outcome ("BUILD" for compile clusters);
  * expected_unblocked = cluster.unblock_estimate; dependency_fanout = 1 + max over the
    cluster's leases' task_ids of dag.fanout(graph)[task] (1 when unknown);
  * confidence: 0.9 when the cluster has a lease_id extracted from an unimplemented marker or
    compile span; 0.6 when lease inferred via feature_ids -> task epic mapping; 0.3 otherwise;
  * cost: 1.0 per distinct owned file (min 1), x2 when any owning lease is sonnet/opus tier;
  * score = SEVERITY_WEIGHT[severity] * expected_unblocked * dependency_fanout * confidence
    / cost (records.SEVERITY_WEIGHT);
  * critical = severity in (CORRUPTION, CRASH) and any owned file under
    ``crates/pgarbor-storage/``, ``crates/pgarbor-fault/``, ``crates/pgarbor-branches/`` or
    ``crates/pgarbor-core/`` (critical storage first: critical tickets sort before all others,
    then by score desc, then id);
  * repro = cluster sample ids; expected/actual behavior strings from the representative's
    first divergence (reference vs target rendered with compare.typed.render_diff);
  * owned_files / leases / ownership_domain = crate of the first owned file; owned_path_glob
    = common directory glob of owned files; deliverable = "Fix root cause: <key>";
    deferred_verification_family = family of the representative;
  * integration_dependencies: [] (file-level dependencies between new tickets are not
    statically known); contract_dependencies: [] unless the cluster's root_hint is
    "interface_drift", then ["AMEND-<epoch+1>-<NNN>"] (the amendment ticket that
    amendments.open_amendment creates; those AMEND nodes are included in the same repair
    DAG so validate_graph passes);
  * acceptance: ["representative case <id> classifies PASS at the next campaign", "no
    regression in previously passing items of families <families>"].

``generate_tickets`` returns tickets sorted; ``write_repair_dag`` writes
``{"tasks": [...], "source_campaign": id, "epoch": n+1}`` and validates it with
dag.validate_graph (errors -> raise).
"""

from __future__ import annotations

import json
import posixpath
from collections import defaultdict
from pathlib import Path
from typing import Any

from pgarbor_harness.model import case_family, write_canonical_json

from .dag import DagError, fanout, validate_graph
from .records import RepairTicket

_CRITICAL_OUTCOMES = frozenset({"CORRUPTION", "CRASH"})
_CRITICAL_PREFIXES = ("crates/pgarbor-storage/", "crates/pgarbor-fault/",
                      "crates/pgarbor-branches/", "crates/pgarbor-core/")
_STRONG_TIERS = ("sonnet", "opus")


def score(severity: str, unblocked: int, fanout: int, confidence: float, cost: float) -> float:
    """records.SEVERITY_WEIGHT[severity] * unblocked * fanout * confidence / max(cost, 1e-9)
    (implemented; unknown severity weight = 1.0)."""
    from .records import SEVERITY_WEIGHT

    return SEVERITY_WEIGHT.get(severity, 1.0) * unblocked * fanout * confidence / max(cost, 1e-9)


def _get(obj: Any, name: str, default: Any = None) -> Any:
    """Attribute or key lookup, so clusters and lease specs may be dataclasses or JSON dicts.
    A None value counts as missing."""
    value = obj.get(name) if isinstance(obj, dict) else getattr(obj, name, None)
    return default if value is None else value


def _crate_of(path: str) -> str:
    parts = path.split("/")
    if len(parts) >= 2 and parts[0] == "crates":
        return parts[1]
    return parts[0]


def _common_glob(files: list[str]) -> str:
    """Common directory of the owned files as a glob (``dir/**``); "" when no files."""
    if not files:
        return ""
    common = posixpath.commonpath([posixpath.dirname(f) or "." for f in files])
    return "**" if common == "." else f"{common}/**"


def _infer_leases(feature_ids: list[str], graph: dict[str, Any],
                  index: dict[str, Any]) -> list[str]:
    """Leases owning a task whose epic matches a feature id (heuristic, confidence 0.6).

    A feature id maps to the epic named by the whole id, by its part before the first '.',
    or by its part before the first '-' (``sql.select`` and ``sql-select`` both map to ``sql``).
    """
    tasks_by_epic: dict[str, set[str]] = defaultdict(set)
    for task in graph.get("tasks", []):
        tasks_by_epic[task.get("epic")].add(task.get("id"))
    matched: set[str] = set()
    for fid in feature_ids:
        for epic in {fid, fid.split(".", 1)[0], fid.split("-", 1)[0]}:
            matched |= tasks_by_epic.get(epic, set())
    return sorted(lid for lid, spec in index.items()
                  if matched & set(_get(spec, "task_ids", [])))


def _divergence(cluster: Any, key: str, outcome: str, rep: str) -> tuple[str, str]:
    """Expected/actual strings. The first divergence is rendered only when the cluster carries
    the representative's diffs (``diffs``); the frozen Cluster shape does not, so otherwise the
    strings name the representative and the cluster."""
    from pgarbor_harness.compare.typed import render_diff

    diffs = list(_get(cluster, "diffs", []))
    if not diffs:
        return (f"PostgreSQL 18.6 reference behavior for {rep} (cluster {key})",
                f"PGArbor outcome {outcome} for {rep} (cluster {key})")
    first = diffs[0]
    expected = f"{first.path} {first.kind}: reference={first.reference!r}"
    return expected, render_diff(diffs, limit=10)


def generate_tickets(clusters: list[Any], graph: dict[str, Any], lease_specs: list[Any],
                     epoch: int) -> list[RepairTicket]:
    """One RepairTicket per cluster, sorted (critical first, score desc, cluster key) and
    numbered REPAIR-<epoch+1>-<NNN> in that order (module docstring has the scoring rules)."""
    fan = fanout(graph)
    index: dict[str, Any] = {_get(spec, "lease_id"): spec for spec in lease_specs}
    drafts: list[dict[str, Any]] = []
    for cluster in clusters:
        key = str(_get(cluster, "key", ""))
        outcome = str(_get(cluster, "outcome", ""))
        rep = str(_get(cluster, "representative", ""))
        lease_ids = sorted({str(x) for x in _get(cluster, "lease_ids", [])})
        if lease_ids:
            confidence = 0.9
        else:
            lease_ids = _infer_leases(sorted(_get(cluster, "feature_histogram", {})), graph, index)
            confidence = 0.6 if lease_ids else 0.3
        specs = [index[lid] for lid in lease_ids if lid in index]
        owned = sorted({f for spec in specs for f in _get(spec, "files", [])})
        strong = any(any(t in str(_get(spec, "model_tier", "")).lower() for t in _STRONG_TIERS)
                     for spec in specs)
        cost = float(max(len(owned), 1)) * (2.0 if strong else 1.0)
        task_ids = [t for spec in specs for t in _get(spec, "task_ids", [])]
        dep_fanout = 1 + max((fan.get(t, 0) for t in task_ids), default=0)
        unblocked = int(_get(cluster, "unblock_estimate", 0))
        critical = outcome in _CRITICAL_OUTCOMES and any(
            f.startswith(p) for f in owned for p in _CRITICAL_PREFIXES)
        if owned:
            ownership = _crate_of(owned[0])
        elif specs:
            ownership = str(_get(specs[0], "crate", "unassigned"))
        else:
            ownership = "unassigned"
        families = sorted({str(f) for f in _get(cluster, "families", [])})
        expected, actual = _divergence(cluster, key, outcome, rep)
        drafts.append({
            "key": key, "outcome": outcome, "rep": rep, "critical": critical,
            "score": score(outcome, unblocked, dep_fanout, confidence, cost),
            "unblocked": unblocked, "fanout": dep_fanout, "confidence": confidence,
            "cost": cost, "owned": owned, "leases": lease_ids, "ownership": ownership,
            "families": families, "expected": expected, "actual": actual,
            "repro": [str(x) for x in _get(cluster, "sample_ids", [])] or [rep],
            "root_hint": str(_get(cluster, "root_hint", "")),
        })
    drafts.sort(key=lambda d: (not d["critical"], -d["score"], d["key"]))
    tickets: list[RepairTicket] = []
    for n, d in enumerate(drafts, start=1):
        number = f"{epoch + 1:02d}-{n:03d}"
        contract_deps = [f"AMEND-{number}"] if d["root_hint"] == "interface_drift" else []
        tickets.append(RepairTicket(
            id=f"REPAIR-{number}",
            title=f"[{d['outcome']}] {d['key']}",
            root_cause=d["key"],
            severity=d["outcome"],
            expected_unblocked=d["unblocked"],
            dependency_fanout=d["fanout"],
            confidence=d["confidence"],
            cost=d["cost"],
            score=d["score"],
            critical=d["critical"],
            repro=d["repro"],
            expected_behavior=d["expected"],
            actual_behavior=d["actual"],
            owned_files=d["owned"],
            leases=d["leases"],
            integration_dependencies=[],
            contract_dependencies=contract_deps,
            ownership_domain=d["ownership"],
            owned_path_glob=_common_glob(d["owned"]),
            deliverable=f"Fix root cause: {d['key']}",
            deferred_verification_family=case_family(d["rep"]) if d["rep"] else "",
            acceptance=[
                f"representative case {d['rep']} classifies PASS at the next campaign",
                "no regression in previously passing items of families "
                f"{', '.join(d['families']) or 'none'}",
            ],
        ))
    return tickets


def _amendment_node(amend_id: str) -> dict[str, Any]:
    """Placeholder DAG node for an AMEND id referenced by a ticket's contract_dependencies.
    The amendment file itself is written by amendments.open_amendment."""
    return {
        "id": amend_id,
        "title": f"Ratify or reject interface-drift amendment {amend_id}",
        "epic": "repair",
        "integration_dependencies": [],
        "contract_dependencies": [],
        "ownership_domain": "contracts",
        "owned_path_glob": "contracts/amendments/**",
        "deliverable": f"Ratify or reject amendment {amend_id}",
        "deferred_verification_family": "harness_self_checks",
        "state": "PLANNED",
        "contract_version": "v1",
        "acceptance": [],
    }


def write_repair_dag(tickets: list[RepairTicket], path: Path, campaign_id: str,
                     epoch: int) -> None:
    """Write ``{"tasks", "source_campaign", "epoch": epoch+1}`` (canonical JSON). The AMEND
    nodes that tickets reference through contract_dependencies are appended so the graph
    validates. Validation errors raise DagError and nothing is written."""
    ticket_ids = {t.id for t in tickets}
    amend_ids: list[str] = []
    for t in tickets:
        for dep in t.contract_dependencies:
            if dep not in ticket_ids and dep not in amend_ids:
                amend_ids.append(dep)
    doc: dict[str, Any] = {
        "tasks": [t.to_json() for t in tickets] + [_amendment_node(a) for a in amend_ids],
        "source_campaign": campaign_id,
        "epoch": epoch + 1,
    }
    errors, _ = validate_graph(doc)
    if errors:
        raise DagError(f"{len(errors)} errors: {json.dumps(errors[:5])}")
    write_canonical_json(path, doc)
