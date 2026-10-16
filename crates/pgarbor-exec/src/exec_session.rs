//! `SessionContext` and `SessionInfo` implementations for the executor (lease EXE-11).
//!
//! * [`ExecSessionContext`] implements `pgarbor_types::context::SessionContext` over shared borrows
//!   of the session environment and the transaction (`ExecSessionContext::new(&cx.se, &*cx.txn)`):
//!   `statement_timestamp`/`transaction_timestamp` from `se`, `clock_timestamp` = `se.clock_now()`,
//!   `fill_random` from `pgarbor_core::session::random_bytes`, `random_f64`/`set_seed` on the session
//!   PRNG cell (`set_seed(s)`: state = `((s + 1.0) * 1e15) as u64 | 1`), `notice` -> `se.notice`.
//!   `call_external(proc, args)` routes to `pgarbor_catalog::sysfuncs::{handles, call}` with a
//!   `CatalogCallCtx{env: &se.env, session: &ExecSessionInfo, io_config: &se.io}`; anything else is
//!   0A000 `function {name} is not supported` (exec-owned functions never come through here: the
//!   evaluator intercepts them, see `session_fns`). `call_external_set` likewise with
//!   `sysfuncs::{handles_set, call_set}` wrapping the returned `Vec<Datum>` in an iterator.
//! * [`ExecSessionInfo`] implements `pgarbor_catalog::session::SessionInfo` for virtual relations
//!   and catalog functions: `settings()` = `guc.settings()`, `setting(name)` = `guc.get(name)`,
//!   `sequence_state(oid)` = `txn.seq_state(oid)` mapped to `SequenceStateRow{last_value: None when
//!   !is_called, start_value from the catalog sequence definition, log_cnt, is_called}` (missing
//!   state -> `Ok(None)`), everything else delegates to `se.services`
//!   (`activity`, `locks`, `prepared_statements`, `cursors`, `backend_pid`, `blocking_pids`,
//!   `relation_size`, `server_start_time`, `config_load_time`, `current_query`).
use pgarbor_catalog::session::{
    ActivityRow, CursorRow, LockRow, PreparedStatementRow, SequenceStateRow, SettingRow, SizeKind,
};
use pgarbor_catalog::sysfuncs::CatalogCallCtx;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_core::sequence::SequenceState;
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::context::SessionContext;
use pgarbor_types::func::SetIter;
use pgarbor_types::Datum;

use crate::context::SessionEnv;
use crate::txn::ExecTxn;

/// `SessionContext` over shared borrows (module docs).
pub struct ExecSessionContext<'x, 'a> {
    pub se: &'x SessionEnv<'a>,
    pub txn: &'x dyn ExecTxn,
}

impl<'x, 'a> ExecSessionContext<'x, 'a> {
    pub fn new(se: &'x SessionEnv<'a>, txn: &'x dyn ExecTxn) -> ExecSessionContext<'x, 'a> {
        ExecSessionContext { se, txn }
    }
}

/// 0A000 for a proc that neither the catalog nor the executor implements at this layer.
fn not_supported_fn(proc: &BuiltinProc) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("function {} is not supported", proc.name))
}

