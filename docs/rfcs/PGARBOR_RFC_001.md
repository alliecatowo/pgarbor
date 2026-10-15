# PGArbor
## RFC 001 — Vision, Product Specification, and Engineering Architecture

**Status:** Proposed design, implementation not started  
**Date:** 2026-10-14  
**Language:** Rust (greenfield engine)  
**PostgreSQL reference baseline:** PostgreSQL 18.x, pinned in CI; track PostgreSQL 19 after general availability  
**Repository/package prefix candidate:** `pgarbor`. No obvious exact-name database/software collision appeared in searches on 2026-10-14, but crate, npm, domain, GitHub repo, and trademark availability have NOT been definitively established. No namespace is reserved. **Do not scaffold until registry ownership has been checked; if unavailable, rename all artifacts before generating code.**

> **PostgreSQL without the server. SQLite without the limitations. Neon without the cloud.**
>
> A portable, branch-native, one-file relational database built in Rust to become a behaviorally compatible alternative to PostgreSQL — especially for local development, CI, and autonomous agents.

---

## 0. Decisions at a glance

| Decision | Resolution |
|---|---|
| Engine | Greenfield Rust SQL engine, planner, execution engine, catalog, transactions, and storage |
| Compatibility ambition | Full behavioral compatibility with a pinned PostgreSQL major version; no deliberately permanent PostgreSQL-subset product positioning |
| Reference | An actual PostgreSQL 18 server and official documentation, source, and regression tests |
| Durable storage | Exactly one authoritative `.pga` file per PGArbor database container; no required WAL or shared-memory sidecars |
| Branching | Persistent internal branch roots sharing immutable pages, plus optional tiny external `.pgabranch` handles |
| Standalone branches | Export/detach branch into an independent `.pga`; prefer filesystem copy-on-write clone when supported, otherwise compact/copy |
| Deployment | Embedded Rust API or transient foreground/background PostgreSQL-wire server, no Docker required |
| Development principle | Ship a useful, correct vertical slice early, but never declare full PostgreSQL compatibility until earned |
| Correctness | Differential PostgreSQL tests, crash fault injection, property/state-machine tests, migration/ORM conformance |
| Scope | A new database engine, not a wrapper around SQLite, Turso, or PostgreSQL |

## 1. Problem and opportunity

Developers deploy PostgreSQL in production but frequently suffer unnecessary complexity locally: containers, volumes, service management, startup latency, test-environment conflicts, credential setup, forgotten state, and difficult database cloning. SQLite simplifies local storage but behaves differently from PostgreSQL on data types, SQL, locking, migrations, catalogs, and protocol behavior. Cloud database branching solves isolation but introduces an external service.

PGArbor intends to make a PostgreSQL-style database as easy to create, store, copy, and delete as an ordinary file, while supporting cheap branches that can be assigned to features, pull requests, test workers, and coding agents.

**North-star demonstration:** Copy a single `.pga` file to another machine, open it there, connect an unmodified PostgreSQL client and application, create a branch without copying the database, run migrations, shut down, and retain valid data after forced crashes.

### 1.1 Primary users

1. Backend developers whose production system uses PostgreSQL.
2. Test/CI engineers who want isolated databases without a container per test.
3. Autonomous coding agents and multi-worktree workflows, each needing its own independent database snapshot.
4. Rust developers who want an embedded, branchable SQL store with familiar PostgreSQL semantics.

### 1.2 Non-goals for the earliest useful release

- Competing with PostgreSQL as a highly available distributed production database.
- Remote multi-region replication or hosted database services.
- A new SQL dialect that permanently diverges from PostgreSQL.
- Automatic merging of arbitrary diverged database contents.
- Perfect PostgreSQL compatibility on day one. It remains the engineering target, not a feature casually waived.

These are *sequencing decisions*, not permanent repudiations of PostgreSQL compatibility.

## 2. Product contract and compatibility policy

### 2.1 Product-level invariants

