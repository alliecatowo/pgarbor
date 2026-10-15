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
use std::time::{Duration};
use pgarbor_common::time::{Instant};

use pgarbor_catalog::model::common::acl_mode;
use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_core::locks::{AdvisoryKey, AdvisoryMode, AdvisoryScope};
use pgarbor_core::sequence::SequenceParams;
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::{Datum, TypeId};

use crate::perm;
use crate::runtime::Executor;
use crate::txn::ExecTxn;

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

/// Longest sleep slice between two interrupt checks (PG sleeps on its latch; we poll every 10 ms).
const SLEEP_SLICE: Duration = Duration::from_millis(10);
/// `NAMEDATALEN`: a channel name must be strictly shorter (async.c `Async_Notify`).
const NAMEDATALEN: usize = 64;
/// `NOTIFY_PAYLOAD_MAX_LENGTH` = BLCKSZ - NAMEDATALEN - 128 (async.c).
const NOTIFY_PAYLOAD_MAX_LENGTH: usize = 8000;
/// Microseconds per day, and `DAYS_PER_MONTH` as PG's interval-to-duration conversion uses them.
const USECS_PER_DAY: i128 = 86_400_000_000;
const DAYS_PER_MONTH: i128 = 30;

/// True if `proc` is implemented here (exact name match against [`EXEC_OWNED`]).
pub fn handles(proc: &BuiltinProc) -> bool {
    EXEC_OWNED.contains(&proc.name)
}

/// Evaluate an exec-owned scalar function (module docs).
pub fn call(ex: &mut Executor, proc: &BuiltinProc, args: &[Datum], arg_types: &[TypeId]) -> Result<Datum> {
    let _ = arg_types;
    call_named(ex, proc.name, args)
}

/// Evaluate an exec-owned set-returning function (`pg_listening_channels`).
pub fn call_set(ex: &mut Executor, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    set_named(ex, proc.name, args)
}

/// Internal argument error for a value the evaluator should have coerced already (XX000).
fn bad_arg(i: usize) -> PgError {
    PgError::internal(format!("unexpected type for argument {} of executor function", i + 1))
}

/// Integer argument as i64 (int8, int4 or int2 values).
fn int_arg(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Int8(v)) => Ok(*v),
        Some(Datum::Int4(v)) => Ok(i64::from(*v)),
        Some(Datum::Int2(v)) => Ok(i64::from(*v)),
        _ => Err(bad_arg(i)),
    }
}

/// `regclass` argument (always `Datum::Oid`).
fn oid_arg(args: &[Datum], i: usize) -> Result<Oid> {
    match args.get(i) {
        Some(Datum::Oid(v)) => Ok(*v),
        _ => Err(bad_arg(i)),
    }
}

/// float8 argument (float4 is widened).
fn float_arg(args: &[Datum], i: usize) -> Result<f64> {
    match args.get(i) {
        Some(Datum::Float8(v)) => Ok(*v),
        Some(Datum::Float4(v)) => Ok(f64::from(*v)),
        _ => Err(bad_arg(i)),
    }
}

/// text argument.
fn text_arg(args: &[Datum], i: usize) -> Result<&str> {
    match args.get(i) {
        Some(d) => d.as_str().ok_or_else(|| bad_arg(i)),
        None => Err(bad_arg(i)),
    }
}

/// Optional boolean argument: absent or NULL gives `default`.
fn bool_or(args: &[Datum], i: usize, default: bool) -> bool {
    args.get(i).and_then(Datum::as_bool).unwrap_or(default)
}

/// Advisory key from the argument list: one bigint, or two int4 values.
fn advisory_key(args: &[Datum]) -> Result<AdvisoryKey> {
    match args.len() {
        1 => Ok(AdvisoryKey::Single(int_arg(args, 0)?)),
        2 => {
            let a = i32::try_from(int_arg(args, 0)?).map_err(|_| bad_arg(0))?;
            let b = i32::try_from(int_arg(args, 1)?).map_err(|_| bad_arg(1))?;
            Ok(AdvisoryKey::Pair(a, b))
        }
        _ => Err(PgError::internal("advisory lock function called with an unexpected number of arguments")),
    }
}

/// Mode and scope encoded in an advisory function name (`_shared` suffix, `_xact_` infix).
fn advisory_flags(name: &str) -> (AdvisoryMode, AdvisoryScope) {
    let mode = if name.ends_with("_shared") { AdvisoryMode::Shared } else { AdvisoryMode::Exclusive };
    let scope = if name.contains("_xact_") { AdvisoryScope::Transaction } else { AdvisoryScope::Session };
    (mode, scope)
}

