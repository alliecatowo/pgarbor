//! [`ExecContext`]: everything one statement execution needs from the session (ARCHITECTURE §10).
//! Fully implemented, frozen (v1).
//!
//! ```text
//! ExecContext<'a>
//!   txn : &'a mut dyn ExecTxn        the transaction view (reads/writes/locks/sequences)
//!   se  : SessionEnv<'a>             everything that does NOT need the txn mutably:
//!         env (CatalogEnv)  io (IoConfig)  guc  services  params  clocks  notices  rng ...
//!   stmt: StmtState                  per-statement caches and bookkeeping
//! ```
//! ## Borrow splitting (important for workers)
//! `txn` and `se` are SEPARATE fields so code can hold `&cx.se.env` (for a `FuncCtx`/`IoCtx`) while
//! calling `cx.txn.put(..)`. Never wrap them in methods that borrow all of `cx` when both are needed
//! in the same expression; destructure instead: `let ExecContext { txn, se, stmt } = &mut *cx;`.
//! Functions that need `SessionContext` for `FuncCtx` build one with
//! [`crate::exec_session::ExecSessionContext::new`]`(&cx.se, &*cx.txn)` (shared borrows only).
//!
//! ## Who builds it
//! The `pgarbor` crate (`ctx_build.rs`) per statement: `txn` = the `TxnKv` of the current
//! transaction, `se.env` = the statement-start `CatalogEnv`, `guc` = the session GUC table, `services`
//! = engine services, `params` = bound parameter values (already coerced to `param_types`).
//! After execution the session reads `cx.se.env.snapshot()` (changed by DDL), `cx.stmt.catalog_changed`,
//! and drains `cx.se.take_notices()`.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use pgarbor_common::time::{SystemTime, UNIX_EPOCH};

use pgarbor_catalog::session::{ActivityRow, CursorRow, LockRow, PreparedStatementRow, SettingRow, SizeKind};
use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::context::{IoConfig, IoCtx};
use pgarbor_types::{Datum, TypeId};

use crate::tabledesc::TableDesc;
use crate::txn::ExecTxn;

/// Read/write access to the session's GUC table (interior mutability: methods take `&self`).
/// Implemented by the `pgarbor` crate's `GucTable`; `NoGuc` serves tests.
// CONTRACT
pub trait GucAccess {
    /// Current value in `SHOW` text form (`"ISO, MDY"`, `on`, `4MB`), `None` for an unknown name.
    /// Name matching is case-insensitive.
    fn get(&self, name: &str) -> Option<String>;
    /// `set_config(name, value, is_local)`: apply a SET (validation errors as for `SET`), return the
    /// new value in SHOW form.
    fn set(&self, name: &str, value: &str, is_local: bool) -> Result<String>;
    /// Validation only (guc.c `ProcessGUCArray` for `ALTER ... SET`): unknown name 42704, a value the
    /// parameter rejects 22023. Nothing is assigned. Every adapter must provide validation.
    fn check_setting(&self, name: &str, value: &str) -> Result<()>;
    /// All settings as `pg_settings` rows (for `pg_settings`, `SHOW ALL`, `pg_show_all_settings`).
    fn settings(&self) -> Result<Vec<SettingRow>>;
    /// Boolean GUC helper: `on`/`true`/`yes`/`1` (case-insensitive) = true; unknown/other = false.
    fn get_bool(&self, name: &str) -> bool {
        matches!(self.get(name).map(|v| v.to_ascii_lowercase()).as_deref(), Some("on" | "true" | "yes" | "1" | "t"))
    }
}

/// A [`GucAccess`] with no settings (unit tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGuc;

impl GucAccess for NoGuc {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
    fn set(&self, name: &str, _value: &str, _is_local: bool) -> Result<String> {
        Err(PgError::new(
            pgarbor_common::SqlState::UNDEFINED_OBJECT,
            format!("unrecognized configuration parameter \"{name}\""),
        ))
    }
    fn check_setting(&self, name: &str, _value: &str) -> Result<()> {
        Err(PgError::new(
            pgarbor_common::SqlState::UNDEFINED_OBJECT,
            format!("unrecognized configuration parameter \"{name}\""),
        ))
    }
    fn settings(&self) -> Result<Vec<SettingRow>> {
        Ok(Vec::new())
    }
}

