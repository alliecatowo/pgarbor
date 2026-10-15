//! Session-identity and engine-state functions served from `SessionInfo` and the naming context.
//! Lease CAT-48. PG sources: `utils/adt/misc.c`, `utils/adt/name.c` (`current_user`, `session_user`),
//! `utils/misc/guc_funcs.c`, `utils/adt/pgstatfuncs.c`, `storage/lmgr/lockfuncs.c`,
//! `commands/prepare.c`, `utils/mmgr/portalmem.c`, `commands/extension.c`.
//!
//! | proc | behavior |
//! |---|---|
//! | `current_user()`, `getpgusername()`, `user` | `ctx.env.current_user()` -> role name as `name` (`Datum::Text`) |
//! | `session_user()` | `ctx.env.session_user()` name |
//! | `system_user()` | NULL (no authentication method name: PG prints `scram-sha-256:alice` for authenticated sessions; v1 returns NULL) |
//! | `current_database()` | `snapshot.database(env.database()).name` |
//! | `current_query()` | `session.current_query()` or NULL |
//! | `pg_backend_pid()` | `session.backend_pid()` (`Int4`) |
//! | `pg_blocking_pids(int4)` | `int4[]` from `session.blocking_pids(pid)` |
//! | `pg_is_in_recovery()` | false |
//! | `pg_jit_available()` | false |
//! | `pg_postmaster_start_time()`, `pg_conf_load_time()` | `TimestampTz` from the session |
//! | `pg_show_all_settings()` (SRF) | one record per `SettingRow` (17 columns of `pg_settings`) |
//! | `pg_stat_get_activity(int4)` (SRF) | `ActivityRow` records (22 columns as `pg_stat_activity` raw function: `datid, pid, usesysid, application_name, state, query, wait_event_type, wait_event, xact_start, query_start, backend_start, state_change, client_addr (NULL), client_hostname, client_port, backend_xid, backend_xmin, backend_type, ssl, sslversion, sslcipher, sslbits, ssl_client_dn, ...` - follow `pgarbor_types::catalog_data::PROCS` `all_arg_types`/`arg_names` of the proc); argument NULL = all backends, else that pid |
//! | `pg_lock_status()` (SRF) | `LockRow` records |
//! | `pg_cursor()` (SRF), `pg_prepared_statement()` (SRF) | `CursorRow` / `PreparedStatementRow` records |
//! | `pg_available_extensions()`, `pg_available_extension_versions()` (SRF) | the `plpgsql` row(s) as in `vrel::pg_builtin` |
//! | `pg_get_wal_replay_pause_state()`, `pg_get_loaded_modules()`, `pg_get_aios()`, `pg_get_backend_memory_contexts()`, `pg_get_shmem_allocations*()`, `pg_get_wait_events()`, `pg_timezone_abbrevs_*`, `pg_timezone_names()`, `pg_get_replication_slots()`, `pg_get_publication_tables()`, `pg_get_multixact_members()`, `pg_get_wal_resource_managers()`, `pg_get_wal_summarizer_state()`, `pg_prepared_xact`-family | `Err(PgError::unimplemented("CAT-48", "<function name>"))` for functions backing UNSUPPORTED relations; functions whose relations are correctly empty return an empty set |
//! Records are `Datum::Record(Vec<Datum>)` in the proc's OUT column order (`BuiltinProc::out_columns`).
use super::CatalogCallCtx;
use crate::session::{ActivityRow, CursorRow, LockRow, PreparedStatementRow, SettingRow};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::{Array, Datum};

/// Scalar procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "current_user",
    "getpgusername",
    "session_user",
    "system_user",
    "current_database",
    "current_query",
    "pg_backend_pid",
    "pg_blocking_pids",
    "pg_is_in_recovery",
    "pg_jit_available",
    "pg_postmaster_start_time",
    "pg_conf_load_time",
    "pg_show_all_settings",
    "pg_stat_get_activity",
    "pg_lock_status",
    "pg_cursor",
    "pg_prepared_statement",
    "pg_available_extensions",
    "pg_available_extension_versions",
    "pg_get_wal_replay_pause_state",
    "pg_get_loaded_modules",
    "pg_get_aios",
    "pg_get_backend_memory_contexts",
    "pg_get_shmem_allocations",
    "pg_get_shmem_allocations_numa",
    "pg_get_wait_events",
    "pg_timezone_abbrevs_abbrevs",
    "pg_timezone_abbrevs_zone",
    "pg_timezone_names",
    "pg_get_replication_slots",
    "pg_get_publication_tables",
    "pg_get_multixact_members",
    "pg_get_wal_resource_managers",
    "pg_get_wal_summarizer_state",
];

