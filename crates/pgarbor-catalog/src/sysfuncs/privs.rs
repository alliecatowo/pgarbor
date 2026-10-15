//! Privilege inquiry functions. Lease CAT-47. PG sources: `utils/adt/acl.c` (`has_table_privilege_*`,
//! `has_any_column_privilege_*`, `has_column_privilege_*`, `has_database_privilege_*`,
//! `has_function_privilege_*`, `has_language_privilege_*`, `has_schema_privilege_*`,
//! `has_sequence_privilege_*`, `has_tablespace_privilege_*`, `has_type_privilege_*`, `pg_has_role_*`,
//! `convert_*_priv_string`), `utils/misc/rls.c` (`row_security_active`). Checks use `crate::acl`
//! (`RoleGraph`, `privilege_mask`) over `ctx.env.snapshot()`.
//!
//! ## Argument forms
//! Every `has_X_privilege` has forms `(user, object, privilege)` and `(object, privilege)`, where
//! `user` is a role name (`name`) or OID, `object` a name (text) or OID, `privilege` a text list.
//! The two-argument form uses `ctx.env.current_user()`. Resolution of names:
//! `table` via `reg_lookup_impl(Class)` -> 42P01 `relation "x" does not exist`; `schema` 3F000 `schema
//! "x" does not exist`; `database` 3D000 `database "x" does not exist`; `function` text signature via
//! `reg_lookup_impl(Procedure)` -> 42883; `type` via `names::parse_type_string` -> 42704; `sequence`
//! like table (and 42809 `"t" is not a sequence` for non-sequences); `language` 42704 `language "x"
//! does not exist`; role names 42704 `role "x" does not exist`. OID arguments for unknown objects
//! yield NULL (no error).
//!
//! ## Privilege strings (`convert_priv_string` family)
//! Comma-separated, case-insensitive, whitespace-trimmed words, each optionally suffixed ` WITH GRANT
//! OPTION`: tables `SELECT INSERT UPDATE DELETE TRUNCATE REFERENCES TRIGGER MAINTAIN`; sequences
//! `USAGE SELECT UPDATE`; schemas `CREATE USAGE`; databases `CREATE TEMPORARY TEMP CONNECT`;
//! functions `EXECUTE`; languages/types `USAGE`; tablespaces `CREATE`; columns `SELECT INSERT UPDATE
//! REFERENCES`. Unknown word -> 22023 `unrecognized privilege type: "foo"`. Result true iff the user
//! holds ANY of the listed privileges (PG semantic: `pg_*_aclmask(...) != 0`), where WITH GRANT OPTION
//! requires the grant-option bit.
//! - `has_table_privilege` on a column-less check also true when the user has the privilege on ALL
//!   columns? NO: table-level only; `has_any_column_privilege` = table-level OR any column ACL;
//!   `has_column_privilege(table, column|attnum, priv)` = table-level OR that column's ACL (unknown
//!   column name -> 42703; dropped attnum -> NULL).
//! - `pg_has_role(user, role, priv)` with `priv` in `USAGE | MEMBER | SET` (+ `WITH ADMIN OPTION`):
//!   USAGE = `has_privs_of_role`, MEMBER = `is_member_of_role`, SET = `can_set_role`, superuser true
//!   for all; unknown word 22023 `unrecognized privilege type: "x"`.
//! - `row_security_active(table)` -> false (policies unsupported; relrowsecurity without policies
//!   does not hide rows from owners; return `rowsecurity && !owner/bypass` = false in v1).
//! - Foreign data/server/large object/parameter variants: `has_foreign_data_wrapper_privilege`,
//!   `has_server_privilege`, `has_largeobject_privilege`, `has_parameter_privilege` -> NULL for any
//!   object (none exist); `has_parameter_privilege(name, priv)` true for superusers only.
use super::CatalogCallCtx;
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::Datum;

/// Procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "has_table_privilege",
    "has_any_column_privilege",
    "has_column_privilege",
    "has_database_privilege",
    "has_function_privilege",
    "has_language_privilege",
    "has_schema_privilege",
    "has_sequence_privilege",
    "has_tablespace_privilege",
    "has_type_privilege",
    "has_foreign_data_wrapper_privilege",
    "has_server_privilege",
    "has_largeobject_privilege",
    "has_parameter_privilege",
    "pg_has_role",
    "row_security_active",
];

/// Dispatch on `proc.name` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("CAT-47", "privilege inquiry functions"))
}