/// Sequence parameters of the relation `relid` from a catalog environment. A relation that exists but
/// is not a sequence gives 42809; an unknown OID gives 42P01.
fn seq_params_in(env: &CatalogEnv, relid: Oid) -> Result<SequenceParams> {
    let snap = env.snapshot();
    let identity = snap.class_identity(relid);
    match snap.sequence(relid) {
        Some(seq) => {
            let name = match identity {
                Some((_, n, _)) => n.to_string(),
                None => seq.name.clone(),
            };
            Ok(SequenceParams {
                name,
                increment: seq.increment,
                min_value: seq.min,
                max_value: seq.max,
                start: seq.start,
                cache: seq.cache,
                cycle: seq.cycle,
            })
        }
        None => match identity {
            Some((_, name, _)) => Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not a sequence"))),
            None => Err(PgError::new(SqlState::UNDEFINED_TABLE, format!("relation with OID {relid} does not exist"))),
        },
    }
}

/// [`seq_params_in`] over the executor's current catalog snapshot.
fn sequence_params(ex: &Executor, relid: Oid) -> Result<SequenceParams> {
    seq_params_in(&ex.se().env, relid)
}

/// Convert a float8 number of seconds to a sleep duration (NaN and negatives sleep 0; infinity is
/// capped at `Duration::MAX`).
fn secs_to_duration(secs: f64) -> Duration {
    if secs.is_nan() || secs <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}

/// Sleep for `total`, checking interrupts before every slice so cancel and statement_timeout end the
/// wait promptly (PG `pg_sleep` loops over WaitLatch the same way).
fn sleep_for(ex: &Executor, total: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        ex.check_interrupts()?;
        let elapsed = start.elapsed();
        if elapsed >= total {
            return Ok(());
        }
        std::thread::sleep((total - elapsed).min(SLEEP_SLICE));
    }
}

