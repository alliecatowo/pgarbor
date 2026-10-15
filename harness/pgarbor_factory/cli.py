"""``python -m pgarbor_factory <command>`` — factory command line (argparse, stdlib only).

Lease FAC-19.

Commands (RFC 003 §8 interface, mapped onto library calls):

  dag validate [--graph PATH] [--leases]       dag.validate_graph (+ leases.validate_catalog
                                               with marker coverage); exit 1 when invalid
  dag waves                                    print integration waves
  leases sync | list [--state S] | grant ID --holder H [--ttl S] | release ID |
         expire                                ledger operations
  inventory [--json]                           inventory.inventory summary by crate/lease
  packet ID                                    packager.write_packet; prints path
  worktree ID                                  worktrees.allocate; prints path
  submit ID --worker W [--assumption A]... [--behavior B]... [--unresolved U]...
  review ID                                    review.review_patch of the submission
  assemble --epoch N --base SHA                assemble.assemble_epoch
  build [--worktree PATH]                      build.cargo_build -> clusters summary
  campaign run [--epoch N] [--mode M] [--only FAMILY]... [--jobs N]
  guard check                                  guard.check_campaign_preconditions; exit 3 if not ok
  report PATH_TO_RUN_DIR                       re-render reports from evidence.json
  repair --from RUN_DIR                        cluster + repair -> repair_dag.json
  halt --budget-epochs N [--window K]          halt.decide over the evidence ledger
  schedule --max-authors N                     scheduler.plan_wave (dry run, prints grants)

Global: ``--repo PATH``; ``--json`` for machine output. Exit codes: 0 ok, 1 validation
failure, 2 usage error (argparse), 3 guard refusal, 4 internal error (traceback to stderr).
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import sys
import traceback
from enum import Enum
from pathlib import Path
from typing import Any, Optional, Sequence

from .leases import LeaseError

# Ledger / run-directory file names (RFC 003 and contracts/crates/harness.md §8).
LEDGER_NAME = "leases.json"
EVIDENCE_NAME = "evidence.json"
CLUSTERS_NAME = "clusters.json"
REPAIR_DAG_NAME = "repair_dag.json"
EVIDENCE_LEDGER_NAME = "evidence_ledger.jsonl"
FROZEN_NAME = "frozen_contracts.json"

#: Default lease time-to-live in seconds when ``leases grant`` gets no ``--ttl``.
DEFAULT_TTL_S = 3600
#: Default verification worker count for ``campaign run``.
DEFAULT_JOBS = 4
CAMPAIGN_MODES: tuple[str, ...] = ("cathedral", "seams", "conventional")

EXIT_OK = 0
EXIT_INVALID = 1
EXIT_USAGE = 2
EXIT_GUARD = 3
EXIT_INTERNAL = 4


class CliError(Exception):
    """A user-facing failure with an explicit exit code (printed without a traceback)."""

    def __init__(self, message: str, code: int = EXIT_INVALID) -> None:
        super().__init__(message)
        self.code = code


# ---------------------------------------------------------------------------------------------
# Argument types
# ---------------------------------------------------------------------------------------------


def _positive_int(text: str) -> int:
    try:
        value = int(text, 10)
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected a positive integer, got {text!r}") from None
    if value < 1:
        raise argparse.ArgumentTypeError(f"expected a positive integer, got {text!r}")
    return value


def _non_negative_int(text: str) -> int:
    try:
        value = int(text, 10)
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected a non-negative integer, got {text!r}") from None
    if value < 0:
        raise argparse.ArgumentTypeError(f"expected a non-negative integer, got {text!r}")
    return value


# ---------------------------------------------------------------------------------------------
# Parser
# ---------------------------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    from .records import LeaseState

    # Global options are accepted before or after the subcommand. SUPPRESS keeps a subcommand
    # from overwriting a value given at the top level.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--repo", metavar="PATH", default=argparse.SUPPRESS,
                        help="repository root (default: PGARBOR_REPO, else discovered from "
                             "the package location)")
    common.add_argument("--json", action="store_true", default=argparse.SUPPRESS,
                        help="machine-readable JSON output")

    parser = argparse.ArgumentParser(
        prog="python -m pgarbor_factory",
        description="PGArbor RightShift factory command line.",
        epilog="exit codes: 0 ok, 1 validation failure, 2 usage error, 3 guard refusal, "
               "4 internal error",
        parents=[common],
    )
    subs = parser.add_subparsers(dest="command", metavar="COMMAND", required=True)

    def add(group: Any, name: str, help_text: str) -> argparse.ArgumentParser:
        return group.add_parser(name, parents=[common], help=help_text, description=help_text)

    # dag ----------------------------------------------------------------------------------
    dag_grp = subs.add_parser("dag", parents=[common], help="task DAG operations")
    dag_sub = dag_grp.add_subparsers(dest="dag_command", metavar="SUBCOMMAND", required=True)
    p = add(dag_sub, "validate", "validate the task graph; --leases also checks the lease catalog")
    p.add_argument("--graph", metavar="PATH", default=None,
                   help="task graph (default: contracts/dag/task_graph.v1.json)")
    p.add_argument("--leases", action="store_true",
                   help="also validate contracts/leases/*.json and marker coverage")
    p.set_defaults(func=_cmd_dag_validate)
    add(dag_sub, "waves", "print integration waves").set_defaults(func=_cmd_dag_waves)

    # leases -------------------------------------------------------------------------------
    lease_grp = subs.add_parser("leases", parents=[common], help="lease ledger operations")
    lease_sub = lease_grp.add_subparsers(dest="leases_command", metavar="SUBCOMMAND",
                                         required=True)
    add(lease_sub, "sync", "sync the ledger with the lease catalog").set_defaults(
        func=_cmd_leases_sync)
    p = add(lease_sub, "list", "list ledger entries")
    p.add_argument("--state", choices=[s.value for s in LeaseState], default=None,
                   help="only entries in this state")
    p.set_defaults(func=_cmd_leases_list)
    p = add(lease_sub, "grant", "grant a lease to a holder")
    p.add_argument("lease_id", metavar="ID")
    p.add_argument("--holder", required=True, metavar="H")
    p.add_argument("--ttl", type=_positive_int, default=DEFAULT_TTL_S, metavar="S",
                   help=f"lease time-to-live in seconds (default {DEFAULT_TTL_S})")
    p.set_defaults(func=_cmd_leases_grant)
    p = add(lease_sub, "release", "release a granted lease")
    p.add_argument("lease_id", metavar="ID")
    p.add_argument("--holder", default=None, metavar="H",
                   help="holder to release from (default: the current holder)")
    p.add_argument("--note", default="", metavar="TEXT")
    p.set_defaults(func=_cmd_leases_release)
    add(lease_sub, "expire", "expire stale leases").set_defaults(func=_cmd_leases_expire)

    # inventory / packet / worktree / submit / review ------------------------------------
    add(subs, "inventory", "stub-marker inventory summary").set_defaults(func=_cmd_inventory)

    p = add(subs, "packet", "write the context packet for a lease")
    p.add_argument("lease_id", metavar="ID")
    p.set_defaults(func=_cmd_packet)

    p = add(subs, "worktree", "allocate the worktree for a lease")
    p.add_argument("lease_id", metavar="ID")
    p.set_defaults(func=_cmd_worktree)

    p = add(subs, "submit", "record a worker submission for a lease")
    p.add_argument("lease_id", metavar="ID")
    p.add_argument("--worker", required=True, metavar="W")
    p.add_argument("--assumption", action="append", default=None, metavar="A")
    p.add_argument("--behavior", action="append", default=None, metavar="B")
    p.add_argument("--unresolved", action="append", default=None, metavar="U")
    p.set_defaults(func=_cmd_submit)

    p = add(subs, "review", "static review of a lease submission")
    p.add_argument("lease_id", metavar="ID")
    p.set_defaults(func=_cmd_review)

    # assemble / build / campaign / guard / report / repair / halt / schedule -------------
    p = add(subs, "assemble", "assemble an epoch from submissions")
    p.add_argument("--epoch", required=True, type=_non_negative_int, metavar="N")
    p.add_argument("--base", required=True, metavar="SHA")
    p.set_defaults(func=_cmd_assemble)

    p = add(subs, "build", "central cargo build with error clustering")
    p.add_argument("--worktree", default=None, metavar="PATH",
                   help="tree to build (default: the repository root)")
    p.set_defaults(func=_cmd_build)

    camp_grp = subs.add_parser("campaign", parents=[common], help="evidence campaigns")
    camp_sub = camp_grp.add_subparsers(dest="campaign_command", metavar="SUBCOMMAND",
                                       required=True)
    p = add(camp_sub, "run", "run a full evidence campaign (or selected families)")
    p.add_argument("--epoch", type=_non_negative_int, default=0, metavar="N")
    p.add_argument("--mode", choices=list(CAMPAIGN_MODES), default="cathedral")
    p.add_argument("--only", action="append", default=None, metavar="FAMILY",
                   help="run only this family (repeatable)")
    p.add_argument("--jobs", type=_positive_int, default=DEFAULT_JOBS, metavar="N")
    p.set_defaults(func=_cmd_campaign_run)

    guard_grp = subs.add_parser("guard", parents=[common], help="verifier guard")
    guard_sub = guard_grp.add_subparsers(dest="guard_command", metavar="SUBCOMMAND",
                                         required=True)
    add(guard_sub, "check", "check campaign preconditions (exit 3 when not ok)").set_defaults(
        func=_cmd_guard_check)

    p = add(subs, "report", "re-render reports from a run directory")
    p.add_argument("run_dir", metavar="PATH_TO_RUN_DIR")
    p.set_defaults(func=_cmd_report)

    p = add(subs, "repair", "cluster failures and write the repair DAG")
    p.add_argument("--from", dest="from_run", required=True, metavar="RUN_DIR")
    p.set_defaults(func=_cmd_repair)

    p = add(subs, "halt", "halt decision over the evidence ledger")
    p.add_argument("--budget-epochs", required=True, type=_positive_int, metavar="N")
    p.add_argument("--window", type=_positive_int, default=3, metavar="K",
                   help="no-progress window in campaigns (default 3)")
    p.set_defaults(func=_cmd_halt)

    p = add(subs, "schedule", "dry-run dispatch wave plan")
    p.add_argument("--max-authors", required=True, type=_positive_int, metavar="N")
    p.set_defaults(func=_cmd_schedule)

    return parser


# ---------------------------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------------------------


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(list(argv) if argv is not None else None)
    except SystemExit as exc:  # argparse: usage error (2) or --help (0)
        return exc.code if isinstance(exc.code, int) else EXIT_USAGE

    handler = getattr(args, "func", None)
    if handler is None:  # unreachable: every leaf sets func and groups are required
        parser.print_usage(sys.stderr)
        return EXIT_USAGE
    try:
        rc = handler(args)
    except CliError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return exc.code
    except LeaseError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EXIT_INVALID
    except Exception:  # internal error: stub markers, I/O faults, bugs
        traceback.print_exc()
        return EXIT_INTERNAL
    return int(rc) if rc is not None else EXIT_OK


# ---------------------------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------------------------


def _paths(args: argparse.Namespace) -> Any:
    from pgarbor_harness.config import Paths

    repo = getattr(args, "repo", None)
    return Paths(root=Path(repo).resolve()) if repo else Paths()


def _want_json(args: argparse.Namespace) -> bool:
    return bool(getattr(args, "json", False))


def _jsonable(obj: Any) -> Any:
    if hasattr(obj, "to_json") and callable(obj.to_json):
        return _jsonable(obj.to_json())
    if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
        return _jsonable(dataclasses.asdict(obj))
    if isinstance(obj, Path):
        return str(obj)
    if isinstance(obj, Enum):
        return obj.value
    if isinstance(obj, (list, tuple)):
        return [_jsonable(item) for item in obj]
    if isinstance(obj, dict):
        return {str(key): _jsonable(value) for key, value in obj.items()}
    return obj


def _print_json(obj: Any) -> None:
    print(json.dumps(_jsonable(obj), indent=2, sort_keys=True))


def _out(args: argparse.Namespace, obj: Any, text: str) -> None:
    """Machine JSON with --json, otherwise the human text line(s)."""
    if _want_json(args):
        _print_json(obj)
    else:
        print(text)


def _load_json(path: Path) -> Any:
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as exc:
        raise CliError(f"cannot read {path}: {exc.strerror or exc}") from exc
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise CliError(f"{path} is not valid JSON: {exc}") from exc


def _read_jsonl(path: Path) -> list[Any]:
    if not path.exists():
        return []
    rows: list[Any] = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError as exc:
            raise CliError(f"{path}:{number} is not valid JSON: {exc}") from exc
    return rows


def _validated_graph(path: Path) -> dict[str, Any]:
    from . import dag

    graph = _load_json(path)
    errors, _stats = dag.validate_graph(graph)
    if errors:
        for err in errors:
            print(json.dumps(err, sort_keys=True), file=sys.stderr)
        raise CliError(f"task graph {path} is invalid ({len(errors)} errors)")
    return graph


def _ledger_path(paths: Any) -> Path:
    return paths.state / LEDGER_NAME


def _ensure_state(paths: Any) -> None:
    paths.state.mkdir(parents=True, exist_ok=True)


def _load_pack_file(path: Path) -> Any:
    from . import records

    doc = _load_json(path)
    try:
        return records.EvidencePack.from_json(doc)
    except (ValueError, KeyError, TypeError) as exc:
        raise CliError(f"{path} is not an evidence pack: {exc}") from exc


def _load_pack(run_dir: Path) -> Any:
    return _load_pack_file(Path(run_dir) / EVIDENCE_NAME)


def _cluster_list(doc: Any, path: Path) -> list[Any]:
    if isinstance(doc, list):
        return doc
    if isinstance(doc, dict) and isinstance(doc.get("clusters"), list):
        return doc["clusters"]
    raise CliError(f"{path} does not hold a cluster list")


def _read_clusters(run_dir: Path) -> list[Any]:
    path = Path(run_dir) / CLUSTERS_NAME
    return _cluster_list(_load_json(path), path)


def _cluster_key(cluster: Any) -> str:
    if isinstance(cluster, dict):
        return str(cluster["key"])
    return str(cluster.key)


def _read_tickets(run_dir: Path) -> list[Any]:
    path = Path(run_dir) / REPAIR_DAG_NAME
    doc = _load_json(path)
    if not isinstance(doc, dict) or not isinstance(doc.get("tasks"), list):
        raise CliError(f"{path} is not a repair DAG ({{\"tasks\": [...]}})")
    return doc["tasks"]


def _read_frozen(path: Path) -> list[str]:
    if not path.exists():
        # No frozen-contract record means nothing is frozen, so nothing is speculatively
        # dispatched. This is the safe default, not a silent pass.
        return []
    doc = _load_json(path)
    if not isinstance(doc, list) or not all(isinstance(x, str) for x in doc):
        raise CliError(f"{path} must be a JSON list of task ids")
    return doc


def _campaign_history(paths: Any) -> tuple[list[Any], Optional[list[list[str]]]]:
    """Evidence packs oldest first, and per-campaign cluster keys (None unless every
    campaign has a clusters file, so that the no-progress rule never runs on partial data)."""
    history: list[Any] = []
    keys: Optional[list[list[str]]] = []
    for row in _read_jsonl(paths.state / EVIDENCE_LEDGER_NAME):
        if not isinstance(row, dict) or "path" not in row:
            raise CliError(f"malformed evidence ledger row: {row!r}")
        evidence = Path(str(row["path"]))
        if not evidence.is_absolute():
            evidence = paths.root / evidence
        history.append(_load_pack_file(evidence))
        run_dir = evidence.parent
        if keys is not None:
            if (run_dir / CLUSTERS_NAME).exists():
                keys.append([_cluster_key(c) for c in _read_clusters(run_dir)])
            else:
                keys = None
    return history, keys


def _print_guard(args: argparse.Namespace, report: Any) -> None:
    if _want_json(args):
        _print_json(report)
        return
    print("guard: ok" if report.ok else "guard: REFUSED")
    for violation in report.violations:
        print(f"  {violation.kind}\t{violation.path}\t{violation.detail}".rstrip())


# ---------------------------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------------------------


def _cmd_dag_validate(args: argparse.Namespace) -> int:
    from . import dag, leases

    paths = _paths(args)
    graph_path = Path(args.graph) if args.graph else paths.dag
    graph = _load_json(graph_path)
    errors, stats = dag.validate_graph(graph)
    if args.leases:
        task_ids = {t["id"] for t in graph.get("tasks", []) if "id" in t}
        docs = [_load_json(p) for p in sorted(paths.leases_dir.glob("*.json"))]
        lease_errors, lease_stats = dag.validate_leases(docs, task_ids)
        errors += lease_errors
        stats.update(lease_stats)
        specs = leases.load_catalog(paths.leases_dir)
        errors += leases.validate_catalog(specs, task_ids, paths.root)
    # Same line format as the original bootstrap validator (dag.main).
    for err in errors:
        print(json.dumps(err, sort_keys=True))
    print(json.dumps({"valid": not errors, **stats}, sort_keys=True))
    return EXIT_OK if not errors else EXIT_INVALID


def _cmd_dag_waves(args: argparse.Namespace) -> int:
    from . import dag

    paths = _paths(args)
    graph = _validated_graph(paths.dag)
    waves = dag.integration_waves(graph)
    if _want_json(args):
        _print_json({"waves": waves})
    else:
        for index, wave in enumerate(waves):
            print(f"wave {index}: {' '.join(wave)}")
    return EXIT_OK


def _cmd_leases_sync(args: argparse.Namespace) -> int:
    from . import leases

    paths = _paths(args)
    _ensure_state(paths)
    specs = leases.load_catalog(paths.leases_dir)
    ledger = leases.sync_ledger(_ledger_path(paths), specs)
    _out(args, ledger, f"ledger has {len(ledger)} leases; catalog has {len(specs)}")
    return EXIT_OK


def _cmd_leases_list(args: argparse.Namespace) -> int:
    from . import leases

    paths = _paths(args)
    ledger = leases.load_ledger(_ledger_path(paths))
    entries = sorted((e for e in ledger.values()
                      if args.state is None or e.state.value == args.state),
                     key=lambda e: e.lease_id)
    if _want_json(args):
        _print_json(entries)
    else:
        for e in entries:
            print(f"{e.lease_id}\t{e.state.value}\t{e.holder or '-'}\t{e.attempts}")
    return EXIT_OK


def _cmd_leases_grant(args: argparse.Namespace) -> int:
    from . import gitutil, leases

    paths = _paths(args)
    _ensure_state(paths)
    base = gitutil.head_sha(paths.root)
    entry = leases.grant(_ledger_path(paths), args.lease_id, args.holder, args.ttl, base)
    _out(args, entry, f"granted {entry.lease_id} to {entry.holder} "
                      f"(expires {entry.expires_unix}, base {entry.base_sha})")
    return EXIT_OK


def _cmd_leases_release(args: argparse.Namespace) -> int:
    from . import leases

    paths = _paths(args)
    _ensure_state(paths)
    path = _ledger_path(paths)
    current = leases.load_ledger(path).get(args.lease_id)
    if current is None:
        raise CliError(f"unknown lease {args.lease_id}")
    holder = args.holder or current.holder
    if not holder:
        raise CliError(f"lease {args.lease_id} has no holder; pass --holder")
    entry = leases.release(path, args.lease_id, holder, args.note)
    _out(args, entry, f"released {entry.lease_id}; state {entry.state.value}")
    return EXIT_OK


def _cmd_leases_expire(args: argparse.Namespace) -> int:
    from . import leases, records

    paths = _paths(args)
    _ensure_state(paths)
    expired = leases.expire_stale(_ledger_path(paths), records.now_unix())
    _out(args, {"expired": expired},
         f"expired {', '.join(expired)}" if expired else "no stale leases")
    return EXIT_OK


def _cmd_inventory(args: argparse.Namespace) -> int:
    from . import inventory

    paths = _paths(args)
    doc = inventory.inventory(paths.root)
    if _want_json(args):
        _print_json(doc)
        return EXIT_OK
    print(f"markers: {doc['total']} (forbidden: {len(doc.get('forbidden', []))})")
    for lease_id, count in sorted(doc.get("by_lease", {}).items()):
        print(f"  lease {lease_id}: {count}")
    for crate, count in sorted(doc.get("by_crate", {}).items()):
        print(f"  crate {crate}: {count}")
    return EXIT_OK


def _cmd_packet(args: argparse.Namespace) -> int:
    from . import packager

    paths = _paths(args)
    path = packager.write_packet(paths.root, args.lease_id)
    _out(args, {"path": path}, str(path))
    return EXIT_OK


def _cmd_worktree(args: argparse.Namespace) -> int:
    from . import gitutil, leases, worktrees

    paths = _paths(args)
    entry = leases.load_ledger(_ledger_path(paths)).get(args.lease_id)
    base = entry.base_sha if entry is not None and entry.base_sha else gitutil.head_sha(paths.root)
    worktree = worktrees.allocate(paths.root, args.lease_id, base)
    _out(args, worktree, str(worktree.path))
    return EXIT_OK


def _cmd_submit(args: argparse.Namespace) -> int:
    from . import submissions

    paths = _paths(args)
    sub = submissions.submit(
        paths.root, args.lease_id, args.worker, branch=None,
        assumptions=args.assumption or [], behaviors=args.behavior or [],
        unresolved=args.unresolved or [],
    )
    _out(args, sub, f"submitted {sub.lease_id} from {sub.branch} at {sub.head_sha} "
                    f"({len(sub.files_changed)} files changed)")
    return EXIT_OK


def _cmd_review(args: argparse.Namespace) -> int:
    from . import leases, review, submissions

    paths = _paths(args)
    specs = leases.load_catalog(paths.leases_dir)
    spec = next((s for s in specs if s.lease_id == args.lease_id), None)
    if spec is None:
        raise CliError(f"unknown lease {args.lease_id}")
    sub = submissions.load_submissions(paths.root).get(args.lease_id)
    if sub is None:
        raise CliError(f"no submission for {args.lease_id}")
    findings = review.review_patch(paths.root, spec, sub.base_sha, sub.head_sha)
    # Review is advisory; reject rules are enforced by assemble.check_patch, not here.
    if _want_json(args):
        _print_json(findings)
    else:
        for finding in findings:
            print(json.dumps(_jsonable(finding), sort_keys=True))
        print(f"{len(findings)} findings for {args.lease_id}")
    return EXIT_OK


def _cmd_assemble(args: argparse.Namespace) -> int:
    from . import assemble

    paths = _paths(args)
    report = assemble.assemble_epoch(paths.root, args.epoch, args.base)
    lines = [f"epoch {report.epoch} {report.epoch_sha}: {len(report.accepted)} accepted, "
             f"{len(report.rejected)} rejected"]
    lines += [f"  rejected {r.lease_id}: {r.reason}" for r in report.rejected]
    _out(args, report, "\n".join(lines))
    return EXIT_OK


def _cmd_build(args: argparse.Namespace) -> int:
    from . import build, leases

    paths = _paths(args)
    worktree = Path(args.worktree).resolve() if args.worktree else paths.root
    log_dir = paths.runs / "build"
    log_dir.mkdir(parents=True, exist_ok=True)
    owners = leases.file_owner_map(leases.load_catalog(paths.leases_dir))
    report = build.cargo_build(worktree, paths.root / "target", owners, log_dir)
    _out(args, report,
         f"build {'ok' if report.ok else 'FAILED'} in {report.duration_s:.1f}s; "
         f"{len(report.clusters)} error clusters")
    return EXIT_OK if report.ok else EXIT_INVALID


def _cmd_campaign_run(args: argparse.Namespace) -> int:
    from . import campaign, guard

    paths = _paths(args)
    cfg = campaign.CampaignConfig(repo_root=paths.root, epoch=args.epoch, mode=args.mode,
                                  jobs=args.jobs)
    if args.only:
        # run_family bypasses run_campaign's own guard step, so check here as well.
        report = guard.check_campaign_preconditions(paths.root)
        if not report.ok:
            _print_guard(args, report)
            return EXIT_GUARD
        families = {name: campaign.run_family(cfg, name) for name in args.only}
        _out(args, families, "ran families: " + ", ".join(families))
        return EXIT_OK
    result = campaign.run_campaign(cfg)
    _out(args, result, f"campaign {result.campaign_id}: exit {result.exit_code}, "
                       f"run dir {result.run_dir}")
    return int(result.exit_code)


def _cmd_guard_check(args: argparse.Namespace) -> int:
    from . import guard

    paths = _paths(args)
    report = guard.check_campaign_preconditions(paths.root)
    _print_guard(args, report)
    return EXIT_OK if report.ok else EXIT_GUARD


def _cmd_report(args: argparse.Namespace) -> int:
    from . import report as reporting

    run_dir = Path(args.run_dir)
    pack = _load_pack(run_dir)
    clusters = _read_clusters(run_dir)
    tickets = _read_tickets(run_dir)
    md_path, json_path = reporting.write_reports(pack, clusters, tickets, run_dir)
    _out(args, {"markdown": md_path, "json": json_path}, f"{md_path}\n{json_path}")
    return EXIT_OK


def _cmd_repair(args: argparse.Namespace) -> int:
    from . import leases, repair

    paths = _paths(args)
    run_dir = Path(args.from_run)
    pack = _load_pack(run_dir)
    clusters = _read_clusters(run_dir)
    graph = _validated_graph(paths.dag)
    specs = leases.load_catalog(paths.leases_dir)
    tickets = repair.generate_tickets(clusters, graph, specs, pack.epoch)
    out = run_dir / REPAIR_DAG_NAME
    repair.write_repair_dag(tickets, out, pack.campaign_id, pack.epoch)
    _out(args, {"path": out, "tickets": len(tickets)},
         f"{len(tickets)} repair tickets -> {out}")
    return EXIT_OK


def _cmd_halt(args: argparse.Namespace) -> int:
    from . import halt

    paths = _paths(args)
    history, cluster_keys = _campaign_history(paths)
    budget = halt.Budget(max_epochs=args.budget_epochs, no_progress_window=args.window)
    decision = halt.decide(history, budget, cluster_keys)
    _out(args, decision, f"{decision.action}" + "".join(f"\n  {r}" for r in decision.reasons))
    return EXIT_OK


def _cmd_schedule(args: argparse.Namespace) -> int:
    from . import leases, scheduler

    paths = _paths(args)
    graph = _validated_graph(paths.dag)
    specs = leases.load_catalog(paths.leases_dir)
    ledger = leases.load_ledger(_ledger_path(paths))
    frozen = _read_frozen(paths.state / FROZEN_NAME)
    plan = scheduler.plan_wave(graph, specs, ledger, frozen, max_authors=args.max_authors)
    lines = [f"grant {lease_id}" for lease_id in plan.grants]
    lines += [f"defer {lease_id}: {why}" for lease_id, why in sorted(plan.deferred.items())]
    _out(args, plan, "\n".join(lines) if lines else "no grants")
    return EXIT_OK
