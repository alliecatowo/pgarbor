# PGArbor — RFC 002: Autonomous Engineering & Verification Factory

**Status:** Proposed. **Date:** 2026-10-14. **Depends on:** RFC 001. **Project name:** PGArbor, conditional on required namespace clearance before any scaffold.

## 0. Decision

PGArbor is a greenfield, branch-native, single-file database written in Rust, with full PostgreSQL compatibility as the uncompromising long-term destination. Its construction process shall be **evaluation-driven autonomous engineering**, informed by RL with verifiable rewards (RLVR) but **without training or updating model weights**. An independent deterministic harness, not an LLM, decides whether a patch has met its acceptance criteria. PostgreSQL 18.x, pinned to a specific build, is the behavioral oracle; a storage-state-machine reference model and fault simulator provide independent correctness oracles for durability and branching.

**Core loop:** produce tests → execute reference and candidate → normalize only legitimate nondeterminism → classify/reduce counterexample → create dependency-aware task → implement in isolated workspace → verify on untouched tests → merge only if no regressions → expand corpus → repeat.

**Limits:** A finite collection of tests does not prove 100% compatibility, and abundant tokens alone do not guarantee convergence, but this architecture maximizes objective feedback, prevents fake success, and makes systematic progress measurable. Never present a partial pass rate as full compatibility.

## 1. Repository topology

Add to RFC 001's workspace as actual responsibilities are implemented (do not scaffold empty crates):

```text
crates/pgarbor-testkit/          # canonical test events and comparator library
crates/pgarbor-fault/            # reference model, injected I/O and crash scheduler
crates/pgarbor-bench/            # performance measurements separate from correctness
harness/
  orchestrator/                  # task scheduler, leases, ownership, merge decisions
  adapters/postgres/             # pinned server, driver and official-suite ingestion
  adapters/pgarbor/             # embedded, TCP and CLI test adapters
  fixtures/orms/                 # unmodified SQLx, node-postgres/Drizzle, Prisma apps
  suites/                        # PG corpus index, differential, protocol, isolation
  generated/                     # reproducible cases: stored by seed and reducer
  dashboards/                    # durable evidence, conformance atlas, metrics
  policies/                      # protected test/checker rules; path permissions
  tools/                         # local harness CLI
ci/                              # trusted, isolated merge verifier jobs
spec/feature-atlas.yaml          # complete PG18 feature catalog and dependencies
```

Keep the reference binary, golden corpus, and verifier implementation **outside worker-writable source** in CI. Local copies are for feedback only. Compare from a clean git checkout of the proposed patch; workers may not submit their own modified acceptance rules.

## 2. Two independent families of oracles

### A. PostgreSQL behavioral parity

**Reference:** PostgreSQL 18.x official binaries/source, fixed patch release, OS/runtime, locale, encoding, timezone, collation provider, GUCs, driver versions, and known extensions. A release bump is an explicit corpus migration, not silent version drift.

**Differential driver:** initialize equivalent datasets, run the same ordered SQL and protocol interactions on PG and PGArbor, record typed results, errors (SQLSTATE, severity, selected structured fields), affected rows, columns/type OIDs, catalog introspection, session state, transaction outcomes and server messages. Judge via typed semantics, not diffing arbitrary pretty-printed strings.

**Normalization rules:** only normalize fields proven nondeterministic by the PostgreSQL specification or pinned environment. Never sort rows if `ORDER BY` determines order; for unordered query results, compare multisets while preserving duplicates. Control `now()`, UUIDs, random inputs and seeds where possible. Do not normalize SQLSTATE, NULLs, type OIDs, transaction result, or relevant precision away.

**Multi-session semantics:** replay controlled schedules where outcomes are deterministic. When multiple outcomes are legal, compare against a permitted-outcome model and use a transaction-history consistency checker rather than requiring identical thread interleavings or lock timing. Test deadlocks, aborted transaction state, SSI, statement snapshots, locks and savepoints independently.

**Official suites:** ingest `src/test/regress/sql` and expected outputs for PostgreSQL 18 as provenance-tagged cases. Use `pg_regress` against a foreign wire-compatible server where applicable (`make installcheck` may initialize/drop regression databases and manipulate global objects). Preserve official scripts; add small adapters to handle expected harness bootstrap features; do not quietly skip incompatible tests. Then expand to `src/test/isolation`, protocol/libpq, client tools, recovery/subscription and extensions. A test that cannot run has status `BLOCKED`, **not** `PASS`.

