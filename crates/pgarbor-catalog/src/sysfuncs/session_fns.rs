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
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::Datum;

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
#[allow(unused_variables)]
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("CAT-48", "session identity and engine-state functions"))
}

/// Set-returning dispatch.
// CONTRACT
#[allow(unused_variables)]
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("CAT-48", "set-returning engine-state functions"))
}
