# PGArbor final review 01

2026-10-15. Current committed base at verification: `416328bd`; subsequent changes are in the working tree. This report distinguishes executed evidence, inherited owner evidence, environment blocks and unrun work. **PGArbor is a working pre-alpha SQL engine, not a PostgreSQL-compatible release.** The final embedded campaign passes **1,992 / 5,837 selected cases (34.13%)** under the current comparator policy. Significant SQL and verifier gaps remain.

## Priority smoke failures

The resumed checkout already contained the owner's fixes for the three reported failures: snapshot-free catalog environment construction (`473bac83` and preceding session repair), single registration of UPDATE/DELETE targets (`9d0a1384`), and builtin information_schema domain resolution (`3e86a779`). The producer binding and pushed-down view/subquery filters were also repaired centrally. I preserved those changes and verified their behavior.

The embedded session target now passes **10 tests**, including SHOW/BEGIN/ROLLBACK as the very first command of a fresh session, explicit failure/COMMIT-to-ROLLBACK, savepoint DDL rollback, UPDATE/DELETE tags and actual effects, filtered information_schema tables/columns, identity sequence naming, duplicate keys and implicit rollback. Additional SQL concurrency tests pass **3/3**: RC refresh, RR stability, and no partial multirow insert on a unique violation.

Three new socket-free raw-byte tests pass through **the actual wire connection and session loop**: startup plus SHOW/BEGIN/ROLLBACK, UPDATE/DELETE and information_schema results, and Parse/Bind/Describe/Execute/Sync with a text parameter yielding 42. The test-only finite duplex captures real backend frames and checks rows, tags and ReadyForQuery states. It does not exercise a listener, kernel I/O, socket timeouts or an independent driver. `harness/smoke/wire_smoke.py` was attempted here and produced **0 ok, 1 connection error**, with the server failing TCP bind with EPERM. Its process exits zero even on that failure; the printed result, not exit status, is the evidence. Owner/Claude must rerun it outside this sandbox.

## Assembly and architecture

The retained assembler ledgers show **63/63** SQL/plan submissions and **58/58** exec/API submissions applied. Wire/CLI applied **41**, rejected **7** unchanged-base SHAs, and recovered three pre-existing worktree implementations (connection, command loop, listener) with recorded hashes. The remaining missing surfaces were completed centrally. WIR-01 was outside the 48-entry args file and was separately applied by owner commit `057ea03e`; its frontend file matches the supplied `987104ddc8` implementation. See `runs/epoch-01/codex-{submissions,assembly}-02{a,b,c}.json` and `codex-recovered-02c.json`. These are assembly records, not evidence every leased behavior is complete.

All existing amendment requests have explicit steward decisions; accepted, partially accepted and deferred functionality are distinguished in their files. No PROPOSED decision remains. Root discovery/remapping, VFS ownership/identity, index NULL layout, catalog encoding, conservative JSON/domain limits and native protocol representations have recorded decisions. Row hints, verified pg_amop metadata, additional Datum families, IANA timezone data, CONTEXT and complete ordered wire COPY remain deferred where not implemented. The dependency DAG remains synchronous and acyclic.

This resume additionally implements arbitrary-order CREATE SEQUENCE options and signed role CONNECTION LIMIT handling in the production parser adapter; binder fixtures now use that adapter. Deferred constraints and row-security activation/access fail closed with leased 0A000 instead of acknowledging unenforced semantics. CLI COPY FROM complete script blocks now execute via the API memory channel. A conservative ordinary-table dump preflights inventory, emits columns/defaults/named NOT NULL/constraints/indexes and INSERT data, and refuses unsupported objects before publishing bytes. Dump/import/compact and failed-COPY rollback regressions pass. Full pg_dump and wire COPY compatibility are not claimed.

## Original review findings

