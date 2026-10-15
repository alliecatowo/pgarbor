# pgarbor (embedded API) — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor.json` (API-01..API-16, seed tasks CLI-08, TXN-05,
TXN-06, SQL-13, CAT-05). Depends on all engine crates (common, storage, branches, types, catalog, core, sql, plan, exec). Consumed
by `pgarbor-wire`, `pgarbor-cli` and language bindings.

## Purpose

The embedded database API and the only place where SQL semantics meet the transaction machinery: `Database` (open/create a
`.pga`, branch administration), `Session` (one connection bound to one branch and one logical database), statement dispatch
(parse -> bind -> plan -> execute), the PostgreSQL transaction-block state machine, the GUC table, the extended-protocol
primitives (prepared statements, portals, Describe), notices, cancel, LISTEN/NOTIFY, and a flat FFI-friendly seam. The wire layer
contains no SQL semantics; it translates protocol messages to the methods below.

## Public API

```rust
let db = Database::open_or_create("dev.pga", OpenOptions::default())?;   // Database::{create, open, open_or_create, in_memory}
let mut s = db.session("main", SessionOptions::default())?;              // Database::{session, connect}
let out = s.simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (1)")?;
let raw = s.simple_query_raw("select 1; select 1/0");                    // never Err: results + error + status byte
s.parse("q1", "SELECT * FROM t WHERE id = $1", &[])?;                    // extended protocol primitives
s.bind("", "q1", &[], &[Some(b"1".to_vec())], &[])?; s.describe_portal("")?; s.execute("", 0)?; s.sync()?;
db.create_branch("feature", "main")?;                                    // O(1) fork
```
| Item | Use |
|---|---|
| `Database` (`Clone`, `Send + Sync`) | open/create/in-memory, `session`/`connect`, `role_auth` (for SCRAM/MD5), `cancel_request(pid, secret)`, `notify_hub()`, `engine()`, `close`; branches: `create_branch`, `create_branch_from_snapshot`, `list_branches`, `delete_branch(name, force)`, `rename_branch`, `reset_branch`, `restore_branch`, `purge_deleted`, `create_snapshot`/`drop_snapshot`, `set_default_branch`, `compare_branches`, `export_branch`, `detach_branch`, `compact` |
| `Session` (`!Send`) | `simple_query`, `simple_query_raw`, `query(sql, &[Datum])`, `prepare`/`execute_prepared`, `parse`/`bind`/`describe_statement`/`describe_portal`/`execute`/`close_statement`/`close_portal`/`sync`, `txn_status()`, `backend_key()`, `cancel_token()`, `initial ParameterStatus` via `guc`, `take_notices`, `take_notifications`, `encode_value`/`encode_row`/`decode_param`, `set_copy_channel`, `setting(name)`, `close` |
| `types::*` | `StatementResult`, `StatementOutput` (notices + result + ParameterStatus changes), `SimpleQueryOutput`, `ColumnInfo` (name, type oid/mod/len, table oid, attnum), `FormatCode`, `StatementDescription`, `PortalOutput`/`PortalStatus`, `TxnStatus` (`I`/`T`/`E`), `OpenOptions` (+`vfs` for WASM/in-memory), `SessionOptions`, `RoleAuth`, `BranchInfo`, `CancelToken`, `Notification` |
| `guc::GucTable`, `guc_def` | complete PG 18.6 parameter table (399 rows generated from the oracle by `gen/gen_guc.py`), SET/RESET/SHOW/SET LOCAL semantics |
| `ffi::*` | handle-based flat API (thread-local handle table, text results, JSON serializer); seam for C/Python/Node/WASM shims |