- **Portable canonical file:** A closed `.pga` contains all persistent logical state required to reopen it, including schemas, rows, indexes, branches, transaction state, and recovery metadata.
- **No silent loss:** SQL operations that cannot be executed to their promised PostgreSQL semantics must error, not turn into no-ops.
- **Branch isolation:** A commit or schema migration on branch A never becomes visible to branch B unless an explicit import, reset, or replication operation takes place.
- **Snapshot consistency:** Branches begin at an atomically committed snapshot. They must never observe partially committed parent changes.
- **Durable acknowledgments:** Under the documented sync mode and filesystem guarantees, an acknowledged COMMIT survives process crash and power loss; testing must validate those assumptions.
- **Engine-independent application:** Native PostgreSQL clients and ORMs do not need bespoke drivers or modified application code.
- **Inspectable truth:** Every feature exposed to users has an associated conformance status and tests.

### 2.2 Meaning of “100% PostgreSQL compatibility”

Full PostgreSQL compatibility is the uncompromising **destination**, with a **versioned specification**. Target PostgreSQL 18.x as the reference behavior in CI; upgrade to newer stable major versions through explicit compatibility profiles. Fidelity includes SQL syntax, types, coercions, functions, operator behavior, result ordering rules, transaction isolation, locks, errors/SQLSTATE, metadata catalogs and OIDs, wire protocol, client behaviors, roles/security, and the documented features of the targeted release. Native third-party extension ABI compatibility is an especially large additional workstream, not something a Rust reimplementation receives automatically.

A finite test suite cannot mathematically prove 100% equivalence. The project therefore defines a reproducible and continuously expanding conformance corpus, publishes uncovered/failed cases, and does not present an incomplete release as fully compatible. PostgreSQL compatibility must **never** be redefined as merely accepting PostgreSQL-looking SQL.

## 3. Developer experience (desired CLI, not implemented)

```sh
pgarbor init ./dev.pga
pgarbor serve ./dev.pga --branch main --port 5432
psql 'postgres://localhost:5432/app'
```

```sh
# Logical branch: constant-cost relative to DB size, all data kept inside dev.pga
pgarbor branch create feature/auth --from main --db ./dev.pga

# An optional tiny branch-handle file, NOT itself a standalone database
pgarbor branch link feature/auth ./feature-auth.pgabranch --db ./dev.pga

# Run command with a URL for an isolated branch and auto-managed local listener
pgarbor branch exec feature/auth --db ./dev.pga -- npm test

# Produce a truly independent portable database file for this branch
pgarbor branch detach feature/auth --db ./dev.pga --out ./auth.pga --strategy auto

pgarbor branch list --db ./dev.pga
pgarbor branch delete feature/auth --db ./dev.pga
pgarbor verify ./dev.pga
pgarbor compact ./dev.pga
pgarbor inspect ./dev.pga
```

`branch exec` chooses/reserves a free loopback port or local socket, starts/reuses a branch-bound listener, injects `DATABASE_URL`, and stops an owned listener on exit; it must not stop a pre-existing listener or overwrite a caller's URL silently. A database name maps to a logical SQL database *within* the chosen branch; separate branch listeners avoid conflating PostgreSQL database names with PGArbor branch names.

### Embedded API (design sketch)

```rust
let db = pgarbor::Database::open("dev.pga").await?;
let branch = db.branch("main")?;
branch.execute("CREATE TABLE events (id UUID PRIMARY KEY)", &[]).await?;
let experiment = db.create_branch("experiment", "main").await?;
```

The wire-server and embedded APIs must use the same semantics, planner, storage, and transaction logic.

## 4. Branch files: three levels of independence

The desirable idea is “each branch is a file, but shared states consume shared physical storage.” Three different properties must be separated:

1. **Branch identity:** A named reference to a consistent branch root.
2. **Self-contained portability:** Moving a file alone preserves all database bytes needed to open it.
3. **Physical storage sharing:** Multiple files point to the same disk extents until changed.

A tiny reference to a master file satisfies 1, but not 2. Filesystem reflinks support 2 and 3 on certain filesystems, but are not universally supported. PGArbor explicitly supports both designs over a single logical branch model.

### 4.1 Default: one master `.pga`, logical branch roots

```text
myapp/
  dev.pga                   <-- contains every branch's durable state
  branches/
    feature-auth.pgabranch    <-- tiny pointer / handle into dev.pga
    agent-42.pgabranch        <-- tiny pointer / handle into dev.pga
```