| Finding | Implemented control and regression evidence | Remaining limit |
|---|---|---|
| P1 export temporary-file race | Exclusive owned temporaries, no removal of another builder's live file, atomic no-replace publication; six storage publication tests and four branch publication tests pass. The analogous detach/export race was fixed too. | OS no-replace is implemented on Linux/Android; other platforms fail closed. |
| P1 JSON nesting abort | Input nesting capped at 128; subprocess parse/output/compare/key encoding/clone/drop on a 2 MiB stack passes, and depth 6,000 is rejected. | Arbitrarily deep manually constructed internal trees are not made safe by an input guard; PG may accept greater depth. |
| P1 domain-cycle abort | Snapshot validation rejects self/mutual cycles, missing bases, domain/array cycles and paths above 64; direct and encoded cases pass. | Broader catalog semantic validation is incomplete. |
| P1 superblock arithmetic | Checked representable page/length/sequence boundaries and allocation headroom; five hostile-header tests pass through public open. | This is not an exhaustive file-format fuzz campaign. |
| P1 stale repeated key-lock outcome | Recompute latest committed versus statement snapshot on repeated grants/upgrades; own overlay remains authoritative; core tests pass. | Broad SQL ON CONFLICT/deadlock/concurrent DDL acceptance is still incomplete. |
| P1 OID literal false PASS | OID row values compare exactly; wrong literal/stored OID mutants are detected; 23 old U<n>-normalized references are blocked for reviewed rerecording. | Descriptor identity renumbering still lacks a complete semantic object-name mapping. Other nondeterministic masks retain provenance risks, below. |
| P2 recorder fixture/public-entry bypass | Failed or absent setup observations prevent publication; public single-case recording requires review and verified live identity; regressions pass. | No oracle rerecord was performed. |
| P2 stale-inode recovery | Revalidate pathname identity after locking and retry retired opens; deterministic compaction interleaving tests pass. | Custom VFS implementations must implement the new identity/publication requirements or fail closed. |

Recorder, regress freshness/launcher failure, zero-work SLT, real fixture-result parsing, lease inventory and OID provenance regressions are in normal pytest. CAT-39 now returns 0A000 instead of fabricated zero statistics. No case SQL or `tests/expected/**` file was changed.

## Build and Rust test results

`cargo build --workspace --all-targets`: **PASS**, with compiler warnings remaining (`codex-build-verified.log`). `cargo test --workspace --no-fail-fast`: **FAIL due to sandbox socket denial only**, **2,409 passed, 11 failed, 272 ignored**. The 11 failures are existing framing/listener socket tests receiving EPERM. Child-process helper result blocks are excluded from totals to avoid counting the same acceptance test twice. Ignored counts include doctests and remain untested.

| Crate, including its integration tests/doctests | PASS | FAIL | Ignored |
|---|---:|---:|---:|
| pgarbor | 113 | 0 | 56 |
| branches | 119 | 0 | 1 |
| catalog | 374 | 0 | 2 |
| CLI | 74 | 0 | 11 |
| common | 3 | 0 | 0 |
| core | 129 | 0 | 1 |
| exec | 236 | 0 | 76 |
| fault | 68 | 0 | 2 |
| plan | 129 | 0 | 17 |
| SQL | 333 | 0 | 64 |
| storage | 196 | 0 | 7 |
| types | 588 | 0 | 22 |
| wire | 47 | 11 environmental | 13 |

Logs: `codex-tests-verified.log`; exact per-target counts: `codex-test-layers-verified.json`. The fault crate's **68 PASS** includes the existing deterministic/property suites; embedded SQL fault recovery separately passes **4 tests** in the API count. Other new API targets pass: branching **4**, compaction **2**, detach/export/handles **3**, durable file/reopen **5**. CLI process/branch/round-trip targets pass **5 + 5 + 7**. These are concrete tested scenarios, not complete coverage of every audit bullet.

## Harness results

Full offline pytest, with inherited Git signing/fsmonitor disabled for synthetic Git fixtures: **1,072 passed, 18 failed, 14 skipped**, zero collection/setup errors. All 18 failures are socket EPERM in HAR-11/24/25. The explicitly socket-excluded run exits zero; its **1,060 PASS / 14 skipped** also match the corresponding JUnit subset. Lease validation and the new four embedded-transport integrity tests pass. See `codex-pytest-verified.{log,xml}` and `codex-pytest-no-sockets.log`.

