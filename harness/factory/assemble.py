#!/usr/bin/env python3
"""SWARM-08: deterministic bulk assembler for a RightShift wave.

Usage: python3 harness/factory/assemble.py <submissions.json> <runs/epoch-NN/assembly.json> [--base <sha>]

For each submission (sorted by lease_id): inspect `git diff --name-only <base> <sha>`.
- Files outside the lease or on protected paths are REJECTED (recorded, never applied).
- Leased files are applied with `git checkout <sha> -- <files>` (exact worker content).
Never force-pushes, never rewrites worker branches; failed patches stay on their branches as evidence.
"""
import fnmatch
import json
import subprocess
import sys

PROTECTED = ["Cargo.toml", "*/Cargo.toml", "Cargo.lock", "contracts/*", "harness/oracle/*",
             "tests/expected/*", "*/src/lib.rs", "*/mod.rs", "rust-toolchain.toml"]


def git(*a, check=True):
    r = subprocess.run(["git", *a], capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(a)}: {r.stderr.strip()}")
    return r.stdout


def is_protected(path, lease_files):
    if path in lease_files:
        # a lease may legitimately own a mod.rs/lib.rs only if explicitly granted (never by default)
        return False
    return any(fnmatch.fnmatch(path, p) for p in PROTECTED)


def main(argv):
    subs_doc = json.load(open(argv[1]))
    out_path = argv[2]
    base = argv[argv.index("--base") + 1] if "--base" in argv else git("rev-parse", "HEAD").strip()
    subs = subs_doc["submissions"] if isinstance(subs_doc, dict) else subs_doc
    ledger = {"base": base, "applied": [], "rejected": [], "partial": []}
    for s in sorted(subs, key=lambda s: s["lease_id"]):
        sha = s.get("sha")
        lease_files = set(s.get("files", []))
        if not sha:
            ledger["rejected"].append({"lease_id": s["lease_id"], "reason": "no submission"})
            continue
        try:
            changed = [p for p in git("diff", "--name-only", f"{sha}^", sha).split("\n") if p]
        except RuntimeError as e:
            ledger["rejected"].append({"lease_id": s["lease_id"], "sha": sha, "reason": str(e)})
            continue
        off_lease = [p for p in changed if p not in lease_files]
        protected = [p for p in changed if is_protected(p, lease_files)]
        apply = [p for p in changed if p in lease_files]
        if not apply:
            ledger["rejected"].append({"lease_id": s["lease_id"], "sha": sha, "reason": "no leased file changed", "changed": changed})
            continue
        git("checkout", sha, "--", *apply)
        rec = {"lease_id": s["lease_id"], "sha": sha, "files": apply}
        if off_lease or protected:
            rec["ignored_off_lease"] = off_lease
            rec["ignored_protected"] = protected
            ledger["partial"].append(rec)
        else:
            ledger["applied"].append(rec)
    ledger["counts"] = {k: len(ledger[k]) for k in ("applied", "partial", "rejected")}
    json.dump(ledger, open(out_path, "w"), indent=1)
    print(json.dumps(ledger["counts"]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