**External test generators:** grammar-based and type-aware generation, sqllogictest-style cases, SQLancer-style metamorphic oracles (TLP and NoREC), generated SQL from public PostgreSQL docs, and real driver/ORM introspection traces. Distinct independent checkers counteract bugs in a single test harness.

### B. Storage, crash and branch invariants

An independent tiny reference model of branch/snapshot transactions: `create/open/write/read/commit/rollback/branch/delete/reset/export/compact/crash/reopen`. It is deliberately simple and slow; its job is correctness, not performance. Compare the production engine's observations with the model under generated operation sequences.

Inject faults at every persistence boundary: short/partial writes, torn pages, reordered or lost flushes within the declared filesystem model, out-of-space, read failures, kill/restart, interrupted compaction and export, simultaneous branch mutations, GC with pinned snapshots, and unexpected process exit. Use deterministic virtual I/O/time and replayable scheduler seeds. Reopen after every permitted crash; check data integrity, acknowledged-commit durability, isolation of sibling branches, index-to-row coherence and no visible partial transactions.

Protect against filesystem-model overclaims: model-level passing is necessary but not sufficient; also run platform-specific real-process kill/restart and hardware/filesystem integration tests. Keep explicit limits for power-loss claims.

**Non-negotiable properties:** no corruption, no acknowledged-data loss within configured durability guarantees, no cross-branch mutation leakage, no successful partial commit, no dangling references after GC, and no false success on export. These failures override any improvement in SQL feature counts.

## 3. Test case schema and evidence

Every case has immutable machine-readable metadata:

```yaml
id: pg18.jsonb.path-000214
family: postgres-differential
reference: postgres:18.x@pinned-image-digest
feature_ids: [jsonb, expressions, operators]
required_capabilities: [create-table, insert, select, jsonb]
setup_sql: |
  CREATE TABLE t (v jsonb);
  INSERT INTO t VALUES ('{"a": [1, 2]}');
actions:
  - session: a
    sql: "SELECT v->'a'->>0 AS x FROM t"
expectation: typed-reference-result
seed: 418212
normalization: defined-in-verifier-policy
provenance: generated-from-postgresql-doc-example
```

Run artifacts include code commit SHA, oracle SHA/image digest, environment settings, seed, deterministic schedule, action transcript, PG observed outcome, PGArbor observed outcome, minimized case, exact machine-readable diff, and classification. For failure cases persist a *copyable reproduction package*; patch acceptance must be reproducible from a clean checkout.

Failure labels: `PASS`, `MISMATCH`, `CRASH`, `HANG`, `CORRUPTION`, `UNEXPECTED_ACCEPTANCE`, `UNSUPPORTED`, `BLOCKED`, `FLAKY`, `INVALID_CASE`, `HARNESS_ERROR`. Only `PASS` counts as pass. `UNSUPPORTED` and `BLOCKED` remain visible in denominator/capability coverage; don't promote unsupported statements to known-compatible by relabeling.

## 4. Feature atlas and task graph

The feature atlas enumerates PostgreSQL 18's grammar, types/functions/operators, catalogs, SQLSTATE, extended protocol, transaction/isolation semantics, extension interfaces, roles/security, dumps/imports and driver APIs. Each feature has mandatory behavior cases, negative/error cases, protocol checks, concurrency interactions and dependencies. Source from official docs, SQL catalogs, source tests, protocol specs and generated cross-feature combinatorics.

At the start, almost all PG tests will fail for predictable missing foundations. Don't launch hundreds of agents against that red wall. Select a narrow **vertical slice** with prerequisites (e.g. CREATE TABLE → INSERT → SELECT → result metadata → COMMIT → reopen), then progressively widen the active suite, always reporting remaining uncovered total corpus. Prioritize bottleneck features that unlock many blocked cases.

For each reproducible discrepancy, the planner clusters by root cause (parser/binder/catalog/driver/storage/transaction rather than one ticket per failing statement), minimizes reproductions, identifies source ownership and dependencies, and issues a finite task with acceptance tests. Agents can suggest novel test cases but must not alter reference results to make themselves green.

## 5. Orchestrator: deterministic state machine

Suggested ticket states:

```
DISCOVERED -> REPRODUCED -> MINIMIZED -> READY -> LEASED
  -> IMPLEMENTED -> LOCAL_VERIFIED -> TRUSTED_VERIFICATION
  -> MERGED -> COMPLETE

Failure paths:
  READY <- FAILED_VERIFY | MERGE_CONFLICT | REGRESSION
  BLOCKED(reason) for missing prerequisites
  QUARANTINED(reason) for nondeterministic or invalid tests
```

A task is `COMPLETE` only after independently verified code has merged; no agent self-report can mark it complete. Leases expire and are renewed; crashed agents automatically release work. Persistent task state lives outside ephemeral model context. Each task records exact base SHA and affected module contracts.

**Scheduler:** one planner owns the feature graph and blocker prioritization; a bounded pool of workers own narrow, testable tasks in distinct git worktrees; an independent compiler/test verifier handles acceptance; an occasional architect/reviewer handles interface changes and suspected specification/oracle bugs. Use optimistic branch integration, not a single shared mutable working tree. Retry after rebase, never force-push over another worker's work. If interface ownership isn't stable, serialize that stage rather than generating incompatible parallel implementations.

**Dynamic worker selection:** cheap/fast model for targeted parse, error-code and test fixture tasks; stronger model for file format, MVCC, planner architecture, concurrency and failed escalations. Workers have bounded context, wall-clock budget, compile budget, CPU/memory budget and failure-retry cap; stuck tasks are subdivided/escalated, not looped forever. Initially 2–4 coding workers and one trusted test runner; adapt based on actual CPU/RAM/CI throughput. Do not launch one Postgres/Docker cluster per worker: reuse reference server with isolated databases or launch bounded instances when isolation needs it.

## 6. Verification gates and anti-reward-hacking

**Local fast gate:** format/lint/compile + targeted tests; an agent can iterate rapidly.

**Trusted merge gate (separate runner):** clean checkout; verify changes only touch permitted paths; restore test corpus/checkers from protected revision; recompute oracle expectations; run reproduction + prerequisite feature tests + fixed regression suite + storage invariants + relevant multi-session tests; inspect diff for disabled tests, unwrap-to-default, fake data, hardcoded outputs, weakened errors/constraints, hidden-test introspection, unsafe bypasses and global stub behavior. Rebase and retest if the baseline changed.

**Nightly/full campaign:** official PG regression and applicable auxiliary suites; all previously fixed cases; mutation and metamorphic fuzzing; deterministic storage simulation; cross-platform crash/reopen; ORM migrations; instrumentation/sanitizers; benchmarking separated from conformance. Run rotating unseen generation seeds and holdout fixtures from read-only infrastructure. Require periodic checking against PostgreSQL itself to detect errors in the normalizer.

Changes to test rules, expected outputs, reference version, or compatibility coverage taxonomy use a distinct review path. They cannot be bundled with a worker feature patch. Do not incentivize lowering the denominator or skipping cases. Scores are informative, not incentives that trump correctness.

Merge when: required behavior tests pass, no loss of existing pass set, zero new critical invariant violations, no disabled test/changed verifier and review has handled catalog/interface cross-effects. Prefer honest `BLOCKED` over a counterfeit green check.

## 7. Task acceptance card example

```
TASK PG18-JSONB-0421
Root cause: JSONB path scalar extraction returns JSON rather than text.
Reference: PostgreSQL 18.x, exact image digest saved.
Reproducer: tests/generated/pg18-jsonb-0421.yaml
Expected: typed text '1', PG type OID 25, no error.
Scope: Rust type/operator resolver and expression evaluator.
Forbidden: changes to canonical oracle, comparator, existing expected values.
Dependencies: SQL SELECT, JSONB storage, row decoding.
Acceptance:
  1. Reproducer matches PG exactly.
  2. Existing JSONB differential corpus remains green.
  3. Added negative/NULL/path-missing cases match PG.
  4. Trusted CI runner records PASS at merged SHA.
```

## 8. Conformance dashboard

Publish raw totals and denominator by feature family, reference release, platform and test suite. Track:

- `passed / runnable / total cataloged` (three denominators shown, never collapsed into one misleading number)
- `unsupported`, `blocked`, `mismatched`, `crashed`, `flaky`, `invalid`
- `previously fixed cases still passing` (regression retention)
- `fresh unseen fuzz-seed pass rate` and `counterexamples per seed-hour`
- `critical storage errors` (must be zero)
- `branch operation latency`, `file size`, `physical shared bytes`, `write amplification`
- `real ORM workflows passing unmodified`, by driver/version
- `compute spend / verified behavior improvement` for agent selection

