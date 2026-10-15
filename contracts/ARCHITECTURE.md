# PGArbor Architecture Contract — v1 (ARCH-03..ARCH-08)

Status: RATIFIED (contract steward: orchestrator). Becomes FROZEN@SHA when tagged `contracts-v1`.
Changing anything here or any `// CONTRACT` item in Rust requires an amendment ticket
(`contracts/amendments/NNNN-*.md`), never a silent edit inside a feature patch.

## 0. Ground rules for every author

1. **No silent stubs.** Code that cannot yet do the PostgreSQL thing returns
   `Err(PgError::unimplemented("LEASE-ID", "what"))` (SQLSTATE `0A000`). Never `todo!()`,
   `unimplemented!()`, `panic!`, fake empty results, or `Ok(())` no-ops for unsupported behavior.
   The factory counts `PgError::unimplemented(` call sites as UNIMPLEMENTED inventory.
2. **No panics on user input.** No `unwrap()`/`expect()` on data that comes from SQL, the wire,
   or the file. Internal invariants may `debug_assert!`. Corruption → `PgError::corruption(..)` (XX001).
3. **Dependency direction is law** (see §1). Storage knows nothing about SQL, types, or catalogs.
4. **One author per file.** Only edit files listed in your lease. `Cargo.toml`, `lib.rs`, `mod.rs`
   files and anything under `contracts/`, `harness/oracle/`, `tests/expected/` are not yours.
5. **The engine is synchronous.** No async runtime inside engine crates (wasm-friendly). Only
   `pgarbor-wire`/`pgarbor-cli` use threads/sockets (std::net, thread per connection).
6. **Reference PG18 is truth.** Pinned: `docker.io/library/postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336`
   (PostgreSQL 18.6), initdb `--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8`,
   `TimeZone=UTC`, `DateStyle=ISO, MDY`, `IntervalStyle=postgres`, `extra_float_digits=1`.
   Default collation is therefore codepoint order (C.UTF-8). Other collations are later work.

## 1. Crates and dependency DAG (acyclic, enforced by Cargo)

```
pgarbor-common   (ids, PgError/SqlState, Kv traits, keycodec helpers)      deps: —
pgarbor-storage  (FileIo, pager, CoW B+tree, container commit/recovery,     deps: common
                  file lock, compaction/export, doctor)
pgarbor-fault    (fault-injecting FileIo, reference model, crash scheduler) deps: common, storage
pgarbor-branches (branch directory, create/delete/rename/reset/compare,     deps: common, storage
                  .pgabranch handles, detach/export, reflink)
pgarbor-types    (Datum, TypeId/OIDs, text+binary IO, casts, operators,     deps: common
                  functions, numeric, datetime, json/jsonb, arrays, memcomparable key encoding)
pgarbor-catalog  (catalog model, persistence over Kv, OID allocation,       deps: common, types
                  pg_catalog + information_schema virtual relations)
pgarbor-core     (Engine: txn manager, snapshots, overlays, savepoints,     deps: common, storage, branches
                  key locks, deadlock detection, sequences store)
pgarbor-sql      (sqlparser 0.63 adapter, binder → bound IR, param typing)  deps: common, types, catalog
pgarbor-plan     (logical plan, rewrite/optimize, physical plan, EXPLAIN)   deps: common, types, catalog, sql
pgarbor-exec     (executor operators, expression eval, DML/DDL, constraints)deps: common, types, catalog, sql, plan, core
pgarbor          (embedded API: Database, Session, statement dispatch,      deps: all of the above
                  GUCs, portals/prepared statements)
pgarbor-wire     (PG v3 protocol server over std::net)                      deps: common, types, pgarbor
pgarbor-cli      (`pgarbor` binary)                                         deps: pgarbor, pgarbor-wire, branches, storage
```

`pgarbor-catalog` and `pgarbor-exec` never import `pgarbor-storage`: they see data only through the
`Kv` traits in `pgarbor-common`. `pgarbor-core` adapts storage trees + overlays into `Kv`.

## 2. Identifiers (pgarbor-common::ids) — ARCH-04

