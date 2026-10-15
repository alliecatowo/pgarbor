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
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, Result};
use pgarbor_types::array::Array;
use pgarbor_types::{oids, Datum};

/// `regtype[]` (element OID 2206). Empty input gives the empty array, not NULL.
fn regtype_array(v: &[Oid]) -> Datum {
    Datum::Array(Box::new(Array::from_vec(oids::REGTYPE, v.iter().map(|o| Datum::Oid(*o)).collect())))
}

/// An OID column where 0 means "not set" (PG prints NULL for InvalidOid in these columns).
fn nonzero_oid(v: Oid) -> Datum {
    if v == 0 {
        Datum::Null
    } else {
        Datum::Oid(v)
    }
}

/// `smallint` nullable.
fn opt_int2(v: Option<i16>) -> Datum {
    v.map(d::int2).unwrap_or(Datum::Null)
}

/// xid as the 32-bit `Oid` column (truncating like PG's `TransactionIdGetDatum` view).
fn opt_xid(v: Option<u64>) -> Datum {
    d::opt_oid(v.map(|x| x as Oid))
}

/// Producer for `pg_settings`.
#[derive(Clone, Copy, Debug)]
pub struct PgSettings;

impl VirtualRelation for PgSettings {
    fn oid(&self) -> Oid {
        relid::PG_SETTINGS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        // Fast path: `name = 'x'` (column 0). The executor re-applies the WHERE clause.
        let only_name: Option<&str> = match req.eq_on(0) {
            Some(Datum::Text(n)) => Some(n.as_str()),
            _ => None,
        };
        let settings = ctx.session.settings()?;
        let mut rows = Vec::with_capacity(settings.len());
        for s in settings.iter().filter(|s| only_name.map_or(true, |n| s.name == n)) {
            let mut b = RowBuilder::new(schema);
            b.set("name", d::text(&s.name))
                .set("setting", d::opt_text(s.setting.as_deref()))
                .set("unit", d::opt_text(s.unit.as_deref()))
                .set("category", d::text(&s.category))
                .set("short_desc", d::text(&s.short_desc))
                .set("extra_desc", d::opt_text(s.extra_desc.as_deref()))
                .set("context", d::text(&s.context))
                .set("vartype", d::text(&s.vartype))
                .set("source", d::text(&s.source))
                .set("min_val", d::opt_text(s.min_val.as_deref()))
                .set("max_val", d::opt_text(s.max_val.as_deref()))
                .set(
                    "enumvals",
                    match &s.enumvals {
                        Some(v) => d::text_array(v),
                        None => Datum::Null,
                    },
                )
                .set("boot_val", d::opt_text(s.boot_val.as_deref()))
                .set("reset_val", d::opt_text(s.reset_val.as_deref()))
                .set("sourcefile", d::opt_text(s.sourcefile.as_deref()))
                .set("sourceline", d::opt_int4(s.sourceline))
                .set("pending_restart", d::bool_(s.pending_restart));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_stat_activity`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatActivity;

impl VirtualRelation for PgStatActivity {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ACTIVITY
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        let activity = ctx.session.activity()?;
        let mut rows = Vec::with_capacity(activity.len());
        for a in &activity {
            // Names are resolved from the catalog when the object still exists; the session's
            // copy is the fallback (e.g. a role dropped while the session was connected).
            let datname: Option<&str> = if a.datid == 0 {
                None
            } else {
                Some(snap.database(a.datid).map(|db| db.name.as_str()).unwrap_or(a.datname.as_str()))
            };
            let usename: Option<&str> = if a.usesysid == 0 {
                None
            } else {
                Some(snap.role_name(a.usesysid).unwrap_or(a.usename.as_str()))
            };
            let mut b = RowBuilder::new(schema);
            b.set("datid", nonzero_oid(a.datid))
                .set("datname", d::opt_text(datname))
                .set("pid", d::int4(a.pid))
                .set("leader_pid", d::opt_int4(a.leader_pid))
                .set("usesysid", nonzero_oid(a.usesysid))
                .set("usename", d::opt_text(usename))
                .set("application_name", d::text(&a.application_name))
                // `inet` has no Datum yet (amendment pgarbor-types-03).
                .set("client_addr", Datum::Null)
                .set("client_hostname", d::opt_text(a.client_hostname.as_deref()))
                .set("client_port", d::opt_int4(a.client_port))
                .set("backend_start", d::opt_tstz(a.backend_start))
                .set("xact_start", d::opt_tstz(a.xact_start))
                .set("query_start", d::opt_tstz(a.query_start))
                .set("state_change", d::opt_tstz(a.state_change))
                .set("wait_event_type", d::opt_text(a.wait_event_type.as_deref()))
                .set("wait_event", d::opt_text(a.wait_event.as_deref()))
                .set("state", d::opt_text(a.state.as_deref()))
                .set("backend_xid", opt_xid(a.backend_xid))
                .set("backend_xmin", opt_xid(a.backend_xmin))
                .set("query_id", d::opt_int8(a.query_id))
                .set("query", d::text(&a.query))
                .set("backend_type", d::text(&a.backend_type));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_locks`.
#[derive(Clone, Copy, Debug)]
pub struct PgLocks;

impl VirtualRelation for PgLocks {
    fn oid(&self) -> Oid {
        relid::PG_LOCKS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let locks = ctx.session.locks()?;
        let mut rows = Vec::with_capacity(locks.len());
        for l in &locks {
            let mut b = RowBuilder::new(schema);
            b.set("locktype", d::text(&l.locktype))
                .set("database", d::opt_oid(l.database))
                .set("relation", d::opt_oid(l.relation))
                .set("page", d::opt_int4(l.page))
                .set("tuple", opt_int2(l.tuple))
                .set("virtualxid", d::opt_text(l.virtualxid.as_deref()))
                .set("transactionid", opt_xid(l.transactionid))
                .set("classid", d::opt_oid(l.classid))
                .set("objid", d::opt_oid(l.objid))
                .set("objsubid", opt_int2(l.objsubid))
                .set("virtualtransaction", d::opt_text(l.virtualtransaction.as_deref()))
                .set("pid", d::opt_int4(l.pid))
                .set("mode", d::text(&l.mode))
                .set("granted", d::bool_(l.granted))
                .set("fastpath", d::bool_(l.fastpath))
                .set("waitstart", d::opt_tstz(l.waitstart));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_prepared_statements`.
#[derive(Clone, Copy, Debug)]
pub struct PgPreparedStatements;

impl VirtualRelation for PgPreparedStatements {
    fn oid(&self) -> Oid {
        relid::PG_PREPARED_STATEMENTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let prepared = ctx.session.prepared_statements()?;
        let mut rows = Vec::with_capacity(prepared.len());
        for p in &prepared {
            let mut b = RowBuilder::new(schema);
            b.set("name", d::text(&p.name))
                .set("statement", d::text(&p.statement))
                .set("prepare_time", d::opt_tstz(Some(p.prepare_time)))
                .set("parameter_types", regtype_array(&p.parameter_types))
                .set(
                    "result_types",
                    match &p.result_types {
                        Some(v) => regtype_array(v),
                        None => Datum::Null,
                    },
                )
                .set("from_sql", d::bool_(p.from_sql))
                .set("generic_plans", d::int8(p.generic_plans))
                .set("custom_plans", d::int8(p.custom_plans));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_cursors`.
#[derive(Clone, Copy, Debug)]
pub struct PgCursors;

impl VirtualRelation for PgCursors {
    fn oid(&self) -> Oid {
        relid::PG_CURSORS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let cursors = ctx.session.cursors()?;
        let mut rows = Vec::with_capacity(cursors.len());
        for c in &cursors {
            let mut b = RowBuilder::new(schema);
            b.set("name", d::text(&c.name))
                .set("statement", d::text(&c.statement))
                .set("is_holdable", d::bool_(c.is_holdable))
                .set("is_binary", d::bool_(c.is_binary))
                .set("is_scrollable", d::bool_(c.is_scrollable))
                .set("creation_time", d::opt_tstz(Some(c.creation_time)));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_stat_ssl`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSsl;

impl VirtualRelation for PgStatSsl {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SSL
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let activity = ctx.session.activity()?;
        let mut rows = Vec::with_capacity(activity.len());
        for a in &activity {
            // No TLS in v1: `ssl` is false and every other column stays NULL.
            let mut b = RowBuilder::new(schema);
            b.set("pid", d::int4(a.pid)).set("ssl", d::bool_(false));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_stat_gssapi`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatGssapi;

impl VirtualRelation for PgStatGssapi {
    fn oid(&self) -> Oid {
        relid::PG_STAT_GSSAPI
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let schema = self.schema()?;
        let activity = ctx.session.activity()?;
        let mut rows = Vec::with_capacity(activity.len());
        for a in &activity {
            // No GSSAPI in v1: not authenticated, not encrypted, no principal, no delegation.
            let mut b = RowBuilder::new(schema);
            b.set("pid", d::int4(a.pid))
                .set("gss_authenticated", d::bool_(false))
                .set("encrypted", d::bool_(false))
                .set("credentials_delegated", d::bool_(false));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] =
    &[&PgSettings, &PgStatActivity, &PgLocks, &PgPreparedStatements, &PgCursors, &PgStatSsl, &PgStatGssapi];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::session::{
        ActivityRow, CursorRow, LockRow, NullSessionInfo, PreparedStatementRow, SessionInfo, SettingRow,
    };
    use crate::testing;

    /// A session whose answers are fixed by the test.
    #[derive(Default)]
    struct FixedSession {
        settings: Vec<SettingRow>,
        activity: Vec<ActivityRow>,
        locks: Vec<LockRow>,
        prepared: Vec<PreparedStatementRow>,
        cursors: Vec<CursorRow>,
    }

    impl SessionInfo for FixedSession {
        fn settings(&self) -> Result<Vec<SettingRow>> {
            Ok(self.settings.clone())
        }
        fn sequence_state(&self, _oid: Oid) -> Result<Option<crate::session::SequenceStateRow>> {
            Ok(None)
        }
        fn activity(&self) -> Result<Vec<ActivityRow>> {
            Ok(self.activity.clone())
        }
        fn locks(&self) -> Result<Vec<LockRow>> {
            Ok(self.locks.clone())
        }
        fn prepared_statements(&self) -> Result<Vec<PreparedStatementRow>> {
            Ok(self.prepared.clone())
        }
        fn cursors(&self) -> Result<Vec<CursorRow>> {
            Ok(self.cursors.clone())
        }
    }

    fn setting(name: &str, enumvals: Option<Vec<String>>) -> SettingRow {
        SettingRow {
            name: name.to_string(),
            setting: Some("1".to_string()),
            category: "Test".to_string(),
            short_desc: "short".to_string(),
            context: "user".to_string(),
            vartype: "integer".to_string(),
            source: "default".to_string(),
            enumvals,
            boot_val: Some("1".to_string()),
            reset_val: Some("1".to_string()),
            ..SettingRow::default()
        }
    }

    fn ctx<'a>(env: &'a CatalogEnv, session: &'a dyn SessionInfo, io: &'a pgarbor_types::IoConfig) -> ScanContext<'a> {
        ScanContext { env, session, io_config: io }
    }

    #[test]
    fn settings_rows_follow_session_order_and_enumvals_shape() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = FixedSession {
            settings: vec![
                setting("log_level", Some(vec!["debug".to_string(), "info".to_string()])),
                setting("work_mem", None),
            ],
            ..FixedSession::default()
        };
        let rows = PgSettings.scan(&ctx(&env, &session, &io), &ScanRequest::all())?;
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0][0], Datum::Text(n) if n == "log_level"));
        assert!(matches!(&rows[1][0], Datum::Text(n) if n == "work_mem"));
        // enumvals (column 11): text[] with two items, or NULL.
        assert!(matches!(&rows[0][11], Datum::Array(a) if a.items.len() == 2));
        assert!(matches!(&rows[1][11], Datum::Null));
        // sourceline (column 15) is NULL when unset.
        assert!(matches!(&rows[0][15], Datum::Null));
        Ok(())
    }

    #[test]
    fn settings_name_filter_keeps_only_matching_row() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = FixedSession {
            settings: vec![setting("a_one", None), setting("b_two", None)],
            ..FixedSession::default()
        };
        let req = ScanRequest { eq: vec![(0, Datum::Text("b_two".to_string()))] };
        let rows = PgSettings.scan(&ctx(&env, &session, &io), &req)?;
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0][0], Datum::Text(n) if n == "b_two"));
        Ok(())
    }