The primary `.pga` contains a durable branch directory and content-addressed or extent-addressed immutable pages. A `.pgabranch` handle is an optional small file containing format version, database UUID, branch UUID, storage location hint, and integrity metadata; its authoritative branch identity/root is resolved inside the master. Branch handles can move with the master if relative lookup succeeds or a path is supplied; moving only a handle is not a database export. Deleting a handle does not silently delete branch data. A mismatched UUID errors loudly. Handle files are not required for branching; all logical branches survive by copying just `dev.pga`.

Creating a branch appends a new branch-root reference to a committed catalog update — no whole database copy. Edits append new page versions and path-copy affected tree nodes. Parent and child remain independent, and garbage collection preserves pages reachable from any retained branch, snapshot, or active transaction.

### 4.2 Optional: physically independent `.pga` files via CoW cloning

On APFS (`clonefile` or equivalent) and supported Linux reflink filesystems (`FICLONE`, e.g. Btrfs or appropriately configured XFS), clone a consistent closed/checkpointed or snapshot-safe `.pga` to an independent `.pga`. Both files have independent filesystem identities while initially sharing unchanged physical extents. They may be renamed, moved, or deleted independently; however moving to another filesystem may materialize copied bytes. Native cloning must not be relied on for data consistency without a database-level snapshot barrier.

A whole-file reflink may retain other branch history until garbage collection in the detached copy. Implement a post-clone prune/compact path and test disk-space behavior. Filesystem cloning is an **optimization**, never the semantic basis of database branching.

If reflink is unavailable, `--strategy auto` must still produce a correct independent `.pga` using streaming compact export/copy, while reporting that it will incur data-proportional I/O. `--strategy reflink-required` errors clearly when unsupported.

### 4.3 Export as canonical standalone database

`pgarbor branch export X --out X.pga` always emits a compact, independently movable database with only branch X and required history, regardless of filesystem. Export must produce a consistent transaction snapshot while the source may be active. Use temporary output plus atomic final rename and directory-sync as supported, and clean up failed exports. An exported branch has a fresh container UUID, preserved schema/data semantics, and a documented branch-identity mapping.

### 4.4 Fork vs merge

Provide `create`, `delete`, `reset`, `rename`, `export`, `detach`, and `compare` first. Do **not** claim SQL row-level merge of arbitrary divergent database histories. A future merge requires separately specified schema-change ordering, row conflict detection, constraint revalidation, sequence reconciliation, and transaction semantics.

## 5. Storage subsystem

### 5.1 Logical model

- Single append-oriented binary container with magic/version, dual recoverable superblock slots, durable commit records, data extents, a free-space/reachability map, and persistent roots.
- Default logical page size is a **provisional** 8 KiB, not an irreversible specification. Validate alignment and performance before freezing the file format.
- Copy-on-write B+tree/page index for tables, secondary indexes, catalog, and per-branch metadata. Physical extents are referenced by stable identifiers; never make a logical pointer depend on a future compaction offset.
- All physical mutations are transactional at the container level. No write-ahead file sidecars; the database file itself contains an ordered recovery log and checkpoint records.
- Checksums of pages/records; format versions; feature flags; fail-closed on unsupported versions.
- Reusable extents only when not reachable from any branch, snapshot, active reader, or recovery checkpoint.

### 5.2 Commit and crash recovery

Proposed commit protocol:

1. Read committed branch root `R` and establish a transactional snapshot.
2. Stage writes as newly allocated immutable pages/extents.
3. Append associated new pages and mutation metadata.
4. Append a complete commit record containing branch UUID, old root, new root, logical transaction metadata, and checksum.
5. Flush records to stable storage in required order, then atomically publish/checkpoint the new root through validated superblock rotation; sync metadata as necessary.
6. Only after the required durability boundary, acknowledge COMMIT to the client.
7. On reopen, choose the latest valid durable checkpoint/commit chain, discard incomplete trailing records, and verify reachability.

The exact algorithm must be proven on Linux/macOS/Windows before claiming power-loss guarantees. Torn writes, reorderings, partial truncation, stale caches, out-of-space, and interrupted compaction must be fault-injected. No undocumented reliance on “rename makes a DB durable.”

### 5.3 MVCC and GC

Use immutable record versions and branch-scoped transaction visibility. Transactions pin read snapshots. Maintain commit sequence numbers, visibility rules, conflicts, and catalog/schema snapshots. Initial writers may serialize within a branch for simplicity; this is an explicitly temporary implementation constraint, not a compatibility exception for the final target. Implement READ COMMITTED first, REPEATABLE READ and SERIALIZABLE/SSI as compatibility milestones. Extend predicate, unique, foreign-key and index locking in lockstep with isolation semantics.

