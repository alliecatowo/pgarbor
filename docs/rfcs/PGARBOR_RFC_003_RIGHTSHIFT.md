# PGArbor RFC 003 — RightShift: Speculative Swarm Development

**Date:** 2026-10-14  
**Status:** Proposed engineering protocol; no claimed running implementation  
**Supersedes:** RFC 002 §§5–6's per-worker compile/test and per-patch verification cadence; retains RFC 001 product invariants and RFC 002 independent oracle/correctness model.  
**Companion:** `PGARBOR_RIGHTSHIFT_TASK_GRAPH.json` (209 dependency-checked tasks), `PGARBOR_RIGHTSHIFT_OVERVIEW.mmd`, `PGARBOR_ULTRACODE_RIGHTSHIFT_BOOTSTRAP.md`.

> **Goal:** Build a clean-room, Rust, single-file, branch-native database targeting **complete behavioral parity with a pinned PostgreSQL 18 release**. Do not redefine the compatibility goal downward. PostgreSQL and model-based storage verification determine what is complete. We optimize when expensive verification occurs, not what correctness means.

## 0. What RightShift changes

Traditional shift-left: build/test/review every small change, repeatedly rebuilding a shared dependency graph. RightShift: strongly specify interfaces, fan out huge waves of independent implementation without requiring every worker to build or test, integrate in batches, then run expensive conformance campaigns on a single assembled artifact. A failed campaign creates the next task DAG.

**Do not confuse absence of worker verification with permission to fake correctness.** Each worker supplies actual implementation and its intended test cases. An autonomous compiler/test verifier has sole authority to mark the assembled result as verified. Model reviews can catch obvious interface drift without executing binaries; their confidence is advisory, not pass/fail evidence.

**Three modes, controlled by the supervisor:**

| Mode | Timing | Intended use |
|---|---|---|
| `cathedral` (default experiment) | No Rust build or suite per coding agent. Assemble a wide implementation epoch, then one clean build and large campaign. | Fastest exploration of the user's hypothesis; maximum integration risk. |
| `seams` | One centralized build at an architectural seam (e.g. file API → transaction API, binder → executor), never per worker. | Fallback when mismatched interfaces prevent progress. |
| `conventional` (control arm) | Narrow compile/test per patch, small verified merges. | Experimental comparison; not quietly substituted for the chosen approach. |

Measure engineering throughput in **verified PostgreSQL behaviors per compute dollar/wall-clock**, not number of commits, tasks or tokens. Keep accurate CPU/IO and token counters. `cathedral` is not automatically superior, so compare it empirically with `conventional` on matching subgraphs and randomized task partitioning.

## 1. Hypothesis and limits

**Hypothesis:** When cheap agents can follow precise API contracts, the savings from avoiding thousands of redundant builds and oracle starts exceeds the extra integration and repair cost of late feedback.

The risk is **compounding contract entropy**. If a foundational trait is interpreted differently across 100 implementations, late integration may be more expensive than all the builds saved. A design document alone cannot make code correct, and no finite test suite mathematically guarantees all PostgreSQL behavior. The mitigation is to treat contract freezing, ownership, and evidence tracking as non-optional while aggressively right-shifting execution.

We retain two truths at once: **100% parity is the uncompromising product objective**, and **a test campaign provides measured evidence rather than magical certainty**.

## 2. Two dependency graphs, not one

Every task has:

- `contract_dependencies`: API/schema artifacts that MUST be frozen and distributed before writing code. Freeze does *not* require the upstream implementation to exist.
- `integration_dependencies`: concrete components needed for an end-to-end binary; these do NOT block speculative work when the contracts exist.
- `ownership_domain`, `owned_path_glob`: a code-writer lease, refined down to non-overlapping files/modules. Two workers must never concurrently edit the same files without an explicit merge experiment.
- `deliverable`: code or documentation artifact with a falsifiable behavioral purpose.
- `deferred_verification_family`: which centralized verifier will examine it later.

Example: the JSONB evaluator may begin before B+tree storage works, because the `Value`, `TypeOid`, coercion, and expression-evaluation contracts are frozen. Its completion means "implementation submitted", not "known correct".

**Contract state machine:** `DRAFT -> RATIFIED -> FROZEN@SHA -> AMENDMENT_PROPOSED -> RATIFIED_NEW_EPOCH`. Only the architect/contract steward can ratify changes. Workers file explicit contract breakages with suggested modifications; they cannot silently redefine the canonical trait in their implementation worktrees.

