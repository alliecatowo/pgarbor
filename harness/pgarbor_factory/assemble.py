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

import fnmatch
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

from pgarbor_harness import config
from pgarbor_harness.model import write_canonical_json

from . import gitutil, leases, review, submissions, worktrees
from .records import LeaseSpec, LeaseState, Submission

#: Crate assembly order (dependency order from ARCHITECTURE §1), harness last.
CRATE_ORDER: tuple[str, ...] = (
    "pgarbor-common", "pgarbor-storage", "pgarbor-fault", "pgarbor-branches", "pgarbor-types",
    "pgarbor-catalog", "pgarbor-core", "pgarbor-sql", "pgarbor-plan", "pgarbor-exec",
    "pgarbor", "pgarbor-wire", "pgarbor-cli", "harness",
)

_STRUCTURAL_NAMES = frozenset({"Cargo.toml", "lib.rs", "mod.rs", "__init__.py", "__main__.py"})
_ASSEMBLER_NAME = "PGArbor Assembler"
_ASSEMBLER_EMAIL = "assembler@pgarbor.invalid"


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


def _matches(path: str, globs: tuple[str, ...]) -> bool:
    # fnmatch's ``*`` already crosses ``/``, so ``dir/**`` matches any depth below dir.
    return any(fnmatch.fnmatchcase(path, g) for g in globs)


def _is_ancestor(repo_root: Path, ancestor: str, descendant: str) -> bool:
    p = subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, descendant],
                       cwd=str(repo_root), capture_output=True, text=True, check=False)
    return p.returncode == 0


def check_patch(repo_root: Path, spec: LeaseSpec, sub: Submission, epoch_base: str) -> PatchCheck:
    """Step 3 checks (no mutation)."""
    lid = spec.lease_id

    def bad(reason: str, paths: list[str]) -> PatchCheck:
        return PatchCheck(lid, False, reason, sorted(set(paths)))

    try:
        changes = gitutil.changed_files(sub.base_sha, sub.head_sha, repo_root)
    except gitutil.GitError:
        return bad("unreadable_diff", [])
    paths = [p for _, p in changes]

    protected = [p for p in paths if _matches(p, config.PROTECTED_GLOBS)]
    if protected:
        return bad("protected_path", protected)

    if not lid.startswith(config.HARNESS_LEASE_PREFIXES):
        verifier = [p for p in paths if _matches(p, config.VERIFIER_GLOBS)]
        if verifier:
            return bad("verifier_path", verifier)

    structural = [p for p in paths if PurePosixPath(p).name in _STRUCTURAL_NAMES]
    if structural:
        return bad("structural_file", structural)

    leased = set(spec.files)
    unleased = [p for p in paths if p not in leased]
    if unleased:
        return bad("unleased_path", unleased)

    deleted = [p for st, p in changes if st == "D"]
    if deleted:
        return bad("deletion", deleted)

    if not _is_ancestor(repo_root, sub.base_sha, epoch_base):
        return bad("stale_base", [])

    findings = review.review_patch(repo_root, spec, sub.base_sha, sub.head_sha)
    rejects = [f for f in findings if f.get("severity") == "reject"]
    if rejects:
        return bad(f"review:{rejects[0].get('rule', 'unknown')}",
                   [str(f["file"]) for f in rejects if f.get("file")])

    return PatchCheck(lid, True, "", sorted(set(paths)))


def order_candidates(specs: dict[str, LeaseSpec], subs: list[Submission]) -> list[Submission]:
    """Step 2 ordering."""
    rank = {c: i for i, c in enumerate(CRATE_ORDER)}

    def key(s: Submission) -> tuple[int, str]:
        spec = specs.get(s.lease_id)
        crate = spec.crate if spec is not None else ""
        return (rank.get(crate, len(CRATE_ORDER)), s.lease_id)

    return sorted(subs, key=key)


def _wt_clean(wt: Path) -> None:
    """Drop any partial state in the assembly worktree."""
    gitutil.git(["reset", "--hard", "-q"], wt)
    gitutil.git(["clean", "-fdq"], wt)


