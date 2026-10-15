"""SWARM-01/02: task DAG schema validation, dependency analysis and wave computation.

Lease FAC-01 (extensions only). The functions ``find_cycle``, ``validate_graph`` and
``validate_leases`` are the original bootstrap implementation (moved here from
harness/factory/dag.py, which is now a compatibility shim) and are FROZEN behavior: other
architects run ``python3 harness/factory/dag.py validate ... --leases ...``. Do not change their
outputs; add new checks in new functions.

New (leased) functions:
  * ``load_graph(path)``: json load + validate_graph; raise DagError listing errors.
  * ``integration_waves(graph)``: Kahn layering over integration_dependencies -> list of
    sorted id lists (wave 0 = no deps). Deterministic (ids sorted within a wave).
  * ``speculative_ready(graph, frozen_contracts)``: ids whose contract_dependencies are all in
    ``frozen_contracts`` (set of task ids whose contract artifacts are FROZEN@SHA) and whose
    state is PLANNED/SPEC_READY — RFC 003 §2: integration deps do NOT block speculation.
  * ``integration_ready(graph, verified)``: ids whose integration_dependencies are all in
    ``verified``.
  * ``fanout(graph)``: id -> number of tasks transitively depending on it (integration edges);
    used by repair scoring and the scheduler.
  * ``merge_repair_nodes(graph, tickets)``: append repair tickets (records.RepairTicket JSON)
    into a NEW graph dict (never mutate the frozen contracts/dag file); ids must be new;
    re-run validate_graph and return (graph, errors).

CLI (kept): ``python3 -m pgarbor_factory.dag validate <graph> [--leases files...]``.
"""
import fnmatch
import json
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


class DagError(ValueError):
    """Invalid task graph."""


REQUIRED = ["id", "title", "epic", "integration_dependencies", "contract_dependencies",
            "ownership_domain", "owned_path_glob", "deliverable", "deferred_verification_family", "state"]
STATES = {"PLANNED", "SPEC_READY", "SPECULATING", "PATCH_SUBMITTED", "ASSEMBLED",
          "CAMPAIGN_FAIL", "CAMPAIGN_PASS", "REPAIR_QUEUED", "VERIFIED", "BLOCKED"}


def find_cycle(nodes, edges):
    """Return one cycle as a list of ids, or None. edges: id -> list of dependency ids."""
    WHITE, GRAY, BLACK = 0, 1, 2
    color = {n: WHITE for n in nodes}
    stack = []

    def visit(n):
        color[n] = GRAY
        stack.append(n)
        for d in edges.get(n, []):
            if d not in color:
                continue
            if color[d] == GRAY:
                return stack[stack.index(d):] + [d]
            if color[d] == WHITE:
                c = visit(d)
                if c:
                    return c
        stack.pop()
        color[n] = BLACK
        return None

    sys.setrecursionlimit(10000)
    for n in nodes:
        if color[n] == WHITE:
            c = visit(n)
            if c:
                return c
    return None


def validate_graph(g):
    errors = []
    tasks = g.get("tasks", [])
    ids = [t.get("id") for t in tasks]
    seen = set()
    for i in ids:
        if i in seen:
            errors.append({"kind": "duplicate_id", "id": i})
        seen.add(i)
    for t in tasks:
        for f in REQUIRED:
            if f not in t:
                errors.append({"kind": "missing_field", "id": t.get("id"), "field": f})
        if t.get("state") not in STATES:
            errors.append({"kind": "bad_state", "id": t.get("id"), "state": t.get("state")})
        for kind in ("integration_dependencies", "contract_dependencies"):
            for d in t.get(kind, []):
                if d not in seen:
                    errors.append({"kind": "unknown_dependency", "id": t["id"], "dep_kind": kind, "dep": d})
                if d == t.get("id"):
                    errors.append({"kind": "self_dependency", "id": t["id"]})
    for kind in ("integration_dependencies", "contract_dependencies"):
        edges = {t["id"]: t.get(kind, []) for t in tasks if "id" in t}
        cyc = find_cycle(list(edges), edges)
        if cyc:
            errors.append({"kind": "cycle", "dep_kind": kind, "cycle": cyc})
    stats = {
        "tasks": len(tasks),
        "integration_edges": sum(len(t.get("integration_dependencies", [])) for t in tasks),
        "contract_edges": sum(len(t.get("contract_dependencies", [])) for t in tasks),
        "epics": sorted({t.get("epic") for t in tasks}),
    }
    return errors, stats


def validate_leases(lease_docs, task_ids):
    """Leases: [{lease_id, task_ids, files:[paths]}]. Files must be disjoint across leases."""
    errors = []
    owner = {}
    for doc in lease_docs:
        for lease in doc.get("leases", []):
            for tid in lease.get("task_ids", []):
                if tid not in task_ids:
                    errors.append({"kind": "lease_unknown_task", "lease": lease["lease_id"], "task": tid})
            for f in lease.get("files", []):
                if any(ch in f for ch in "*?["):
                    errors.append({"kind": "lease_glob_not_file", "lease": lease["lease_id"], "file": f})
                if f in owner and owner[f] != lease["lease_id"]:
                    errors.append({"kind": "lease_overlap", "file": f, "leases": [owner[f], lease["lease_id"]]})
                owner[f] = lease["lease_id"]
    protected = ["Cargo.toml", "*/Cargo.toml", "*/lib.rs", "*/mod.rs", "contracts/*", "harness/oracle/*", "tests/expected/*"]
    for f, l in owner.items():
        if any(fnmatch.fnmatch(f, p) for p in protected):
            errors.append({"kind": "lease_on_protected_path", "file": f, "lease": l})
    return errors, {"leases": sum(len(d.get("leases", [])) for d in lease_docs), "files": len(owner)}


def load_graph(path: Path) -> dict[str, Any]:
    """Load + validate; raise DagError("<n> errors: <first 5 as JSON>") when invalid."""
    raise NotImplementedError("FAC-01: load and validate graph")


def integration_waves(graph: dict[str, Any]) -> list[list[str]]:
    """Kahn layering (see module docstring); raise DagError on a cycle."""
    raise NotImplementedError("FAC-01: integration waves")


def speculative_ready(graph: dict[str, Any], frozen_contracts: Iterable[str]) -> list[str]:
    """Sorted ids eligible for speculative dispatch (module docstring)."""
    raise NotImplementedError("FAC-01: speculative readiness")


def integration_ready(graph: dict[str, Any], verified: Iterable[str]) -> list[str]:
    """Sorted ids whose integration dependencies are all verified."""
    raise NotImplementedError("FAC-01: integration readiness")


def fanout(graph: dict[str, Any]) -> dict[str, int]:
    """Transitive dependent counts over integration edges."""
    raise NotImplementedError("FAC-01: dependency fanout")


def merge_repair_nodes(graph: dict[str, Any], tickets: list[dict[str, Any]]) -> \
        tuple[dict[str, Any], list[dict[str, Any]]]:
    """New graph with tickets appended + validation errors (module docstring)."""
    raise NotImplementedError("FAC-01: merge repair tickets")


def main(argv):
    if len(argv) < 3 or argv[1] != "validate":
        print(__doc__)
        return 2
    g = json.load(open(argv[2]))
    errors, stats = validate_graph(g)
    if "--leases" in argv:
        paths = argv[argv.index("--leases") + 1:]
        docs = [json.load(open(p)) for p in paths]
        e2, s2 = validate_leases(docs, {t["id"] for t in g["tasks"]})
        errors += e2
        stats.update(s2)
    for e in errors:
        print(json.dumps(e))
    print(json.dumps({"valid": not errors, **stats}))
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
