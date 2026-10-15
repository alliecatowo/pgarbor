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
use pgarbor_common::{Oid, PgError, Result};
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
    #[allow(unused_variables)]
    fn call_external(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
        Err(PgError::unimplemented("EXE-11", "external function dispatch (ExecSessionContext::call_external)"))
    }
    #[allow(unused_variables)]
    fn call_external_set(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<SetIter> {
        Err(PgError::unimplemented("EXE-11", "external set function dispatch (ExecSessionContext::call_external_set)"))
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

impl pgarbor_catalog::SessionInfo for ExecSessionInfo<'_, '_> {
    #[allow(unused_variables)]
    fn settings(&self) -> Result<Vec<SettingRow>> {
        Err(PgError::unimplemented("EXE-11", "pg_settings rows (ExecSessionInfo::settings)"))
    }
    #[allow(unused_variables)]
    fn sequence_state(&self, oid: Oid) -> Result<Option<SequenceStateRow>> {
        Err(PgError::unimplemented("EXE-11", "sequence state rows (ExecSessionInfo::sequence_state)"))
    }
    #[allow(unused_variables)]
    fn relation_size(&self, oid: Oid, kind: SizeKind) -> Result<i64> {
        Err(PgError::unimplemented("EXE-11", "relation sizes (ExecSessionInfo::relation_size)"))
    }
    #[allow(unused_variables)]
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        Err(PgError::unimplemented("EXE-11", "pg_stat_activity rows (ExecSessionInfo::activity)"))
    }
    #[allow(unused_variables)]
    fn locks(&self) -> Result<Vec<LockRow>> {
        Err(PgError::unimplemented("EXE-11", "pg_locks rows (ExecSessionInfo::locks)"))
    }
    #[allow(unused_variables)]
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        Err(PgError::unimplemented("EXE-11", "pg_prepared_statements rows (ExecSessionInfo::prepared_statements)"))
    }
    #[allow(unused_variables)]
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        Err(PgError::unimplemented("EXE-11", "pg_cursors rows (ExecSessionInfo::cursors)"))
    }
    fn backend_pid(&self) -> i32 {
        self.se.services.backend_pid()
    }
    #[allow(unused_variables)]
    fn blocking_pids(&self, pid: i32) -> Result<Vec<i32>> {
        Err(PgError::unimplemented("EXE-11", "pg_blocking_pids (ExecSessionInfo::blocking_pids)"))
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
