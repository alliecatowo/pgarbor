# Claude Cloud continuation prompt

You are lead engineer on PGArbor. Start from the latest `origin/main`. Read
`AGENTS.md`, `README.md`, `STATUS.md`, `contracts/ARCHITECTURE.md`, and
`runs/review/codex-final-02.md` first. Read the crate contracts and leases before
editing each crate. The review-02 fixes are already committed; do not reapply
`patches-02/*.patch` or the rejected inherited snapshot.

The verified baseline is 2,154 / 5,837 selected cases PASS (36.90%): 2,251 MISMATCH,
1,194 UNSUPPORTED, 36 UNEXPECTED_ACCEPTANCE, 10 HARNESS_ERROR, 152 BLOCKED and 40
quarantined cases. There are zero CRASH classifications, timeouts, and XX-class
errors in both complete transcripts and bridge failures. No original review-01
PASS regressed. Rust: 2,476 passed, 0 failed, 272 ignored. Harness: 1,090 passed,
0 failed, 14 skipped. These compare existing PG18 recordings; they do not prove
live oracle, wire, ORM, or native-suite compatibility.

Continue implementation, not just planning:

1. Triage and repair the 36 unexpected acceptances from
   `runs/review/embedded-final-02/results.jsonl`. Prioritize integrity and semantic
   enforcement: dependency checks, constraints/domain arrays, sequence boundaries,
   permissions/locking, and SQL/PLpgSQL create-time validation. Collation and SSI
   gaps remain. Implement correct PG18 behavior or return a specific leased 0A000
   when genuinely unsupported; never acknowledge unenforced behavior as success.
2. Then work down actionable mismatch clusters by count. The largest first
   divergences are 890 result_count, 458 syntax errors, 122 transaction states,
   and 103 missing types. Most result_count failures stop in setup: inspect the
   actual error before treating them as transcript framing defects. The summary
   and all 400 family rates are in `codex-differential-summary-02.json` and
   `codex-differential-by-family-02.csv` under runs/review.
3. Inspect the 10 bridge failures too. Fix anonymous composite descriptor, enum
   and timezone rendering gaps where feasible. Do not mask bridge/internal errors
   or alter classifier/normalization policy to improve the score.
4. Add focused regression tests and rerun affected cases after each cluster. Keep
   all prior PASS cases passing, and preserve zero observed XX errors.
5. Run workspace build/tests and harness pytest. Run the full embedded corpus to
   a fresh `runs/review/embedded-final-03` directory. Report selected-denominator
   pass rates by family against 02, outcome transitions, regressions and remaining
   clusters. Write `runs/review/claude-final-03.md`, update STATUS.md, commit logical
   units and push. Report only executed evidence; distinguish blocked/unrun work.

Useful commands (from repository root unless specified):

```sh
cargo build --workspace --all-targets
cargo test --workspace --no-fail-fast
cargo build -p pgarbor-cli --example embedded_case
cd harness
uv sync --frozen
GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=commit.gpgsign GIT_CONFIG_VALUE_0=false \
GIT_CONFIG_KEY_1=core.fsmonitor GIT_CONFIG_VALUE_1=false \
uv run pytest -o addopts=""
uv run python -m pgarbor_harness.runner.embedded \
  --binary ../target/debug/examples/embedded_case \
  --out ../runs/review/embedded-final-03 --jobs 4 --timeout 60
```

The embedded runner supports repeated `--family F` for focused campaigns; use a
fresh output directory each time. Its exit 1 means at least one non-PASS outcome,
not necessarily a launch failure. Recheck workload timeouts separately with a
longer timeout and preserve raw results. Reuse the runner/comparator rather than
inventing comparison logic. `analyze_embedded_02.py` is a fixed review-02 analysis
script, not a generic campaign tool; write a separate 03 analysis.

Case-level ledgers for final-02 and verified final-01 are tracked directly. For
saved transcripts/intermediate runs, from the repository root:

```sh
sha256sum runs/review/embedded-evidence-02.tar.gz
# Compare with runs/review/embedded-evidence-02.json, then:
tar -xzf runs/review/embedded-evidence-02.tar.gz
```

The archive restores paths under runs/review. The archive hash is
`5c762e37b67bef25a4958969a994ff7034bddbd29f0db4de6ac5f5fa01d7ab53`.

Follow AGENTS.md hard rules: pinned PostgreSQL 18.6 is authoritative. Never
hand-edit expected outputs or change case SQL to make PGArbor pass. Oracle
recordings require the documented reviewed pinned-oracle workflow. Engine crates
stay synchronous and respect the dependency DAG. Contract changes require an
amendment. No fake results, panics or unwraps on user/wire/file input. Commit
messages end in Co-Authored-By trailers only, with no session URLs. Any worktrees
go under `.claude/worktrees/` in the repository.

Read Taskwarrior with `task project:pgarbor list`; modify tasks by UUID only. A
sanitized snapshot is `runs/review/task-handoff-02.json` if the cloud environment
has no shared task database. The observed 102-XX000 task is already completed;
broader composite support, mismatch repair, unexpected acceptances, SSI and live
wire/app/native campaigns remain open. Do not claim a task update if that database
is unavailable. If live oracle or socket access is unavailable, state that limit
and continue with existing recordings and source evidence.
