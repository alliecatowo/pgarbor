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

from pathlib import Path
from typing import Any

from .records import RepairTicket


def score(severity: str, unblocked: int, fanout: int, confidence: float, cost: float) -> float:
    """records.SEVERITY_WEIGHT[severity] * unblocked * fanout * confidence / max(cost, 1e-9)
    (implemented; unknown severity weight = 1.0)."""
    from .records import SEVERITY_WEIGHT

    return SEVERITY_WEIGHT.get(severity, 1.0) * unblocked * fanout * confidence / max(cost, 1e-9)


def generate_tickets(clusters: list[Any], graph: dict[str, Any], lease_specs: list[Any],
                     epoch: int) -> list[RepairTicket]:
    raise NotImplementedError("FAC-12: generate repair tickets")


def write_repair_dag(tickets: list[RepairTicket], path: Path, campaign_id: str,
                     epoch: int) -> None:
    raise NotImplementedError("FAC-12: write repair DAG")