**Implementation state machine:** `SPEC_READY -> SPECULATING -> PATCH_SUBMITTED -> ASSEMBLED -> CAMPAIGN_FAIL | CAMPAIGN_PASS -> REPAIR_QUEUED | VERIFIED`. A compile failure is not a worker quality metric; it is evidence that the assembled graph needs reconciliation. No `VERIFIED` without trusted campaign evidence and a precise repository SHA.

## 3. Build the contracts hard

Before a 500-agent implementation wave, a smaller high-reasoning architect team produces and freezes:

1. Canonical Rust trait/function signatures and type IDs for storage, branches, snapshots, transactions, SQL values, error states, catalog lookup, AST/IR, logical plans, execution, wire messages and embedded API.
2. Behavioral contracts: null/cast/error conventions, ownership/lifetime/locking assumptions, sync durability guarantees, branch-root atomicity, row visibility, prepared statement state, SQLSTATE taxonomy, session boundaries and dependency direction.
3. File and crate ownership map. Implementation workers know where to write and which imports are permitted. One authoritative module publishes each shared trait, not N independently recreated interfaces.
4. Small illustrative examples and golden traces (derived from PostgreSQL where appropriate) to prevent semantic interpretation drift.
5. Contract amendments: urgent changes flow through a single canonical amendment ticket and broadcast to affected workers. Do not let the loudest implementation agent redefine public APIs.

Contract reviews may use tokens aggressively (architecture debate, cross-domain adversarial reviewers). Runtime verification is not required at this stage; explicit type declarations and design artifacts are the source of truth. Avoid busywork scaffolding: do not create hundreds of empty crates or mocks that masquerade as functionality.

## 4. Swarm composition and concurrency

**Supervisors:** one scheduler/architect, one contract steward, one failure classifier, one central integrator, one verification coordinator. Separate expensive model roles are reserved for API decisions, concurrency and architectural repair; cheap agents implement scoped components or mine source behavior.

**Workers:** ticket authors in isolated Git worktrees, ideally bounded to unique leaf modules. Each gets a thin, versioned contract context instead of the entire repository. Each must return a patch/commit, assumption ledger, implemented behaviors, expected integration points, and test cases **not necessarily executed**.

**Research/cargo-cult workers:** source readers harvest reference facts from PostgreSQL 18, SQLite, Neon, Turso, SQLancer, and SQL logic test tooling. Convert them into original-language behavior cards and minimal reproducible examples. Keep original source citations, license constraints, and behavior-version tags. Do **not** blindly paste GPL-incompatible material, PG C code, or copied implementations without license/provenance review. PostgreSQL and SQLite reference implementation details are useful; engine structure must still conform to our Rust architecture. Reference clones for tests are not runtime dependencies.

**Review workers:** read-only reviews with no cargo commands. They identify likely contract breaks, fake implementations, no-op error handling, weird behavioral interpretation, overlapping edits, and ambiguous source semantics. They do not award PASS.

**Ownership rule:** massively parallel agents may research/plan/speculate on overlapping topics; only **one author lease** writes a given actual path. When two implementations of a leaf algorithm are desirable, place them in separate experiments and select one during assembly rather than interleave patches into the same file.

**Resource rule:** budget active authors by memory, file contention and orchestration overhead, not available token quota alone. Agents do not each start Docker, PostgreSQL, Cargo, LLVM, or the whole test suite. Reference PostgreSQL instances are pooled when campaign time arrives.

## 5. The four manufacturing epochs

### Epoch 0: Spec and ownership freeze

Complete naming preflight **before scaffolding**. If the project name collides with a repo, crate, or software namespace, pick and verify a new name immediately, then update *all* planned identifiers in the manifest. Do not bake a collided provisional name into crates. Freeze the cross-crate contract ledger, testing oracle schemas, product invariants and task DAG.

This epoch may itself fan out tens of reference-reading agents but uses a small central design authority. `JOIN-01` captures `FROZEN@SHA`.

### Epoch 1: Massive speculative foundation

Authorize independent workers across file encoding, B+trees, append log, crash recovery, snapshot/pin model, branch roots, MVCC, types, catalog, SQL lexer, binder, PostgreSQL oracle fixtures, and the orchestration harness. Some implementation dependencies will not exist; agents implement strictly against frozen contracts anyway.

