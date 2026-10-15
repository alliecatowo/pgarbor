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
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


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


def run_campaign(cfg: CampaignConfig) -> CampaignResult:
    raise NotImplementedError("FAC-09: run full evidence campaign")


def run_family(cfg: CampaignConfig, family: str) -> object:
    """Run one phase in isolation (used by the CLI ``campaign --only <family>``); returns the
    records.FamilyEvidence for it."""
    raise NotImplementedError("FAC-09: run one family")
