"""SWARM-08: deterministic bulk merge assembler with path-permission enforcement.

Lease FAC-06 (sonnet: git plumbing, conflict attribution, determinism).

``assemble_epoch(repo_root, epoch, base_sha)``:
  1. worktree = worktrees.allocate_assembly(epoch, base_sha) (branch ``epoch/<NN>-assembly``);
  2. candidates = submissions in ledger state PATCH_SUBMITTED, ordered by
     (lease crate order per ARCHITECTURE §1 dependency DAG — common, storage, fault, branches,
     types, catalog, core, sql, plan, exec, pgarbor, wire, cli, then harness — then lease_id);
     this order is the ONLY ordering input (deterministic: same inputs -> same epoch SHA,
     given fixed commit metadata, see 5);
  3. per candidate, ``check_patch``:
       * every changed path must be in the lease's ``files`` (records.LeaseSpec.files);
         else reject ``unleased_path``;
       * no changed path may match pgarbor_harness.config.PROTECTED_GLOBS (``contracts/**``,
         ``harness/oracle/**``, ``tests/expected/**``) -> reject ``protected_path``;
       * non-harness leases (prefix not HAR-/FAC-) touching config.VERIFIER_GLOBS -> reject
         ``verifier_path``; Cargo.toml / lib.rs / mod.rs / __init__.py / __main__.py ->
         reject ``structural_file``;
       * deleted files -> reject ``deletion`` (skeleton files are contracts);
       * the submission's base_sha must be an ancestor of (or equal to) the epoch base_sha
         (``git merge-base --is-ancestor``) -> else reject ``stale_base``;
       * static review (review.review_patch) findings with severity "reject" -> reject
         ``review:<rule>``.
  4. apply: squash the lease's diff ``git diff base_sha head_sha -- <its files>`` and apply
     with ``git apply --index --3way`` in the assembly worktree; conflict -> reject
     ``conflict`` with the conflicting paths and the lease that last touched them in this
     epoch (attribution); then commit with fixed metadata:
     author/committer ``PGArbor Assembler <assembler@pgarbor.invalid>`` and fixed dates via
     env ``GIT_AUTHOR_DATE``/``GIT_COMMITTER_DATE`` = ``@<epoch*86400 + index> +0000``
     (index = 0-based position among accepted patches), message
     ``assemble <LEASE-ID> <head_sha[:12]>``. Fixed metadata makes the epoch SHA a pure
     function of base_sha + accepted patches;
  5. ledger: accepted -> ASSEMBLED; rejected -> REJECTED with reason; record every decision
     in ``runs/epoch-<NN>/assembly.json``: {"base_sha", "epoch_sha", "accepted": [ids],
     "rejected": [{"lease_id", "reason", "paths"}]}.
A rejected patch is never partially applied.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .records import LeaseSpec, Submission

#: Crate assembly order (dependency order from ARCHITECTURE §1), harness last.
CRATE_ORDER: tuple[str, ...] = (
    "pgarbor-common", "pgarbor-storage", "pgarbor-fault", "pgarbor-branches", "pgarbor-types",
    "pgarbor-catalog", "pgarbor-core", "pgarbor-sql", "pgarbor-plan", "pgarbor-exec",
    "pgarbor", "pgarbor-wire", "pgarbor-cli", "harness",
)


@dataclass
class PatchCheck:
    lease_id: str
    ok: bool
    reason: str = ""
    paths: list[str] = field(default_factory=list)


@dataclass
class AssemblyReport:
    epoch: int
    base_sha: str
    epoch_sha: str
    accepted: list[str] = field(default_factory=list)
    rejected: list[PatchCheck] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {"epoch": self.epoch, "base_sha": self.base_sha, "epoch_sha": self.epoch_sha,
                "accepted": self.accepted,
                "rejected": [{"lease_id": r.lease_id, "reason": r.reason, "paths": r.paths}
                             for r in self.rejected]}


def check_patch(repo_root: Path, spec: LeaseSpec, sub: Submission, epoch_base: str) -> PatchCheck:
    """Step 3 checks (no mutation)."""
    raise NotImplementedError("FAC-06: patch permission checks")


def order_candidates(specs: dict[str, LeaseSpec], subs: list[Submission]) -> list[Submission]:
    """Step 2 ordering."""
    raise NotImplementedError("FAC-06: deterministic ordering")


def assemble_epoch(repo_root: Path, epoch: int, base_sha: str) -> AssemblyReport:
    """Steps 1-5."""
    raise NotImplementedError("FAC-06: assemble epoch")