/// Set-returning procs among [`NAMES`].
// CONTRACT
pub const SET_NAMES: &[&str] = &[
    "pg_show_all_settings",
    "pg_stat_get_activity",
    "pg_lock_status",
    "pg_cursor",
    "pg_prepared_statement",
    "pg_available_extensions",
    "pg_available_extension_versions",
    "pg_get_loaded_modules",
    "pg_get_aios",
    "pg_get_backend_memory_contexts",
    "pg_get_shmem_allocations",
    "pg_get_shmem_allocations_numa",
    "pg_get_wait_events",
    "pg_timezone_abbrevs_abbrevs",
    "pg_timezone_abbrevs_zone",
    "pg_timezone_names",
    "pg_get_replication_slots",
    "pg_get_publication_tables",
    "pg_get_multixact_members",
    "pg_get_wal_resource_managers",
];

/// Scalar dispatch on `proc.name`.
// CONTRACT
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    match proc.name {
        "current_user" | "getpgusername" => role_name(ctx, ctx.env.current_user()),
        "session_user" => role_name(ctx, ctx.env.session_user()),
        // No authentication-method name is tracked (PG prints `scram-sha-256:alice`): NULL in v1.
        "system_user" => Ok(Datum::Null),
        "current_database" => {
            let oid = ctx.env.database();
            match ctx.env.snapshot().database(oid) {
                Some(db) => Ok(Datum::Text(db.name.clone())),
                None => Err(PgError::internal(format!("database with OID {oid} does not exist"))),
            }
        }
        "current_query" => Ok(opt_text(ctx.session.current_query())),
        "pg_backend_pid" => Ok(Datum::Int4(ctx.session.backend_pid())),
        "pg_blocking_pids" => match arg_int4(args, 0)? {
            None => Ok(Datum::Null),
            Some(pid) => Ok(int4_array(ctx.session.blocking_pids(pid)?)),
        },
        "pg_is_in_recovery" | "pg_jit_available" => Ok(Datum::Bool(false)),
        "pg_postmaster_start_time" => Ok(Datum::TimestampTz(ctx.session.server_start_time())),
        "pg_conf_load_time" => Ok(Datum::TimestampTz(ctx.session.config_load_time())),
        "pg_get_wal_replay_pause_state" | "pg_get_wal_summarizer_state" => {
            Err(PgError::unimplemented("CAT-48", proc.name))
        }
        other => Err(PgError::internal(format!("function {other} is not a scalar session function"))),
    }
}

/// Set-returning dispatch.
// CONTRACT
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    match proc.name {
        "pg_show_all_settings" => Ok(ctx.session.settings()?.into_iter().map(setting_record).collect()),
        "pg_stat_get_activity" => activity_records(ctx, proc, arg_int4(args, 0)?),
        "pg_lock_status" => Ok(ctx.session.locks()?.iter().map(lock_record).collect()),
        "pg_cursor" => Ok(ctx.session.cursors()?.into_iter().map(cursor_record).collect()),
        "pg_prepared_statement" => Ok(ctx.session.prepared_statements()?.into_iter().map(prepared_record).collect()),
        "pg_available_extensions" => extension_records(ctx, false),
        "pg_available_extension_versions" => extension_records(ctx, true),
        "pg_get_loaded_modules"
        | "pg_get_aios"
        | "pg_get_backend_memory_contexts"
        | "pg_get_shmem_allocations"
        | "pg_get_shmem_allocations_numa"
        | "pg_get_wait_events"
        | "pg_timezone_abbrevs_abbrevs"
        | "pg_timezone_abbrevs_zone"
        | "pg_timezone_names"
        | "pg_get_replication_slots"
        | "pg_get_publication_tables"
        | "pg_get_multixact_members"
        | "pg_get_wal_resource_managers" => Err(PgError::unimplemented("CAT-48", proc.name)),
        other => Err(PgError::internal(format!("function {other} is not a set-returning session function"))),
    }
}

// ---------------------------------------------------------------- helpers

const TEXT_OID: Oid = 25;
const INT4_OID: Oid = 23;
const REGTYPE_OID: Oid = 2206;

