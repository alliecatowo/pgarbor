//! Catalog-backed SQL functions: the `ImplModule::External` procs that need only a catalog
//! snapshot, the session's naming context and [`SessionInfo`]. The `pgarbor` crate's
//! `SessionContext::call_external` / `call_external_set` first asks [`handles`]; a `true` answer
//! means "call [`call`] / [`call_set`]", `false` means the proc belongs to the executor/engine
//! (`nextval`, `currval`, `lastval`, `setval`, `set_config`, `current_setting`, advisory locks,
//! `pg_sleep*`, `txid_*`/`pg_current_xact_id*`, `pg_notify`, `pg_listening_channels`,
//! `pg_xact_status`, `pg_last_committed_xact`, `pg_trigger_depth`, `pg_export_snapshot`,
//! `pg_cancel_backend`, `pg_terminate_backend`, `pg_advisory_*`).
//!
//! Strictness (NULL-in -> NULL-out) is applied by the caller (`func::FuncHandle::call`) for strict
//! procs; handlers still tolerate `Datum::Null` arguments of non-strict procs.
//!
//! | file | procs |
//! |---|---|
//! | [`visible`] | `pg_*_is_visible`, `to_reg*`, `regclass`, `current_schema(s)`, `pg_my_temp_schema`, `pg_is_other_temp_schema` |
//! | [`info`] | `pg_get_*def`, `pg_get_expr`, `pg_get_function_*`, `pg_get_serial_sequence`, `pg_get_userbyid`, `pg_describe_object`, `pg_identify_object*`, `obj_description`, `col_description`, `shobj_description` |
//! | [`introspect`] | sequence info, updatability, relation/database sizes, index property functions, `pg_get_keywords`, `pg_options_to_table`, `pg_input_is_valid`/`pg_input_error_info`, `_pg_truetypid`/`_pg_truetypmod`, partition stubs |
//! | [`privs`] | `has_*_privilege`, `pg_has_role`, `row_security_active` |
//! | [`session_fns`] | `current_user`, `session_user`, `system_user`, `current_database`, `current_query`, `pg_backend_pid`, `pg_is_in_recovery`, server times, `pg_blocking_pids`, `pg_show_all_settings`, `pg_stat_get_activity`, `pg_lock_status`, `pg_cursor`, `pg_prepared_statement`, `pg_available_extensions*` |
//!
//! ## `pg_node_tree` arguments
//! Catalog columns of type `pg_node_tree` (`pg_attrdef.adbin`, `pg_constraint.conbin`,
//! `pg_index.indexprs/indpred`, `pg_proc.proargdefaults`) hold the canonical expression TEXT in
//! PGArbor (see `vrel`), so `pg_get_expr(node_tree, relid [, pretty])` simply returns its first
//! argument unchanged (NULL in -> NULL out).
pub mod info;
pub mod introspect;
pub mod privs;
pub mod session_fns;
pub mod visible;

use crate::env::CatalogEnv;
use crate::session::SessionInfo;
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::{Datum, IoConfig};

/// Everything a catalog function may read.
// CONTRACT
pub struct CatalogCallCtx<'a> {
    pub env: &'a CatalogEnv,
    pub session: &'a dyn SessionInfo,
    pub io_config: &'a IoConfig,
}

/// True if a catalog function implements `proc` (name match against the per-file `NAMES` tables).
pub fn handles(proc: &BuiltinProc) -> bool {
    visible::NAMES.contains(&proc.name)
        || info::NAMES.contains(&proc.name)
        || introspect::NAMES.contains(&proc.name)
        || privs::NAMES.contains(&proc.name)
        || session_fns::NAMES.contains(&proc.name)
}

/// True if `proc` is set-returning and implemented here (then use [`call_set`]).
pub fn handles_set(proc: &BuiltinProc) -> bool {
    proc.retset && handles(proc)
}

/// Evaluate a scalar catalog function.
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    if visible::NAMES.contains(&proc.name) {
        visible::call(ctx, proc, args)
    } else if info::NAMES.contains(&proc.name) {
        info::call(ctx, proc, args)
    } else if introspect::NAMES.contains(&proc.name) {
        introspect::call(ctx, proc, args)
    } else if privs::NAMES.contains(&proc.name) {
        privs::call(ctx, proc, args)
    } else if session_fns::NAMES.contains(&proc.name) {
        session_fns::call(ctx, proc, args)
    } else {
        Err(PgError::internal(format!("function {} is not a catalog function", proc.name)))
    }
}

/// Evaluate a set-returning catalog function; records are `Datum::Record`.
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    if info::SET_NAMES.contains(&proc.name) {
        info::call_set(ctx, proc, args)
    } else if introspect::SET_NAMES.contains(&proc.name) {
        introspect::call_set(ctx, proc, args)
    } else if session_fns::SET_NAMES.contains(&proc.name) {
        session_fns::call_set(ctx, proc, args)
    } else {
        Err(PgError::internal(format!("function {} is not a catalog set-returning function", proc.name)))
    }
}