def _commit(wt: Path, message: str, when: int) -> None:
    env = dict(os.environ)
    date = f"@{when} +0000"
    env.update({
        "GIT_AUTHOR_NAME": _ASSEMBLER_NAME, "GIT_AUTHOR_EMAIL": _ASSEMBLER_EMAIL,
        "GIT_COMMITTER_NAME": _ASSEMBLER_NAME, "GIT_COMMITTER_EMAIL": _ASSEMBLER_EMAIL,
        "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date,
    })
    p = subprocess.run(
        ["git", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "--no-verify",
         "--no-gpg-sign", "-m", message],
        cwd=str(wt), capture_output=True, text=True, env=env, check=False)
    if p.returncode != 0:
        raise gitutil.GitError(f"git commit failed ({p.returncode}): {p.stderr.strip()}")


_PATCH_FAILED = re.compile(r"^error: patch failed: (.+?):\d+$", re.MULTILINE)
_NOT_APPLY = re.compile(r"^error: (.+?): patch does not apply$", re.MULTILINE)


def _apply(wt: Path, diff: str) -> tuple[bool, list[str]]:
    """Apply a squashed diff; on failure return the conflicting paths and clean the worktree."""
    p = subprocess.run(
        ["git", "apply", "--index", "--3way", "--whitespace=nowarn"],
        cwd=str(wt), capture_output=True, text=True, input=diff, check=False)
    if p.returncode == 0:
        return True, []
    conflicted = set(gitutil.git(["diff", "--name-only", "--diff-filter=U", "-z"], wt,
                                 check=False).split("\0"))
    conflicted.update(_PATCH_FAILED.findall(p.stderr))
    conflicted.update(_NOT_APPLY.findall(p.stderr))
    conflicted.discard("")
    _wt_clean(wt)
    return False, sorted(conflicted)


def assemble_epoch(repo_root: Path, epoch: int, base_sha: str) -> AssemblyReport:
    """Steps 1-5."""
    repo_root = Path(repo_root)
    pths = config.Paths(root=repo_root)
    base_sha = gitutil.rev_parse(base_sha, repo_root)
    wt = worktrees.allocate_assembly(repo_root, epoch, base_sha)
    _wt_clean(wt.path)

    specs = {s.lease_id: s for s in leases.load_catalog(pths.leases_dir)}
    ledger_path = pths.state / "leases.json"
    ledger = leases.load_ledger(ledger_path)
    subs = [s for lid, s in submissions.load_submissions(repo_root).items()
            if lid in ledger and ledger[lid].state == LeaseState.PATCH_SUBMITTED]
    ordered = order_candidates(specs, subs)

    report = AssemblyReport(epoch=epoch, base_sha=base_sha, epoch_sha=base_sha)
    last_touch: dict[str, str] = {}

    for sub in ordered:
        spec = specs.get(sub.lease_id)
        if spec is None:
            report.rejected.append(PatchCheck(sub.lease_id, False, "unknown_lease", []))
            continue
        chk = check_patch(repo_root, spec, sub, base_sha)
        if not chk.ok:
            report.rejected.append(chk)
            continue
        diff = gitutil.git(
            ["--literal-pathspecs", "diff", "--binary", "--full-index", "--no-renames",
             "--no-ext-diff", "--no-color", "--src-prefix=a/", "--dst-prefix=b/",
             sub.base_sha, sub.head_sha, "--", *spec.files], repo_root)
        if diff.strip():
            ok, conflicted = _apply(wt.path, diff)
        else:
            ok, conflicted = True, []
        if not ok:
            paths = conflicted or chk.paths
            owners = sorted({last_touch[p] for p in paths if p in last_touch})
            who = ",".join(owners) if owners else "base"
            report.rejected.append(PatchCheck(
                sub.lease_id, False, f"conflict (last touched by {who})", paths))
            continue
        index = len(report.accepted)
        _commit(wt.path, f"assemble {sub.lease_id} {sub.head_sha[:12]}", epoch * 86400 + index)
        report.accepted.append(sub.lease_id)
        for p in chk.paths:
            last_touch[p] = sub.lease_id

    report.epoch_sha = gitutil.head_sha(wt.path)

    for lid in report.accepted:
        leases.transition(ledger_path, lid, LeaseState.ASSEMBLED, f"epoch {epoch}")
    for r in report.rejected:
        if r.lease_id in ledger:
            leases.transition(ledger_path, r.lease_id, LeaseState.REJECTED, r.reason)

    out = pths.runs / f"epoch-{epoch:02d}" / "assembly.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    write_canonical_json(out, report.to_json())
    return report