/// `name` value of a role OID (PG `current_user` / `session_user`).
fn role_name(ctx: &CatalogCallCtx, oid: Oid) -> Result<Datum> {
    match ctx.env.snapshot().role_name(oid) {
        Some(name) => Ok(Datum::Text(name.to_string())),
        None => Err(PgError::internal(format!("role with OID {oid} does not exist"))),
    }
}

/// Integer argument `index`: `None` for an absent or SQL NULL argument.
fn arg_int4(args: &[Datum], index: usize) -> Result<Option<i32>> {
    match args.get(index) {
        None | Some(Datum::Null) => Ok(None),
        Some(Datum::Int4(v)) => Ok(Some(*v)),
        Some(Datum::Int2(v)) => Ok(Some(i32::from(*v))),
        Some(_) => Err(PgError::internal("session function expected an integer argument")),
    }
}

fn opt_text(v: Option<String>) -> Datum {
    match v {
        Some(s) => Datum::Text(s),
        None => Datum::Null,
    }
}

fn opt_i16(v: Option<i16>) -> Datum {
    v.map_or(Datum::Null, Datum::Int2)
}

fn opt_i32(v: Option<i32>) -> Datum {
    v.map_or(Datum::Null, Datum::Int4)
}

fn opt_i64(v: Option<i64>) -> Datum {
    v.map_or(Datum::Null, Datum::Int8)
}

fn opt_ts(v: Option<i64>) -> Datum {
    v.map_or(Datum::Null, Datum::TimestampTz)
}

fn opt_oid(v: Option<Oid>) -> Datum {
    v.map_or(Datum::Null, Datum::Oid)
}

/// xid values are 32-bit; the engine carries them in 64 bits, truncated like PG's xid view.
fn opt_xid(v: Option<u64>) -> Datum {
    v.map_or(Datum::Null, |x| Datum::Oid(x as u32))
}

fn text_array(items: Vec<String>) -> Datum {
    Datum::Array(Box::new(Array::from_vec(TEXT_OID, items.into_iter().map(Datum::Text).collect())))
}

fn int4_array(items: Vec<i32>) -> Datum {
    Datum::Array(Box::new(Array::from_vec(INT4_OID, items.into_iter().map(Datum::Int4).collect())))
}

fn regtype_array(items: &[Oid]) -> Datum {
    Datum::Array(Box::new(Array::from_vec(REGTYPE_OID, items.iter().map(|o| Datum::Oid(*o)).collect())))
}

fn setting_record(s: SettingRow) -> Datum {
    Datum::Record(vec![
        Datum::Text(s.name),
        opt_text(s.setting),
        opt_text(s.unit),
        Datum::Text(s.category),
        Datum::Text(s.short_desc),
        opt_text(s.extra_desc),
        Datum::Text(s.context),
        Datum::Text(s.vartype),
        Datum::Text(s.source),
        opt_text(s.min_val),
        opt_text(s.max_val),
        s.enumvals.map_or(Datum::Null, text_array),
        opt_text(s.boot_val),
        opt_text(s.reset_val),
        opt_text(s.sourcefile),
        opt_i32(s.sourceline),
        Datum::Bool(s.pending_restart),
    ])
}

fn lock_record(l: &LockRow) -> Datum {
    Datum::Record(vec![
        Datum::Text(l.locktype.clone()),
        opt_oid(l.database),
        opt_oid(l.relation),
        opt_i32(l.page),
        opt_i16(l.tuple),
        opt_text(l.virtualxid.clone()),
        opt_xid(l.transactionid),
        opt_oid(l.classid),
        opt_oid(l.objid),
        opt_i16(l.objsubid),
        opt_text(l.virtualtransaction.clone()),
        opt_i32(l.pid),
        Datum::Text(l.mode.clone()),
        Datum::Bool(l.granted),
        Datum::Bool(l.fastpath),
        opt_ts(l.waitstart),
    ])
}

fn cursor_record(c: CursorRow) -> Datum {
    Datum::Record(vec![
        Datum::Text(c.name),
        Datum::Text(c.statement),
        Datum::Bool(c.is_holdable),
        Datum::Bool(c.is_binary),
        Datum::Bool(c.is_scrollable),
        Datum::TimestampTz(c.creation_time),
    ])
}

fn prepared_record(p: PreparedStatementRow) -> Datum {
    let parameter_types = regtype_array(&p.parameter_types);
    let result_types = match &p.result_types {
        Some(v) => regtype_array(v),
        None => Datum::Null,
    };
    Datum::Record(vec![
        Datum::Text(p.name),
        Datum::Text(p.statement),
        Datum::TimestampTz(p.prepare_time),
        parameter_types,
        result_types,
        Datum::Bool(p.from_sql),
        Datum::Int8(p.generic_plans),
        Datum::Int8(p.custom_plans),
    ])
}

