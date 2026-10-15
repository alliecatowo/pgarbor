"""``python -m pgarbor_harness <command>`` — harness command line (argparse, stdlib only).

Lease HAR-29.

Commands (each maps to library calls; the CLI adds no semantics):

  oracle up [--recreate]          oracle.container.OracleContainer().ensure(); print endpoint
  oracle down                     stop the harness container (never touches pgarbor-oracle)
  oracle status                   status + fingerprint verification (exit 1 on drift)
  oracle fingerprint              print fingerprint JSON + sha
  cases validate [paths...]       load + validate corpus; print INVALID entries; exit 1 if any
  record [--family F] [--id GLOB] [--rerecord] [--dry-run] [--jobs N]
                                  oracle.recorder.record_corpus (needs PGARBOR_ORACLE_REVIEW=1
                                  unless --dry-run)
  diff --target pgarbor|pg|dsn [--binary PATH] [--dsn CONNINFO] [--family F] [--id GLOB]
       [--jobs N] [--out DIR]     runner.differential.run_cases; prints counts by Outcome;
                                  exit 0 even with failures (the factory interprets results),
                                  exit 2 on HARNESS_ERROR-only runs
  minimize --case ID --target ... [--max-runs N]
  regress [--target ...] [--only t1,t2] [--use-existing] [--record-baseline]
  isolation [--target ...] [--only ...]
  slt run|record PATH...
  wire record --client LABEL --listen-port P | wire replay PATH --target ...
  generate --generator NAME --seed N [--out tests/cases]
  selftest [--live]               selftest.run; exit 1 when not ok
  manifest freeze|check           oracle.manifest

Global flags: ``--repo PATH`` (sets PGARBOR_REPO), ``--json`` (machine output on stdout;
human text otherwise). Output dirs default to ``runs/adhoc/<UTC yyyymmddThhmmssZ>/``.

Notes on choices this module makes (documented so the factory can rely on them):

* ``--target`` for the live commands (minimize, regress, isolation, slt run, wire replay)
  selects the endpoint: ``pgarbor`` (default for the suite commands; a PGArbor process per
  case via ``--binary``, default ``target/release/pgarbor`` under the repo root), ``pg`` (the
  pinned oracle container) or ``dsn`` (an externally managed server given by ``--dsn``).
* Exit codes: 0 ok; 1 validation/verification failure; 2 usage error (argparse, a missing
  option value or a HARNESS_ERROR-only run); 4 internal error (traceback on stderr).
* Commands that write expected outputs (record, slt record, wire record, manifest freeze,
  regress --record-baseline) refuse to run unless ``PGARBOR_ORACLE_REVIEW=1``; the library
  enforces the same gate.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as _dt
import os
import sys
import threading
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterator, Optional, Sequence

from .config import PG_IMAGE_DIGEST, PG_SERVER_VERSION, paths
from .model import Endpoint, Outcome, canonical_json

_TARGET_CHOICES = ("pgarbor", "pg", "dsn")
_GENERATOR_CHOICES = ("operators", "casts", "io", "ddl-dml")
_RECORD_FAILURES = ("NONDETERMINISTIC", "INTENT_MISMATCH", "ORACLE_ERROR")
_CLIENT_LABEL_OK = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")


class _UsageError(Exception):
    """A usage problem detected after argparse (reported on stderr, exit 2)."""


@dataclass
class _Live:
    """A running endpoint for a live command, plus optional liveness/restart hooks."""

    endpoint: Endpoint
    alive: Optional[Callable[[], bool]] = None
    restart: Optional[Callable[[], None]] = None


# ---------------------------------------------------------------------------------------------
# Parser construction
# ---------------------------------------------------------------------------------------------


def _common_parser() -> argparse.ArgumentParser:
    """Global flags accepted both before and after the subcommand (SUPPRESS keeps the value
    set by the top-level parser when the flag is not repeated in the subcommand)."""
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--json", action="store_true", default=argparse.SUPPRESS,
                        help="machine output (canonical JSON) on stdout")
    common.add_argument("--repo", metavar="PATH", default=argparse.SUPPRESS,
                        help="repository root (sets PGARBOR_REPO)")
    return common


def _add_target_args(p: argparse.ArgumentParser, default: Optional[str],
                     required: bool = False) -> None:
    p.add_argument("--target", choices=_TARGET_CHOICES, default=default, required=required,
                   help="endpoint under test: pgarbor (per-case process), pg (oracle), dsn")
    p.add_argument("--binary", metavar="PATH", default=None,
                   help="pgarbor binary for --target pgarbor "
                        "(default: <repo>/target/release/pgarbor)")
    p.add_argument("--dsn", metavar="CONNINFO", default=None,
                   help="libpq conninfo for --target dsn")


def _add_out_arg(p: argparse.ArgumentParser) -> None:
    p.add_argument("--out", metavar="DIR", default=None,
                   help="output directory (default runs/adhoc/<UTC timestamp>/)")


def _add_jobs_arg(p: argparse.ArgumentParser, default: int = 4) -> None:
    p.add_argument("--jobs", type=int, default=default, metavar="N",
                   help=f"parallel workers (default {default})")


def build_parser() -> argparse.ArgumentParser:
    """All subcommands/flags above."""
    common = _common_parser()
    parser = argparse.ArgumentParser(prog="python -m pgarbor_harness",
                                     description="PGArbor verification harness.")
    parser.add_argument("--json", action="store_true",
                        help="machine output (canonical JSON) on stdout")
    parser.add_argument("--repo", metavar="PATH", default=None,
                        help="repository root (sets PGARBOR_REPO)")
    sub = parser.add_subparsers(dest="command", required=True, metavar="<command>")

    # oracle ------------------------------------------------------------------------------
    p_oracle = sub.add_parser("oracle", parents=[common], help="pinned PG18 oracle container")
    osub = p_oracle.add_subparsers(dest="oracle_cmd", required=True, metavar="<action>")
    p = osub.add_parser("up", parents=[common], help="ensure the oracle container runs")
    p.add_argument("--recreate", action="store_true",
                   help="recreate the container when its spec drifted")
    p.set_defaults(func=cmd_oracle_up)
    p = osub.add_parser("down", parents=[common], help="stop the harness container")
    p.set_defaults(func=cmd_oracle_down)
    p = osub.add_parser("status", parents=[common], help="status and fingerprint check")
    p.set_defaults(func=cmd_oracle_status)
    p = osub.add_parser("fingerprint", parents=[common], help="print the oracle fingerprint")
    p.set_defaults(func=cmd_oracle_fingerprint)

    # cases -------------------------------------------------------------------------------
    p_cases = sub.add_parser("cases", parents=[common], help="case corpus")
    csub = p_cases.add_subparsers(dest="cases_cmd", required=True, metavar="<action>")
    p = csub.add_parser("validate", parents=[common], help="validate the corpus or files")
    p.add_argument("paths", nargs="*", metavar="PATH",
                   help="case files or directories (default tests/cases)")
    p.set_defaults(func=cmd_cases_validate)

    # record ------------------------------------------------------------------------------
    p = sub.add_parser("record", parents=[common], help="record PG reference transcripts")
    p.add_argument("--family", default=None, help="only this family")
    p.add_argument("--id", dest="id_glob", default=None, metavar="GLOB", help="case id glob")
    p.add_argument("--rerecord", action="store_true",
                   help="allow overwriting existing expected files")
    p.add_argument("--dry-run", action="store_true",
                   help="record into a temp dir and report what would change")
    _add_jobs_arg(p)
    p.set_defaults(func=cmd_record)

    # diff --------------------------------------------------------------------------------
    p = sub.add_parser("diff", parents=[common], help="differential run of the corpus")
    _add_target_args(p, default=None, required=True)
    p.add_argument("--family", default=None, help="only this family")
    p.add_argument("--id", dest="id_glob", default=None, metavar="GLOB", help="case id glob")
    _add_jobs_arg(p)
    _add_out_arg(p)
    p.set_defaults(func=cmd_diff)

    # minimize ----------------------------------------------------------------------------
    p = sub.add_parser("minimize", parents=[common], help="minimize one failing case")
    p.add_argument("--case", required=True, metavar="ID", help="case id")
    _add_target_args(p, default=None, required=True)
    p.add_argument("--max-runs", type=int, default=400, metavar="N",
                   help="candidate execution budget (default 400)")
    _add_out_arg(p)
    p.set_defaults(func=cmd_minimize)

    # regress -----------------------------------------------------------------------------
    p = sub.add_parser("regress", parents=[common], help="official pg_regress schedule")
    _add_target_args(p, default="pgarbor")
    p.add_argument("--only", default=None, metavar="t1,t2",
                   help="comma-separated tests (test_setup is always included)")
    p.add_argument("--use-existing", action="store_true",
                   help="do not recreate the regression database")
    p.add_argument("--record-baseline", action="store_true",
                   help="record PG baseline outputs (oracle review path)")
    _add_out_arg(p)
    p.set_defaults(func=cmd_regress)

    # isolation ---------------------------------------------------------------------------
    p = sub.add_parser("isolation", parents=[common], help="isolation spec suite")
    _add_target_args(p, default="pgarbor")
    p.add_argument("--only", default=None, metavar="t1,t2", help="comma-separated tests")
    _add_jobs_arg(p)
    _add_out_arg(p)
    p.set_defaults(func=cmd_isolation)

    # slt ---------------------------------------------------------------------------------
    p_slt = sub.add_parser("slt", parents=[common], help="sqllogictest files")
    ssub = p_slt.add_subparsers(dest="slt_cmd", required=True, metavar="<action>")
    p = ssub.add_parser("run", parents=[common], help="run recorded .slt files")
    _add_target_args(p, default="pgarbor")
    p.add_argument("paths", nargs="+", metavar="PATH", help="expected .slt files")
    p.set_defaults(func=cmd_slt_run)
    p = ssub.add_parser("record", parents=[common],
                        help="record .slt files from PG (oracle review path)")
    p.add_argument("paths", nargs="+", metavar="PATH",
                   help="input .slt files under tests/slt/")
    p.set_defaults(func=cmd_slt_record)

    # wire --------------------------------------------------------------------------------
    p_wire = sub.add_parser("wire", parents=[common], help="wire transcripts")
    wsub = p_wire.add_subparsers(dest="wire_cmd", required=True, metavar="<action>")
    p = wsub.add_parser("record", parents=[common],
                        help="run a recording proxy in front of the oracle (review path)")
    p.add_argument("--client", required=True, metavar="LABEL", help="client label")
    p.add_argument("--listen-port", required=True, type=int, metavar="P",
                   help="local port to listen on (0 = ephemeral)")
    p.add_argument("--out", metavar="PATH", default=None,
                   help="transcript file (default tests/expected/wire/<LABEL>.jsonl)")
    p.set_defaults(func=cmd_wire_record)
    p = wsub.add_parser("replay", parents=[common], help="replay a recorded transcript")
    p.add_argument("path", metavar="PATH", help="recorded .jsonl transcript")
    _add_target_args(p, default=None, required=True)
    _add_out_arg(p)
    p.set_defaults(func=cmd_wire_replay)

    # generate ----------------------------------------------------------------------------
    p = sub.add_parser("generate", parents=[common], help="generate seed cases")
    p.add_argument("--generator", required=True, choices=_GENERATOR_CHOICES,
                   help="generator name")
    p.add_argument("--seed", required=True, type=int, metavar="N", help="seed")
    p.add_argument("--out", metavar="DIR", default=None,
                   help="output corpus root (default tests/cases)")
    p.set_defaults(func=cmd_generate)

    # selftest ----------------------------------------------------------------------------
    p = sub.add_parser("selftest", parents=[common], help="verifier self-test")
    p.add_argument("--live", action="store_true",
                   help="also run the live PG-vs-PG self-test (needs the oracle)")
    p.set_defaults(func=cmd_selftest)

    # manifest ----------------------------------------------------------------------------
    p_man = sub.add_parser("manifest", parents=[common], help="oracle manifest")
    msub = p_man.add_subparsers(dest="manifest_cmd", required=True, metavar="<action>")
    p = msub.add_parser("freeze", parents=[common],
                        help="freeze the oracle manifest (oracle review path)")
    p.set_defaults(func=cmd_manifest_freeze)
    p = msub.add_parser("check", parents=[common], help="check the frozen manifest")
    p.set_defaults(func=cmd_manifest_check)

    return parser


# ---------------------------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------------------------


def _emit(args: argparse.Namespace, payload: Any, text: str) -> None:
    """--json prints canonical JSON of ``payload``; otherwise ``text``."""
    if getattr(args, "json", False):
        sys.stdout.write(canonical_json(payload))
    else:
        sys.stdout.write(text.rstrip("\n") + "\n")
    sys.stdout.flush()


def _rel(path: Path) -> str:
    root = paths().root
    try:
        return path.resolve().relative_to(root).as_posix()
    except ValueError:
        return str(path)


def _out_dir(args: argparse.Namespace) -> Path:
    raw = getattr(args, "out", None)
    if raw:
        out = Path(raw).resolve()
    else:
        stamp = _dt.datetime.now(_dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        out = paths().root / "runs" / "adhoc" / stamp
    out.mkdir(parents=True, exist_ok=True)
    return out


def _split_list(raw: Optional[str]) -> Optional[list[str]]:
    if raw is None:
        return None
    items = [x.strip() for x in raw.split(",") if x.strip()]
    return items or None


def _review_gate() -> bool:
    """Expected-output writers are the oracle review path only (PGARBOR_ORACLE_REVIEW=1)."""
    if os.environ.get("PGARBOR_ORACLE_REVIEW") == "1":
        return True
    sys.stderr.write("refused: this writes expected outputs; it is the oracle review path "
                     "and needs PGARBOR_ORACLE_REVIEW=1\n")
    return False


def _parse_dsn(dsn: str) -> Endpoint:
    """libpq conninfo -> Endpoint (psycopg is a runtime dependency; imported lazily)."""
    from psycopg.conninfo import conninfo_to_dict

    d = conninfo_to_dict(dsn)
    return Endpoint(host=str(d.get("host") or "127.0.0.1"),
                    port=int(d.get("port") or 5432),
                    dbname=str(d.get("dbname") or "postgres"),
                    user=str(d.get("user") or "postgres"),
                    password=d.get("password"))


def _dsn_endpoint(args: argparse.Namespace) -> Endpoint:
    if not args.dsn:
        raise _UsageError("--target dsn needs --dsn CONNINFO")
    return _parse_dsn(args.dsn)


def _pgarbor_target(args: argparse.Namespace, work_dir: Path) -> Any:
    from .runner.targets import PgarborCommand, PgarborTarget

    binary = args.binary or str(paths().root / "target" / "release" / "pgarbor")
    return PgarborTarget(work_dir, command=PgarborCommand(binary=binary))


def _make_target(args: argparse.Namespace, work_dir: Path) -> Any:
    """Target object for run_cases / run_suite / minimize (see module docstring)."""
    kind = args.target
    if kind == "pg":
        from .oracle.pool import OraclePool
        from .runner.targets import PostgresTarget

        return PostgresTarget(OraclePool(max_parallel=max(1, getattr(args, "jobs", 1))))
    if kind == "dsn":
        from .runner.targets import DsnTarget

        return DsnTarget(_dsn_endpoint(args), name="dsn")
    return _pgarbor_target(args, work_dir)


def _reference_target() -> Any:
    from .oracle.pool import OraclePool
    from .runner.targets import PostgresTarget

    return PostgresTarget(OraclePool(max_parallel=1), name="pg18-oracle")


@contextlib.contextmanager
def _live(args: argparse.Namespace, work_dir: Path, label: str) -> Iterator[_Live]:
    """Provide one live endpoint for ``args.target``; a pgarbor process is released on exit."""
    kind = args.target or "pgarbor"
    if kind == "pg":
        from .oracle.container import OracleContainer

        yield _Live(OracleContainer().ensure())
        return
    if kind == "dsn":
        yield _Live(_dsn_endpoint(args))
        return
    target = _pgarbor_target(args, work_dir)
    target.start()
    env = target.provision(label, 1)
    try:
        yield _Live(endpoint=env.endpoint,
                    alive=lambda: target.is_alive(env),
                    restart=lambda: target.restart(env, "clean"))
    finally:
        target.release(env)
        target.stop()


def _outcome_counts(outcomes: Sequence[Any]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for o in outcomes:
        key = str(getattr(o, "value", o))  # Outcome and RecordStatus are str enums
        counts[key] = counts.get(key, 0) + 1
    return dict(sorted(counts.items()))


def _counts_text(counts: dict[str, int]) -> list[str]:
    return [f"  {k:<22} {v}" for k, v in counts.items()]


# ---------------------------------------------------------------------------------------------
# Handlers: oracle
# ---------------------------------------------------------------------------------------------


def cmd_oracle_up(args: argparse.Namespace) -> int:
    from .oracle.container import OracleContainer

    ep = OracleContainer().ensure(recreate_on_drift=args.recreate)
    payload = {"host": ep.host, "port": ep.port, "dbname": ep.dbname, "user": ep.user}
    _emit(args, payload, f"oracle ready at {ep.host}:{ep.port} (db {ep.dbname})")
    return 0


def cmd_oracle_down(args: argparse.Namespace) -> int:
    from .oracle.container import OracleContainer

    OracleContainer().stop()
    _emit(args, {"stopped": True}, "oracle stopped")
    return 0


def cmd_oracle_status(args: argparse.Namespace) -> int:
    from .oracle import fingerprint
    from .oracle.container import OracleContainer, OracleError

    container = OracleContainer()
    st = container.status()
    drift = container.drift(st)
    errors: list[str] = []
    fp_sha: Optional[str] = None
    if st.running:
        try:
            _, fp_sha = fingerprint.verify_live(container)
        except OracleError as exc:
            errors.append(str(exc))
    else:
        errors.append("container not running")
    ok = not drift and not errors
    payload = {"exists": st.exists, "running": st.running, "image_digest": st.image_digest,
               "host_port": st.host_port, "drift": drift, "fingerprint_sha256": fp_sha,
               "errors": errors, "ok": ok}
    lines = [f"exists={st.exists} running={st.running} port={st.host_port}",
             f"fingerprint_sha256={fp_sha}"]
    lines += [f"DRIFT: {d}" for d in drift] + [f"ERROR: {e}" for e in errors]
    lines.append("ok" if ok else "NOT OK")
    _emit(args, payload, "\n".join(lines))
    return 0 if ok else 1


def cmd_oracle_fingerprint(args: argparse.Namespace) -> int:
    from .oracle import fingerprint
    from .oracle.container import OracleContainer, OracleError

    try:
        doc, sha = fingerprint.verify_live(OracleContainer())
    except OracleError as exc:
        sys.stderr.write(f"fingerprint failed: {exc}\n")
        return 1
    _emit(args, {"fingerprint": doc, "sha256": sha}, f"{sha}\n{canonical_json(doc)}")
    return 0


# ---------------------------------------------------------------------------------------------
# Handlers: cases / record / diff / minimize
# ---------------------------------------------------------------------------------------------


def cmd_cases_validate(args: argparse.Namespace) -> int:
    from .cases.loader import InvalidCase, load_corpus, load_file

    root = paths().root
    targets = [Path(p).resolve() for p in args.paths] or [paths().cases]
    valid = 0
    invalid: list[InvalidCase] = []
    for target in targets:
        if not target.exists():
            raise _UsageError(f"no such file or directory: {target}")
        if target.is_dir():
            corpus = load_corpus(target, root)
            valid += len(corpus.cases)
            invalid.extend(corpus.invalid)
        else:
            result = load_file(target, root)
            if isinstance(result, InvalidCase):
                invalid.append(result)
            else:
                valid += 1
    payload = {"valid": valid,
               "invalid": [{"path": _rel(Path(i.path)) if Path(i.path).is_absolute() else i.path,
                            "case_id": i.case_id, "errors": list(i.errors)} for i in invalid]}
    lines = []
    for item in payload["invalid"]:
        for err in item["errors"]:
            lines.append(f"INVALID {item['path']}: {err}")
    lines.append(f"{valid} valid, {len(invalid)} invalid")
    _emit(args, payload, "\n".join(lines))
    return 1 if invalid else 0


def cmd_record(args: argparse.Namespace) -> int:
    from .cases.loader import load_corpus, select
    from .oracle import recorder
    from .oracle.pool import OraclePool

    if not args.dry_run and not _review_gate():
        return 1
    cfg = paths()
    corpus = load_corpus(cfg.cases, cfg.root)
    for bad in corpus.invalid:
        sys.stderr.write(f"warning: invalid case file skipped: {bad.path}\n")
    cases = select(corpus, families=[args.family] if args.family else (),
                   id_glob=args.id_glob)
    pool = OraclePool(max_parallel=max(1, args.jobs))
    try:
        pool.start()
        results = recorder.record_corpus(pool, cases, cfg.expected,
                                         allow_overwrite=args.rerecord,
                                         dry_run=args.dry_run, jobs=args.jobs)
    finally:
        pool.close()
    rows = [{"case_id": r.case_id, "status": r.status.value, "path": r.path,
             "messages": list(r.messages)} for r in results]
    counts = _outcome_counts([r.status for r in results])
    failed = any(r.status.value in _RECORD_FAILURES for r in results)
    failed = failed or (not args.dry_run and any(r.status.value == "WOULD_CHANGE"
                                                 for r in results))
    lines = []
    for row in rows:
        suffix = ("  " + "; ".join(row["messages"])) if row["messages"] else ""
        lines.append(f"{row['status']:<16} {row['case_id']}{suffix}")
    lines += ["counts:"] + _counts_text(counts)
    _emit(args, {"results": rows, "counts": counts}, "\n".join(lines))
    return 1 if failed else 0


def cmd_diff(args: argparse.Namespace) -> int:
    from .cases.loader import load_corpus, select
    from .runner.differential import invalid_results, run_cases

    cfg = paths()
    out = _out_dir(args)
    corpus = load_corpus(cfg.cases, cfg.root)
    cases = select(corpus, families=[args.family] if args.family else (),
                   id_glob=args.id_glob)
    target = _make_target(args, out / "work")
    results = list(run_cases(cases, target, cfg.expected, out, live_reference=None,
                             jobs=max(1, args.jobs), reruns=1))
    if not args.family and not args.id_glob:
        results += invalid_results(corpus.invalid)
    counts = _outcome_counts([r.outcome for r in results])
    payload = {"target": target.name, "out_dir": _rel(out), "counts": counts,
               "results": [r.to_json() for r in results]}
    lines = [f"target {target.name}: {len(results)} case(s); out {_rel(out)}"]
    lines += _counts_text(counts)
    _emit(args, payload, "\n".join(lines))
    all_harness = bool(results) and all(r.outcome == Outcome.HARNESS_ERROR for r in results)
    return 2 if all_harness else 0


def cmd_minimize(args: argparse.Namespace) -> int:
    import yaml

    from .cases.loader import load_corpus, select
    from .minimize.ddmin import minimize_case, same_failure_predicate
    from .runner.differential import run_cases

    cfg = paths()
    corpus = load_corpus(cfg.cases, cfg.root)
    case = corpus.by_id().get(args.case)
    if case is None:
        raise _UsageError(f"no valid case with id {args.case!r}")
    out = _out_dir(args)
    target = _make_target(args, out / "work")
    reference = _reference_target()
    original_list = run_cases([case], target, cfg.expected, out, jobs=1)
    original = original_list[0]
    if original.outcome == Outcome.PASS:
        _emit(args, {"case_id": case.id, "outcome": original.outcome.value,
                     "minimized": False}, f"{case.id} passes on {target.name}; nothing to "
                                          "minimize")
        return 1
    source = Path(case.path)
    if not source.is_absolute():
        source = cfg.root / source
    raw = yaml.safe_load(source.read_text(encoding="utf-8"))
    predicate = same_failure_predicate(original, reference, target)
    target.start()
    reference.start()
    try:
        result = minimize_case(raw, predicate, max_runs=args.max_runs)
    finally:
        target.stop()
        reference.stop()
    dest_dir = out / "minimized"
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / f"{case.id}.min.yaml"
    dest.write_text(yaml.safe_dump(result.case_dict, sort_keys=False, allow_unicode=True,
                                   width=1000), encoding="utf-8")
    payload = {"case_id": case.id, "outcome": original.outcome.value, "minimized": True,
               "units_before": result.units_before, "units_after": result.units_after,
               "runs": result.runs, "complete": result.complete, "path": _rel(dest)}
    text = (f"{case.id}: {result.units_before} -> {result.units_after} units in "
            f"{result.runs} run(s){'' if result.complete else ' (budget exhausted)'}\n"
            f"wrote {_rel(dest)}")
    _emit(args, payload, text)
    return 0


# ---------------------------------------------------------------------------------------------
# Handlers: suites
# ---------------------------------------------------------------------------------------------


def cmd_regress(args: argparse.Namespace) -> int:
    from .regress import manifest as regress_manifest
    from .regress import runner as regress_runner

    cfg = paths()
    schedule = regress_manifest.load(cfg.pg_regress_dir)
    out = _out_dir(args)
    if args.record_baseline:
        if not _review_gate():
            return 1
        from .oracle.container import OracleContainer

        ep = OracleContainer().ensure()
        statuses = regress_runner.record_baseline(schedule, cfg.pg_regress_dir, ep.host,
                                                  ep.port, out / "baseline", cfg.expected)
        counts = _outcome_counts(list(statuses.values()))
        payload = {"oracle_official_status": dict(sorted(statuses.items())),
                   "out_dir": _rel(out)}
        _emit(args, payload, f"baseline recorded for {len(statuses)} test(s); out "
                             f"{_rel(out)}\n" + "\n".join(f"  {k}: {v}"
                                                         for k, v in counts.items()))
        return 0
    baseline_dir = cfg.expected / "regress"
    kind = args.target or "pgarbor"
    with _live(args, out / "work", "regress") as live:
        results = regress_runner.run_schedule(
            schedule, cfg.pg_regress_dir, live.endpoint.host, live.endpoint.port, out,
            baseline_dir if baseline_dir.is_dir() else None,
            target_alive=live.alive, use_existing=args.use_existing, max_parallel=20,
            only=_split_list(args.only), restart_target=live.restart)
    counts = _outcome_counts([r.official for r in results])
    payload = {"target": kind, "out_dir": _rel(out), "official_counts": counts,
               "results": [r.to_json() for r in results]}
    lines = [f"regress target {kind}: {len(results)} test(s); out {_rel(out)}"]
    lines += _counts_text(counts)
    _emit(args, payload, "\n".join(lines))
    return 0


def cmd_isolation(args: argparse.Namespace) -> int:
    from .isolation.tester import run_suite

    cfg = paths()
    out = _out_dir(args)
    target = _make_target(args, out / "work")
    target.start()
    try:
        results = run_suite(cfg.pg_isolation_dir, target, out,
                            only=_split_list(args.only), jobs=max(1, args.jobs))
    finally:
        target.stop()
    counts = _outcome_counts([r.outcome for r in results])
    payload = {"target": target.name, "out_dir": _rel(out), "counts": counts,
               "results": [{"test": r.test, "outcome": r.outcome.value, "reason": r.reason,
                            "output_path": r.output_path, "diff_lines": r.diff_lines,
                            "wait_detection": r.wait_detection,
                            "elapsed_ms": r.elapsed_ms} for r in results]}
    lines = [f"isolation target {target.name}: {len(results)} spec(s); out {_rel(out)}"]
    lines += _counts_text(counts)
    _emit(args, payload, "\n".join(lines))
    return 0


# ---------------------------------------------------------------------------------------------
# Handlers: slt / wire
# ---------------------------------------------------------------------------------------------


def cmd_slt_run(args: argparse.Namespace) -> int:
    from .sqllogictest import runner as slt_runner

    rows: list[dict[str, Any]] = []
    work_root = _out_dir(args) / "work"
    for raw in args.paths:
        path = Path(raw).resolve()
        if not path.is_file():
            raise _UsageError(f"no such file: {path}")
        with _live(args, work_root, path.stem) as live:
            res = slt_runner.run(path, live.endpoint)
        rows.append({"path": _rel(path), "outcome": res.outcome.value, "reason": res.reason,
                     "records_total": res.records_total, "records_passed": res.records_passed,
                     "records_skipped": res.records_skipped,
                     "first_failure_line": res.first_failure_line,
                     "details": list(res.details)})
    lines = [f"{r['outcome']:<22} {r['path']}  ({r['records_passed']}/{r['records_total']})"
             + (f" {r['reason']}" if r["reason"] else "") for r in rows]
    _emit(args, {"results": rows}, "\n".join(lines))
    return 0 if all(r["outcome"] == "PASS" for r in rows) else 1


def cmd_slt_record(args: argparse.Namespace) -> int:
    from .oracle.container import OracleContainer
    from .sqllogictest import runner as slt_runner

    if not _review_gate():
        return 1
    cfg = paths()
    slt_root = cfg.root / "tests" / "slt"
    endpoint = OracleContainer().ensure()
    failed = False
    rows: list[dict[str, Any]] = []
    for raw in args.paths:
        src = Path(raw).resolve()
        if not src.is_file():
            raise _UsageError(f"no such file: {src}")
        try:
            rel = src.relative_to(slt_root)
        except ValueError:
            raise _UsageError(f"slt inputs must live under {_rel(slt_root)}/: {src}")
        dest = cfg.expected / "slt" / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        problems = slt_runner.record(src, endpoint, dest)
        failed = failed or bool(problems)
        rows.append({"input": _rel(src), "output": _rel(dest), "problems": list(problems)})
    lines = []
    for row in rows:
        status = "FAIL" if row["problems"] else "ok"
        lines.append(f"{status:<5}{row['output']}")
        lines += [f"      {p}" for p in row["problems"]]
    _emit(args, {"results": rows}, "\n".join(lines))
    return 1 if failed else 0


def cmd_wire_record(args: argparse.Namespace) -> int:
    from .oracle.container import OracleContainer
    from .wire_transcripts.proxy import RecordingProxy

    if not args.client or not set(args.client) <= _CLIENT_LABEL_OK:
        raise _UsageError("--client must be non-empty and use only [A-Za-z0-9._-]")
    if not _review_gate():
        return 1
    cfg = paths()
    out = Path(args.out).resolve() if args.out else cfg.expected / "wire" / f"{args.client}.jsonl"
    out.parent.mkdir(parents=True, exist_ok=True)
    oracle = OracleContainer().ensure()
    proxy = RecordingProxy(listen_port=args.listen_port, upstream_host=oracle.host,
                           upstream_port=oracle.port, out_path=out,
                           client_label=args.client)
    bound = proxy.start()
    _emit(args, {"listen": f"127.0.0.1:{bound}", "out": _rel(out)},
          f"recording proxy listening on 127.0.0.1:{bound}; writing {_rel(out)} "
          "(Ctrl-C to stop)")
    stop_event = threading.Event()
    try:
        while not stop_event.wait(1.0):
            pass
    except KeyboardInterrupt:
        pass
    finally:
        proxy.stop()
    return 0


def cmd_wire_replay(args: argparse.Namespace) -> int:
    from .wire_transcripts.replay import replay

    path = Path(args.path).resolve()
    if not path.is_file():
        raise _UsageError(f"no such file: {path}")
    out = _out_dir(args)
    with _live(args, out / "work", path.stem) as live:
        res = replay(path, live.endpoint.host, live.endpoint.port)
    payload = {"name": res.name, "outcome": res.outcome.value, "reason": res.reason,
               "connection": res.connection, "message_index": res.message_index,
               "expected": res.expected, "actual": res.actual, "notes": list(res.notes)}
    text = f"{res.outcome.value} {res.name}" + (f": {res.reason}" if res.reason else "")
    _emit(args, payload, text)
    return 0 if res.outcome == Outcome.PASS else 1


# ---------------------------------------------------------------------------------------------
# Handlers: generate / selftest / manifest
# ---------------------------------------------------------------------------------------------


def cmd_generate(args: argparse.Namespace) -> int:
    from .generate import seeds

    cfg = paths()
    ref_dir = cfg.root / "contracts" / "reference" / "pg18"
    cases = seeds.GENERATORS[args.generator](args.seed, ref_dir)
    out_root = Path(args.out).resolve() if args.out else cfg.cases
    conflicts = seeds.write_cases(cases, out_root)
    payload = {"generator": args.generator, "seed": args.seed, "count": len(cases),
               "out": _rel(out_root), "conflicts": list(conflicts)}
    lines = [f"{args.generator}@{args.seed}: {len(cases)} case(s) -> {_rel(out_root)}"]
    lines += [f"CONFLICT (not overwritten): {c}" for c in conflicts]
    _emit(args, payload, "\n".join(lines))
    return 1 if conflicts else 0


def cmd_selftest(args: argparse.Namespace) -> int:
    from .selftest import run as selftest_run

    report = selftest_run.run_offline()
    checks = list(report.checks)
    ok = bool(report.ok)
    if args.live:
        from .cases.loader import load_corpus, select
        from .oracle.pool import OraclePool

        cfg = paths()
        cases = select(load_corpus(cfg.cases, cfg.root))
        pool = OraclePool()
        try:
            pool.start()
            live = selftest_run.run_live(pool, cases)
        finally:
            pool.close()
        checks += list(live.checks)
        ok = ok and bool(live.ok)
    payload = {"ok": ok, "checks": [{"name": c.name, "ok": c.ok, "detail": c.detail}
                                    for c in checks]}
    lines = [("ok    " if c.ok else "FAIL  ") + c.name + (f": {c.detail}" if c.detail else "")
             for c in checks]
    lines.append("selftest ok" if ok else "selftest FAILED")
    _emit(args, payload, "\n".join(lines))
    return 0 if ok else 1


def cmd_manifest_freeze(args: argparse.Namespace) -> int:
    from .oracle import fingerprint, manifest
    from .oracle.container import OracleContainer

    if not _review_gate():
        return 1
    _, fp_sha = fingerprint.verify_live(OracleContainer())
    reference = {"image_digest": PG_IMAGE_DIGEST, "server_version": PG_SERVER_VERSION,
                 "fingerprint_sha256": fp_sha}
    doc = manifest.freeze(paths(), reference)
    _emit(args, doc, f"oracle manifest frozen: {manifest.MANIFEST_RELPATH}")
    return 0


def cmd_manifest_check(args: argparse.Namespace) -> int:
    from .oracle import manifest

    res = manifest.check(paths())
    payload = {"ok": res.ok, "message": res.message,
               "reference_changed": res.reference_changed,
               "changes": [{"tree": c.tree, "path": c.path, "change": c.change}
                           for c in res.changes]}
    lines = [res.message or ("manifest ok" if res.ok else "manifest check failed")]
    lines += [f"  {c.change:<8} {c.tree}: {c.path}" for c in res.changes]
    _emit(args, payload, "\n".join(lines))
    return 0 if res.ok else 1


# ---------------------------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------------------------


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Parse args, dispatch, return exit code."""
    parser = build_parser()
    args = parser.parse_args(argv)  # usage errors exit 2 via argparse
    try:
        if args.repo:
            os.environ["PGARBOR_REPO"] = str(Path(args.repo).resolve())
        return int(args.func(args))
    except _UsageError as exc:
        sys.stderr.write(f"usage error: {exc}\n")
        return 2
    except KeyboardInterrupt:
        sys.stderr.write("interrupted\n")
        return 130
    except Exception:  # top-level only: report and exit 4
        traceback.print_exc(file=sys.stderr)
        return 4
