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
from typing import Optional, Sequence


def build_parser() -> argparse.ArgumentParser:
    raise NotImplementedError("FAC-19: argparse definition")


def main(argv: Optional[Sequence[str]] = None) -> int:
    raise NotImplementedError("FAC-19: cli dispatch")