What the wire layer needs, and where it is: startup parameters (`SessionOptions`: user, database, application_name, `options -c`
GUCs via `startup_gucs`, client addr/port, cancel key length), authentication data (`Database::role_auth`), BackendKeyData
(`backend_key`), the initial ParameterStatus set (`GucTable::startup_report`: in_hot_standby, integer_datetimes, TimeZone,
IntervalStyle, search_path, is_superuser, application_name, default_transaction_read_only, scram_iterations, DateStyle,
standard_conforming_strings, session_authorization, client_encoding, server_version `18.6`, server_encoding), per-statement
ParameterStatus changes and notices inside `StatementOutput`/`PortalOutput`, ReadyForQuery byte (`TxnStatus::byte`),
RowDescription (`ColumnInfo`), DataRow encoding (`encode_row`), CancelRequest (`Database::cancel_request`), async notifications
(`take_notifications` + `NotifyHub::set_waker`), COPY (`set_copy_channel` with a channel that wraps CopyData messages).

## Statement pipeline (see `run_stmt.rs`)

activity/limits -> failed-block check -> `begin_statement` snapshot (not for session-control statements) -> catalog env refresh
(`ctx_build::refresh_env`, version-keyed `CatalogCache`) -> bind -> session statements (`txn_block`, `session_utility`, `prepared`)
or plan -> `pgarbor_exec::execute_with_channel` -> result shaping. Catalog retry under READ COMMITTED (`is_catalog_retry`) re-runs
snapshot/bind/plan/execute at most 8 times. Plans are custom for the first 5 executions of a prepared statement, then generic
(cached; invalidated by `catalog::invalidation::plan_is_stale`); result-type changes after DDL raise 0A000
`cached plan must not change result type`.

## Transaction semantics (oracle-verified; derived from xact.c and exec_simple_query)

States `Idle`, `Started` (autocommit), `ImplicitBlock` (multi-statement string / pipelined extended protocol), `InBlock`, `Failed`;
`txn_block.rs` documents every transition and warning text. Highlights:
* A simple Query string with several statements is ONE implicit transaction; `BEGIN` inside converts it, `COMMIT` ends it (with
  WARNING 25P01 `there is no transaction in progress` when no `BEGIN` was seen) and the remaining statements start a new implicit
  transaction; an error rolls back the current implicit transaction and skips the rest.
* The whole string is parsed first: a syntax error fails before anything runs.
* `COMMIT`/`ROLLBACK` outside a block warn 25P01; `BEGIN` in a block warns 25001; `SAVEPOINT` outside a block is error 25P01;
  statements in a failed block fail 25P02 `current transaction is aborted, commands ignored until end of transaction block`;
  COMMIT of a failed block has tag `ROLLBACK`.
* Extended protocol: the implicit transaction lasts until Sync; an error rolls it back at once and the wire layer skips until Sync.
* GUC changes follow the transaction: `SET` is undone by abort, `SET LOCAL` by any end; savepoints nest.

## Key invariants

1. One `Session` = one core `SessionHandle` = one branch + one database (oid 5 in v1); `Session` is `!Send` (portals own `Rc` operator
   trees); `Database`, `CancelToken`, `NotifyHub` are `Send + Sync`.
2. A core `Txn` exists exactly when the block state is not `Idle`; commit/rollback clear portals, apply/drop LISTEN/NOTIFY work and
   GUC levels.
3. `StatementOutput` carries notices in wire order (before the result) and ParameterStatus changes (after).
4. No SQL semantics in `ffi.rs` or the wire layer; both go through `Session`.
5. Unimplemented behavior is `Err(PgError::unimplemented("API-NN", ..))`.

## Error conventions

PG 18.6 wording throughout, e.g. 42704 `unrecognized configuration parameter "x"`, 55P02 `parameter "x" cannot be changed`,
22023 `invalid value for parameter "x": "v"` (+ HINT `Available values: ...`), 26000 `prepared statement "x" does not exist`,
42P05 `prepared statement "x" already exists`, 34000 `portal "x" does not exist`, 08P01 `bind message supplies N parameters, but
prepared statement "x" requires M`, 28000 `role "x" does not exist`, 3D000 `database "x" does not exist`, 25001/25P01/25P02 as above.