**Default:** no per-worker `cargo build`, `cargo test`, `clippy`, Docker, PostgreSQL startup or broad CI. A code-only reviewer may flag likely nonsensical patches; the integrator batches committed work into `epoch/01-assembly`. Resolve textual merge conflicts deterministically; escalate semantic conflicts to `SWARM-09` rather than silently guess.

### Epoch 2: Query engine, wire, client compatibility and parity breadth

Open planner, executor, SQL DML, DDL, MVCC locking, catalog views, PostgreSQL type encodings, wire protocol, CLI, branch URL routing, unmodified ORM integration fixtures and advanced feature families. Continue implementing ahead of integration where contracts permit. Avoid "hello world" stubs presented as done.

Assemble a coherent vertical slice: create `.pga` → create table → insert rows → commit → shutdown → reopen → branch → mutate child → connect psql via PG wire → query both branches. Test fixtures are authored but not executed per worker.

### Epoch 3: Integration and first expensive verification

Run `cargo build --workspace` **once centrally** at the assembled epoch SHA, not across 700 worktrees. Classify compile errors into root causes rather than mailing one error per worker. A bounded repair mini-swarm reconciles API drift; build again at the new integrated SHA. This integration sub-loop exists because an uncompilable binary cannot run the behavioral campaign.

When the program is runnable, execute a single **full evidence campaign**: fundamental file invariants, crash/fault seeds, branch isolation, PostgreSQL typed differential cases, core official regressions with discovered capability prerequisites, PG isolation cases where applicable, psql/SQLx/Drizzle/Prisma fixtures, basic performance and single-file relocation. Record *all* failures and uncovered tests, not just the successes.

### Epoch 4+: Conformance repair campaigns

Freeze complete campaign report. Cluster mismatches by common root cause, shrink reproductions, rank fixes by expected unlock volume and dependency centrality, then generate a new ticket DAG. Patch in another speculative batch. Reintegrate and verify at next epoch boundary. Continue until the pinned PostgreSQL 18 feature inventory and conformance suites have no unresolved mismatches *and* all storage/branch durability requirements are satisfied; continue adversarial and unseen testing to seek counterexamples. Every subsequent version reopens the frontier.

There is no manufactured "we're done" based on 100% pass of a cherry-picked denominator. Explicitly classify `PASS`, `FAIL`, `UNSUPPORTED`, `BLOCKED`, `UNRUN`, `FLAKY`, and `INVALID TEST`.

## 6. Verification campaign as a scarce shared resource

A campaign is one orchestrated verifier execution (potentially many test shards), not necessarily a literal single process.

1. **Clean build:** single target directory/cache, pinned toolchain and dependency lockfile, deterministic artifacts. Use `sccache` and shared caches where supported. Compile failure clustering precedes SQL tests.
2. **Storage model:** property-based state-machine oracle on put/get/commit/reopen/fork/reset/GC/export with deterministic fault injection. Check acknowledged-commit durability, isolation, allocator reachability, checksum, and long-lived readers.
3. **PostgreSQL reference:** pinned PostgreSQL 18 instance(s); same SQL and session transcripts, typed row values, type OIDs, error SQLSTATE, catalog metadata and transaction outcomes.
4. **Official PG suites:** core `pg_regress`/`installcheck`, then PostgreSQL isolation schedules, with explicit capability prerequisites and isolation environments. These should be treated as distinct families, not all claimed covered by core regression pass.
5. **Logic fuzzing:** `sqllogictest`, SQLancer-like metamorphic transformations, deterministic grammar fuzzing and unseen holdout seeds. Sqllogictest checks query results, **not** durability or concurrency.
6. **Real clients:** unmodified `psql`, `libpq`, SQLx, `pg`, Drizzle and Prisma migration/introspection fixtures; no modifying app test code to accommodate PGArbor.
7. **Compatibility inventory:** each documented PG18 feature linked to tests or marked uncovered/blocked; a feature not exercised is not recorded as PASS.
8. **Evidence pack:** exact git SHA, platform/target, rustc version, PostgreSQL image digest/flags, test corpus hashes, random seeds, test resource spend, and minimized mismatches.

**Amortization:** one PostgreSQL reference startup per isolated shard or pooled bounded instance; no per-worker containers. Snapshot/restore known seed DBs, deduplicate identical expected-result generation, pre-record reference outputs, content-address build artifacts, test only the integrated SHA. When a big batch is known broken at compile stage, do not launch high-cost PG runtime tests prematurely.