/// Scalar dispatch by `pg_proc.proname` (module docs).
fn call_named(ex: &mut Executor, name: &str, args: &[Datum]) -> Result<Datum> {
    match name {
        "nextval" => {
            let relid = oid_arg(args, 0)?;
            let params = sequence_params(ex, relid)?;
            perm::require_sequence_privilege(ex.se(), relid, acl_mode::USAGE | acl_mode::UPDATE)?;
            let v = ex.txn().seq_nextval(relid, &params)?;
            Ok(Datum::Int8(v))
        }
        "currval" => {
            let relid = oid_arg(args, 0)?;
            let params = sequence_params(ex, relid)?;
            perm::require_sequence_privilege(ex.se(), relid, acl_mode::SELECT | acl_mode::USAGE)?;
            let v = ex.txn().seq_currval(relid, &params.name)?;
            Ok(Datum::Int8(v))
        }
        "lastval" => {
            let v = ex.txn().seq_lastval()?;
            Ok(Datum::Int8(v))
        }
        "setval" => {
            let relid = oid_arg(args, 0)?;
            let value = int_arg(args, 1)?;
            let is_called = bool_or(args, 2, true);
            let params = sequence_params(ex, relid)?;
            perm::require_sequence_privilege(ex.se(), relid, acl_mode::UPDATE)?;
            let v = ex.txn().seq_setval(relid, &params, value, is_called)?;
            Ok(Datum::Int8(v))
        }
        "set_config" => {
            let setting = text_arg(args, 0)?;
            let is_local = bool_or(args, 2, false);
            match args.get(1) {
                // NULL resets to the default, which needs a GUC reset that `GucAccess` does not offer.
                Some(Datum::Null) | None => Err(PgError::unimplemented(
                    "EXE-10",
                    "set_config(name, NULL) reset to default (GucAccess has no reset)",
                )),
                Some(_) => {
                    let value = text_arg(args, 1)?;
                    let new_value = ex.se().guc.set(setting, value, is_local)?;
                    Ok(Datum::Text(new_value))
                }
            }
        }
        "current_setting" => {
            let setting = text_arg(args, 0)?;
            let missing_ok = bool_or(args, 1, false);
            match ex.se().guc.get(setting) {
                Some(v) => Ok(Datum::Text(v)),
                None if missing_ok => Ok(Datum::Null),
                None => Err(PgError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("unrecognized configuration parameter \"{setting}\""),
                )),
            }
        }
        "pg_advisory_lock" | "pg_advisory_lock_shared" | "pg_advisory_xact_lock" | "pg_advisory_xact_lock_shared" => {
            let key = advisory_key(args)?;
            let (mode, scope) = advisory_flags(name);
            ex.txn().advisory_lock(key, mode, scope, true)?;
            Ok(Datum::Null)
        }
        "pg_try_advisory_lock"
        | "pg_try_advisory_lock_shared"
        | "pg_try_advisory_xact_lock"
        | "pg_try_advisory_xact_lock_shared" => {
            let key = advisory_key(args)?;
            let (mode, scope) = advisory_flags(name);
            let got = ex.txn().advisory_lock(key, mode, scope, false)?;
            Ok(Datum::Bool(got))
        }
        "pg_advisory_unlock" | "pg_advisory_unlock_shared" => {
            let key = advisory_key(args)?;
            let mode = if name.ends_with("_shared") { AdvisoryMode::Shared } else { AdvisoryMode::Exclusive };
            let result = ex.txn().advisory_unlock(key, mode)?;
            if let Some(warning) = result.warning {
                ex.se().notice(warning);
            }
            Ok(Datum::Bool(result.released))
        }
        "pg_advisory_unlock_all" => {
            ex.txn().advisory_unlock_all()?;
            Ok(Datum::Null)
        }
        "pg_sleep" => {
            let secs = float_arg(args, 0)?;
            sleep_for(ex, secs_to_duration(secs))?;
            Ok(Datum::Null)
        }
        "pg_sleep_for" => {
            let iv = args.first().and_then(Datum::as_interval).ok_or_else(|| bad_arg(0))?;
            // Months count as 30 days and days as 24 hours, as PG's interval-to-time conversion does.
            let total = i128::from(iv.micros)
                + i128::from(iv.days) * USECS_PER_DAY
                + i128::from(iv.months) * DAYS_PER_MONTH * USECS_PER_DAY;
            let duration = if total <= 0 {
                Duration::ZERO
            } else {
                Duration::from_micros(u64::try_from(total).unwrap_or(u64::MAX))
            };
            sleep_for(ex, duration)?;
            Ok(Datum::Null)
        }
        "pg_sleep_until" => {
            let target = match args.first() {
                Some(Datum::TimestampTz(t)) => *t,
                _ => return Err(bad_arg(0)),
            };
            let now = ex.se().clock_now();
            let micros = i128::from(target) - i128::from(now);
            let duration = if micros <= 0 {
                Duration::ZERO
            } else {
                Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
            };
            sleep_for(ex, duration)?;
            Ok(Datum::Null)
        }
        // v1: the transaction id is returned even before the first write (module docs allow this).
        "txid_current" | "txid_current_if_assigned" | "pg_current_xact_id" | "pg_current_xact_id_if_assigned" => {
            let id = ex.txn().txn_id();
            Ok(Datum::Int8(id as i64))
        }
        "txid_status" | "pg_xact_status" => {
            let id = int_arg(args, 0)?;
            let current = ex.txn().txn_id();
            let status = match u64::try_from(id) {
                Ok(v) if v < current => Some("committed"),
                Ok(v) if v == current => Some("in progress"),
                _ => None,
            };
            Ok(match status {
                Some(s) => Datum::text(s),
                None => Datum::Null,
            })
        }
        "pg_last_committed_xact" => Err(PgError::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "could not get commit timestamp data",
        )
        .with_hint("Make sure the configuration parameter \"track_commit_timestamp\" is set.")),
        "pg_notify" => {
            let channel = text_arg(args, 0)?;
            let payload = text_arg(args, 1)?;
            if channel.is_empty() {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "channel name cannot be empty"));
            }
            if channel.len() >= NAMEDATALEN {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "channel name too long"));
            }
            if payload.len() >= NOTIFY_PAYLOAD_MAX_LENGTH {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "payload string too long"));
            }
            ex.se().services.notify(channel, payload)?;
            Ok(Datum::Null)
        }
        "pg_listening_channels" => Err(PgError::internal(
            "set-returning function pg_listening_channels called in a scalar position",
        )),
        "pg_cancel_backend" => {
            let pid = i32::try_from(int_arg(args, 0)?).map_err(|_| bad_arg(0))?;
            let signalled = ex.se().services.cancel_backend(pid, false)?;
            Ok(Datum::Bool(signalled))
        }
        "pg_terminate_backend" => {
            let pid = i32::try_from(int_arg(args, 0)?).map_err(|_| bad_arg(0))?;
            // The timeout is validated like PG's; the engine service exposes no wait for the backend to
            // exit, so the result is the termination signal's result (known gap).
            if args.len() >= 2 && int_arg(args, 1)? < 0 {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "\"timeout\" must not be negative"));
            }
            let signalled = ex.se().services.cancel_backend(pid, true)?;
            Ok(Datum::Bool(signalled))
        }
        "pg_trigger_depth" => Ok(Datum::Int4(0)),
        "pg_export_snapshot" => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "pg_export_snapshot is not supported")),
        other => Err(PgError::internal(format!("function {other} is not executor-owned"))),
    }
}