Ensure reference PG itself passes relevant official suites on the pinned environment, and periodically test the verifier by deliberately introducing a known behavior mutation. A harness incapable of flagging a deliberately broken engine is not a trustworthy harness.

## 9. First six implementation increments

**0. Name and repository preflight:** clear exact namespaces as required by RFC 001; rename immediately if collision, before creating package names. Pin PG 18.x distribution by digest and deterministic environment.

**1. Oracle first:** build a tiny driver-independent diff runner able to compare *two PostgreSQL 18 instances*. Add a deliberately incorrect comparator fixture so the harness proves it can report errors and preserve exact reproductions. No PGArbor engine required yet.

**2. Reference storage model:** define abstract key/page log operations and branch roots. Add state-machine/property testing, crash scheduler and single-file persistence before SQL.

**3. Vertical-slice engine:** `CREATE TABLE`, `INSERT`, `SELECT`, `COMMIT`, reopen, and branch isolation, all tested against appropriate PostgreSQL behavior and independent storage invariants.

**4. Wire/protocol and ORM vertical slice:** real client `psql`, basic extended-protocol Parse/Bind/Describe/Execute, stable type OIDs and catalogs, migrations and typed CRUD from at least one **unmodified** app fixture.

**5. Autonomous compatibility expansion:** enable failure reduction → root-cause clustering → dependency-aware ticketing → bounded parallel workers → trusted merging; track feature coverage and nightly fuzz sweeps. Only widen parallelism after the harness is stable.

**6. Durability + concurrency challenge:** orchestrated interleavings, serializable histories, crash injection, corruption checks, garbage collection and branch export across filesystems. Never trade these invariants for SQL pass-rate improvements.

## 10. Suggested local commands (proposed, not implemented)

```bash
pgarbor-harness doctor
pgarbor-harness oracle verify --pg-version 18
pgarbor-harness diff --suite smoke
pgarbor-harness diff --case PG18-JSONB-0421 --minimize
pgarbor-harness simulate --seed 81433 --faults disk,crash,oom
pgarbor-harness tasks plan --from failures --max-workers 4
pgarbor-harness swarm run --suite active --workers 4 --until milestone-gate
pgarbor-harness verify --commit <sha> --trusted
pgarbor-harness report --reference pg18 --format html
```

Avoid a 24/7 unlimited loop by default. Define daily spend/CPU/thermal budgets, milestone-specific termination, alerting on persistent hard blockers, and nightly integrity campaigns. A supervised long-running process is justified once the harness, test oracle and patch gates are demonstrably sound.

## 11. Design principles

1. **Tests precede agent scale.** Hundreds of workers without a trustworthy verifier produce code volume, not correctness.
2. **Correctness has independent oracles.** PG for SQL/protocol, model/fault injection for storage, history checkers for transactions.
3. **Never game the tests.** The verifier owns success, not the coder.
4. **Small end-to-end slices unlock features.** A million failing tests with no working engine has no educational value.
5. **Failure is a first-class artifact.** Minimized replayable counterexamples are the primary unit of work.
6. **Branch fidelity is separate from SQL fidelity.** A PG test pass cannot compensate for lost writes.
7. **100% remains the destination.** Publish evidence instead of claiming logical certainty from finite tests.

## Sources and precedents

- Cursor, *Scaling long-running autonomous coding* (2026): https://cursor.com/blog/scaling-agents
- Cursor, *CursorBench* (2026): https://cursor.com/blog/cursorbench
- Cursor, *Reward hacking is swamping model intelligence gains* (2026): https://cursor.com/blog/reward-hacking-coding-benchmarks
- PostgreSQL 18, *Regression Tests*: https://www.postgresql.org/docs/18/regress.html
- PostgreSQL 18, *Running Tests*: https://www.postgresql.org/docs/18/regress-run.html
- PostgreSQL isolation suite: https://github.com/postgres/postgres/blob/master/src/test/isolation/README
- SQLancer: https://github.com/sqlancer/sqlancer
- SQLite `sqllogictest`: https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki
- FoundationDB deterministic testing: https://apple.github.io/foundationdb/testing.html
- Jepsen Elle: https://github.com/jepsen-io/elle

**RFC scope:** Architecture proposal and acceptance criteria only; no harness or database is claimed implemented.