Garbage collection must trace reachability from branch heads, named snapshots, pinned readers, and crash-recovery roots. It must be incremental and bounded; on-disk compaction is atomic, interruptible, and cannot discard data referenced by a branch. Provide `inspect --space` showing logical vs physical usage and retained bytes pinned by branches.

## 6. SQL engine and PostgreSQL behavioral compatibility

### 6.1 Pipeline

`wire/embedded API → parser → binder/type resolver → catalog → logical plan → optimizer → physical execution → transactional storage`.

Start with a Rust PostgreSQL-oriented SQL parser (`sqlparser-rs` can be evaluated and extended). It provides parsing, not PostgreSQL semantics. The `pg_query` library uses PostgreSQL's C parser internally, so making it a core dependency would compromise a strictly all-Rust build; use it as an optional oracle during development rather than pretending it is pure Rust. Maintain an internal typed AST/IR decoupled from third-party parser ASTs.

### 6.2 v1 useful SQL vertical slice

- DDL: CREATE/ALTER/DROP TABLE, schemas, basic indexes, primary/unique/foreign keys, defaults, null constraints; migrations transactional wherever PostgreSQL makes them transactional.
- DML: INSERT, UPDATE, DELETE, RETURNING, ON CONFLICT, parameterized operations.
- Reads: projection, filtering, equijoins and general join predicates, GROUP BY, HAVING, ORDER BY, LIMIT/OFFSET, common expressions, subqueries, CTEs as needed by selected ORM fixtures.
- Types: bool, integer family, text/varchar, bytea, numeric/decimal, UUID, date/time/timestamp/timestamptz, JSON/JSONB, basic arrays and enums. Exact formatting, coercions and null behavior are tested.
- Indexes: B-tree unique and secondary; planner statistics and other index families later.
- SQL transactions: BEGIN/COMMIT/ROLLBACK/SAVEPOINT and PostgreSQL error-state behavior.
- Catalogs: dynamic `pg_catalog`, `information_schema`, stable type OIDs and schema introspection needed for drivers and migrations.
- Functions/operators: implement on demand from actual SQL and ORM traces, differential-tested; no stubs returning fake data.

### 6.3 Full compatibility roadmap (no permanent subset)

Additional features include the complete SQL grammar and binder, full built-in type/operator/function catalogs, SQL and PL/pgSQL functions, triggers, views/materialized views, sequences/identity, partitioning, generated columns, collations and locale-sensitive comparison, privileges/roles/RLS, sophisticated locks and deadlock handling, SERIALIZABLE/SSI, advisory locks, async notifications, COPY, GIN/GiST/BRIN/SP-GiST and other index families, large objects, extension interfaces, FDWs, replication protocols, `pg_dump`/`pg_restore` interoperability, query statistics, and PostgreSQL tooling.

Native C-extension ABI compatibility is a major distinct subsystem. A Rust engine cannot automatically load arbitrary PostgreSQL C extensions merely because it speaks the wire protocol; design a credible PG ABI layer or compatibility sandbox and track extensions explicitly. Do not call an unsupported extension “compatible” merely because CREATE EXTENSION parses.

## 7. PostgreSQL wire server and session model

- Support PostgreSQL v3.0 baseline protocol, startup negotiation, simple and extended query protocols, Parse/Bind/Describe/Execute/Sync, named and unnamed prepared statements, portals, text/binary values, cancellation, transaction state, errors and notices.
- Accurate type OIDs, column metadata, SQLSTATE and affected-row counts; support common driver introspection queries, `SHOW`, `SET`, `search_path`, and selected GUCs by real semantics.
- Listener defaults to loopback, with an automatically selected free port for branch-managed test processes; opt-in external binding, proper auth and TLS before any network exposure.
- Each connection binds to one branch at session start. A connection never changes branch midway through a transaction. If a branch is reset/deleted, existing sessions get documented safe behavior.
- Do not conflate PGArbor branches and PostgreSQL databases. One branch may contain multiple logical PostgreSQL databases.
- Use a supported Rust networking/runtime crate, but all protocol state transitions must be tested against PostgreSQL.

