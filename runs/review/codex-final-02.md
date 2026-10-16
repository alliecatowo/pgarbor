# PGArbor final review 02

2026-10-15 (America/Los_Angeles). Final working tree based on `727c2dc5` and the
actual base SHA in [patches-02/manifest.json](patches-02/manifest.json).
PGArbor remains a pre-alpha engine. This review repairs the named internal-error
clusters; it does not establish PostgreSQL compatibility or finish all semantic gaps.

## Verified result

**2,154 / 5,837 selected cases PASS (36.90%), versus 1,992 / 5,837
(34.13%) in final-01: +162 cases, +2.78 percentage points.**
No previously passing case regressed. CRASH classifications fell from 102 to
0. A separate scan of every complete transcript and bridge error found
**zero XX-class errors**, including zero XX000. Confirmed process failures and final
process timeouts: 0 and 0. Unexpected acceptances remain
**36**, down from 58; these are unfinished work, not waived failures.

| Verification | Result |
|---|---|
| `cargo build --workspace --all-targets` | PASS; compiler warnings remain |
| `cargo test --workspace --no-fail-fast` | 2,476 passed, 0 failed, 272 ignored |
| Harness pytest (`-o addopts=""`) | 1,090 passed, 0 failed, 14 skipped |
| Final embedded corpus | All 5,837 selected cases accounted for; 400 families |
| Original CRASH/UNEXPECTED_ACCEPTANCE selection | 83 PASS, 16 UNSUPPORTED, 25 MISMATCH, 34 UNEXPECTED_ACCEPTANCE, 2 HARNESS_ERROR |
| Case SQL and recorded expectations | No changes |

Rust counts exclude filtered child-process helper summaries to avoid double counting.
Per-target evidence is [codex-test-layers-02.json](codex-test-layers-02.json).
Logs: `codex-build-02.log`, `codex-tests-02.log`, `codex-pytest-02.{log,xml}`,
`codex-regressions-02.log`, `codex-targetlist-tests-02.log`,
`codex-semantic-tests-02.log`, `codex-guc-tests-02.log`, `codex-parser-tests-02.log`.
The final workspace test includes every retained regression target. Ignored/skipped
tests remain untested. No fresh wire/app/ORM/native-suite compatibility claim is made.

## Review of inherited edits

Every inherited changed file has a decision in
[codex-change-audit-02.json](codex-change-audit-02.json). The early audit snapshot is
`patches-02/inherited.patch`; it contains rejected code and must not be applied.

- Reverted `pgarbor-types/src/datetime/tz.rs`: the inherited deletion of timezone
  aliases lacked pinned-oracle evidence available here. Removed its matching
  unverified rejection assertions from the session regression.
- Reverted `pgarbor/src/portal.rs`: converting every NULL void datum into an empty
  value would erase SQL NULL semantics.
- Repaired the build: ProjectSet unit constructors still used the old type;
  a test in the committed CTE repair still called removed `scope_key` machinery.
  Removed the untracked print-only `zz_dbg.rs` target.
- Replaced global tableoid constants with identity slots below joins. Whole-row
  reconstruction preserves outer-join NULL extension, all-NULL real rows, and
  dropped attribute positions. Unsupported row-locking combinations fail closed.
- Simplified aggregate/window expressions consistently before collecting slots;
  carried functionally dependent columns through ordinary and grouping-set
  aggregation. Upper projections reuse already-computed subqueries.
- Kept and extended VARIADIC handling, including explicit arrays, NULL arrays,
  stored defaults and view expressions. Kept the constant-folding guard.
- Lowered ALTER TYPE USING columns to executable row slots and assignment-coerced
  rebound volatile defaults to the column type.
- The inherited recursive batching patch hung on its cycle test. Reworked it to
  produce one row per pull and increment the executor epoch when the work table
  changes. Regressions assert the graph's 16 rows and sequence side effects proving
  LIMIT does not evaluate unused seed rows. The committed scope fix is preserved.
- Fixed nested LATERAL ownership using recorded query parentage; local and outer
  range indexes are no longer confused. Non-LATERAL references are rejected.
- Retained validated GUC storage, CHECKPOINT privilege checks, view dependencies,
  builtin-function ACL fail-closed behavior, schema REINDEX expansion, routine
  parameter deparsing and pg_notify NULL-channel behavior. Strengthened index
  system-column rejection to run before deparsing. GUC validation is a required
  adapter method, rather than the inherited unconditional-success default.