/// One `pg_stat_get_activity` value, chosen by the proc's OUT column name, so each record follows
/// the generated proc signature. Columns PGArbor has no source for are NULL (`client_addr`: no inet
/// Datum yet; SSL and GSSAPI details, since connections are plain).
fn activity_column(row: &ActivityRow, name: &str) -> Datum {
    match name {
        "pid" => Datum::Int4(row.pid),
        "datid" => Datum::Oid(row.datid),
        "usesysid" => Datum::Oid(row.usesysid),
        "application_name" => Datum::Text(row.application_name.clone()),
        "state" => opt_text(row.state.clone()),
        "query" => Datum::Text(row.query.clone()),
        "wait_event_type" => opt_text(row.wait_event_type.clone()),
        "wait_event" => opt_text(row.wait_event.clone()),
        "xact_start" => opt_ts(row.xact_start),
        "query_start" => opt_ts(row.query_start),
        "backend_start" => opt_ts(row.backend_start),
        "state_change" => opt_ts(row.state_change),
        "client_hostname" => opt_text(row.client_hostname.clone()),
        "client_port" => opt_i32(row.client_port),
        "backend_xid" => opt_xid(row.backend_xid),
        "backend_xmin" => opt_xid(row.backend_xmin),
        "backend_type" => Datum::Text(row.backend_type.clone()),
        "ssl" | "gss_auth" | "gss_enc" | "gss_delegation" => Datum::Bool(false),
        "leader_pid" => opt_i32(row.leader_pid),
        "query_id" => opt_i64(row.query_id),
        _ => Datum::Null,
    }
}

fn activity_records(ctx: &CatalogCallCtx, proc: &BuiltinProc, pid_filter: Option<i32>) -> Result<Vec<Datum>> {
    let columns = proc.out_columns();
    let mut out = Vec::new();
    for row in ctx.session.activity()? {
        if pid_filter.is_some_and(|pid| pid != row.pid) {
            continue;
        }
        out.push(Datum::Record(columns.iter().map(|&(name, _)| activity_column(&row, name)).collect()));
    }
    Ok(out)
}

/// Compiled-in extension control data (PG `*.control` values) for the entries of
/// `builtin::EXTENSIONS`. Only `plpgsql` is compiled in.
struct ExtControl {
    comment: &'static str,
    superuser: bool,
    trusted: bool,
}

fn extension_control(name: &str) -> Option<ExtControl> {
    match name {
        "plpgsql" => Some(ExtControl { comment: "PL/pgSQL procedural language", superuser: true, trusted: true }),
        _ => None,
    }
}