## 8. Transaction and concurrency model

One in-process runtime coordinates all branch readers/writers for a given opened `.pga` and enforces file/process locking. Multiple processes may connect through the local server. Opening the same database file for simultaneous writes from unrelated engine processes must never succeed silently; choose one owner or implement explicit cross-process coordination. Branch-bound network listeners can share one engine instance.

Isolation is part of compatibility, not an optimization. Read committed statement snapshots, repeatable read transaction snapshots, serializable conflicts and SSI, unique-index conflict handling, FK visibility, sequences, deadlock detection, and lock timing must match the pinned PostgreSQL reference. Early releases must report supported isolation precisely and return clear errors for modes not implemented.

## 9. Branch semantics in detail

- **Create:** Atomically install a branch head referencing a committed root; transaction histories visible to the new branch end exactly at that root.
- **Schema:** DDL belongs to the branch history; changes do not leak to siblings.
- **Sequences:** Branch-specific sequence state; non-transactional sequence semantics within a branch should match PostgreSQL. Fork copies state at the branch snapshot.
- **Readers:** Ongoing readers pin immutable snapshots; branch deletion is logical until last readers release references.
- **Rename:** Atomic catalog rename; handles identify branch UUIDs so renaming is not data corruption.
- **Reset:** Mutating destructive operation with confirmation or `--force` in noninteractive mode; preserve previous root as optional recovery snapshot under explicit policy.
- **Compare:** Initially show migration/schema diff, page/row counts and divergence stats; row-level merge is not implied.
- **Detach:** Produces standalone independent `.pga`, checked for integrity and reopened before success is reported.
- **GC:** Shared pages remain until no live root or pinned snapshot references them.
- **Branch limits:** Define/test memory and open-reader limits rather than pretending unbounded branching is free.

## 10. CLI and integration ergonomics

Commands: `init`, `serve`, `shell`, `exec`, `status`, `inspect`, `verify`, `compact`, `branch {create,list,link,exec,serve,rename,delete,reset,compare,detach,export}`, `import`, `dump`, `doctor`, and `version`. Offer structured `--json` output for agents and CI. Include deterministic exit codes, progress for large export/GC, machine-parseable diagnostics, and no surprise destructive actions.

The earliest “real app” compatibility targets are:

1. `psql` using simple queries and prepared statements.
2. Rust `sqlx` with migrations and typed queries.
3. TypeScript `pg` plus Drizzle ORM migrations and generated CRUD.
4. Prisma migrate/introspection once catalogs and protocol semantics suffice.
5. Typical project tests spawning independent branch listeners.

Use actual user-application fixtures rather than handcrafted compatibility demos exclusively.

## 11. Differential testing: the product's credibility engine

### 11.1 Reference harness

- Boot a pinned official PostgreSQL 18.x reference instance in CI or test infrastructure; Docker is allowed **for test infrastructure**, never required to run PGArbor itself.
- Run the same schema, seed data, queries and transaction schedules against PostgreSQL and PGArbor.
- Compare result rows/columns, types and OIDs, NULL semantics, coercions, JSON, timestamps, decimals, errors and SQLSTATE, catalogs, transaction outcomes, affected rows, and query protocol responses.
- Normalize only inherently nondeterministic outputs (timestamps, generated random values, unspecified row order, transaction IDs) under documented rules. Never normalize away real semantic discrepancies.
- Generate cases from PostgreSQL docs and regression tests; test fixtures from real ORMs; property-based and grammar-based fuzzing; minimize failing counterexamples.
- Maintain explicit classes: pass, known incompatibility, unsupported, crash, unexpected acceptance, timeouts, nondeterministic, and fixture error.

### 11.2 Storage correctness matrix

- Create, commit, reopen and verify checksums.
- Crash between each logical commit step; truncate/reorder/tear writes in modeled fault harnesses.
- Space exhaustion, rename failures, unclean shutdown, process termination, interrupted export/compaction.
- Many branches/snapshots; deleting parent; long-lived readers; compaction with pinned data; branch cross-contamination tests.
- Cross-filesystem export and reopen; APFS reflink clone, Linux reflink, unsupported filesystem fallback.
- Multiprocess lock contention and CI concurrency stress.
- Memory and filesystem sanitizers where available; Miri for applicable pure Rust components.

### 11.3 Compatibility scorecard