**Do not allow agents to pass by changing the verifier**: freeze it separately. Reference/normalizer fixes require independent review and an anti-mutation check. Don't delete tests, skip cases, suppress errors, turn unimplemented features into fake empty outputs, or silently alter the expected denominator.

## 7. Failure-driven next-wave prioritization

The campaign generates a structural diagnostic graph:

- **Compile cluster:** unknown trait method, mismatched type, missing export → one interface-drift cluster, potentially affects 60 leaf tasks.
- **Semantic cluster:** PG expected `numeric`, PGArbor returned `float` in 5,000 cases → one numeric/coercion cluster.
- **Durability cluster:** committed root lost after fsync interruption → critical storage stop-the-line issue, independent of SQL parity.
- **Protocol cluster:** Parse/Bind parameter OID mismatch from one client → implement wire/cast/catalog correction, replay many ORM cases.
- **No progress cluster:** 40 retries same failure with no newly covered tests → escalate architect/model, revise assumptions or reference implementation approach, not infinite token consumption.

Priority is a heuristic, not a scientific guarantee:

`priority ≈ severity × expected_unblocked_cases × dependency_fanout × confidence / estimated_compute_cost`

Critical durability/corruption/invariant failures override scores. Work must still be allocated based on code ownership and coherent contracts; otherwise the scheduler creates conflict storms.

Repair tickets contain: root-cause hypothesis, exact repro/shard/seed, expected PG behavior, actual PGArbor behavior, impact graph, paths owned, frozen contract version, submission requirements, and next-campaign acceptance. No repairs may erase failing fixtures or weaken the comparator.

## 8. The graph and wave topology

`PGARBOR_RIGHTSHIFT_TASK_GRAPH.json` contains **209 actionable tasks in 16 implementation domains and 344 explicit integration-dependency edges**, automatically validated for missing IDs and cycles. This is the seed DAG. It is *not* 209 simultaneous authors in the same crate: code ownership must be split into nonoverlapping concrete files before leases. The scheduler should dynamically expand leaf tasks from PostgreSQL source chapters, regression cases, SQLSTATE families, extension interfaces and fault seeds, potentially to hundreds or thousands of tickets. No arbitrary "607" or "739" hardcoded scale target.

Major dependency arcs: `ARCH → STORE/BRANCH/TXN/TYPE/CAT/SQL → PLAN/EXEC → WIRE/CLI → APP → verification`. In parallel: `ARCH → ORACLE`, `ARCH → SWARM`. `PG` advanced parity families fan out as far as their contracts allow. Integration follows implementation; **speculative execution does not need integration dependencies to be complete**.

Example campaign command interface (proposed, not implemented):

```sh
pgarbor-factory plan import PGARBOR_RIGHTSHIFT_TASK_GRAPH.json
pgarbor-factory contracts freeze --sha <commit>
pgarbor-factory swarm launch --mode cathedral --max-authors 120 --cpu-budget 12 --ram-budget-gb 24 --epoch 1
pgarbor-factory assemble --epoch 1 --base <locked-sha>
pgarbor-factory campaign run --epoch 1 --reference pg18 --resource-profile shared
pgarbor-factory graph repair --from runs/epoch1.json --maximize-unblocked
pgarbor-factory swarm launch --epoch 2 --mode cathedral
```

These are specification-level commands. No actual CLI or implementation is claimed.

## 9. Cargo-culting, done strategically

We deliberately learn from established systems, but do not turn the project into a jigsaw of incompatible architectures.

| Source | Extract | Must not blindly transplant |
|---|---|---|
| PostgreSQL 18 | Grammar/casts, `pg_catalog`, OIDs, SQLSTATE, planner semantics, transaction and wire behavior; regression tests | C process model, in-memory structs or native C extension ABI pretending to be Rust-native |
| SQLite | Atomic file format patterns, page format/recovery thinking, db state machine, sqllogictest | SQLite semantic differences and locking model as PG behavior |
| Neon | Copy-on-write history, storage pointers, branch lineage and write separation | Cloud-centric architecture or unsupported promise of single-file physical portability |
| Turso | Rust virtual machine/storage implementation lessons and frontend separation | Its explicitly partial PostgreSQL compatibility contract |
| FoundationDB | Deterministic simulation/fault scheduling ideas | Distributed-service complexity in a local single-file DB |
| SQLancer / sqllogictest | Generated reference differential and metamorphic oracles | Treating query result agreement as durability proof |

