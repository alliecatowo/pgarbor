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
"""

from __future__ import annotations

import argparse
from typing import Optional, Sequence


def build_parser() -> argparse.ArgumentParser:
    """All subcommands/flags above."""
    raise NotImplementedError("HAR-29: argparse definition")


def main(argv: Optional[Sequence[str]] = None) -> int:
    """Parse args, dispatch, return exit code."""
    raise NotImplementedError("HAR-29: cli dispatch")
