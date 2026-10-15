#!/usr/bin/env python3
"""Codex luna wave: one `codex exec` worker per file lease, each in its own git worktree.

Usage: python3 harness/factory/luna_wave.py <wave-args.json> [--jobs N] [--model gpt-6-luna] [--only L1,L2]

Same contract as harness/factory/workflows/wave.js, but runs on the Codex CLI (ChatGPT plan) instead of
Claude. Writes runs/epoch-01/submissions-<epoch>.json (input for assemble.py) and per-lease logs under
runs/epoch-01/luna/<epoch>/. Worktrees live in .claude/worktrees/luna-<epoch>-<lease>; branches stay as
evidence. Reruns skip leases that already have a commit on their branch.
"""
import argparse
import json
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REG = Path.home() / ".cargo/registry/src/index.crates.io-1949cf8c6b5b557f"


def git(*a, cwd=ROOT, check=True):
    r = subprocess.run(["git", *a], cwd=cwd, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(a)}: {r.stderr.strip()}")
    return r.stdout.strip()


def prompt(epoch, l):
    lid, crate, files = l["lease_id"], l["crate"], ", ".join(l["files"])
    return f"""You are an implementation worker in the PGArbor swarm ({epoch}). Your cwd is an ISOLATED git
worktree of the repo. PGArbor is a Rust single-file, branch-native database targeting exact PostgreSQL 18 behavior.

YOUR LEASE: {lid} (crate {crate})
FILES YOU OWN (edit ONLY these): {files}
FULL LEASE SPEC: your entry (lease_id {lid}) in contracts/leases/{crate}.json. Its summary, spec, reads,
depends_on_leases and acceptance are binding.
READ FOR CONTEXT: AGENTS.md, contracts/ARCHITECTURE.md, contracts/crates/{crate}.md, the lease's 'reads'
files, and the doc comments in your files (the frozen spec). Read the upstream crate source you call so your
calls match real signatures exactly. Dependency sources: {REG}/. PostgreSQL 18.6 source for reference:
~/develop/reference/postgres-18 (read to understand; write original Rust, never paste C).

RULES:
- Do NOT run cargo, rustc or tests (other workers build concurrently; a central build happens later). Write
  carefully: exact types, imports, lifetimes, error conversions, exhaustive matches. Re-read once for compile errors.
- Keep every public signature marked "// CONTRACT" exactly. Private helpers and #[cfg(test)] mod tests are fine.
  If the contract is insufficient, work around it inside your files and say so in your final message.
- Replace every PgError::unimplemented("{lid}", ...) body you can with a real, PostgreSQL-faithful
  implementation. Never todo!(), unimplemented!(), panic! on user/wire/file data, fake data, or silent no-ops.
  Exact PG SQLSTATEs and message text.
- Prefer clear, fast, allocation-conscious code.
FINISH: do not run git; the harness commits your leased files. End with a short summary: what is
implemented, what still returns unimplemented and why, and any contract problems."""


def run_one(epoch, model, l, logdir):
    lid = l["lease_id"]
    branch = f"luna-{epoch}-{lid}"
    wt = ROOT / ".claude/worktrees" / branch
    base = git("rev-parse", "HEAD")
    if not wt.exists():
        if git("branch", "--list", branch):
            git("worktree", "add", str(wt), branch)
        else:
            git("worktree", "add", "-b", branch, str(wt), base)
    head = git("rev-parse", "HEAD", cwd=wt)
    already = head != base and git("log", "--format=%s", "-1", cwd=wt).startswith(f"{epoch} {lid}")
    if not already:
        log = logdir / f"{lid}.log"
        with open(log, "w") as fh:
            subprocess.run(["codex", "exec", "-m", model, "-C", str(wt), "--skip-git-repo-check",
                            "--sandbox", "workspace-write",
                            "-o", str(logdir / f"{lid}.final.txt"), prompt(epoch, l)],
                           stdout=fh, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, timeout=3600)
    changed = [f for f in l["files"] if (wt / f).exists() and git("status", "--porcelain", "--", f, cwd=wt)]
    if changed:
        git("add", "--", *changed, cwd=wt)
        git("commit", "-qm", f"{epoch} {lid}: codex {model} implementation", cwd=wt)
    head = git("rev-parse", "HEAD", cwd=wt)
    ok = head != base
    print(f"{lid}: {'ok ' + head[:10] if ok else 'NO COMMIT'}", flush=True)
    return {"lease_id": lid, "crate": l["crate"], "files": l["files"], "branch": branch,
            "sha": head if ok else None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("args_json")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--model", default="gpt-6-luna")
    ap.add_argument("--only", default="")
    a = ap.parse_args()
    spec = json.load(open(a.args_json))
    epoch, leases = spec["epoch"], spec["leases"]
    if a.only:
        keep = set(a.only.split(","))
        leases = [l for l in leases if l["lease_id"] in keep]
    logdir = ROOT / "runs/epoch-01/luna" / epoch
    logdir.mkdir(parents=True, exist_ok=True)

    def safe(l):
        try:
            return run_one(epoch, a.model, l, logdir)
        except Exception as e:  # one broken lease must not stop the wave
            print(f"{l['lease_id']}: ERROR {e}", flush=True)
            return {"lease_id": l["lease_id"], "crate": l["crate"], "files": l["files"], "sha": None,
                    "error": str(e)}

    with ThreadPoolExecutor(max_workers=a.jobs) as ex:
        subs = list(ex.map(safe, leases))
    out = ROOT / f"runs/epoch-01/submissions-{epoch.split('-')[-1]}.json"
    json.dump({"epoch": epoch, "submissions": subs}, open(out, "w"), indent=1)
    missing = [s["lease_id"] for s in subs if not s.get("sha")]
    print(f"submitted {len(subs) - len(missing)}/{len(subs)}; missing: {','.join(missing)}; wrote {out}")


if __name__ == "__main__":
    sys.exit(main())