/// Set-returning dispatch by `pg_proc.proname` (module docs).
fn set_named(ex: &mut Executor, name: &str, args: &[Datum]) -> Result<Vec<Datum>> {
    let _ = args;
    match name {
        "pg_listening_channels" => Ok(ex.se().services.listening_channels().into_iter().map(Datum::Text).collect()),
        other => Err(PgError::internal(format!("function {other} is not an executor-owned set function"))),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use pgarbor_catalog::ddl::spec::{CreateSequenceSpec, SequenceOptions};
    use pgarbor_catalog::model::relation::Persistence;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;

    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// Bootstrapped catalog with one sequence `s` (definition `opts`), its state seeded in a `MemTxn`.
    fn setup(opts: SequenceOptions) -> (MemTxn, CatalogEnv, Oid) {
        let (mut kv, env) = testing::bootstrapped().unwrap();
        let namespace = env.creation_namespace().unwrap();
        let owner = env.current_user();
        let spec = CreateSequenceSpec {
            namespace,
            name: "s".to_string(),
            owner,
            persistence: Persistence::Permanent,
            options: opts,
            owned_by: None,
            if_not_exists: false,
        };
        let mut created: Option<Oid> = None;
        let (_res, next_env) = testing::run(&mut kv, &env, |t| {
            created = t.create_sequence(&spec)?;
            Ok(())
        })
        .unwrap();
        let relid = created.unwrap();
        let params = seq_params_in(&next_env, relid).unwrap();
        let mut txn = MemTxn::new();
        txn.kv = kv;
        txn.seq_create(relid, &params).unwrap();
        (txn, next_env, relid)
    }

    fn default_opts() -> SequenceOptions {
        SequenceOptions {
            type_oid: None,
            increment: None,
            minvalue: None,
            maxvalue: None,
            start: None,
            restart: None,
            cache: None,
            cycle: None,
        }
    }

    /// Run `f` with an executor over `txn` and `env`.
    fn run_ex<T>(txn: &mut MemTxn, env: CatalogEnv, f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    #[test]
    fn secs_conversion_handles_edges() {
        assert_eq!(secs_to_duration(-1.0), Duration::ZERO);
        assert_eq!(secs_to_duration(f64::NAN), Duration::ZERO);
        assert_eq!(secs_to_duration(0.5), Duration::from_millis(500));
        assert_eq!(secs_to_duration(f64::INFINITY), Duration::MAX);
    }

    #[test]
    fn advisory_flags_follow_names() {
        assert_eq!(advisory_flags("pg_advisory_lock"), (AdvisoryMode::Exclusive, AdvisoryScope::Session));
        assert_eq!(advisory_flags("pg_try_advisory_xact_lock_shared"), (AdvisoryMode::Shared, AdvisoryScope::Transaction));
        assert_eq!(advisory_flags("pg_advisory_unlock_shared"), (AdvisoryMode::Shared, AdvisoryScope::Session));
    }

    #[test]
    fn unknown_relation_is_42p01() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let err = run_ex(&mut txn, env, |ex| call_named(ex, "nextval", &[Datum::Oid(999_999)])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42P01");
        assert_eq!(err.message, "relation with OID 999999 does not exist");
    }

    #[test]
    fn nextval_sequence_1_2_3_and_currval() {
        let (mut txn, env, relid) = setup(default_opts());
        let out = run_ex(&mut txn, env, |ex| {
            let a = call_named(ex, "nextval", &[Datum::Oid(relid)])?;
            let b = call_named(ex, "nextval", &[Datum::Oid(relid)])?;
            let c = call_named(ex, "nextval", &[Datum::Oid(relid)])?;
            let cur = call_named(ex, "currval", &[Datum::Oid(relid)])?;
            let last = call_named(ex, "lastval", &[])?;
            Ok(vec![a, b, c, cur, last])
        })
        .unwrap();
        let ints: Vec<i64> = out.iter().map(|d| d.as_i64().unwrap()).collect();
        assert_eq!(ints, vec![1, 2, 3, 3, 3]);
    }

    #[test]
    fn setval_then_nextval() {
        let (mut txn, env, relid) = setup(default_opts());
        let out = run_ex(&mut txn, env, |ex| {
            let set = call_named(ex, "setval", &[Datum::Oid(relid), Datum::Int8(10)])?;
            let next = call_named(ex, "nextval", &[Datum::Oid(relid)])?;
            Ok((set, next))
        })
        .unwrap();
        assert_eq!(out.0.as_i64(), Some(10));
        assert_eq!(out.1.as_i64(), Some(11));
    }

    #[test]
    fn currval_unused_sequence_is_55000() {
        let (mut txn, env, relid) = setup(default_opts());
        let err = run_ex(&mut txn, env, |ex| call_named(ex, "currval", &[Datum::Oid(relid)])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        assert_eq!(err.message, "currval of sequence \"s\" is not yet defined in this session");
    }

    #[test]
    fn advisory_unlock_never_taken_warns() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let (released, notices) = run_ex(&mut txn, env, |ex| {
            let v = call_named(ex, "pg_advisory_unlock", &[Datum::Int8(42)])?;
            let n = ex.se().take_notices();
            Ok((v, n))
        })
        .unwrap();
        assert_eq!(released.as_bool(), Some(false));
        assert_eq!(notices.len(), 1);
        assert!(notices[0].message.contains("you don't own a lock of type ExclusiveLock"));
    }

    #[test]
    fn advisory_lock_then_unlock_is_true() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let (locked, unlocked) = run_ex(&mut txn, env, |ex| {
            let l = call_named(ex, "pg_advisory_lock", &[Datum::Int4(1), Datum::Int4(2)])?;
            let u = call_named(ex, "pg_advisory_unlock", &[Datum::Int4(1), Datum::Int4(2)])?;
            Ok((l, u))
        })
        .unwrap();
        assert!(locked.is_null());
        assert_eq!(unlocked.as_bool(), Some(true));
    }

    #[test]
    fn current_setting_unknown_name() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let (missing, err) = run_ex(&mut txn, env, |ex| {
            let ok = call_named(ex, "current_setting", &[Datum::text("no_such_setting"), Datum::Bool(true)])?;
            Ok((ok, call_named(ex, "current_setting", &[Datum::text("no_such_setting")]).unwrap_err()))
        })
        .unwrap();
        assert!(missing.is_null());
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "unrecognized configuration parameter \"no_such_setting\"");
    }

    #[test]
    fn txid_functions_use_transaction_id() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let out = run_ex(&mut txn, env, |ex| {
            let cur = call_named(ex, "txid_current", &[])?;
            let committed = call_named(ex, "txid_status", &[Datum::Int8(2)])?;
            let running = call_named(ex, "pg_xact_status", &[Datum::Int8(3)])?;
            let future = call_named(ex, "txid_status", &[Datum::Int8(4)])?;
            Ok((cur, committed, running, future))
        })
        .unwrap();
        assert_eq!(out.0.as_i64(), Some(3));
        assert_eq!(out.1.as_str(), Some("committed"));
        assert_eq!(out.2.as_str(), Some("in progress"));
        assert!(out.3.is_null());
    }

    #[test]
    fn pg_sleep_returns_after_the_duration() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let start = Instant::now();
        let v = run_ex(&mut txn, env, |ex| call_named(ex, "pg_sleep", &[Datum::Float8(0.05)])).unwrap();
        assert!(v.is_null());
        assert!(start.elapsed() >= Duration::from_millis(45));
    }

    #[test]
    fn notify_rejects_empty_channel() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let err = run_ex(&mut txn, env, |ex| call_named(ex, "pg_notify", &[Datum::text(""), Datum::text("x")])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "22023");
        assert_eq!(err.message, "channel name cannot be empty");
    }

    #[test]
    fn listening_channels_empty_without_services() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let rows = run_ex(&mut txn, env, |ex| set_named(ex, "pg_listening_channels", &[])).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn last_committed_xact_needs_commit_timestamps() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let err = run_ex(&mut txn, env, |ex| call_named(ex, "pg_last_committed_xact", &[])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        assert_eq!(err.message, "could not get commit timestamp data");
    }

    #[test]
    fn trigger_depth_is_zero() {
        let mut txn = MemTxn::new();
        let (kv, env) = testing::bootstrapped().unwrap();
        txn.kv = kv;
        let v = run_ex(&mut txn, env, |ex| call_named(ex, "pg_trigger_depth", &[])).unwrap();
        assert_eq!(v, Datum::Int4(0));
    }

    #[test]
    #[ignore = "needs EXE-42"]
    fn nextval_requires_sequence_privilege() {
        let (mut txn, env, relid) = setup(default_opts());
        let _ = run_ex(&mut txn, env, |ex| call_named(ex, "nextval", &[Datum::Oid(relid)])).unwrap();
    }
}
