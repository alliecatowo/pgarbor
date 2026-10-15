# pgarbor-exec — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-exec.json` (EXE-01..EXE-42, seed tasks
EXEC-01..17, parts of CAT-05). Amendments: `pgarbor-exec-01-unique-null-entries`, `pgarbor-exec-02-upstream-requirements`.
Depends on: pgarbor-common, pgarbor-types, pgarbor-catalog, pgarbor-sql, pgarbor-plan, pgarbor-core (public APIs only).

## Purpose

Runs everything after planning: executes physical plans (`pgarbor_plan::plan::Plan`) as a pull-based operator tree, evaluates
expressions, performs INSERT / UPDATE / DELETE / ON CONFLICT with all constraint enforcement, maintains btree indexes, runs
DDL through the catalog writer (`CatalogTxn`) with its data-level follow-ups, and executes the utility statements that need
no session state (LOCK, EXPLAIN [ANALYZE], COPY, TRUNCATE, REINDEX ...). It never parses or plans (except EXPLAIN, which calls
`pgarbor_plan::plan`) and knows nothing about transactions beyond the `ExecTxn` trait.

## Public API

| Item | Use |
|---|---|
| `execute(stmt, plan, cx) -> ExecResult`, `execute_with_channel(.., chan)` | one bound statement (ARCHITECTURE §10); `ExecResult::{Empty, Command{tag}, Rows{desc, rows, tag}}` |
| `execute_plan(plan, cx)` / `build::open_plan(plan, cx) -> PlanStream` | planned query/DML to completion / resumable stream (`PlanStream::fetch(cx, max_rows)`, `command_tag()`, `rows_affected()`) used by portals |
| `ExecContext { txn: &mut dyn ExecTxn, se: SessionEnv, stmt: StmtState }` | per-statement environment; `SessionEnv { env: CatalogEnv, io: IoConfig, guc: &dyn GucAccess, services: &dyn EngineServices, params, param_types, statement_ts, txn_ts, rng, notices, options }` |
| `ExecTxn` (trait, adapter `impl ExecTxn for TxnKv`) | the transaction as the executor sees it: KV read/write, `alloc_rowids`, `put_fresh`, `lock_and_read_latest`, `lock_relation`, `lock_ddl`, `alloc_oid`, sequences, advisory locks |
| `GucAccess`, `EngineServices` (traits) | implemented by the `pgarbor` crate (`GucTable`, `SessionServices`) |
| `memtxn::MemTxn` | in-memory `ExecTxn` for unit tests |
| `error::*` | every PG-exact executor error/notice constructor + `is_catalog_retry` |
| `copy::{CopyChannel, MemChannel, execute_copy}` | COPY endpoints |
| `ddl_driver::run_ddl`, `ddl_followup::{run_followups, ColumnRewrite}` | catalog write protocol and data follow-ups |
| `tabledesc::{TableDesc, table_desc}`, `rowstore`, `index_key`, `index_maint` | storage access layer shared by DML/DDL |

## Execution model (read `runtime.rs`, `plan.rs`)

* `open_plan` turns a `Plan` into **owned** operators (`Box<dyn RowSource>`: `next`, `rescan(params)`, `affected_rows`); nothing
  borrows the plan, so a `PlanStream` lives in a portal across Execute messages. Rows are `Vec<Datum>` of exactly
  `PlanNode::width()` slots.
* The transient `Executor { rt: &mut Runtime, cx: &mut ExecContext }` is passed to every operator call. `Runtime` holds
  `OuterRef` parameter frames (`with_outer`), the function-handle cache, sub-plan and CTE runtimes, domain-constraint cache,
  CASE test-value stack and EXPLAIN ANALYZE counters. `epoch` increments on every parameter frame push and lets buffering
  operators (Materialize, hash build, sort) decide whether to reuse their buffer on `rescan`.
* **Operators never keep KV iterators between calls.** Scans fetch bounded batches (`SCAN_BATCH`) and re-seek from
  `Excluded(last_key)`. Reads see the transaction's own writes; they do not see other transactions' later commits.
* **ModifyTable drains its whole input before writing** (Halloween protection) and applies foreign-key checks at statement end.
* Volatile functions, sub-plans and `nextval` are evaluated exactly once per row at the position the plan puts them.
* Errors abort the statement; the transaction layer (the `pgarbor` crate) rolls back an implicit transaction or marks an
  explicit block failed. No partial-statement cleanup exists in this crate.

## Data formats (ON-DISK, byte-exact, frozen v1)