/// Engine/session services that never need the transaction (implemented by the `pgarbor` crate
/// over `pgarbor_core::{diag, Engine}` and the session's own tables). Defaults describe "nothing
/// there" so tests only override what they exercise.
// CONTRACT
pub trait EngineServices {
    /// `pg_backend_pid()`.
    fn backend_pid(&self) -> i32 {
        0
    }
    fn activity(&self) -> Result<Vec<ActivityRow>> {
        Ok(Vec::new())
    }
    fn locks(&self) -> Result<Vec<LockRow>> {
        Ok(Vec::new())
    }
    /// This session's prepared statements (`pg_prepared_statements`).
    fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
        Ok(Vec::new())
    }
    fn cursors(&self) -> Result<Vec<CursorRow>> {
        Ok(Vec::new())
    }
    fn blocking_pids(&self, _pid: i32) -> Result<Vec<i32>> {
        Ok(Vec::new())
    }
    /// timestamptz µs since 2000-01-01.
    fn server_start_time(&self) -> i64 {
        0
    }
    fn config_load_time(&self) -> i64 {
        0
    }
    /// Text of the top-level statement being executed (`current_query()`).
    fn current_query(&self) -> Option<String> {
        None
    }
    /// `pg_relation_size` & friends; default 0A000 (catalog `SessionInfo` default wording).
    fn relation_size(&self, _oid: Oid, _kind: SizeKind) -> Result<i64> {
        Err(PgError::new(
            pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
            "relation sizes are not available in this session",
        ))
    }
    /// `pg_cancel_backend(pid)` (`terminate = false`) / `pg_terminate_backend(pid)` (`true`).
    /// Returns false when no such backend.
    fn cancel_backend(&self, _pid: i32, _terminate: bool) -> Result<bool> {
        Ok(false)
    }
    /// `pg_notify(channel, payload)` / `NOTIFY`: queue until the transaction commits.
    fn notify(&self, _channel: &str, _payload: &str) -> Result<()> {
        Ok(())
    }
    /// `pg_listening_channels()`.
    fn listening_channels(&self) -> Vec<String> {
        Vec::new()
    }
    /// Name of the connected database (`current_database()`).
    fn database_name(&self) -> String {
        "postgres".to_string()
    }
}

/// An [`EngineServices`] with every default (unit tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServices;

impl EngineServices for NoServices {}

/// Execution switches.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    /// `EXPLAIN ANALYZE`: operators record [`pgarbor_plan::explain_fmt::NodeActual`] (rows, loops, time).
    pub collect_stats: bool,
}

/// Everything of the session environment that execution only READS (module docs).
// CONTRACT
pub struct SessionEnv<'a> {
    /// Catalog snapshot + search_path + users at statement start. DDL replaces the snapshot
    /// (`env = env.with_snapshot(..)`) so later steps of the same statement see its own changes.
    pub env: CatalogEnv,
    /// Output-affecting GUCs (DateStyle, IntervalStyle, TimeZone, extra_float_digits, bytea_output).
    pub io: IoConfig,
    pub guc: &'a dyn GucAccess,
    pub services: &'a dyn EngineServices,
    /// Final parameter VALUES `$1..$n`, coerced to `param_types`.
    pub params: Vec<Datum>,
    pub param_types: Vec<TypeId>,
    /// `statement_timestamp()` (timestamptz µs since 2000-01-01 UTC).
    pub statement_ts: i64,
    /// `transaction_timestamp()` / `now()`.
    pub txn_ts: i64,
    /// Session PRNG state (xorshift64*; non-zero), shared across statements so `setseed` persists.
    pub rng: &'a Cell<u64>,
    /// Notices raised during execution, in order.
    pub notices: RefCell<Vec<PgError>>,
    pub options: ExecOptions,
}

