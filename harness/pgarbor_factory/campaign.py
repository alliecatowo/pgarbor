"""Full evidence campaign orchestration (RFC 003 §5 Epoch 3, §6; RFC 002 §6 nightly).

Lease FAC-09 (sonnet: orchestration order, failure isolation, resource accounting).

``run_campaign(cfg)`` — one campaign at one integrated SHA, sequential phases; each phase's
failure is RECORDED and later phases still run unless they depend on it:

  0. id = ``<UTC yyyymmddThhmmssZ>-<sha[:8]>``; run dir ``runs/<id>/``; write
     ``runs/<id>/campaign.json`` (config) first.
  1. guard.check_campaign_preconditions -> refuse (exit 3) when not ok. Nothing else runs.
  2. inventory.inventory -> ``inventory.json``.
  3. build.cargo_build (release profile for the server binary:
     ``cargo build --release -p pgarbor-cli`` after the workspace check) -> ``build.json``.
     Build failure: families 5-9 get status SKIPPED_PRECONDITION with every cataloged item
     counted UNRUN; family 4 still runs only if its crates built (``cargo test -p
     pgarbor-fault`` builds its own deps).
  4. storage fault suite: ``build.cargo_test(packages=["pgarbor-fault", "pgarbor-storage",
     "pgarbor-branches"], extra_args=["--", "--include-ignored"])`` with env
     ``PGARBOR_FAULT_SEEDS=<cfg.fault_seeds>``; any failed test whose name contains
     ``crash``/``durab``/``corrupt``/``isolation`` -> counted CORRUPTION (critical), others
     MISMATCH. Family name "storage-fault".
  5. differential cases: pgarbor_harness.runner.differential.run_cases with a PgarborTarget
     (binary ``target/release/pgarbor``) over the whole corpus (``cfg.case_filter``);
     reference identity from the live oracle fingerprint. Family "differential". Invalid
     corpus entries counted as INVALID_CASE. Cases never selected count UNRUN.
  6. regress: pgarbor_harness.regress.runner.run_schedule -> families "regress-official" and
     "regress-differential".
  7. isolation: pgarbor_harness.isolation.tester.run_suite -> family "isolation".
  8. wire transcripts: replay every ``tests/expected/wire/*.jsonl`` -> family "wire".
  9. client fixtures (``harness/fixtures/<client>/run.sh`` contract: each fixture is an
     UNMODIFIED app; the script receives ``PGARBOR_DSN`` and exits 0 on success; stdout
     JSON lines ``{"test": name, "status": "pass"|"fail", "detail"}``) -> family "clients";
     missing fixtures dir -> family status BLOCKED with reason "no fixtures".
  10. evidence.build_pack + report.write_reports + cluster.cluster_failures +
      repair.generate_tickets (-> ``repair_dag.json``) + halt.decide.
Exit code: 0 campaign completed (even with failures), 3 guard refused, 4 harness error.

Parallelism: phases are sequential (one shared machine); inside phases use cfg.jobs. The PG
oracle container is started once (pool) and reused by all phases. Resource accounting:
``resource.getrusage(RUSAGE_CHILDREN)`` before/after -> evidence ``resource``.

Implementation notes (private): every phase is a module-level ``_phase_*`` function resolved
at call time (monkeypatchable); the family phases return ``list[FamilyEvidence]`` and are run
through ``_guarded`` which converts any exception into HARNESS_ERROR family evidence.
Per-phase errors that are not families (inventory, oracle, cluster, repair, halt, report) are
written to ``phase_errors.json``. Family denominators are computed locally by
``_make_family`` (cataloged items without a result count UNRUN).
"""

from __future__ import annotations

import json
import os
import re
import resource
import subprocess
import time
import traceback
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Iterable, Optional
from urllib.parse import quote

from pgarbor_harness import config
from pgarbor_harness.model import ReferenceIdentity, write_canonical_json

from . import (build, cluster, evidence, gitutil, guard, halt, inventory, leases, report,
               repair)
from .records import EvidencePack, FamilyEvidence, empty_counts


@dataclass
class CampaignConfig:
    repo_root: Path
    epoch: int = 0
    mode: str = "cathedral"                      #: cathedral | seams | conventional
    jobs: int = 4
    fault_seeds: list[int] = field(default_factory=lambda: [1, 2, 3, 42, 4242])
    case_filter: dict[str, list[str]] = field(default_factory=dict)  #: families/features/tiers
    families: tuple[str, ...] = ("storage-fault", "differential", "regress", "isolation",
                                 "wire", "clients")
    pgarbor_binary: str = "target/release/pgarbor"
    time_budget_s: Optional[float] = None        #: halt rule input
    run_dir: Optional[Path] = None               #: set by run_campaign


@dataclass
class CampaignResult:
    campaign_id: str
    run_dir: Path
    exit_code: int
    evidence_path: Optional[Path] = None
    report_path: Optional[Path] = None
    repair_dag_path: Optional[Path] = None