- Fixed mixed-integer DISTINCT comparisons, scalar LIMIT and JOIN-condition
  subqueries, OID Datum relabels, composite COMMENT existence, rowtype-default dependencies, and domain-array
  validation. Outer-level aggregates, multi-column hypothetical aggregates and
  unavailable anonymous record descriptors retain specific leased unsupported
  errors; they are not falsely reported as implemented.
- Expanded single-field composite table-function results correctly and rendered/accessed
  missing trailing attributes of older stored composite text values as NULL.
  These remove two real XX000 bridge failures found by inspecting HARNESS_ERROR.

Contract changes are recorded in amendments `pgarbor-exec-08`, `pgarbor-exec-09`,
`pgarbor-plan-04`, and `pgarbor-catalog-08`. No frozen contract document was edited.
The engine dependency direction stays synchronous and acyclic.

## Original clusters, case-level outcomes

These are whole-case outcomes: eliminating one internal error does not make a
case PASS if another step still differs. PostgreSQL's own 0A000 errors can also
produce the harness's UNSUPPORTED classification even when that particular
rejection matches PostgreSQL.

| Original cluster | Cases in 01 | Outcomes in 02 |
|---|---:|---|
| Missing subplan variable | 28 | 1 HARNESS_ERROR, 6 MISMATCH, 17 PASS, 4 UNSUPPORTED |
| VARIADIC argument metadata | 16 | 16 PASS |
| Committed CTE scope repair | 13 | 1 MISMATCH, 12 PASS |
| Non-executable Column / ALTER USING | 8 | 2 MISMATCH, 6 PASS |
| Missing aggregate target | 7 | 7 PASS |
| All syntax-error unexpected acceptances | 23 | 13 PASS, 10 UNEXPECTED_ACCEPTANCE |

The separate affected-case rerun is `embedded-clusters-02/results.jsonl` (160
cases, fresh databases/processes). Its command is in `rerun_clusters_02.py` and its
outcome totals agree with selecting those IDs from the final full campaign.

## Full differential accounting

| Outcome | final-01 | final-02 | Change |
|---|---:|---:|---:|
| PASS | 1,992 | 2,154 | +162 |
| MISMATCH | 2,320 | 2,251 | -69 |
| UNSUPPORTED | 1,163 | 1,194 | +31 |
| CRASH | 102 | 0 | -102 |
| UNEXPECTED_ACCEPTANCE | 58 | 36 | -22 |
| HARNESS_ERROR | 10 | 10 | +0 |
| BLOCKED | 152 | 152 | +0 |
| INVALID_CASE | 40 | 40 | +0 |
| HANG | 0 | 0 | +0 |

Selected denominator: 5,837. Existing valid references: 5,767.
Complete engine executions: 5,635; bridge failures do not count as
complete executions. PASS / executed: 38.23%.
The 152 blocked and 40 quarantined cases remain in the selected denominator.

Command (exit 1 means non-PASS cases were found, not a runner launch failure):

```sh
cd harness
UV_CACHE_DIR=/tmp/pgarbor-uv uv run --offline --no-sync python -m pgarbor_harness.runner.embedded \
  --binary ../target/debug/examples/embedded_case \
  --out ../runs/review/embedded-final-02 --jobs 4 --timeout 60
```

Final elapsed runner time: 51.77 seconds.
Binary SHA-256: `468e2c63077d8e4222f20b5620d5a635ed87d85020168eddd6367753b73e022d`.
Raw results and transcripts are in `embedded-final-02/`; summary is
`embedded-final-02/summary.json`. The final raw run needs no timeout merges.
Earlier diagnostic runs remain separately named `embedded-audit-02`,
`embedded-prevalidation-02`, and `embedded-before-output-fixes-02`.
The two timeouts in an earlier run completed on 90-second rechecks: btree
PASS at 20,870 ms; planner-statistics MISMATCH at 20,554 ms. These earlier
rechecks are not counted again in the final campaign.

Comparison uses existing recorded PG18 references and the unchanged embedded
runner/comparator. No oracle rerecord was performed. The live oracle connection
at 127.0.0.1:54318 was refused; Podman configuration access failed in the sandbox.
Source checks used the local PostgreSQL 18 tree. Existing normalization/provenance
limitations from final-01 remain. The embedded transport proves SQL API behavior,
not PostgreSQL wire compatibility.

## Per-family comparison

All **400** families, selected denominators, PASS counts, rates and deltas are in
[codex-differential-by-family-02.csv](codex-differential-by-family-02.csv).
[codex-differential-summary-02.json](codex-differential-summary-02.json) contains
outcome transitions, remaining cluster counts and the no-regression check.
The largest PASS-count gains are:

| Family | final-01 PASS / selected | final-02 PASS / selected | PASS gain |
|---|---:|---:|---:|
| cmd-drop-materialized-view | 1/15 (6.67%) | 11/15 (73.33%) | +10 |
| functional-deps | 5/15 (33.33%) | 14/15 (93.33%) | +9 |
| cmd-alter-materialized-view | 0/18 (0.00%) | 7/18 (38.89%) | +7 |
| cmd-create-materialized-view | 0/16 (0.00%) | 7/16 (43.75%) | +7 |
| doc-queries-with | 3/19 (15.79%) | 10/19 (52.63%) | +7 |
| matview | 1/15 (6.67%) | 8/15 (53.33%) | +7 |
| text | 5/12 (41.67%) | 12/12 (100.00%) | +7 |
| cmd-set-session-auth | 1/13 (7.69%) | 7/13 (53.85%) | +6 |
| cmd-set-role | 0/13 (0.00%) | 5/13 (38.46%) | +5 |
| cmd-create-role | 7/16 (43.75%) | 11/16 (68.75%) | +4 |
| cmd-refresh-materialized-view | 0/12 (0.00%) | 4/12 (33.33%) | +4 |
| cmd-start-transaction | 0/14 (0.00%) | 4/14 (28.57%) | +4 |

## Continued mismatch work and remaining limits

After repairing the named clusters, I ranked first divergences by count. The
largest result-count bucket mostly stops in setup at unsupported features; it
is not evidence that result framing is wrong. In the first audit, 679 such cases
had a leased 0A000 setup failure, and 185 had a 42601 setup failure. Those setup
features require implementation or specific parser support, not comparator changes.

The command-tag bucket had 129 cases in that audit, including 34 RESET/SET and
15 START TRANSACTION/BEGIN first divergences. Fixed those spellings and populated
materialized-view SELECT counts. The final command-tag first-divergence bucket
contains 17 cases.
No unsupported functionality was made to succeed solely to improve a count.

Largest remaining first-divergence buckets are 890 result_count, 458 syntax error at or near "?", 122 txn_status, 103 type "?" does not exist, 60 row_multiset, 54 value, 41 column "?" does not exist, 35 column_type.
Detailed signatures and case IDs remain in the raw results. These are outstanding:

1. **36 unexpected acceptances:** SQL/PLpgSQL body validation,
   collation conflicts, SSI, locks/privileges, dependencies, domain arrays,
   sequence boundaries and other semantics remain incomplete. This work does
   not claim those clusters are finished. Two acceptance cases are newly exposed
   behind earlier divergences; all original PASS cases still pass.
2. **2,251 mismatches and 1,194 unsupported cases:** setup feature
   breadth, error parity, transaction states and catalog/type metadata dominate.
   Current code still does not implement SSI or user-defined function execution.
3. **10 bridge failures:** inspect `bridge_error` in results.jsonl. Anonymous
   composite descriptors, unsupported timezone rendering and enum output remain
   incomplete. None contains an XX-class error in this final campaign.
4. No new oracle recordings, native upstream runs, ORM campaign, or live wire
   campaign was performed. Binary composite evolution beyond the tested text
   output path is not newly verified.

## Owner handoff

Git was read-only during implementation. During the authorized wrap-up, the
restriction was removed and all nine groups were committed. Their actual commit
IDs, complete messages and recovery patches are in
[patches-02/manifest.json](patches-02/manifest.json). Patches are historical snapshots;
do not reapply them over the committed changes.

The verified final-01 and final-02 result ledgers are tracked for direct cloud
triage. Raw transcripts and all intermediate campaigns are preserved in
[embedded-evidence-02.tar.gz](embedded-evidence-02.tar.gz), with checksum and restore
instructions in [embedded-evidence-02.json](embedded-evidence-02.json). The archive
is 8,763,351 bytes; expanded local campaign directories are ignored. This changes
packaging only, not the recorded measurements or comparator.

Taskwarrior was inaccessible during implementation but became available during
wrap-up. The observed-XX000 repair task was completed; mismatch and acceptance
tasks were updated by UUID. Broader composite support and warning cleanup remain
open. The Claude Cloud continuation prompt and current task snapshot are in
[claude-cloud-handoff-02.md](claude-cloud-handoff-02.md) and
[task-handoff-02.json](task-handoff-02.json).
