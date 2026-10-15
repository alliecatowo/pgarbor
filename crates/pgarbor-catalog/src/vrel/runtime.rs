//! Session/engine-backed relations: `pg_settings`, `pg_stat_activity`, `pg_locks`,
//! `pg_prepared_statements`, `pg_cursors`, `pg_stat_ssl`, `pg_stat_gssapi`. Lease CAT-38. All data
//! comes from `ctx.session` (`SessionInfo`); the catalog adds only role/database name resolution.
//! PG sources: `utils/misc/guc_funcs.c` (`show_all_settings`), `utils/adt/pgstatfuncs.c`
//! (`pg_stat_get_activity`), `storage/lmgr/lockfuncs.c` (`pg_lock_status`), `commands/prepare.c`
//! (`pg_prepared_statement`), `utils/mmgr/portalmem.c` (`pg_cursor`), system_views.sql.
//!
//! - **pg_settings**: one row per `SettingRow` of `session.settings()?` in that order (names
//!   sorted): `name, setting, unit, category, short_desc, extra_desc, context, vartype, source,
//!   min_val, max_val, enumvals` (`text[]` or NULL), `boot_val, reset_val, sourcefile, sourceline`
//!   (`Int4`), `pending_restart`. All `text`. Fast path: eq on `name` (col 0).
//! - **pg_stat_activity**: one row per `ActivityRow` of `session.activity()?`: `datid, datname, pid,
//!   leader_pid, usesysid, usename, application_name, client_addr` (always NULL: `inet` has no Datum,
//!   amendment pgarbor-types-03), `client_hostname, client_port, backend_start, xact_start,
//!   query_start, state_change` (`TimestampTz`), `wait_event_type, wait_event, state, backend_xid`
//!   (`Oid`, i.e. xid; the u64 is truncated to u32 like PG's 32-bit xid view), `backend_xmin,
//!   query_id` (`Int8`), `query, backend_type`. PG also lists background workers
//!   (`walwriter`, ...); PGArbor has none: client sessions only.
//! - **pg_locks**: one row per `LockRow` of `session.locks()?`: `locktype, database, relation, page,
//!   tuple, virtualxid, transactionid` (xid -> `Oid`), `classid, objid, objsubid, virtualtransaction,
//!   pid, mode, granted, fastpath, waitstart`.
//! - **pg_prepared_statements**: `session.prepared_statements()?`: `name, statement, prepare_time,
//!   parameter_types` (`regtype[]` = `oid[]` Array with elem 2206 `regtype`... use elem OID of
//!   `regtype`, 2206), `result_types` (same, NULL when `None`), `from_sql, generic_plans,
//!   custom_plans`.
//! - **pg_cursors**: `session.cursors()?`: `name, statement, is_holdable, is_binary, is_scrollable,
//!   creation_time`.
//! - **pg_stat_ssl** / **pg_stat_gssapi**: one row per activity row (`pid`, `ssl`/`gss_authenticated`
//!   false, all other columns NULL; `gss_authenticated` false, `encrypted` false): PGArbor has no TLS or
//!   GSS in v1.
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_settings`.
#[derive(Clone, Copy, Debug)]
pub struct PgSettings;

impl VirtualRelation for PgSettings {
    fn oid(&self) -> Oid {
        relid::PG_SETTINGS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_settings rows"))
    }
}

/// Producer for `pg_stat_activity`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatActivity;

impl VirtualRelation for PgStatActivity {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ACTIVITY
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_stat_activity rows"))
    }
}

/// Producer for `pg_locks`.
#[derive(Clone, Copy, Debug)]
pub struct PgLocks;

impl VirtualRelation for PgLocks {
    fn oid(&self) -> Oid {
        relid::PG_LOCKS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_locks rows"))
    }
}

/// Producer for `pg_prepared_statements`.
#[derive(Clone, Copy, Debug)]
pub struct PgPreparedStatements;

impl VirtualRelation for PgPreparedStatements {
    fn oid(&self) -> Oid {
        relid::PG_PREPARED_STATEMENTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_prepared_statements rows"))
    }
}

/// Producer for `pg_cursors`.
#[derive(Clone, Copy, Debug)]
pub struct PgCursors;

impl VirtualRelation for PgCursors {
    fn oid(&self) -> Oid {
        relid::PG_CURSORS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_cursors rows"))
    }
}

/// Producer for `pg_stat_ssl`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSsl;

impl VirtualRelation for PgStatSsl {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SSL
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_stat_ssl rows"))
    }
}

/// Producer for `pg_stat_gssapi`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatGssapi;

impl VirtualRelation for PgStatGssapi {
    fn oid(&self) -> Oid {
        relid::PG_STAT_GSSAPI
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-38", "pg_stat_gssapi rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] =
    &[&PgSettings, &PgStatActivity, &PgLocks, &PgPreparedStatements, &PgCursors, &PgStatSsl, &PgStatGssapi];