# ---------------------------------------------------------------------------------------------
# Private state and constants
# ---------------------------------------------------------------------------------------------

EXIT_OK = 0
EXIT_GUARD = 3
EXIT_HARNESS = 4

#: Canonical phase order; "regress" yields the two families regress-official/-differential.
_FAMILY_ORDER: tuple[str, ...] = ("storage-fault", "differential", "regress", "isolation",
                                  "wire", "clients")
_REGRESS_FAMILIES: tuple[str, str] = ("regress-official", "regress-differential")
_STORAGE_PACKAGES: tuple[str, ...] = ("pgarbor-fault", "pgarbor-storage", "pgarbor-branches")
#: Crates whose compile errors make the storage-fault suite unrunnable (it builds its own deps).
_STORAGE_CRATES: tuple[str, ...] = _STORAGE_PACKAGES + ("pgarbor-common", "pgarbor-core",
                                                        "pgarbor-types")
_CRITICAL_WORDS: tuple[str, ...] = ("crash", "durab", "corrupt", "isolation")
_FAILURE_OUTCOMES: frozenset[str] = frozenset({
    "MISMATCH", "CRASH", "HANG", "CORRUPTION", "UNEXPECTED_ACCEPTANCE", "UNSUPPORTED", "FLAKY"})
_CLIENT_TIMEOUT_S = 600.0
_RELEASE_BUILD_TIMEOUT_S = 3600.0
_MAX_BUILD_FAILURE_ITEMS = 2000


@dataclass
class _Ctx:
    """Mutable campaign state shared by the phases."""

    repo_root: Path
    paths: config.Paths
    run_dir: Path
    campaign_id: str
    sha: str
    build_report: Any = None                     #: build.BuildReport or None
    build_ok: bool = True
    build_dict: dict[str, Any] = field(default_factory=dict)
    reference: Optional[ReferenceIdentity] = None
    pool: Any = None
    failures: list[Any] = field(default_factory=list)   #: cluster.FailureItem
    errors: list[dict[str, str]] = field(default_factory=list)


def _record_error(ctx: _Ctx, phase: str, exc: BaseException) -> str:
    text = f"{type(exc).__name__}: {exc}"
    ctx.errors.append({"phase": phase, "error": text,
                       "traceback": "".join(traceback.format_exception(exc))[-4000:]})
    return text


def _ov(outcome: Any) -> str:
    """Outcome / str -> its string value."""
    return outcome.value if isinstance(outcome, Enum) else str(outcome)


def _make_family(name: str, status: str, outcomes: Iterable[Any], cataloged: int,
                 duration_s: float = 0.0, artifacts: Optional[list[str]] = None,
                 notes: Optional[list[str]] = None) -> FamilyEvidence:
    """FamilyEvidence with the three RFC 002 §8 denominators: ``cataloged`` items without an
    outcome count UNRUN; total_cataloged is the sum of all counts; runnable excludes BLOCKED,
    INVALID_CASE and UNRUN; passed is PASS."""
    counts = empty_counts()
    ran = 0
    for o in outcomes:
        v = _ov(o)
        counts[v if v in counts else "HARNESS_ERROR"] += 1
        ran += 1
    counts["UNRUN"] += max(0, cataloged - ran)
    total = sum(counts.values())
    runnable = total - counts["BLOCKED"] - counts["INVALID_CASE"] - counts["UNRUN"]
    return FamilyEvidence(name=name, status=status, duration_s=round(duration_s, 3),
                          counts=counts, total_cataloged=total, runnable=runnable,
                          passed=counts["PASS"], artifacts=list(artifacts or []),
                          notes=list(notes or []))


def _status_of(outcomes: list[str]) -> str:
    """RAN, or BLOCKED when every item was BLOCKED (the family could not run at all)."""
    if outcomes and all(o == "BLOCKED" for o in outcomes):
        return "BLOCKED"
    return "RAN"


def _skipped(name: str, cataloged: int, reason: str) -> FamilyEvidence:
    return _make_family(name, "SKIPPED_PRECONDITION", [], cataloged, notes=[reason])


def _write_items(ctx: _Ctx, name: str, items: list[dict[str, Any]]) -> str:
    """Write per-item outcomes of a family; return the run-dir-relative path."""
    rel = f"families/{name}.items.json"
    out = ctx.run_dir / rel
    out.parent.mkdir(parents=True, exist_ok=True)
    write_canonical_json(out, {"family": name, "items": items})
    return rel


def _template(msg: str) -> str:
    msg = re.sub(r"`[^`]*`", "`?`", msg)
    return re.sub(r"\d+", "N", msg)


def _failure(**kw: Any) -> Any:
    return cluster.FailureItem(**kw)


# ---------------------------------------------------------------------------------------------
# Catalogs (what a family WOULD run; used for UNRUN accounting)
# ---------------------------------------------------------------------------------------------