**Research ticket format:** hypothesis → upstream source/commit/link → observed PG18 behavior → representative SQL + expected typed result → integration contract → implementation sketch → provenance/license note. Reference cards are high-value inputs to the implementation swarm. The swarm can "cargo-cult" behavior with genuine understanding and original implementation instead of cargo-culting source code.

## 10. Pragmatic experiment design: show RightShift actually wins

Run two isolated matched subgraphs (e.g. `TYPE` expression operations vs same-size alternate expression family):

- **Cathedral arm:** only frozen contracts before large speculative coding; one campaign at epoch boundary.
- **Conventional arm:** narrow tests/compiles per patch; same model family, similar task count and hardware budgets.

Metrics: total tokens, compute-minutes, disk read/write, cloud cost, wall-clock, merged-compilable fraction, PG test coverage growth, unseen test pass rate, repair overhead, number of integration conflicts, regression carry-forward, detected durable corruption. Blindly choosing a faster arm by *lines of code* is invalid.

Decision point after the first complete integrated campaign. If cathedral wins, scale it. If 10,000 compiler failures reduce to three contract errors, retain cathedral and invest in contracts. If failures are independent but scattered across 700 patches, introduce targeted centralized seam builds rather than reverting to 700 full builds.

## 11. Failure policies

- **Naming collision:** stop before namespacing; choose and verify a different name; regenerate manifests and locked contracts. No "temporary project name" in committed crates.
- **Shared contract drift:** suspend dependent worker leases; create one amendment, broadcast version; don't silently permit multiple incompatible APIs.
- **Bad test/oracle:** quarantine fixture with visible evidence, independently fix harness, rerun; do not accept a feature patch that changes its own expected results.
- **Critical data loss or corruption:** freeze release candidate and prioritize storage fix ahead of additional SQL feature breadth.
- **Out-of-resources:** pause work spawning; preserve patch/task state; leave existing leases resumable; cache artifacts centrally.
- **Stalled progress:** restructure task rather than blindly re-ask 100 cheap agents. Escalate semantic blockers to strong reasoning model; use competing implementations only in explicitly isolated experiments.
- **Claims:** `IMPLEMENTED` is not `VERIFIED`; even a green full campaign demonstrates tested coverage and reported limitations, not a mathematical proof of universal parity.

## 12. Concrete initial swarm allocation (recommendation, adaptable)

**Contract wave:** 8–16 senior reasoning/review agents; 12 architecture tasks, plus up to 13 oracle catalog tasks and 15 harness tasks. Outputs are signatures, frozen dependency maps and reference evidence.

**Build wave:** start with 40–120 concurrent code authors, constrained by ownership and compute resources. Let hundreds of additional *research* agents extract documented PG behaviors and generate isolated conformance/feature cards without touching common Rust files. Expand active author count only when there are sufficient genuinely disjoint leaf implementations; don't inflate agent counts for display.

**Integrate:** one coordinator and bounded compile repair agents. Then one centralized evidence campaign (sharded in parallel as machine resources permit).

**Repair:** dynamic fanout based on clustered failures and graph unlocks, not arbitrary permanent worker roles. Agents may spawn subagents for sharply bounded subtasks, but their parent is responsible for returning the artifact.

## 13. Near-term acceptance

The *factory* must eventually demonstrate, without human intervention: load task graph → freeze contracts → lease disjoint tickets → collect patch artifacts → assemble epoch → invoke one centralized build → classify errors → run PostgreSQL + storage oracles → automatically generate next wave. The *database* must pass the RFC 001 north-star demo: one portable `.pga`, reopen, branch, isolate schema changes, connect unmodified PG clients, migrate a real ORM fixture, and survive modeled failures.

**The intent is ferocious code generation and relentless verification — but verification is concentrated at deliberate epoch boundaries rather than sprayed across every worker.**

## References

- Cursor, *Scaling long-running autonomous coding*: https://cursor.com/blog/scaling-agents
- SQLite, *sqllogictest*: https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki
- PostgreSQL, isolation suite README: https://github.com/postgres/postgres/blob/master/src/test/isolation/README
- PostgreSQL, regression test workflow: https://www.postgresql.org/docs/18/regress-run.html
- FoundationDB, deterministic simulation: https://apple.github.io/foundationdb/testing.html
- Neon source: https://github.com/neondatabase/neon
- Turso source: https://github.com/tursodatabase/turso
