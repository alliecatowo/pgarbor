"""SWARM-13: evidence pack builder and immutable evidence ledger.

Lease FAC-10.

``build_pack(...)`` assembles records.EvidencePack from campaign outputs:
  * git: gitutil.head_sha / current_branch / is_dirty (a dirty tree is allowed but recorded;
    reports print a warning banner);
  * toolchain: ``rustc -vV`` and ``cargo -V`` stdout verbatim;
  * platform: platform.system(), platform.release(), platform.machine(),
    platform.python_version();
  * reference: from the oracle fingerprint (image digest, server_version, fingerprint sha);
  * corpus: pgarbor_harness.hashing.tree_sha256 over cases (tests/cases/**/*.yaml), expected
    (tests/expected/**), verifier (config.VERIFIER_GLOBS), protected (config.PROTECTED_GLOBS),
    plus sha256 of canonical regress / isolation manifests (None if unavailable);
  * families: FamilyEvidence per phase; ``totals`` = per-key sums; ``critical`` =
    {"corruption": totals CORRUPTION, "crash": totals CRASH};
  * denominators per family: total_cataloged = sum(counts.values()), runnable = total minus
    (BLOCKED + INVALID_CASE + UNRUN), passed = PASS.

``save_pack(pack, run_dir)`` writes ``evidence.json`` with mode 0o444 after writing (immutable
by convention) and appends ``{"campaign_id", "git_sha", "path", "sha256", "totals"}`` to
``runs/state/evidence_ledger.jsonl``; updates ``runs/state/last_evidence.json`` pointer.
Refuses to overwrite an existing evidence.json (FileExistsError).

``family_from_results(name, results, cataloged_ids)``: build FamilyEvidence from
model.CaseResult-like objects (anything with ``.outcome``) — items in ``cataloged_ids`` with
no result count as UNRUN.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Iterable, Optional

from .records import EvidencePack, FamilyEvidence


def family_from_results(name: str, results: Iterable[Any], cataloged_ids: Iterable[str],
                        duration_s: float = 0.0, status: str = "RAN") -> FamilyEvidence:
    raise NotImplementedError("FAC-10: family evidence from results")


def build_pack(repo_root: Path, campaign_id: str, epoch: int, mode: str,
               reference: dict[str, str], seeds: list[int], started_unix: int,
               finished_unix: int, build: dict[str, Any], families: list[FamilyEvidence],
               halt: Optional[dict[str, Any]] = None,
               resource: Optional[dict[str, Any]] = None) -> EvidencePack:
    raise NotImplementedError("FAC-10: build evidence pack")


def save_pack(pack: EvidencePack, run_dir: Path, repo_root: Path) -> Path:
    raise NotImplementedError("FAC-10: save evidence pack + ledger")


def load_last(repo_root: Path) -> Optional[EvidencePack]:
    """Previous campaign's pack via runs/state/last_evidence.json (None if none)."""
    raise NotImplementedError("FAC-10: load last evidence")