An earlier raw invocation also hit inherited GPG signing failures in synthetic Git repositories; that environmental issue was corrected for the verification run, not suppressed in production. Pure unit fixtures with incorrect MD5, SASL frame-length or formatting arithmetic were corrected using independent byte/hash calculations. Oracle cases/expected outputs were untouched.

## Differential corpus: embedded API

The new runner executes each eligible case in a fresh Rust process/in-memory database, uses Session text encodings and the existing Transcript/comparator/classifier, and records the binary hash. It does not change the frozen wire Target protocol. SQL is attempted and unsupported engine features retain real errors. Async/protocol/restart capabilities are explicitly blocked; they do not count as executed compatibility evidence.

Command: `cd harness && UV_CACHE_DIR=/tmp/pgarbor-uv uv run --offline --no-sync python -m pgarbor_harness.runner.embedded --binary ../target/debug/examples/embedded_case --out ../runs/review/embedded-final-01 --jobs 4 --timeout 10`.

| Accounting | Cases |
|---|---:|
| Selected, all 400 families | 5,837 |
| Reference valid under current hash/normalization policy | 5,767 |
| Complete execution or process timeout initially attempted | 5,635 |
| PASS after two 60-second timeout rechecks | 1,992 |
| MISMATCH | 2,320 |
| UNSUPPORTED | 1,163 |
| CRASH classifier outcome, all unexpected internal XX000 results | 102 |
| UNEXPECTED_ACCEPTANCE | 58 |
| HARNESS_ERROR, bridge cannot encode/complete transcript | 10 |
| BLOCKED | 152 |
| INVALID_CASE, quarantined | 40 |
| Confirmed HANG after rechecks | 0 |
| Unaccounted selected cases | 0 |

**PASS / selected = 34.13%; PASS / executed = 35.35%.** All 400 per-family pass rates and denominators are in [codex-differential-by-family.csv](codex-differential-by-family.csv). The report does not substitute an executed-only percentage for the selected denominator. Reference validity checks existing records; live oracle identity was not verified here.

Initial raw results had 1,991 PASS, 2,319 MISMATCH and two 10-second HANG classifications. Both were large insert workloads, not confirmed engine hangs: `btree-index.delete-vacuum-reinsert` passed at 19.7 s; `cmd-create-statistics.planner-row-estimate-dependencies` mismatched at 19.1 s under 60-second reruns. Raw results remain unchanged; `results-verified.jsonl` and `summary-verified.json` merge the rechecks exactly once per case. There were **zero observed process aborts**. The 102 CRASH outcomes are the harness's existing internal-error classification, not 102 terminated processes.

Evidence: `embedded-final-01/{results.jsonl,results-verified.jsonl,summary-verified.json,transcripts/}`, `embedded-timeout-rechecks/results.json`, and `codex-embedded-final.log`. Binary SHA-256: `bf6b1e9c1c95bf61294d906304b79f693871f3874d02f9ba04d3cdaa548d8841`.

The ten bridge errors concern output of anonymous composite values or unsupported timezone rendering; they are HARNESS_ERROR, never PASS. The 152 blocked cases are 121 unsupported step/scheduling cases, 23 stale OID-normalized references, seven missing references, and one explicit extended-protocol capability case. The 40 quarantined cases remain visible in the denominator.

## Native suites and fixture apps

Native smoke commands and exit codes are retained in `harness-evidence-01/commands.json`. Regress selections `test_setup,boolean,int4` were attempted for PG and PGArbor and blocked at environment/target startup (exit 4). Isolation selections `deadlock-simple,read-write-unique` were attempted; the PGArbor adapter emitted two CRASH records caused by socket startup denial, not executed SQL permutations. Its CLI exits zero while collecting failures. PG isolation startup was blocked. **Official native executions and differential native executions: 0. Full upstream regress/isolation: UNRUN.** Neither identical startup failures nor manifest parsing is counted as compatibility PASS.

