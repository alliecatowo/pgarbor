//! Functions implemented by the executor because they need the transaction mutably or session
//! state the catalog cannot see (lease EXE-10, north-star). These are `pg_proc` rows with
//! `ImplModule::External` that `pgarbor_catalog::sysfuncs::handles` does NOT claim. The evaluator
//! (`eval_call::call_function`) checks [`handles`] BEFORE using `FuncHandle::call`.
//!
//! | function | semantics (PG 18.6, oracle-verified) |
//! |---|---|
//! | `nextval(regclass)` | `ExecTxn::seq_nextval(relid, params)`. `params` from `env.snapshot().sequence(relid)` (`SequenceParams{name, increment, min_value: min, max_value: max, start, cache, cycle}`); a relation that is not a sequence -> 42809 `"t" is not a sequence`; unknown OID -> 42P01 `relation with OID N does not exist`. Limit reached -> 2200H `nextval: reached maximum value of sequence "s" (2)` (raised by core). Returns int8. |
//! | `currval(regclass)` | `seq_currval(relid, name)`; 55000 `currval of sequence "s" is not yet defined in this session`. |
//! | `lastval()` | `seq_lastval()`; 55000 `lastval is not yet defined in this session`. |
//! | `setval(regclass, bigint)` / `(regclass, bigint, boolean)` | `seq_setval(relid, params, value, is_called)` (default `is_called = true`); out of range 22003 `setval: value 11 is out of bounds for sequence "s" (1..10)`; returns the value. |
//! | `set_config(name, value, is_local)` | `guc.set(name, value, is_local)`; NULL value = reset to default; returns the new value text. |
//! | `current_setting(name)` / `(name, missing_ok)` | `guc.get`; unknown -> 42704 `unrecognized configuration parameter "x"` unless `missing_ok` (then NULL). |
//! | `pg_advisory_lock[_shared]`, `pg_advisory_xact_lock[_shared]`, `pg_try_advisory_*`, `pg_advisory_unlock[_shared]`, `pg_advisory_unlock_all` | `ExecTxn::advisory_*`: `(bigint)` -> `AdvisoryKey::Single`, `(int4, int4)` -> `AdvisoryKey::Pair`; `lock` forms return void (`Datum::Null`), `try` forms boolean, unlock forms boolean plus the WARNING from `UnlockResult::warning` pushed to `cx.se.notice`. Scope: session for `pg_advisory_lock*`/`pg_try_advisory_lock*`, transaction for `*_xact_*`. |
//! | `pg_sleep(float8)`, `pg_sleep_for(interval)`, `pg_sleep_until(timestamptz)` | sleep in slices of <= 10 ms calling `ex.check_interrupts()` between slices (cancel/statement_timeout must interrupt); returns void (`Null`). |
//! | `txid_current()`, `pg_current_xact_id()` | `Int8(txn_id)` (xid8 maps to `Datum::Int8`); the `_if_assigned` forms return NULL until the transaction has written (use `ExecTxn::txn_id` and a "has written" hint: always return the id once any write happened, else NULL; v1 may return the id). |
//! | `txid_status(bigint)`, `pg_xact_status(xid8)` | `committed` for ids below the current one, `in progress` for the current id, NULL otherwise (v1 approximation). |
//! | `pg_last_committed_xact()` | 0A000 (`track_commit_timestamp` is off in PG by default: `could not get commit timestamp data`, 55000, HINT `Make sure the configuration parameter "track_commit_timestamp" is set.`). |
//! | `pg_notify(channel, payload)` | `services.notify`; returns void. NULL channel -> 22004 `channel name cannot be empty`... (PG: empty string error `channel name cannot be empty`). |
//! | `pg_listening_channels()` (set-returning) | `services.listening_channels()` one text row each. |
//! | `pg_cancel_backend(pid)`, `pg_terminate_backend(pid[, timeout])` | `services.cancel_backend(pid, terminate)`. |
//! | `pg_trigger_depth()` | 0. `pg_export_snapshot()` | 0A000. |
//! Privileges (`perm::require_sequence_privilege`): nextval USAGE|UPDATE, currval SELECT|USAGE, setval UPDATE;
//! failure 42501 `permission denied for sequence s`.
//! Strictness is applied by the evaluator before calling (strict procs never see NULL arguments).
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::{Datum, TypeId};

use crate::runtime::Executor;

/// Names of the procs this module implements (exact `pg_proc.proname`, all overloads).
pub const EXEC_OWNED: &[&str] = &[
    "nextval",
    "currval",
    "lastval",
    "setval",
    "set_config",
    "current_setting",
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_shared",
    "pg_advisory_unlock_all",
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    "txid_current",
    "txid_current_if_assigned",
    "pg_current_xact_id",
    "pg_current_xact_id_if_assigned",
    "txid_status",
    "pg_xact_status",
    "pg_last_committed_xact",
    "pg_notify",
    "pg_listening_channels",
    "pg_cancel_backend",
    "pg_terminate_backend",
    "pg_trigger_depth",
    "pg_export_snapshot",
];

/// True if `proc` is implemented here (exact name match against [`EXEC_OWNED`]).
pub fn handles(proc: &BuiltinProc) -> bool {
    EXEC_OWNED.contains(&proc.name)
}

/// Evaluate an exec-owned scalar function (module docs).
#[allow(unused_variables)]
pub fn call(ex: &mut Executor, proc: &BuiltinProc, args: &[Datum], arg_types: &[TypeId]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-10", "executor-owned functions (session_fns::call)"))
}

/// Evaluate an exec-owned set-returning function (`pg_listening_channels`).
#[allow(unused_variables)]
pub fn call_set(ex: &mut Executor, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-10", "executor-owned set functions (session_fns::call_set)"))
}