def _differential_corpus(cfg: CampaignConfig, paths: config.Paths) -> tuple[Any, list[Any]]:
    from pgarbor_harness.cases import loader

    corpus = loader.load_corpus(paths.cases, cfg.repo_root)
    flt = cfg.case_filter or {}
    globs = flt.get("ids") or flt.get("id_glob") or []
    cases = loader.select(
        corpus, families=flt.get("families", []), features=flt.get("features", []),
        tiers=flt.get("tiers", []), id_glob=(globs[0] if globs else None),
        include_quarantined=True)
    return corpus, cases


def _catalog_count(cfg: CampaignConfig, key: str) -> int:
    """Number of items the family key would run (best effort)."""
    paths = config.Paths(root=cfg.repo_root)
    if key == "differential":
        from pgarbor_harness.cases import loader

        corpus = loader.load_corpus(paths.cases, cfg.repo_root)
        return len(corpus.cases) + len(corpus.invalid)
    if key in ("regress", *_REGRESS_FAMILIES):
        from pgarbor_harness.regress import manifest as rmanifest

        return len(rmanifest.load(paths.pg_regress_dir).ordered_tests())
    if key == "isolation":
        from pgarbor_harness.isolation import spec_parser

        text = (paths.pg_isolation_dir / "isolation_schedule").read_text(encoding="utf-8")
        return len(spec_parser.parse_schedule(text))
    if key == "wire":
        return len(list((paths.expected / "wire").glob("*.jsonl")))
    if key == "clients":
        fx = cfg.repo_root / "harness" / "fixtures"
        return len([d for d in fx.iterdir() if (d / "run.sh").is_file()]) if fx.is_dir() else 0
    if key == "storage-fault":
        n = 0
        for pkg in _STORAGE_PACKAGES:
            for f in (cfg.repo_root / "crates" / pkg).rglob("*.rs"):
                text = f.read_text(encoding="utf-8", errors="replace")
                n += len(re.findall(r"#\[(?:tokio::)?test\b", text))
        return n
    return 0


def _catalog(cfg: CampaignConfig, key: str) -> int:
    try:
        return _catalog_count(cfg, key)
    except Exception:
        return 0


# ---------------------------------------------------------------------------------------------
# Targets
# ---------------------------------------------------------------------------------------------


def _make_target(cfg: CampaignConfig, ctx: _Ctx, sub: str) -> Any:
    """A PgarborTarget for the release binary with its own work dir."""
    from pgarbor_harness.runner.targets import PgarborCommand, PgarborTarget

    binary = Path(cfg.pgarbor_binary)
    if not binary.is_absolute():
        binary = cfg.repo_root / binary
    return PgarborTarget(work_dir=ctx.run_dir / "work" / sub,
                         command=PgarborCommand(binary=str(binary)),
                         name=f"pgarbor@{ctx.sha[:8]}")


def _dsn(endpoint: Any) -> str:
    auth = quote(endpoint.user, safe="")
    if endpoint.password:
        auth += ":" + quote(endpoint.password, safe="")
    return f"postgresql://{auth}@{endpoint.host}:{endpoint.port}/{endpoint.dbname}"


# ---------------------------------------------------------------------------------------------
# Phases. Each takes (cfg[, ctx]); the family phases return a list of FamilyEvidence.
# ---------------------------------------------------------------------------------------------


def _phase_guard(cfg: CampaignConfig) -> Any:
    """Phase 1: guard.check_campaign_preconditions -> GuardReport."""
    return guard.check_campaign_preconditions(cfg.repo_root)


def _phase_inventory(cfg: CampaignConfig, ctx: _Ctx) -> None:
    """Phase 2: inventory.json."""
    doc = inventory.inventory(cfg.repo_root)
    write_canonical_json(ctx.run_dir / "inventory.json", doc)


def _release_build(cfg: CampaignConfig, ctx: _Ctx, target_dir: Path) -> dict[str, Any]:
    """``cargo build --release -p pgarbor-cli`` (the differential server binary)."""
    log = ctx.run_dir / "build" / "release.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir), CARGO_TERM_COLOR="never")
    t0 = time.monotonic()
    try:
        p = subprocess.run(["cargo", "build", "--release", "--offline", "-p", "pgarbor-cli"],
                           cwd=str(cfg.repo_root), env=env, capture_output=True, text=True,
                           timeout=_RELEASE_BUILD_TIMEOUT_S, check=False)
        log.write_text(p.stdout + "\n" + p.stderr, encoding="utf-8")
        ok, code = p.returncode == 0, p.returncode
    except (OSError, subprocess.TimeoutExpired) as e:
        log.write_text(f"{type(e).__name__}: {e}\n", encoding="utf-8")
        ok, code = False, -1
    binary = Path(cfg.pgarbor_binary)
    if not binary.is_absolute():
        binary = cfg.repo_root / binary
    if ok and not binary.is_file():
        ok = False
    return {"ok": ok, "returncode": code, "duration_s": round(time.monotonic() - t0, 3),
            "log": str(log), "binary": str(binary)}