Publish percentage/pass counts by feature family and reference version, with a reproducible corpus size and known failures. Pass rate is a progress metric, **not** an assertion that uncovered behavior is compatible.

## 12. Performance targets (aspirational, subject to benchmarks)

- Logical branch creation independent of database size, avoiding full data copies.
- Single-file DB start and reopen without a database init service, targeting subsecond startup for typical local schemas once practical.
- Cold local CRUD and multi-branch tests competitive with Dockerized PostgreSQL where workloads permit, without advertising universal speed superiority.
- Open 100+ branch roots with modest incremental metadata overhead; workloads and hardware must be documented.
- Publish benchmark methodology, CPU, memory, SSD usage, fsync mode, baseline, and warm/cold cache conditions.
- Report physical shared bytes, retained garbage, compacted bytes, branch write amplification and branch startup cost.

Do not optimize via durability shortcuts or relaxed SQL behavior without exposing an explicit opt-in unsafe mode.

## 13. Modules and workspace layout

```text
pgarbor/
  Cargo.toml
  crates/
    pgarbor-core/        # engine lifetime, shared runtime, transactions
    pgarbor-storage/     # binary format, pages, CoW, recovery, GC
    pgarbor-branches/    # branch graph, snapshots, export/detach
    pgarbor-catalog/     # databases, schemas, object IDs, pg_catalog
    pgarbor-types/       # PG type system, casts, functions, operators
    pgarbor-sql/         # parser adapter, binder, typed IR
    pgarbor-plan/        # logical and physical planning
    pgarbor-exec/        # expression and relational execution
    pgarbor-wire/        # PostgreSQL protocol and sessions
    pgarbor-cli/         # `pgarbor` executable
    pgarbor-testkit/     # differential and fault test runners
  tests/
    pg-differential/
    storage-crash/
    orm-fixtures/
    branch-lifecycle/
  docs/
    rfcs/
    compatibility/
    internals/
  examples/
```

These are architectural boundaries; do not generate dozens of empty crates as implementation theater. Start with a compact workspace, extract when real dependencies justify it. Keep storage ignorant of SQL, and query semantics ignorant of physical branch layouts.

### Dependency policy

Using Rust crates for SQL parsing, protocol framing, networking, CRC/checksums, error reporting and testing is allowed. Study and cite PostgreSQL, Neon, SQLite and Turso implementations, but implement an original storage engine and planner rather than copying incompatible code into the source tree without licenses and provenance. If a C-backed parser is introduced as an optional development oracle, do not falsely describe the resulting binary as pure Rust.

## 14. Implementation plan and acceptance gates

**Phase 0 — Repository/name preflight**
- Search GitHub, crates.io, npm, package registries and obvious trademark conflicts for `PGArbor`/`pgarbor`; register or reserve relevant namespaces if available. If a collision appears, **select and verify a new name before scaffolding**; update all crate IDs, CLI names, file extensions and documentation at once. No placeholder collision name may reach committed code.
- Create minimal workspace, formatting/lint/test CI, docs and versioning policy.
- Produce a reference PostgreSQL fixture suite.

**Phase 1 — One-file transactional storage**
- Create/write/read `.pga`, recover across restart, commit atomicity and checksums.
- Persistent page tree, single writer, snapshot readers, crash-fault suite.
- Gate: fuzzed crash/restart never violates acknowledged-commit or root-reachability invariants under documented assumptions.

**Phase 2 — Branch engine before SQL**
- Branch roots, snapshot, independent writes, delete/reset/rename, GC, `.pgabranch` handles, standalone export, reflink optimization.
- Gate: branch A and B diverge under concurrent read/write stress, and independent exported `.pga` survives relocation and reopen.

**Phase 3 — Minimal real relational database**
- Catalog, table schemas, B-tree indexes, basic SQL binder/planner/executor, transaction statements and error handling.
- Gate: CRUD and migrations persist and branch independently; no silent SQL acceptance.

**Phase 4 — PostgreSQL wire/ORM alpha**
- Protocol v3.0 simple/extended, prepared statements, OIDs, common catalogs, constraints/types and `RETURNING`.
- Gate: psql + `sqlx` + `pg`/Drizzle app fixture works unmodified across migration/test workflows.