impl SessionContext for ExecSessionContext<'_, '_> {
    fn statement_timestamp(&self) -> i64 {
        self.se.statement_ts
    }
    fn transaction_timestamp(&self) -> i64 {
        self.se.txn_ts
    }
    fn clock_timestamp(&self) -> i64 {
        self.se.clock_now()
    }
    fn fill_random(&self, buf: &mut [u8]) {
        match pgarbor_core::session::random_bytes(buf.len()) {
            Ok(bytes) => buf.copy_from_slice(&bytes[..buf.len()]),
            Err(_) => {
                for b in buf.iter_mut() {
                    *b = (self.se.next_random() * 256.0) as u8;
                }
            }
        }
    }
    fn random_f64(&self) -> f64 {
        self.se.next_random()
    }
    fn set_seed(&self, seed: f64) {
        self.se.rng.set((((seed + 1.0) * 1e15) as u64) | 1);
    }
    fn notice(&self, notice: PgError) {
        self.se.notice(notice);
    }
    fn call_external(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
        if pgarbor_catalog::sysfuncs::handles(proc) {
            let info = ExecSessionInfo::new(self.se, self.txn);
            let ctx = CatalogCallCtx { env: &self.se.env, session: &info, io_config: &self.se.io };
            return pgarbor_catalog::sysfuncs::call(&ctx, proc, args);
        }
        Err(not_supported_fn(proc))
    }
    fn call_external_set(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<SetIter> {
        if pgarbor_catalog::sysfuncs::handles_set(proc) {
            let info = ExecSessionInfo::new(self.se, self.txn);
            let ctx = CatalogCallCtx { env: &self.se.env, session: &info, io_config: &self.se.io };
            let rows: Vec<Datum> = pgarbor_catalog::sysfuncs::call_set(&ctx, proc, args)?;
            let it: SetIter = Box::new(rows.into_iter().map(|d: Datum| -> Result<Datum> { Ok(d) }));
            return Ok(it);
        }
        Err(not_supported_fn(proc))
    }
}

/// `SessionInfo` over shared borrows (module docs).
pub struct ExecSessionInfo<'x, 'a> {
    pub se: &'x SessionEnv<'a>,
    pub txn: &'x dyn ExecTxn,
}

impl<'x, 'a> ExecSessionInfo<'x, 'a> {
    pub fn new(se: &'x SessionEnv<'a>, txn: &'x dyn ExecTxn) -> ExecSessionInfo<'x, 'a> {
        ExecSessionInfo { se, txn }
    }
}

/// True when `e` is the error the sequence store raises for a sequence whose state row is absent
/// (`pgarbor_core::sequence::store::missing_state`, reached through `ExecTxn::seq_state`).
fn is_missing_seq_state(e: &PgError, oid: Oid) -> bool {
    e.sqlstate.code() == "XX000" && e.message == format!("sequence state for relation {oid} is missing")
}

/// Map a live sequence state onto the `pg_sequences`/`SELECT * FROM seq` row shape: `last_value` is
/// NULL until the sequence has been used (`is_called = false`).
fn seq_state_row(st: SequenceState, start_value: i64) -> SequenceStateRow {
    SequenceStateRow {
        last_value: if st.is_called { Some(st.last_value) } else { None },
        start_value,
        log_cnt: st.log_cnt,
        is_called: st.is_called,
    }
}

impl pgarbor_catalog::SessionInfo for ExecSessionInfo<'_, '_> {
    fn settings(&self) -> Result<Vec<SettingRow>> {
        self.se.guc.settings()
    }
    fn setting(&self, name: &str) -> Result<Option<String>> {
        Ok(self.se.guc.get(name))
    }
    fn sequence_state(&self, oid: Oid) -> Result<Option<SequenceStateRow>> {
        let start_value = match self.se.env.snapshot().sequence(oid) {
            Some(seq) => seq.start,
            None => return Ok(None),
        };
        let st = match self.txn.seq_state(oid) {
            Ok(st) => st,
            Err(e) if is_missing_seq_state(&e, oid) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(seq_state_row(st, start_value)))
    }
    fn relation_size(&self, oid: Oid, kind: SizeKind) -> Result<i64> {
        self.se.services.relation_size(oid, kind)
    }
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        self.se.services.activity()
    }
    fn locks(&self) -> Result<Vec<LockRow>> {
        self.se.services.locks()
    }
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        self.se.services.prepared_statements()
    }
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        self.se.services.cursors()
    }
    fn backend_pid(&self) -> i32 {
        self.se.services.backend_pid()
    }
    fn blocking_pids(&self, pid: i32) -> Result<Vec<i32>> {
        self.se.services.blocking_pids(pid)
    }
    fn server_start_time(&self) -> i64 {
        self.se.services.server_start_time()
    }
    fn config_load_time(&self) -> i64 {
        self.se.services.config_load_time()
    }
    fn current_query(&self) -> Option<String> {
        self.se.services.current_query()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices};
    use crate::memtxn::MemTxn;
    use pgarbor_catalog::SessionInfo as _;
    use pgarbor_core::sequence::SequenceParams;
    use pgarbor_types::catalog_data::procs_named;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    fn proc(name: &str) -> Result<&'static BuiltinProc> {
        procs_named(name).next().ok_or_else(|| PgError::internal(format!("no builtin proc {name}")))
    }

    /// GUC table with one fixed setting, to check `settings`/`setting` routing.
    struct OneGuc;

    impl crate::context::GucAccess for OneGuc {
        fn get(&self, name: &str) -> Option<String> {
            if name.eq_ignore_ascii_case("DateStyle") {
                Some("ISO, MDY".to_string())
            } else {
                None
            }
        }
        fn set(&self, _name: &str, value: &str, _is_local: bool) -> Result<String> {
            Ok(value.to_string())
        }
        fn check_setting(&self, name: &str, value: &str) -> Result<()> {
            crate::context::GucAccess::check_setting(&NoGuc, name, value)
        }
        fn settings(&self) -> Result<Vec<SettingRow>> {
            Ok(vec![SettingRow { name: "datestyle".to_string(), setting: Some("ISO, MDY".to_string()), ..Default::default() }])
        }
    }

    #[test]
    fn current_database_routes_to_catalog() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let txn = MemTxn::new();
        let cx = ExecSessionContext::new(&se, &txn);
        let d = cx.call_external(proc("current_database")?, &[])?;
        assert!(matches!(&d, Datum::Text(s) if s.as_str() == "postgres"));
        Ok(())
    }

    #[test]
    fn exec_owned_function_is_0a000() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let txn = MemTxn::new();
        let cx = ExecSessionContext::new(&se, &txn);
        let err = match cx.call_external(proc("nextval")?, &[]) {
            Err(e) => e,
            Ok(_) => return Err(PgError::internal("nextval must not be routed through call_external")),
        };
        assert_eq!(err.sqlstate.code(), "0A000");
        assert_eq!(err.message, "function nextval is not supported");
        Ok(())
    }

    #[test]
    fn settings_come_from_guc() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = OneGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let txn = MemTxn::new();
        let info = ExecSessionInfo::new(&se, &txn);
        let rows = info.settings()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "datestyle");
        assert_eq!(info.setting("DATESTYLE")?.as_deref(), Some("ISO, MDY"));
        assert_eq!(info.setting("nope")?, None);
        Ok(())
    }

    #[test]
    fn unknown_sequence_oid_is_none() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let txn = MemTxn::new();
        let info = ExecSessionInfo::new(&se, &txn);
        assert_eq!(info.sequence_state(999_999)?, None);
        Ok(())
    }

    #[test]
    fn never_used_sequence_row_has_null_last_value() {
        let row = seq_state_row(SequenceState::initial(7), 7);
        assert_eq!(row.last_value, None);
        assert!(!row.is_called);
        assert_eq!(row.start_value, 7);
        assert_eq!(row.log_cnt, 0);

        let used = seq_state_row(SequenceState { last_value: 9, log_cnt: 3, is_called: true }, 7);
        assert_eq!(used.last_value, Some(9));
        assert!(used.is_called);
        assert_eq!(used.log_cnt, 3);
    }

    #[test]
    fn missing_state_message_is_recognised_only_for_its_oid() {
        let e = PgError::internal("sequence state for relation 42 is missing");
        assert!(is_missing_seq_state(&e, 42));
        assert!(!is_missing_seq_state(&e, 43));
        let other = PgError::internal("something else");
        assert!(!is_missing_seq_state(&other, 42));
    }

    #[test]
    fn memtxn_seq_state_after_create_is_unused() -> Result<()> {
        use crate::txn::ExecTxn as _;
        let mut txn = MemTxn::new();
        let params = SequenceParams {
            name: "s".to_string(),
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            start: 5,
            cache: 1,
            cycle: false,
        };
        txn.seq_create(16_500, &params)?;
        let st = txn.seq_state(16_500)?;
        let row = seq_state_row(st, params.start);
        assert_eq!(row.last_value, None);
        assert!(!row.is_called);
        assert_eq!(row.start_value, 5);
        Ok(())
    }

    #[test]
    #[ignore = "needs a catalog sequence fixture: pgarbor_catalog::testing has no CREATE SEQUENCE helper yet"]
    fn sequence_state_of_catalog_sequence_is_unused() -> Result<()> {
        Ok(())
    }
}