    #[test]
    fn activity_client_addr_null_and_xid_truncated() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = FixedSession {
            activity: vec![ActivityRow {
                pid: 42,
                application_name: "app".to_string(),
                client_addr: Some("127.0.0.1".to_string()),
                backend_xid: Some((1u64 << 32) | 5),
                backend_xmin: None,
                query: "select 1".to_string(),
                backend_type: "client backend".to_string(),
                ..ActivityRow::default()
            }],
            ..FixedSession::default()
        };
        let rows = PgStatActivity.scan(&ctx(&env, &session, &io), &ScanRequest::all())?;
        assert_eq!(rows.len(), 1);
        // client_addr (column 7) is always NULL.
        assert!(matches!(&rows[0][7], Datum::Null));
        // backend_xid (column 17) is the low 32 bits.
        assert!(matches!(&rows[0][17], Datum::Oid(5)));
        // backend_xmin (column 18) is NULL when invalid.
        assert!(matches!(&rows[0][18], Datum::Null));
        // datid 0 gives NULL datid and datname.
        assert!(matches!(&rows[0][0], Datum::Null));
        assert!(matches!(&rows[0][1], Datum::Null));
        Ok(())
    }

    #[test]
    fn empty_session_gives_no_lock_prepared_or_cursor_rows() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = NullSessionInfo;
        let c = ctx(&env, &session, &io);
        assert!(PgLocks.scan(&c, &ScanRequest::all())?.is_empty());
        assert!(PgPreparedStatements.scan(&c, &ScanRequest::all())?.is_empty());
        assert!(PgCursors.scan(&c, &ScanRequest::all())?.is_empty());
        assert!(PgStatActivity.scan(&c, &ScanRequest::all())?.is_empty());
        Ok(())
    }

    #[test]
    fn prepared_parameter_types_is_regtype_array() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = FixedSession {
            prepared: vec![PreparedStatementRow {
                name: "s1".to_string(),
                statement: "select $1".to_string(),
                parameter_types: vec![23, 25],
                result_types: None,
                from_sql: true,
                ..PreparedStatementRow::default()
            }],
            ..FixedSession::default()
        };
        let rows = PgPreparedStatements.scan(&ctx(&env, &session, &io), &ScanRequest::all())?;
        assert_eq!(rows.len(), 1);
        // parameter_types (column 3): regtype[] with element OID 2206.
        assert!(matches!(&rows[0][3], Datum::Array(a) if a.elem == 2206 && a.items.len() == 2));
        // result_types (column 4) is NULL when the statement has no result set.
        assert!(matches!(&rows[0][4], Datum::Null));
        Ok(())
    }

    #[test]
    fn ssl_and_gssapi_rows_one_per_activity_row() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = pgarbor_types::IoConfig::default();
        let session = FixedSession {
            activity: vec![ActivityRow { pid: 7, ..ActivityRow::default() }],
            ..FixedSession::default()
        };
        let c = ctx(&env, &session, &io);
        let ssl = PgStatSsl.scan(&c, &ScanRequest::all())?;
        assert_eq!(ssl.len(), 1);
        assert!(matches!(&ssl[0][0], Datum::Int4(7)));
        assert!(matches!(&ssl[0][1], Datum::Bool(false)));
        assert!(matches!(&ssl[0][2], Datum::Null));
        let gss = PgStatGssapi.scan(&c, &ScanRequest::all())?;
        assert_eq!(gss.len(), 1);
        assert!(matches!(&gss[0][1], Datum::Bool(false)));
        assert!(matches!(&gss[0][2], Datum::Null));
        assert!(matches!(&gss[0][3], Datum::Bool(false)));
        Ok(())
    }
}