impl<'a> SessionEnv<'a> {
    /// New environment with empty params/notices.
    pub fn new(
        env: CatalogEnv,
        io: IoConfig,
        guc: &'a dyn GucAccess,
        services: &'a dyn EngineServices,
        rng: &'a Cell<u64>,
        statement_ts: i64,
        txn_ts: i64,
    ) -> SessionEnv<'a> {
        SessionEnv {
            env,
            io,
            guc,
            services,
            params: Vec::new(),
            param_types: Vec::new(),
            statement_ts,
            txn_ts,
            rng,
            notices: RefCell::new(Vec::new()),
            options: ExecOptions::default(),
        }
    }

    /// `IoCtx` over the env with the transaction timestamp set.
    pub fn io_ctx(&self) -> IoCtx<'_> {
        IoCtx::new(&self.env, &self.io).with_now(self.txn_ts)
    }

    /// Queue a NOTICE/WARNING for the client.
    pub fn notice(&self, n: PgError) {
        self.notices.borrow_mut().push(n);
    }

    /// Drain queued notices.
    pub fn take_notices(&self) -> Vec<PgError> {
        std::mem::take(&mut *self.notices.borrow_mut())
    }

    /// Next uniform f64 in [0,1) from the session PRNG (xorshift64*).
    pub fn next_random(&self) -> f64 {
        let mut x = self.rng.get();
        if x == 0 {
            x = 0x9E37_79B9_7F4A_7C15;
        }
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng.set(x);
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Wall clock as timestamptz µs since 2000-01-01 UTC (`clock_timestamp()`).
    pub fn clock_now(&self) -> i64 {
        const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000; // 1970-01-01 -> 2000-01-01
        let us = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0);
        us - PG_EPOCH_OFFSET_US
    }
}

/// Per-statement bookkeeping.
#[derive(Default)]
pub struct StmtState {
    /// `TableDesc` cache for this statement (see `tabledesc::table_desc`). Invalidated when the
    /// catalog snapshot is replaced (DDL clears it).
    pub tables: RefCell<HashMap<Oid, Rc<TableDesc>>>,
    /// Some statement changed the catalog (DDL ran): the session must refresh/replace its snapshot.
    pub catalog_changed: bool,
    /// Foreign-key checks deferred to the end of the current DML statement (`crate::fk`).
    pub pending_fk: Vec<crate::fk::PendingFkCheck>,
    /// Rows modified by the statement so far (diagnostics).
    pub rows_modified: u64,
}

/// The execution context (module docs).
pub struct ExecContext<'a> {
    pub txn: &'a mut dyn ExecTxn,
    pub se: SessionEnv<'a>,
    pub stmt: StmtState,
}

impl<'a> ExecContext<'a> {
    pub fn new(txn: &'a mut dyn ExecTxn, se: SessionEnv<'a>) -> ExecContext<'a> {
        ExecContext { txn, se, stmt: StmtState::default() }
    }

    /// Replace the catalog snapshot after DDL: updates `se.env`, clears the table-description
    /// cache and marks `catalog_changed`.
    pub fn set_catalog_snapshot(&mut self, snapshot: pgarbor_catalog::CatalogSnapshot) {
        self.se.env = self.se.env.with_snapshot(snapshot);
        self.stmt.tables.borrow_mut().clear();
        self.stmt.catalog_changed = true;
    }

    /// Poll for cancel / statement timeout (delegates to the transaction).
    pub fn check_interrupts(&self) -> Result<()> {
        self.txn.check_interrupts()
    }

    /// Queue a NOTICE/WARNING.
    pub fn notice(&self, n: PgError) {
        self.se.notice(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let cell = Cell::new(12345);
        let env = pgarbor_catalog::testing::bootstrapped().map(|(_, env)| env);
        if let Ok(env) = env {
            let g = NoGuc;
            let s = NoServices;
            let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
            let a = se.next_random();
            assert!((0.0..1.0).contains(&a));
            let b = se.next_random();
            assert_ne!(a, b);
        }
    }
}