| Type | Repr | Meaning |
|---|---|---|
| `PageId(u64)` | page number in file; 0 = superblock area, never a tree page | storage-internal |
| `CommitSeq(u64)` | monotonically increasing container commit counter | durable ordering |
| `DatabaseUuid([u8;16])` | random per container (fresh on export) | file identity |
| `BranchId([u8;16])` | random UUID per branch; stable across rename | branch identity |
| `TxnId(u64)` | in-process, monotonically increasing; NOT persisted | locks/diagnostics |
| `Oid(u32)` | PostgreSQL OID; user objects allocated from 16384 per branch | catalog |
| `RelId = Oid` | relation (table/index/sequence/view) OID | catalog |

## 3. Errors (pgarbor-common::error) — ARCH-05/08, SQL-13

`PgError { sqlstate: SqlState, severity: Severity, message, detail, hint, position, schema, table,
column, datatype, constraint }`. `SqlState` is a newtype over `[u8;5]` with named constants for the
PG18 errcodes we use (full `errcodes.txt` table generated later). `Result<T> = std::result::Result<T, PgError>`.
Messages copy PostgreSQL's wording exactly (differential tests compare SQLSTATE always and
message text where deterministic).

## 4. Key/value layer (pgarbor-common::kv) — ARCH-06

```rust
pub trait KvRead {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Ascending iteration over [start, end) bounds, bytewise key order.
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>>;
}
pub trait KvWrite: KvRead {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, key: &[u8]) -> Result<bool>;
}
```
**Keyspace of one branch tree.** Every key starts with a 4-byte big-endian *space id*:
- `0x0000_0001` catalog (owned by pgarbor-catalog, layout in catalog contract)
- `0x0000_0002` sequences state (owned by pgarbor-core::sequence)
- `0x0000_0003` branch-local metadata (next OID counters, etc.)
- `oid >= 16384`: relation data. Table rows: `[relid][rowid: u64 BE]` → encoded tuple.
  Index entries: `[index relid][memcomparable key][rowid u64 BE if non-unique]` → rowid u64 BE.
- System catalog relation OIDs < 16384 never hold row data (pg_catalog views are virtual).

## 5. Storage contract — ARCH-06, STORE-*

- `FileIo` trait (`read_at`, `write_at`, `sync_data`, `sync_all`, `len`, `set_len`) is the ONLY path to
  bytes. `OsFile` (std::fs) for real files, `MemFile` for wasm/tests, `FaultFile` in pgarbor-fault.
- File = superblock area (2 slots of 4096 bytes at offsets 0 and 4096) + pages of `page_size`
  (default 8192) starting at offset 8192. PageId n ↦ offset `8192 + (n-1)*page_size`, n ≥ 1.
- Every page: 32-byte header `{magic u32, kind u8, flags u8, reserved u16, page_id u64, commit_seq u64,
  crc32c u32, payload_len u32}` + payload; checksum covers header (crc field zeroed) + payload.
- Superblock: `{magic "PGARBOR\0", format_version u32, feature_flags u64, page_size u32,
  db_uuid, commit_seq, directory_root PageId, page_count u64, freelist_root PageId (0 = none),
  created_unix_ms u64, crc32c}`. Two slots; the valid slot with the higher `commit_seq` wins.
  Unknown `format_version` or required feature flag → refuse to open (`XX001`-class error, fail closed).
- **Commit protocol** (STORE-09/10): all new pages are appended past `page_count` (never overwrite a
  page reachable from either superblock slot) → `sync_data` → write superblock into the *older*
  slot with `commit_seq+1` → `sync_data` → only then return `CommitSeq` (acknowledged).
  Recovery = read both slots, take highest valid; pages past its `page_count` are garbage.
- Copy-on-write B+tree (`BTree`): byte keys/values, ordered bytewise. Values larger than
  `page_size/4` go to overflow page chains. Mutation produces a new root `PageId`; old pages stay
  readable for any snapshot until compaction proves them unreachable.
- Space reuse in v1: append-only; `compact()` writes a fresh file with only reachable pages (live
  branches + pinned roots), syncs it, then atomically renames over the original and syncs the dir.
- `Container::lock` — exclusive OS advisory lock (`fs2`) on open; second opener gets an error, not
  silent sharing (STORE-12, ADR-004). One engine instance per file per process.

## 6. Branch contract — ARCH-07, BRANCH-*