* **Heap row**: key `[relid u32 BE][rowid u64 BE]` (12 bytes) -> `tuple::encode_row(values)` (self-describing, one value per
  attribute slot at write time). Row ids come from `ExecTxn::alloc_rowids` (core amendment 01: in-memory counters seeded from
  max rowid + 1, start at 1, never reused; the "rowid allocation stored in space 3" wording of the task text is superseded).
  Rows written before an `ADD COLUMN` have fewer values: `TableDesc::complete_row` pads with `Column::missing_value` (parsed with
  `io::input`) or NULL; dropped slots are NULL. `UPDATE` rewrites the value at the same key (the rowid is the row's identity);
  the transaction overlay supplies MVCC.
* **Index entry** (btree, amendment `pgarbor-exec-01`): key `[index oid u32 BE][keycodec(key columns)][rowid u64 BE]?` ->
  value `rowid u64 BE`. The suffix is present for non-unique indexes and for unique-index entries with a NULL key column (unless
  NULLS NOT DISTINCT); a unique key without suffix is its own uniqueness probe. Partial indexes store entries only for rows whose
  predicate is TRUE; expression keys are evaluated over the full row. The rowid is ALWAYS read from the value.
* **Sequences**: state under space 2 owned by core; `CreateSequenceState`/`RestartSequence`/`DropSequenceState` follow-ups call
  `ExecTxn::seq_create/seq_restart/seq_drop`.
* Everything else (catalog rows, `cat:version`) is written by `pgarbor-catalog` through the same transaction view.

## Concurrency rules (the parts workers must not get wrong)

1. `TxnKv::put/delete` lock the key but do NOT fail under READ COMMITTED when a concurrent transaction changed it. All unique
   checks, ON CONFLICT probes, FK parent locks and UPDATE/DELETE row locks therefore go through
   `ExecTxn::lock_and_read_latest(key, mode, policy)`, which returns the latest committed value merged with own writes and a
   `changed` flag (under REPEATABLE READ/SERIALIZABLE a change is the 40001 error raised inside the call).
2. READ COMMITTED UPDATE/DELETE of a changed row: EvalPlanQual-lite (`modify::epq_policy`): the scan quals are re-evaluated on the
   latest version for single-table plans; join plans raise 40001. Known gap: PG would re-run the join.
3. DDL: `run_ddl` takes the catalog write lock first; a concurrent catalog change returns `error::catalog_retry()`; the SESSION
   re-snapshots, re-binds and retries under READ COMMITTED.
4. Foreign keys: child checks and NO ACTION parent checks are queued and run at statement end (`fk::run_pending`); RESTRICT and
   CASCADE/SET actions run immediately; the referenced/referencing rows are locked with `ForKeyShare`/`ForUpdate`.

## Error conventions

All user-visible executor errors are built by `error.rs` (oracle-verified wording):
23502 `null value in column "c" of relation "t" violates not-null constraint` + `Failing row contains (...)`;
23505 `duplicate key value violates unique constraint "t_pkey"` + `Key (id)=(1) already exists.`;
23505 `could not create unique index "i"` + `Key (a)=(1) is duplicated.`;
23514 `new row for relation "t" violates check constraint "c"` + `Failing row contains (...)`;
23503 `insert or update on table "c" violates foreign key constraint "fk"` + `Key (a)=(1) is not present in table "p".` and
`update or delete on table "p" violates foreign key constraint "fk" on table "c"` + `Key (a)=(1) is still referenced from table "c".`;
21000 `ON CONFLICT DO UPDATE command cannot affect row a second time`, `more than one row returned by a subquery used as an expression`;
2201W/2201X `LIMIT/OFFSET must not be negative`; 55P03 `could not obtain lock on row in relation "t"`; 25006 read-only; 42501
`permission denied for table t`. Value rendering in details: text output functions, `null`, no quoting, 64-byte clip with `...`.
Unimplemented behavior is `Err(PgError::unimplemented("EXE-NN", ..))`, never a wrong result.

## How downstream crates use it

* **pgarbor** (`Session`): wraps the live `TxnKv` as `ExecContext` per statement (`ctx_build::with_exec`), calls
  `execute_with_channel` or `open_plan`, handles `is_catalog_retry`, owns transaction control / SET / SHOW / PREPARE / LISTEN
  (the executor returns XX000 for those), provides `GucAccess` and `EngineServices`.
* **pgarbor-wire** never calls this crate.

## Non-goals and known gaps (v1; also for docs/compatibility/KNOWN_GAPS.md)

* No triggers, rules, row-level security, inheritance, partitioning, temp tables, cursors (`DECLARE`), `MERGE`.
* User-defined SQL/PL functions are stored by the catalog but NOT executed (0A000 in expressions, `DO`, `CALL`).
* Deferrable / INITIALLY DEFERRED constraints are checked at statement end, not commit (`SET CONSTRAINTS` is a no-op).
* EvalPlanQual is limited to single-table UPDATE/DELETE/FOR UPDATE (module `modify`, `node_lock`).
* No spilling: sorts, hashes, aggregates and partitions are held in memory. `Backward` index scans collect the interval's keys.
* No planner row-count hints (amendment plan-01 needs a storage source).
* COPY errors lack PG's `CONTEXT: COPY t, line N` (needs `PgError.context`, amendment types-02).
* `IGNORE NULLS` only where PG honors it; windows with `RANGE` offsets require the key type's `+`/`-` operators.
* Serializable is REPEATABLE READ plus write-conflict rules (core known gap).

## Lease index
| lease | file | tier | scope |
|---|---|---|---|
| EXE-01 | tabledesc.rs | haiku | [north-star] TableDesc: per-statement table description (columns, missing values, indexes, CHECKs, FKs) bound from the catalog snapshot. |
| EXE-02 | rowstore.rs | sonnet | [north-star] Heap row storage over Kv: keys, encode/decode with missing-value completion, insert/get/write/delete, lock_row, batched HeapCursor. |
| EXE-03 | index_key.rs | sonnet | [north-star] Index entry construction (unique/NULL suffix rules, partial predicates, expression keys) and scan key-interval arithmetic. |
| EXE-04 | index_maint.rs | sonnet | [north-star] Index maintenance: insert/delete/update entries with race-safe unique checks, conflict probing, unique lookups, index build. |
| EXE-05 | eval.rs | haiku | [north-star] Expression evaluator dispatcher and control-flow kinds (AND/OR/CASE/COALESCE/NULLIF/GREATEST/tests/SQL value functions). |
| EXE-06 | eval_call.rs | haiku | [north-star] Function and operator calls, ANY/ALL over arrays, row comparison; exec-owned vs catalog vs builtin routing. |
| EXE-07 | eval_cast.rs | haiku | [north-star] Casts, domain constraints, ARRAY/ROW constructors, field select, subscripts. |
| EXE-08 | eval_srf.rs | haiku | Set-returning function start (lazy iterators), record expansion, SRF detection. |
| EXE-09 | subplan.rs | sonnet | Sub-plan evaluation: EXISTS/scalar/ARRAY/ROW/ANY/ALL with correlation frames, init-plan caching and hashed ANY. |
| EXE-10 | session_fns.rs | haiku | [north-star] Executor-owned functions: nextval/currval/lastval/setval, set_config/current_setting, advisory locks, pg_sleep, txid_*, pg_notify, cancel. |
| EXE-11 | exec_session.rs | haiku | SessionContext / SessionInfo implementations over shared borrows (external function routing to catalog sysfuncs, pg_settings / sequence-state rows). |
| EXE-12 | node_scan.rs | sonnet | [north-star] Leaf operators: Result, SeqScan, IndexScan (batched, backward via key collection), VirtualScan, ValuesScan. |
| EXE-13 | node_basic.rs | haiku | Filter, Project, Append, Materialize operators. |
| EXE-14 | node_func.rs | haiku | FunctionScan (zip, ordinality) and ProjectSet (lock-step SRFs). |
| EXE-15 | node_cte.rs | sonnet | CTE runtime: lazy evaluation, WITH RECURSIVE iteration with dedup, eager data-modifying CTEs, CteScan/WorkTableScan. |
| EXE-16 | node_join.rs | haiku | [north-star] Nested loop join (inner/left/semi/anti) with parameterized inner rescans. |
| EXE-17 | node_hashjoin.rs | sonnet | [north-star] Hash join on memcomparable key bytes, all join types, residual qual, lazy output. |
| EXE-18 | node_mergejoin.rs | sonnet | Merge join over pre-sorted inputs for all join types with group buffering. |
| EXE-19 | node_agg.rs | sonnet | [north-star] Aggregation: plain/hashed/sorted, GROUP BY, grouping sets with mask, DISTINCT, FILTER, ORDER BY inside aggregates, HAVING. |
| EXE-20 | node_window.rs | sonnet | WindowAgg: partitions, peers, ROWS/RANGE/GROUPS frames with offsets and EXCLUDE, aggregates over frames (incremental / retract). |
| EXE-21 | window_funcs.rs | haiku | Pure window functions: row_number, rank, dense_rank, percent_rank, cume_dist, ntile, lag, lead, first_value, last_value, nth_value. |
| EXE-22 | sortutil.rs | haiku | Sort helpers: key comparison with USING operators, stable row sorting, top-N, memcomparable row keys. |
| EXE-23 | node_sort.rs | haiku | [north-star] Sort (full and top-N) and Limit/Offset/WITH TIES operators. |
| EXE-24 | node_setops.rs | haiku | Distinct (hashed and sorted, DISTINCT ON key slots) and SetOp (INTERSECT/EXCEPT [ALL]). |
| EXE-25 | node_lock.rs | sonnet | LockRows: SELECT FOR UPDATE/SHARE with NOWAIT/SKIP LOCKED and concurrent-change handling. |
| EXE-26 | modify.rs | sonnet | [north-star] ModifyTable driver: drain input (Halloween), per-row INSERT/UPDATE/DELETE dispatch, end-of-statement FK checks, RETURNING, row counts, EPQ policy. |
| EXE-27 | dml_insert.rs | haiku | [north-star] INSERT row assembly (supplied columns, defaults, generated), constraint+heap+index+FK pipeline, ON CONFLICT hand-off. |
| EXE-28 | dml_update.rs | sonnet | [north-star] UPDATE row processing with row lock, EvalPlanQual-lite re-check, new-row construction, generated recompute, in-place write + index/FK maintenance. |
| EXE-29 | dml_delete.rs | haiku | DELETE row processing (lock + EPQ-lite) and the low-level apply_delete used by FK cascades. |
| EXE-30 | dml_conflict.rs | sonnet | INSERT ... ON CONFLICT DO NOTHING / DO UPDATE with arbiters, EXCLUDED rows, WHERE, retry on lost races, affect-twice detection. |
| EXE-31 | constraints.rs | haiku | [north-star] NOT NULL and CHECK enforcement with PG-exact errors and failing-row details. |
| EXE-32 | fk.rs | sonnet | Foreign keys: child-side checks, parent-side NO ACTION/RESTRICT/CASCADE/SET NULL/SET DEFAULT actions, end-of-statement queue, validation. |
| EXE-33 | ddl_table.rs | haiku | [north-star] CREATE TABLE and ALTER TABLE execution through the catalog writer with follow-ups and rewrites. |
| EXE-34 | ddl_followup.rs | sonnet | FollowUp execution (index builds, validations, rewrites, data drops, sequence state) and the table rewrite. |
| EXE-35 | ddl_objects.rs | haiku | [north-star] CREATE SCHEMA/INDEX/SEQUENCE/VIEW/TYPE/DOMAIN/FUNCTION/ROLE, COMMENT, GRANT/REVOKE, REFRESH MATERIALIZED VIEW execution. |
| EXE-36 | ddl_alter.rs | haiku | ALTER RENAME/SCHEMA/OWNER/SEQUENCE/TYPE/DOMAIN/FUNCTION/ROLE/DATABASE execution. |
| EXE-37 | ddl_drop.rs | haiku | DROP (all kinds), TRUNCATE (restart identity, cascade), DROP OWNED, REASSIGN OWNED. |
| EXE-38 | ctas.rs | haiku | CREATE TABLE AS / SELECT INTO and materialized view population / refresh. |
| EXE-39 | utility.rs | haiku | LOCK TABLE, DO/CALL (unsupported), VACUUM/ANALYZE/CLUSTER/CHECKPOINT no-ops and REINDEX. |
| EXE-40 | explain_exec.rs | haiku | EXPLAIN and EXPLAIN ANALYZE execution (plan, run with stats, render). |
| EXE-41 | copy.rs | haiku | COPY text/CSV/binary codecs and COPY FROM/TO execution over a CopyChannel. |
| EXE-42 | perm.rs | haiku | Data-access privilege checks (SELECT/INSERT/UPDATE/DELETE/TRUNCATE on relations and columns, sequence USAGE) over catalog ACLs. |

`depends_on_leases` lists only EARLIER-numbered leases whose code the unit tests of a lease exercise. All cross-file signatures
are frozen, so workers never wait for each other (tests that need an unfinished lease are `#[ignore = "needs ..."]`).

Frozen and implemented (not leased): `lib.rs`, `error.rs`, `result.rs`, `txn.rs`, `memtxn.rs`, `context.rs`, `runtime.rs`,
`build.rs`, `execute.rs`, `ddl_driver.rs`.