def _phase_build(cfg: CampaignConfig, ctx: _Ctx) -> None:
    """Phase 3: workspace check build, then the release server binary; sets ctx.build_*."""
    specs = leases.load_catalog(ctx.paths.leases_dir)
    owner = leases.file_owner_map(specs)
    target_dir = cfg.repo_root / "target"
    rep = build.cargo_build(cfg.repo_root, target_dir, owner, ctx.run_dir / "build")
    ctx.build_report = rep
    doc = rep.to_json()
    ok = bool(rep.ok)
    duration = float(rep.duration_s)
    if ok:
        rel = _release_build(cfg, ctx, target_dir)
        doc["release"] = rel
        ok = bool(rel["ok"])
        duration += float(rel["duration_s"])
    ctx.build_ok = ok
    write_canonical_json(ctx.run_dir / "build.json", doc)
    ctx.build_dict = {"ok": ok, "duration_s": duration, "error_clusters": len(rep.clusters),
                      "log": rep.log_path or ""}
    for d in rep.errors[:_MAX_BUILD_FAILURE_ITEMS]:
        ctx.failures.append(_failure(
            family="build", item_id=f"{d.file}:{d.line}:{d.code}", outcome="BUILD",
            sqlstate=d.code, message_template=_template(d.message),
            files=[d.file] if d.file else [], lease_ids=[d.lease_id] if d.lease_id else []))


def _phase_oracle(cfg: CampaignConfig, ctx: _Ctx) -> None:
    """Start the oracle pool once and take the live reference identity."""
    from pgarbor_harness.oracle import fingerprint
    from pgarbor_harness.oracle.pool import OraclePool

    pool = OraclePool(max_parallel=max(1, cfg.jobs))
    pool.start()
    ctx.pool = pool
    doc, sha = fingerprint.verify_live(pool.container)
    ctx.reference = ReferenceIdentity(str(doc["image_digest"]), str(doc["server_version"]), sha)


