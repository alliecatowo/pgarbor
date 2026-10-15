"""Run reports: markdown + JSON with PASS/FAIL/BLOCKED/UNRUN counts and three denominators.

Lease FAC-13.

``write_reports(pack, clusters, tickets, run_dir)`` writes:

``report.json`` = {"campaign_id", "git_sha", "dirty", "families": [{name, status,
total_cataloged, runnable, passed, counts}], "totals", "critical", "top_clusters": first 20
clusters (key, outcome, size, leases), "tickets": first 50 ticket ids+scores, "halt"}.

``report.md`` (exact section order):
  1. ``# PGArbor campaign <id>`` + line ``git <sha> (dirty!)`` when dirty + reference digest;
  2. a banner when critical.corruption > 0 or critical.crash > 0:
     ``> **STOP THE LINE:** N corruption, M crash failures`` (RFC 002 §2B);
  3. ``## Families`` table: Family | Status | Passed | Runnable | Cataloged | then one column
     per records.COUNT_KEYS except PASS (zeros printed as ``0``, never blank);
     NEVER a single percentage of passed/total without the three denominators (RFC 002 §8);
  4. ``## Totals`` the same columns summed; FAIL is reported as the sum of MISMATCH, CRASH,
     HANG, CORRUPTION, UNEXPECTED_ACCEPTANCE (plus a separate row for UNSUPPORTED, BLOCKED,
     FLAKY, INVALID_CASE, HARNESS_ERROR, UNRUN — none of them is ever folded into PASS);
  5. ``## Top failure clusters`` (20); 6. ``## Next-epoch repair tickets`` (50);
  7. ``## Halt decision``; 8. ``## Provenance`` (toolchain, platform, corpus hashes, seeds).
Markdown table cells escape ``|`` as ``\\|``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .records import EvidencePack


def render_markdown(pack: EvidencePack, clusters: list[Any], tickets: list[Any]) -> str:
    raise NotImplementedError("FAC-13: markdown report")


def render_json(pack: EvidencePack, clusters: list[Any], tickets: list[Any]) -> dict[str, Any]:
    raise NotImplementedError("FAC-13: json report")


def write_reports(pack: EvidencePack, clusters: list[Any], tickets: list[Any],
                  run_dir: Path) -> tuple[Path, Path]:
    """Write report.md and report.json; return their paths."""
    raise NotImplementedError("FAC-13: write reports")