**Phase 5 — Real development replacement**
- Robust CLI, auto-managed per-branch listeners, importer/exporter, failure diagnostics, concurrent testing, schema introspection, wider PG compatibility.
- Gate: an existing moderately realistic PostgreSQL app replaces local Docker Postgres with only a `DATABASE_URL` change; errors are documented and incompatibility rate measurably declines.

**Phase 6 — Compatibility and robustness campaign**
- Full transaction isolation, broad SQL features, performance, extensions and tooling compatibility, enterprise-grade observability.
- Gate: pinned PostgreSQL conformance suite; all core compatibility gaps tracked and retired before any full-compatibility claim.

### Milestone discipline

Do not begin a new milestone until its gate passes. Parallelize components only with stable interfaces; avoid large agent swarms that introduce unmerged speculative subsystems. Every completed milestone should produce a short demo, benchmark, regression-suite changes, and an architecture note capturing tradeoffs.

## 15. ADRs required before freezing the format

- ADR-001 — canonical internal branching vs filesystem clone (decision above).
- ADR-002 — physical page size and allocation unit.
- ADR-003 — durable commit/superblock ordering and power-loss model.
- ADR-004 — single file locks and multi-process behavior.
- ADR-005 — type/collation semantics and reference PostgreSQL version.
- ADR-006 — MVCC, unique conflicts, read-committed snapshots and serializable strategy.
- ADR-007 — parser abstraction and use of third-party Rust parsing crates.
- ADR-008 — page rewrite amplification, compaction, reachability and GC.
- ADR-009 — branch storage policy for copy-on-write snapshots, detached files and export.
- ADR-010 — extension ABI strategy and full compatibility scope.

## 16. Risks that must be treated seriously

| Risk | Response |
|---|---|
| Incorrect durability claims | Fault injection, explicit filesystem assumptions, durable acknowledgment tests |
| PostgreSQL semantics too deep | Differential-test-led development and no silent stubs; pin one PG version |
| Catalog/ORM incompatibility | Real ORM fixture applications and protocol-level tests from the start |
| Branch GC leaks huge retained history | Reachability tracing, per-branch storage accounting, benchmark retention |
| Filesystem-specific CoW | Logical branching works on any supported filesystem; reflinks are optional |
| Extension ABI reimplementation | Track separately; full compatibility remains goal; don't claim unsupported ABIs |
| Multi-process file corruption | Single owning engine with reliable locks; explicit refusal otherwise |
| Scope explosion | Vertical slices with proven end-to-end gates; avoid speculative rewrites |
| Name collisions | Name/namespace preflight before scaffolding; immediate rename when collision found |

## 17. Showcase and résumé narrative

This is not “yet another SQL parser.” It is a database-systems project demonstrating durable file formats, transactional storage, immutable indexes, MVCC, PostgreSQL protocol semantics, database catalogs, branch-native copy-on-write, concurrency, crash recovery, differential testing and agent-focused developer tooling. A convincing demonstration includes an unmodified app, a copied single file, instant logical branches, real migrations, and documented PostgreSQL conformance results.

### Success definition

**The system succeeds when a developer can replace their local PostgreSQL container with PGArbor for a meaningful application, keep existing database-client tooling and migrations, create isolated branches cheaply, recover safely from failures, and move durable state in one file — with a clear path toward full PostgreSQL behavioral compatibility.**

---

## Primary technical references

- PostgreSQL protocol (v18): https://www.postgresql.org/docs/18/protocol.html
- PostgreSQL overview of extended query messaging: https://www.postgresql.org/docs/18/protocol-overview.html
- PostgreSQL regression testing: https://www.postgresql.org/docs/18/regress.html
- PostgreSQL source: https://github.com/postgres/postgres
- Turso engine and PostgreSQL frontend: https://github.com/tursodatabase/turso
- Neon branchable PostgreSQL architecture: https://github.com/neondatabase/neon
- Linux reflink / FICLONE semantics: https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html
- Apple APFS clone semantics: https://developer.apple.com/documentation/foundation/about-apple-file-system
- Rust SQL parser ecosystem: https://github.com/apache/datafusion-sqlparser-rs (verify current canonical repository before use)
- PostgreSQL-derived parser in Rust (includes C backend): https://github.com/pganalyze/pg_query.rs

**Document control:** Engineering vision/specification only. No performance, compatibility, or durability results are claimed to be implemented or benchmarked.