Directory tree (root in superblock.directory_root), keys owned by pgarbor-branches:
`b"B" + branch_id(16)` → `BranchRecord`, `b"N" + name utf8` → branch_id,
`b"M" + name` → container metadata. `BranchRecord { id, name, data_root: PageId, parent: Option<BranchId>,
created_at_seq: CommitSeq, head_seq: CommitSeq, created_unix_ms, state: Active|Deleted,
snapshots: Vec<(label, PageId)> }`. A new container has branch `main`.
Create = one commit inserting a record whose `data_root` = parent's current `data_root` (O(1)).
Branch names: `[A-Za-z0-9._/-]{1,200}`, no leading `-` or `/`.
`.pgabranch` handle: small JSON `{format:"pgabranch/1", db_uuid, branch_id, branch_name_hint,
db_path_hint, created_unix_ms, crc32c}`; resolution validates db_uuid, errors loudly on mismatch.

## 7. Transactions — ARCH-07, TXN-*

- `Engine` owns `Container`; per-branch head = `BranchRecord.data_root`.
- `Txn` = snapshot root (`PageId`) + **overlay** (`BTreeMap<Vec<u8>, Option<Vec<u8>>>`, None = delete)
  + savepoint stack (overlay checkpoints) + isolation + `TxnState {Idle, InProgress, Failed}`.
- Reads see overlay over snapshot. READ COMMITTED refreshes the snapshot root at every statement
  start (`Txn::begin_statement`). REPEATABLE READ keeps the first statement's snapshot.
  SERIALIZABLE: until SSI exists (TXN-11), it runs with REPEATABLE READ snapshot + write-conflict
  rules. That matches PG for histories without rw-antidependency cycles but can admit write-skew
  that PG would abort. This is a KNOWN INCOMPATIBILITY recorded in `docs/compatibility/KNOWN_GAPS.md`
  and the inventory (`serializable_ssi = UNIMPLEMENTED`), never reported as PASS.
- **Row/key locks** (`LockManager`): every write (`put`/`delete` on relation data or unique index keys,
  catalog rows) first takes an exclusive key lock (branch_id, key). Conflict → wait on the holder;
  wait-for graph cycle → `40P01 deadlock_detected` for the requester; `lock_timeout` GUC (ms, 0 = ∞)
  → `55P03`. After acquiring, `Txn` compares latest committed value vs snapshot value:
  `LockOutcome::{Unchanged, Changed(Option<Vec<u8>>)}`. RC callers re-evaluate; RR callers raise
  `40001` ("could not serialize access due to concurrent update").
- Commit: take the container write mutex (short), re-read branch head, apply overlay to the head tree
  (keys are locked, so no lost updates), write new `BranchRecord.data_root`, container commit,
  release key locks. Rollback = drop overlay + release locks. Failed state: every statement except
  ROLLBACK / ROLLBACK TO SAVEPOINT / COMMIT(→ROLLBACK) errors `25P02`.
- Sequences: `nextval` writes go to a separate immediate mini-commit (non-transactional, like PG).

## 8. Types — ARCH-05, TYPE-*

`TypeId { oid: Oid, typmod: i32 }` (typmod -1 = none). `Datum` enum (values never carry type):
`Null, Bool, Int2(i16), Int4(i32), Int8(i64), Float4(f32), Float8(f64), Numeric(Numeric), Text(String)`
(text, varchar, bpchar, name, enum labels), `Bytea(Vec<u8>), Uuid([u8;16]), Date(i32)` (days since
2000-01-01, PG epoch; ±infinity as i32::MAX/MIN), `Time(i64)` µs, `TimeTz(i64 µs, i32 offset secs)`,
`Timestamp(i64)`/`TimestampTz(i64)` µs since 2000-01-01 UTC (±infinity = i64::MAX/MIN),
`Interval{months:i32, days:i32, micros:i64}`, `Json(String)`, `Jsonb(Jsonb)`, `Array(Box<Array>)`
(`Array{elem: Oid, dims: Vec<(i32 len, i32 lbound)>, items: Vec<Datum>}`), `Oid(u32)`, `Char(u8)`
("char"), `Record(Vec<Datum>)`.
Text I/O: `type_input(TypeId, &str) -> Result<Datum>` / `type_output(TypeId, &Datum) -> Result<String>`
byte-exact with PG18 under the pinned GUCs. Binary: `type_send`/`type_recv` per PG `*send/*recv`.
Tuple storage encoding (`tuple::encode_row/decode_row`) is engine-internal, versioned (leading u8).
Memcomparable encoding (`keycodec::encode_key`) preserves PG ordering for the default collation,
NULLS LAST ascending; used for index keys.