/// `pg_available_extensions()` (3 columns) when `versions` is false, `pg_available_extension_versions()`
/// (8 columns) when true; one row per compiled-in extension.
fn extension_records(ctx: &CatalogCallCtx, versions: bool) -> Result<Vec<Datum>> {
    let mut out = Vec::new();
    for ext in crate::builtin::EXTENSIONS {
        let ctl = extension_control(ext.name)
            .ok_or_else(|| PgError::internal(format!("no control data for extension {}", ext.name)))?;
        if versions {
            let schema = match ctx.env.snapshot().namespace(ext.namespace) {
                Some(ns) => ns.name.clone(),
                None => return Err(PgError::internal(format!("schema with OID {} does not exist", ext.namespace))),
            };
            out.push(Datum::Record(vec![
                Datum::Text(ext.name.to_string()),
                Datum::Text(ext.version.to_string()),
                Datum::Bool(ctl.superuser),
                Datum::Bool(ctl.trusted),
                Datum::Bool(ext.relocatable),
                Datum::Text(schema),
                Datum::Null,
                Datum::Text(ctl.comment.to_string()),
            ]));
        } else {
            out.push(Datum::Record(vec![
                Datum::Text(ext.name.to_string()),
                Datum::Text(ext.version.to_string()),
                Datum::Text(ctl.comment.to_string()),
            ]));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::session::{SequenceStateRow, SessionInfo};
    use crate::testing;
    use pgarbor_common::Result;
    use pgarbor_types::catalog_data::procs_named;
    use pgarbor_types::IoConfig;

    #[derive(Default)]
    struct TestSession {
        pid: i32,
        settings: Vec<SettingRow>,
        activity: Vec<ActivityRow>,
    }

    impl SessionInfo for TestSession {
        fn settings(&self) -> Result<Vec<SettingRow>> {
            Ok(self.settings.clone())
        }
        fn sequence_state(&self, _oid: Oid) -> Result<Option<SequenceStateRow>> {
            Ok(None)
        }
        fn activity(&self) -> Result<Vec<ActivityRow>> {
            Ok(self.activity.clone())
        }
        fn backend_pid(&self) -> i32 {
            self.pid
        }
    }

    fn proc_named(name: &str) -> &'static BuiltinProc {
        procs_named(name).next().expect("builtin proc present in PROCS")
    }

    fn ctx<'a>(env: &'a CatalogEnv, session: &'a dyn SessionInfo, io: &'a IoConfig) -> CatalogCallCtx<'a> {
        CatalogCallCtx { env, session, io_config: io }
    }

    #[test]
    fn current_user_and_database_come_from_env() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession::default();
        let c = ctx(&env, &session, &io);
        let expected = env.snapshot().role_name(env.current_user()).expect("current role exists").to_string();
        let user = call(&c, proc_named("current_user"), &[])?;
        assert!(matches!(&user, Datum::Text(s) if s.as_str() == expected.as_str()));
        let db = call(&c, proc_named("current_database"), &[])?;
        assert!(matches!(&db, Datum::Text(s) if s.as_str() == "postgres"));
        Ok(())
    }

    #[test]
    fn backend_pid_comes_from_session() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession { pid: 4242, ..Default::default() };
        let c = ctx(&env, &session, &io);
        let pid = call(&c, proc_named("pg_backend_pid"), &[])?;
        assert!(matches!(pid, Datum::Int4(4242)));
        Ok(())
    }

    #[test]
    fn show_all_settings_has_seventeen_fields() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession {
            settings: vec![SettingRow {
                name: "work_mem".to_string(),
                setting: Some("4096".to_string()),
                enumvals: Some(vec!["a".to_string()]),
                sourceline: Some(7),
                pending_restart: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = ctx(&env, &session, &io);
        let rows = call_set(&c, proc_named("pg_show_all_settings"), &[])?;
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            Datum::Record(fields) => {
                assert_eq!(fields.len(), 17);
                assert!(matches!(&fields[0], Datum::Text(s) if s.as_str() == "work_mem"));
                assert!(matches!(fields[15], Datum::Int4(7)));
                assert!(matches!(fields[16], Datum::Bool(true)));
            }
            _ => panic!("pg_show_all_settings must emit records"),
        }
        Ok(())
    }

    #[test]
    fn stat_get_activity_filters_by_pid_and_null_means_all() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession {
            activity: vec![
                ActivityRow { pid: 10, query: "select 1".to_string(), ..Default::default() },
                ActivityRow { pid: 20, query: "select 2".to_string(), ..Default::default() },
            ],
            ..Default::default()
        };
        let c = ctx(&env, &session, &io);
        let p = proc_named("pg_stat_get_activity");
        let all = call_set(&c, p, &[Datum::Null])?;
        assert_eq!(all.len(), 2);
        let one = call_set(&c, p, &[Datum::Int4(20)])?;
        assert_eq!(one.len(), 1);
        match &one[0] {
            Datum::Record(fields) => {
                assert_eq!(fields.len(), p.out_columns().len());
                assert!(matches!(fields[1], Datum::Int4(20)));
            }
            _ => panic!("pg_stat_get_activity must emit records"),
        }
        Ok(())
    }

    #[test]
    fn available_extensions_lists_plpgsql() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession::default();
        let c = ctx(&env, &session, &io);
        let rows = call_set(&c, proc_named("pg_available_extensions"), &[])?;
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            Datum::Record(f) => {
                assert!(matches!(&f[0], Datum::Text(s) if s.as_str() == "plpgsql"));
                assert!(matches!(&f[1], Datum::Text(s) if s.as_str() == "1.0"));
            }
            _ => panic!("pg_available_extensions must emit records"),
        }
        Ok(())
    }

    #[test]
    fn unsupported_backing_functions_raise_unimplemented() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = TestSession::default();
        let c = ctx(&env, &session, &io);
        let err = call_set(&c, proc_named("pg_get_aios"), &[]).err().expect("pg_get_aios is unsupported");
        assert!(err.is_unimplemented());
        Ok(())
    }
}