## Non-goals and known gaps (v1)

* Single logical database per branch (`CREATE DATABASE` is 0A000); no cursors (`DECLARE`/`FETCH`), no temp schemas.
* `LISTEN` takes effect at commit and notifications are not WAL-ordered across crashes (in-memory hub).
* Row-count hints for the planner (plan amendment 01) are not supplied.
* Statement timeout is enforced by core `Interrupts` polling in executor loops.
* Role-level `ALTER ROLE ... SET` and database-level settings are applied at connect time only.

## Lease index
| lease | file | tier | scope |
|---|---|---|---|
| API-01 | guc_value.rs | haiku | GUC value parsing/validation/formatting by type (bool, int with units, real, enum, string, SET value lists). |
| API-02 | guc.rs | sonnet | [north-star] GucTable: SET/RESET/SHOW/SET LOCAL with transactional rollback, savepoint levels, ParameterStatus reporting, pg_settings rows. |
| API-03 | guc_derive.rs | haiku | Typed views of the GUC table: IoConfig, SearchPath, transaction defaults, wait limits, planner options. |
| API-04 | database.rs | haiku | [north-star] Database open/create/in-memory, session creation, role lookup, cancel, close. |
| API-05 | bootstrap.rs | sonnet | [north-star] Catalog bootstrap of a new branch and the full connect path (role/database resolution, startup GUCs, session registration). |
| API-06 | txn_block.rs | sonnet | [north-star] Transaction block state machine and BEGIN/COMMIT/ROLLBACK/SAVEPOINT/SET TRANSACTION with PG's implicit-block semantics and failed-state rules. |
| API-07 | simple_query.rs | sonnet | [north-star] Simple Query protocol: whole-string parse, multi-statement implicit block semantics, error handling, outputs. |
| API-08 | run_stmt.rs | sonnet | [north-star] One statement through parse->bind->plan->execute with session-statement dispatch, activity/limits, DDL retry. |
| API-09 | ctx_build.rs | haiku | Per-statement context assembly: catalog env refresh, Bind/Plan/Exec contexts, ColumnInfo conversion. |
| API-10 | extended.rs | sonnet | [north-star] Extended protocol primitives: Parse/Bind/Describe/Execute/Close/Sync with PG's error and transaction semantics. |
| API-11 | prepared.rs | haiku | Prepared statement store, parse-time binding, revalidation/re-bind on catalog changes, plan cache (custom/generic), SQL PREPARE/EXECUTE/DEALLOCATE. |
| API-12 | portal.rs | sonnet | Portals (lazy start, row-limit suspension over PlanStream, buffering) and wire value encoding/decoding. |
| API-13 | session_utility.rs | haiku | SET/RESET/SHOW/SET ROLE/SET SESSION AUTHORIZATION/DISCARD/LISTEN/UNLISTEN/NOTIFY. |
| API-14 | engine_info.rs | haiku | EngineServices for the executor: activity/locks/prepared statements/cancel/notify over core diag and session tables. |
| API-15 | ffi.rs | haiku | FFI-friendly flat handle API (thread-local handle table, text results, JSON serializer, panic safety). |
| API-16 | branches.rs | haiku | Branch administration on Database (create/list/delete/rename/reset/restore/snapshots/compare/export/detach/compact). |

Frozen and implemented (not leased): `lib.rs`, `types.rs`, `session.rs` (struct + accessors), `guc_def.rs`, `guc_table.rs`
(generated by `gen/gen_guc.py` from `gen/pg_settings_full.csv`), `notify.rs`, plus the small implemented helpers inside leased files
(`txn_block::{BlockState, allowed_in_failed_block, begin/end_implicit_block}`, `session_utility::exec_session_statement`,
`prepared::PreparedStore`, `portal::PortalStore`, `GucTable::new`).