## 9. Catalog — ARCH-08, CAT-*

Catalog rows are stored under space 1: `[0,0,0,1][kind u8][oid u32 BE]` → serialized object
(serde_json bytes, versioned). Kinds: namespace, class (table/index/view/sequence), type (enum/domain/
composite), constraint, attrdef, database, role, description. `CatalogSnapshot::load(&dyn KvRead)`
builds an immutable in-memory view; `CatalogTxn` writes through `&mut dyn KvWrite`.
Built-in objects (pg_catalog namespace 11, public 2200, information_schema, built-in types/procs,
role `postgres` oid 10, database oid 5 `postgres`) are compiled-in constants with PG18's OIDs.
pg_catalog / information_schema relations are **virtual**: generated rows from `CatalogSnapshot`.

## 10. SQL → plan → exec — ARCH-05

- `pgarbor-sql::parse(sql) -> Result<Vec<RawStatement>>` wraps `sqlparser` 0.63 (`PostgreSqlDialect`),
  keeping source text + byte offsets. Syntax errors → `42601` with PG-style message and `position`.
- `pgarbor-sql::bind(stmt, &BindContext) -> Result<BoundStatement>`: name resolution via
  search_path, type resolution/coercion via pgarbor-types, parameter type inference ($n).
  Unsupported syntax → `0A000` with the construct named, never ignored.
- `pgarbor-plan::plan(BoundStatement) -> Result<Plan>`; `Plan` is a tree of `PhysicalNode`s plus
  output `Vec<ColumnDesc{name, type_id, table_oid, column_attnum}>`.
- `pgarbor-exec::execute(plan, &mut ExecContext) -> Result<ExecResult>` where ExecContext gives
  `&mut dyn KvWrite` (txn view), catalog snapshot, params, GUC values, lock hooks.
  `ExecResult::{Rows{desc, rows: Vec<Vec<Datum>>, tag}, Command{tag}}`; command tags exactly PG's
  (`INSERT 0 3`, `UPDATE 2`, `SELECT 5`, `CREATE TABLE`, ...).

## 11. Embedded API (pgarbor crate) — CLI-08

```rust
let db = pgarbor::Database::open("dev.pga", OpenOptions::default())?;   // or ::create
let mut s = db.session("main", SessionOptions { user: "postgres".into(), database: "postgres".into() })?;
let results = s.simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (1)")?;
let stmt = s.prepare("q1", "SELECT * FROM t WHERE id = $1", &[])?;    // extended protocol
db.create_branch("feature", "main")?;
```
Session exposes everything the wire layer needs (simple query, parse/bind/describe/execute/sync/close,
transaction status `I|T|E`, parameter status, notices, cancel flag). The wire layer contains **no**
SQL semantics.

## 12. Wire — ARCH-08, WIRE-*

PostgreSQL protocol 3.0 (accept 3.2 negotiation via NegotiateProtocolVersion down to 3.0), SSLRequest →
'N' until TLS exists, GSSENCRequest → 'N', CancelRequest via (pid, secret) map. Auth: trust (default,
loopback only) and SCRAM-SHA-256/cleartext when a password is configured. Startup params `user`,
`database`, `application_name`, `options` (-c GUCs); branch is bound at listener level.

## 13. Test oracle schema — ORACLE-03/04

Case files: `tests/cases/**/*.yaml` (schema `contracts/oracle/case.schema.json`). Expected outputs are
*recorded* from the pinned PG18 into `tests/expected/**` by the trusted recorder — never authored by
hand or by feature workers. Comparison: typed values (text format as PG prints), column names, type
OIDs, command tags, SQLSTATE; multiset comparison when no ORDER BY. Outcome classes: PASS, MISMATCH,
CRASH, HANG, CORRUPTION, UNEXPECTED_ACCEPTANCE, UNSUPPORTED, BLOCKED, FLAKY, INVALID_CASE, HARNESS_ERROR.
