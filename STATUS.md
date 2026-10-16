# PGArbor project state — 2026-10-15

PGArbor is a running pre-alpha SQL engine. It is not a PostgreSQL-compatible
release. The committed CTE scope repair is preserved. Final review 02 changes are now
committed in nine logical groups; the earlier read-only Git restriction was
removed for wrap-up. Case-level evidence and a compact raw archive are included.

Verified on the final working tree:

| Layer | Result |
|---|---|
| `cargo build --workspace --all-targets` | PASS; warnings remain |
| `cargo test --workspace --no-fail-fast` | 2,476 passed, 0 failed, 272 ignored |
| Harness pytest (`-o addopts=""`) | 1,090 passed, 0 failed, 14 skipped |
| Full embedded corpus | 2,154 / 5,837 PASS (36.90%), versus 1,992 / 5,837 (34.13%) in 01 |

The embedded campaign has 2,251 MISMATCH, 1,194 UNSUPPORTED,
0 CRASH, 36 UNEXPECTED_ACCEPTANCE, 10 HARNESS_ERROR,
152 BLOCKED and 40 quarantined cases. Final process timeouts: 0.
No original PASS case regressed. Separate inspection of complete transcripts and
bridge errors found zero XX-class errors. Cases and recorded expectations were
not edited; comparison used existing PG18 recordings, not a fresh live oracle.

The named slot-resolution, ALTER USING and VARIADIC internal-error clusters are
repaired or fail closed for specific unsupported features. Added regressions cover
outer-join whole rows/tableoid, functional dependencies, recursive CTE laziness
and cache invalidation, LATERAL, GUCs, composites and completion tags. Semantic
unexpected acceptances remain; SSI, function-body validation/execution, catalog
breadth, timezone/type support and native/wire COPY parity are unfinished.

Details, limits and executed commands: [runs/review/codex-final-02.md](runs/review/codex-final-02.md).
All 400 per-family comparisons: [runs/review/codex-differential-by-family-02.csv](runs/review/codex-differential-by-family-02.csv).
Committed change groups: [runs/review/patches-02/manifest.json](runs/review/patches-02/manifest.json).

Earlier owner checks of wire smoke, WASM and bindings remain in review 01 and
its preceding STATUS history. They were not repeated as part of this campaign.
Taskwarrior became available during wrap-up: the 102-XX000 task is completed,
and mismatch/acceptance tasks now carry the measured remaining counts. The
continuation prompt is in [runs/review/claude-cloud-handoff-02.md](runs/review/claude-cloud-handoff-02.md).

Cloud startup follow-up: `scripts/cloud-setup.sh` was already present on every
published branch. The new `scripts/cloud-bootstrap.sh` handles setup shells
outside the checkout; use the environment field text in [docs/CLOUD.md](docs/CLOUD.md).
Published feature branches are already merged. The local factory branches and
169 worktrees were reconciled against assembly records; the three dirty wire
files add no implementation missing from main. Inventory and validation:
[runs/review/branch-reconciliation-02.md](runs/review/branch-reconciliation-02.md).