def _phase_storage_fault(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 4: storage fault suite."""
    t0 = time.monotonic()
    prev = os.environ.get("PGARBOR_FAULT_SEEDS")
    os.environ["PGARBOR_FAULT_SEEDS"] = ",".join(str(s) for s in cfg.fault_seeds)
    try:
        runs = build.cargo_test(cfg.repo_root, cfg.repo_root / "target", list(_STORAGE_PACKAGES),
                                extra_args=["--", "--include-ignored"],
                                log_dir=ctx.run_dir / "storage-fault")
    finally:
        if prev is None:
            os.environ.pop("PGARBOR_FAULT_SEEDS", None)
        else:
            os.environ["PGARBOR_FAULT_SEEDS"] = prev
    outcomes: list[str] = []
    items: list[dict[str, Any]] = []
    notes: list[str] = [f"seeds={list(cfg.fault_seeds)}"]
    cataloged = 0
    for r in runs:
        cataloged += len(r.passed) + len(r.failed) + len(r.ignored)
        for n in r.passed:
            outcomes.append("PASS")
            items.append({"id": f"{r.package}::{n}", "outcome": "PASS"})
        for n in r.ignored:
            # Cataloged but not executed: visible as UNRUN, never folded into PASS.
            outcomes.append("UNRUN")
            items.append({"id": f"{r.package}::{n}", "outcome": "UNRUN", "reason": "ignored"})
        for n in r.failed:
            low = n.lower()
            o = "CORRUPTION" if any(w in low for w in _CRITICAL_WORDS) else "MISMATCH"
            outcomes.append(o)
            items.append({"id": f"{r.package}::{n}", "outcome": o})
            ctx.failures.append(_failure(
                family="storage-fault", item_id=f"{r.package}::{n}", outcome=o,
                message_template=_template(n), diff_kind="fault_test"))
        if r.status == "timeout":
            outcomes.append("HANG")
            items.append({"id": r.package, "outcome": "HANG", "reason": "timeout"})
            notes.append(f"{r.package}: timeout")
            ctx.failures.append(_failure(family="storage-fault", item_id=r.package,
                                         outcome="HANG", message_template="timeout"))
        elif r.status == "build_failed":
            outcomes.append("BLOCKED")
            items.append({"id": r.package, "outcome": "BLOCKED", "reason": "build_failed"})
            notes.append(f"{r.package}: build_failed")
    art = _write_items(ctx, "storage-fault", items)
    return [_make_family("storage-fault", _status_of(outcomes), outcomes, cataloged,
                         time.monotonic() - t0, [art], notes)]


def _phase_differential(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 5: differential cases against the release binary."""
    from pgarbor_harness.runner import differential

    t0 = time.monotonic()
    corpus, cases = _differential_corpus(cfg, ctx.paths)
    out_dir = ctx.run_dir / "differential"
    out_dir.mkdir(parents=True, exist_ok=True)
    target = _make_target(cfg, ctx, "differential")
    results = list(differential.run_cases(cases, target, ctx.paths.expected, out_dir,
                                          live_reference=ctx.reference, jobs=cfg.jobs))
    results += list(differential.invalid_results(corpus.invalid))
    steps = {c.id: len(c.steps) for c in cases}
    outcomes = [_ov(r.outcome) for r in results]
    items = []
    for r, o in zip(results, outcomes):
        items.append({"id": r.case_id, "outcome": o, "reason": r.reason})
        if o in _FAILURE_OUTCOMES:
            ctx.failures.append(cluster.from_case_result(r, steps=steps.get(r.case_id, 0)))
    notes = []
    if ctx.reference is None:
        notes.append("live oracle reference unavailable; expected files not fingerprint-checked")
    art = _write_items(ctx, "differential", items)
    return [_make_family("differential", _status_of(outcomes), outcomes,
                         len(corpus.cases) + len(corpus.invalid), time.monotonic() - t0,
                         [art, "differential/results.jsonl"], notes)]


def _phase_regress(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 6: regress schedule -> regress-official and regress-differential."""
    from pgarbor_harness.regress import manifest as rmanifest
    from pgarbor_harness.regress import runner as rrunner

    t0 = time.monotonic()
    schedule = rmanifest.load(ctx.paths.pg_regress_dir)
    target = _make_target(cfg, ctx, "regress")
    out_dir = ctx.run_dir / "regress" / ctx.sha[:8]
    target.start()
    try:
        env = target.provision("regress")
        try:
            results = rrunner.run_schedule(
                schedule, ctx.paths.pg_regress_dir, env.endpoint.host, env.endpoint.port,
                out_dir, baseline_dir=ctx.paths.expected / "regress",
                target_alive=lambda: target.is_alive(env), use_existing=False)
        finally:
            target.release(env)
    finally:
        target.stop()
    official = [_ov(r.official) for r in results]
    differ = [_ov(r.differential) if r.differential is not None else _ov(r.official)
              for r in results]
    for r, o in zip(results, official):
        if o in _FAILURE_OUTCOMES:
            ctx.failures.append(_failure(
                family="regress", item_id=r.test, outcome=o, message_template=r.reason,
                diff_kind="regress_diff"))
    n = len(schedule.ordered_tests())
    dur = time.monotonic() - t0
    fams = []
    for name, outs in zip(_REGRESS_FAMILIES, (official, differ)):
        items = [{"id": r.test, "outcome": o, "reason": r.reason}
                 for r, o in zip(results, outs)]
        fams.append(_make_family(name, _status_of(outs), outs, n, dur,
                                 [_write_items(ctx, name, items)]))
    return fams


def _phase_isolation(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 7: isolation suite."""
    from pgarbor_harness.isolation import spec_parser, tester

    t0 = time.monotonic()
    target = _make_target(cfg, ctx, "isolation")
    target.start()
    try:
        results = tester.run_suite(ctx.paths.pg_isolation_dir, target,
                                   ctx.run_dir / "isolation", jobs=cfg.jobs)
    finally:
        target.stop()
    outcomes = [_ov(r.outcome) for r in results]
    items = []
    for r, o in zip(results, outcomes):
        items.append({"id": r.test, "outcome": o, "reason": r.reason})
        if o in _FAILURE_OUTCOMES:
            ctx.failures.append(_failure(family="isolation", item_id=r.test, outcome=o,
                                         message_template=r.reason, diff_kind="isolation_diff"))
    text = (ctx.paths.pg_isolation_dir / "isolation_schedule").read_text(encoding="utf-8")
    n = len(spec_parser.parse_schedule(text))
    return [_make_family("isolation", _status_of(outcomes), outcomes, n,
                         time.monotonic() - t0, [_write_items(ctx, "isolation", items)])]


def _phase_wire(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 8: replay every tests/expected/wire/*.jsonl."""
    from pgarbor_harness.wire_transcripts import replay

    t0 = time.monotonic()
    files = sorted((ctx.paths.expected / "wire").glob("*.jsonl"))
    if not files:
        return [_make_family("wire", "BLOCKED", [], 0, 0.0, notes=["no wire transcripts"])]
    target = _make_target(cfg, ctx, "wire")
    target.start()
    outcomes: list[str] = []
    items: list[dict[str, Any]] = []
    try:
        for f in files:
            try:
                env = target.provision(f.stem)
            except Exception as e:  # the engine cannot start for this transcript
                o, reason = "BLOCKED", f"target_start:{type(e).__name__}"
            else:
                try:
                    r = replay.replay(f, env.endpoint.host, env.endpoint.port)
                    o, reason = _ov(r.outcome), r.reason
                except Exception as e:
                    o, reason = "HARNESS_ERROR", f"exception:{type(e).__name__}"
                finally:
                    target.release(env)
            outcomes.append(o)
            items.append({"id": f.stem, "outcome": o, "reason": reason})
            if o in _FAILURE_OUTCOMES:
                ctx.failures.append(_failure(family="wire", item_id=f.stem, outcome=o,
                                             message_template=reason, diff_kind="wire_diff"))
    finally:
        target.stop()
    return [_make_family("wire", _status_of(outcomes), outcomes, len(files),
                         time.monotonic() - t0, [_write_items(ctx, "wire", items)])]


def _parse_client_lines(stdout: str) -> list[tuple[str, str, str]]:
    """(test, status, detail) from the fixture's JSON stdout lines; other lines ignored."""
    res = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if isinstance(d, dict) and "test" in d and "status" in d:
            res.append((str(d["test"]), str(d["status"]), str(d.get("detail", ""))))
    return res


def _phase_clients(cfg: CampaignConfig, ctx: _Ctx) -> list[FamilyEvidence]:
    """Phase 9: client fixtures (``harness/fixtures/<client>/run.sh``)."""
    t0 = time.monotonic()
    fx = cfg.repo_root / "harness" / "fixtures"
    fixtures = sorted(d for d in fx.iterdir() if (d / "run.sh").is_file()) if fx.is_dir() else []
    if not fixtures:
        return [_make_family("clients", "BLOCKED", [], 0, 0.0, notes=["no fixtures"])]
    target = _make_target(cfg, ctx, "clients")
    target.start()
    outcomes: list[str] = []
    items: list[dict[str, Any]] = []

    def add(item_id: str, o: str, reason: str = "") -> None:
        outcomes.append(o)
        items.append({"id": item_id, "outcome": o, "reason": reason})
        if o in _FAILURE_OUTCOMES:
            ctx.failures.append(_failure(family="clients", item_id=item_id, outcome=o,
                                         message_template=_template(reason),
                                         diff_kind="client_fixture"))

    try:
        for d in fixtures:
            try:
                env = target.provision(f"client-{d.name}")
            except Exception as e:
                add(d.name, "BLOCKED", f"target_start:{type(e).__name__}")
                continue
            try:
                p = subprocess.run(["bash", str(d / "run.sh")], cwd=str(d),
                                   env=dict(os.environ, PGARBOR_DSN=_dsn(env.endpoint)),
                                   capture_output=True, text=True, timeout=_CLIENT_TIMEOUT_S,
                                   check=False)
            except subprocess.TimeoutExpired:
                add(d.name, "HANG", "timeout")
            except OSError as e:
                add(d.name, "HARNESS_ERROR", f"exception:{type(e).__name__}")
            else:
                lines = _parse_client_lines(p.stdout)
                for test, status, detail in lines:
                    add(f"{d.name}::{test}", "PASS" if status == "pass" else "MISMATCH", detail)
                if p.returncode != 0 and not any(s != "pass" for _, s, _ in lines):
                    add(d.name, "MISMATCH", f"exit {p.returncode}: {p.stderr.strip()[-200:]}")
                elif not lines and p.returncode == 0:
                    add(d.name, "PASS")
            finally:
                target.release(env)
    finally:
        target.stop()
    return [_make_family("clients", _status_of(outcomes), outcomes, len(fixtures),
                         time.monotonic() - t0, [_write_items(ctx, "clients", items)])]


# ---------------------------------------------------------------------------------------------
# Phase 10: evidence, reports, clusters, repair, halt
# ---------------------------------------------------------------------------------------------


def _load_history(repo_root: Path, limit: int = 50) -> tuple[list[EvidencePack], list[list[str]]]:
    """Earlier packs from the ledger (oldest first) and their cluster keys."""
    ledger = config.Paths(root=repo_root).state / "evidence_ledger.jsonl"
    packs: list[EvidencePack] = []
    keys: list[list[str]] = []
    if not ledger.is_file():
        return packs, keys
    for line in ledger.read_text(encoding="utf-8").splitlines()[-limit:]:
        try:
            p = Path(json.loads(line)["path"])
            if not p.is_absolute():
                p = repo_root / p
            pack = EvidencePack.from_json(json.loads(p.read_text(encoding="utf-8")))
        except (ValueError, KeyError, TypeError, OSError):
            continue
        packs.append(pack)
        ck: list[str] = []
        try:
            doc = json.loads((p.parent / "clusters.json").read_text(encoding="utf-8"))
            if isinstance(doc, dict):
                doc = doc.get("clusters", [])
            ck = [str(c["key"]) for c in doc if isinstance(c, dict) and "key" in c]
        except (ValueError, OSError, TypeError):
            pass
        keys.append(ck)
    return packs, keys


def _reference_dict(ctx: _Ctx) -> dict[str, str]:
    if ctx.reference is not None:
        return ctx.reference.to_json()
    return {"image_digest": config.PG_IMAGE_DIGEST, "server_version": config.PG_SERVER_VERSION,
            "fingerprint_sha256": "unverified"}


def _finalize(cfg: CampaignConfig, ctx: _Ctx, families: list[FamilyEvidence],
              started_unix: int, wall_t0: float, ru0: Any) -> CampaignResult:
    """Phase 10. Evidence build/save failure -> exit 4; the other steps degrade."""
    run_dir = ctx.run_dir
    res = CampaignResult(ctx.campaign_id, run_dir, EXIT_OK)
    ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    res_doc = {
        "cpu_s": round((ru1.ru_utime + ru1.ru_stime) - (ru0.ru_utime + ru0.ru_stime), 3),
        "wall_s": round(time.monotonic() - wall_t0, 3),
        "max_rss_mb": round(ru1.ru_maxrss / 1024.0, 1),
    }
    build_doc = ctx.build_dict or {"ok": ctx.build_ok, "duration_s": 0.0, "error_clusters": 0,
                                   "log": ""}
    try:
        pack = evidence.build_pack(
            cfg.repo_root, ctx.campaign_id, cfg.epoch, cfg.mode, _reference_dict(ctx),
            list(cfg.fault_seeds), started_unix, int(time.time()), build_doc, families,
            halt=None, resource=res_doc)
    except Exception as e:
        _record_error(ctx, "evidence.build_pack", e)
        res.exit_code = EXIT_HARNESS
        return res

    clusters: list[Any] = []
    tickets: list[Any] = []
    try:
        clusters = list(cluster.cluster_failures(ctx.failures))
        cluster.write_clusters(clusters, run_dir / "clusters.json")
    except Exception as e:
        _record_error(ctx, "cluster", e)
    try:
        paths = ctx.paths
        graph = json.loads(paths.dag.read_text(encoding="utf-8"))
        specs = leases.load_catalog(paths.leases_dir)
        tickets = list(repair.generate_tickets(clusters, graph, specs, cfg.epoch))
        dag_path = run_dir / "repair_dag.json"
        repair.write_repair_dag(tickets, dag_path, ctx.campaign_id, cfg.epoch)
        res.repair_dag_path = dag_path
    except Exception as e:
        _record_error(ctx, "repair", e)
    try:
        history, hist_keys = _load_history(cfg.repo_root)
        cur_keys = [str(getattr(c, "key", "")) for c in clusters]
        decision = halt.decide(history + [pack], halt.Budget(max_wall_s=cfg.time_budget_s),
                               hist_keys + [cur_keys])
        pack.halt = decision.to_json()
    except Exception as e:
        _record_error(ctx, "halt", e)
    try:
        res.evidence_path = evidence.save_pack(pack, run_dir, cfg.repo_root)
    except Exception as e:
        _record_error(ctx, "evidence.save_pack", e)
        res.exit_code = EXIT_HARNESS
        return res
    try:
        md, _js = report.write_reports(pack, clusters, tickets, run_dir)
        res.report_path = md
    except Exception as e:
        _record_error(ctx, "report", e)
    return res


# ---------------------------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------------------------


def _wants(cfg: CampaignConfig, key: str) -> bool:
    names = set(cfg.families)
    if key == "regress":
        return bool(names & {"regress", *_REGRESS_FAMILIES})
    return key in names


def _family_names(key: str) -> tuple[str, ...]:
    return _REGRESS_FAMILIES if key == "regress" else (key,)


def _phase_table() -> dict[str, Callable[[CampaignConfig, _Ctx], list[FamilyEvidence]]]:
    # Resolved at call time so that tests can monkeypatch the module-level phase functions.
    return {"storage-fault": _phase_storage_fault, "differential": _phase_differential,
            "regress": _phase_regress, "isolation": _phase_isolation, "wire": _phase_wire,
            "clients": _phase_clients}


def _guarded(cfg: CampaignConfig, ctx: _Ctx, key: str) -> list[FamilyEvidence]:
    """Run one family phase; an exception becomes HARNESS_ERROR family evidence."""
    t0 = time.monotonic()
    try:
        return list(_phase_table()[key](cfg, ctx))
    except Exception as e:
        text = _record_error(ctx, key, e)
        n = _catalog(cfg, key)
        fams = []
        for name in _family_names(key):
            f = _make_family(name, "HARNESS_ERROR", [], n, time.monotonic() - t0,
                             notes=[text])
            if n == 0:
                f.counts["HARNESS_ERROR"] = 1
                f.total_cataloged = 1
                f.runnable = 1
            fams.append(f)
        return fams


def _storage_buildable(ctx: _Ctx) -> bool:
    """Family 4 still runs after a build failure unless its own crates failed to compile."""
    rep = ctx.build_report
    if rep is None:
        return True
    for d in getattr(rep, "errors", []):
        if any(c in (d.crate or "") for c in _STORAGE_CRATES):
            return False
    return True


def _new_campaign_id(sha: str, now: float) -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(now)) + "-" + sha[:8]


def _head_sha(repo_root: Path) -> str:
    try:
        return gitutil.head_sha(repo_root)
    except Exception:
        return "unknown0"


def _cfg_doc(cfg: CampaignConfig, campaign_id: str, sha: str, started: int) -> dict[str, Any]:
    return {"campaign_id": campaign_id, "git_sha": sha, "started_unix": started,
            "repo_root": str(cfg.repo_root), "epoch": cfg.epoch, "mode": cfg.mode,
            "jobs": cfg.jobs, "fault_seeds": list(cfg.fault_seeds),
            "case_filter": {k: list(v) for k, v in cfg.case_filter.items()},
            "families": list(cfg.families), "pgarbor_binary": cfg.pgarbor_binary,
            "time_budget_s": cfg.time_budget_s}


def _close_pool(ctx: _Ctx) -> None:
    if ctx.pool is not None:
        try:
            ctx.pool.close()
        except Exception as e:
            _record_error(ctx, "oracle.close", e)


def run_campaign(cfg: CampaignConfig) -> CampaignResult:
    started = int(time.time())
    wall_t0 = time.monotonic()
    ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    # Phase 0
    sha = _head_sha(cfg.repo_root)
    cid = _new_campaign_id(sha, started)
    paths = config.Paths(root=cfg.repo_root)
    run_dir = paths.runs / cid
    run_dir.mkdir(parents=True, exist_ok=True)
    cfg.run_dir = run_dir
    ctx = _Ctx(repo_root=cfg.repo_root, paths=paths, run_dir=run_dir, campaign_id=cid, sha=sha)
    write_canonical_json(run_dir / "campaign.json", _cfg_doc(cfg, cid, sha, started))

    def finish(res: CampaignResult) -> CampaignResult:
        if ctx.errors:
            write_canonical_json(run_dir / "phase_errors.json", {"errors": ctx.errors})
        return res

    # Phase 1: guard. Refusal (or inability to check) runs nothing else.
    try:
        gate = _phase_guard(cfg)
    except Exception as e:
        _record_error(ctx, "guard", e)
        return finish(CampaignResult(cid, run_dir, EXIT_HARNESS))
    try:
        write_canonical_json(run_dir / "guard.json", gate.to_json())
    except Exception as e:
        _record_error(ctx, "guard.write", e)
    if not gate.ok:
        return finish(CampaignResult(cid, run_dir, EXIT_GUARD))

    # Phase 2
    try:
        _phase_inventory(cfg, ctx)
    except Exception as e:
        _record_error(ctx, "inventory", e)
    # Phase 3
    try:
        _phase_build(cfg, ctx)
    except Exception as e:
        ctx.build_ok = False
        ctx.build_dict = {"ok": False, "duration_s": 0.0, "error_clusters": 0, "log": ""}
        _record_error(ctx, "build", e)
    # Oracle pool, once, before the SQL families.
    if ctx.build_ok and _wants(cfg, "differential"):
        try:
            _phase_oracle(cfg, ctx)
        except Exception as e:
            _record_error(ctx, "oracle", e)

    families: list[FamilyEvidence] = []
    try:
        for key in _FAMILY_ORDER:
            if not _wants(cfg, key):
                continue
            if not ctx.build_ok and (key != "storage-fault" or not _storage_buildable(ctx)):
                for name in _family_names(key):
                    families.append(_skipped(name, _catalog(cfg, key), "build failed"))
                continue
            families.extend(_guarded(cfg, ctx, key))
    finally:
        _close_pool(ctx)

    try:
        res = _finalize(cfg, ctx, families, started, wall_t0, ru0)
    except Exception as e:
        _record_error(ctx, "finalize", e)
        res = CampaignResult(cid, run_dir, EXIT_HARNESS)
    return finish(res)


def run_family(cfg: CampaignConfig, family: str) -> object:
    """Run one phase in isolation (used by the CLI ``campaign --only <family>``); returns the
    records.FamilyEvidence for it. ``regress`` returns regress-official (ask for
    ``regress-differential`` to get the other verdict). The release binary is assumed built."""
    key = "regress" if family in _REGRESS_FAMILIES else family
    if key not in _FAMILY_ORDER:
        raise ValueError(f"unknown family {family!r}; expected one of "
                         f"{', '.join(_FAMILY_ORDER)} or {', '.join(_REGRESS_FAMILIES)}")
    now = time.time()
    sha = _head_sha(cfg.repo_root)
    cid = _new_campaign_id(sha, now)
    paths = config.Paths(root=cfg.repo_root)
    run_dir = cfg.run_dir or (paths.runs / f"{cid}-{family}")
    run_dir.mkdir(parents=True, exist_ok=True)
    cfg.run_dir = run_dir
    ctx = _Ctx(repo_root=cfg.repo_root, paths=paths, run_dir=run_dir, campaign_id=cid, sha=sha)
    try:
        if key == "differential":
            try:
                _phase_oracle(cfg, ctx)
            except Exception as e:
                _record_error(ctx, "oracle", e)
        fams = _guarded(cfg, ctx, key)
    finally:
        _close_pool(ctx)
        if ctx.errors:
            write_canonical_json(run_dir / "phase_errors.json", {"errors": ctx.errors})
    for f in fams:
        if f.name == family:
            return f
    return fams[0]