The current sandbox cannot start the server needed by four fixture apps: **this resume's live sqlx/node-pg/Drizzle/Prisma runs are BLOCKED, substantive checks executed here: 0**. Inherited owner evidence in `orm-fixtures-01.md` reports sqlx 16/16, Prisma CRUD 16/16 with four schema-engine failures, and node-pg/Drizzle migration blocks. That predates later parser/catalog repairs and is not a fresh rerun or a claim those apps now pass. Re-run all four with fresh files and migration/introspection checks outside the sandbox. Installation/typecheck stages are not SQL compatibility tests.

## Remaining P1/P2 work

1. **P1 verifier integrity:** opt-in timestamp/UUID/XID/PID masks can still hide deterministic adjacent values without column/expression provenance. Descriptor OID renumbering also does not prove semantic object identity. Therefore even the measured PASS rate has these policy limits. Add provenance-aware mutants/policy and reviewed reference migration; never hand-edit expected output.
2. **P1/P2 negative acceptance:** 58 cases were unexpectedly accepted. Triage privilege/transaction/dependency cases first, including checkpoint permissions, ALTER ROLE values and DROP dependency behavior. Row-security/deferred guards added here prevent those specific unsupported semantics from succeeding, but do not establish complete privilege or constraint enforcement.
3. **P2 SQL integration:** 102 cases return internal XX000. Largest clusters: 28 missing subplan variables, 16 function-argument metadata mismatches, 13 CTE index errors, eight non-executable Column nodes, seven missing aggregate slots. Preserve correct unsupported errors while fixing actual supported-path invariants; do not globally translate every XX000 into 0A000.
4. **P2 compatibility breadth:** 2,320 mismatches and 1,163 unsupported cases remain. Use saved first divergences by family; priorities are bind/plan slot layouts, builtin metadata/variadics, recursive CTEs, catalog/introspection, then error-message parity and deferred feature families.
5. **P2 transaction semantics:** SSI is not wired into real reads/writes/finish. SERIALIZABLE is still RR and can allow write skew. Broad SQL concurrent ON CONFLICT, catalog retries, locks/deadlocks and savepoint recovery need more session-level tests; current core lock tests are not a substitute.
6. **P2 coverage/environment:** run all ignored tests deliberately, independent psql/psycopg wire smoke (including failure→Sync, cancel, auth and COPY), apps and real native binaries outside the socket sandbox. Fix native environment/provisioned-database parity before full campaigns. Remaining transport tests are not proven merely because scripted frames pass.
7. **P2 features/platforms:** full streaming COPY, pg_dump breadth, non-Linux publication, CONTEXT, timezone/type families, force-admin wait guarantees and larger resource limits remain deferred/unverified. Conservative dump refuses unsupported inventory; it must not be expanded by silently omitting metadata.

## Durable handoff and operations

STATUS.md is updated to these measurements. Taskwarrior cannot open its database/WAL (error 14), so no UUIDs could be read and no tasks were modified; desired updates are retained in `codex-outstanding-task-updates.json`. Do not invent task UUIDs or modify by display IDs.

Normal `git add`/commit is blocked: `.git/index.lock` cannot be created on the read-only mount. Earlier assembled changes were committed by the owner (including `d094da51`, `745ab5e1` and follow-ups); **this resume creates no commits**. Prepared logical patch groups retain the current uncommitted source changes for review. Push was attempted: first the inherited SSH config permissions failed; retrying with `ssh -F /dev/null` failed DNS resolution for github.com. No permissions escalation or alternate repository metadata was used. See `codex-push-{current,retry}.log`.

Next maintainer sequence: review/apply the prepared groups, commit with Co-Authored-By trailers only, rerun targeted/full gates outside the sandbox, update Taskwarrior by real UUID, then push. Preserve the fixed corpus denominator and all failed/blocked/unrun outcomes when reporting progress.
