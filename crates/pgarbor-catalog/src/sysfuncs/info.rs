//! Definition and description functions (`pg_get_*def`, `pg_get_expr`, function text, `pg_describe_object`,
//! `obj_description`, ...). Lease CAT-46 (sonnet: many small functions, exact PG outputs). Sizes, sequence
//! info, index properties, keywords and option helpers live in `introspect.rs` (CAT-49). PG sources: `utils/adt/ruleutils.c`, `utils/adt/misc.c`,
//! `commands/comment.c` (`GetComment`), `catalog/objectaddress.c` (`pg_describe_object`,
//! `pg_identify_object*`), `utils/adt/dbsize.c`, `utils/adt/amutils.c`, `utils/adt/format_type.c`,
//! `utils/adt/regproc.c`, `commands/sequence.c`, `parser/keywords.c`.
//! Definition text comes from `crate::deparse` (CAT-26).
//!
//! | proc | behavior |
//! |---|---|
//! | `pg_get_expr(pg_node_tree, oid [, bool])` | first argument unchanged (see `sysfuncs/mod.rs`) |
//! | `pg_get_constraintdef(oid [, bool])` | `deparse::constraintdef`; unknown OID -> NULL |
//! | `pg_get_indexdef(oid)`, `pg_get_indexdef(oid, int, bool)` | `deparse::indexdef`; the 3-arg form with column number |
//! | `pg_get_viewdef(oid)`, `(oid, bool)`, `(oid, int)`, `(text)`, `(text, bool)` | `deparse::viewdef`; text forms resolve via `reg_lookup_impl(Class)`; non-view -> 42809? PG returns NULL for unknown OID and raises `"t" is not a view` for non-view relations: follow PG |
//! | `pg_get_ruledef(oid [, bool])` | for view rules: `CREATE RULE "_RETURN" AS ON SELECT TO t DO INSTEAD <viewdef>`-style text from `pg_rewrite` row (rule OID = view OID); unknown -> NULL |
//! | `pg_get_triggerdef(oid [, bool])` | no triggers: `XX000 could not find tuple for trigger <oid>` (PG wording, `errors::cache_lookup_failed`-style) |
//! | `pg_get_function_arguments(oid)`, `pg_get_function_identity_arguments(oid)`, `pg_get_function_result(oid)`, `pg_get_functiondef(oid)`, `pg_get_function_sqlbody(oid)`, `pg_get_function_arg_default(oid, int)` | `deparse::*`; builtin functions work too (`BuiltinProc`) |
//! | `pg_get_serial_sequence(text, text)` | table name resolved via `reg_lookup_impl(Class)`, column by name (42703 `column "x" of relation "t" does not exist`), result via `deparse::serial_sequence`; a table that does not exist -> 42P01 |
//! | `pg_get_userbyid(oid)` | role name, unknown -> `unknown (OID=<n>)` |
//! | `pg_get_partkeydef`, `pg_get_partition_constraintdef`, `pg_get_replica_identity_index(oid)` | NULL / NULL / the replica-identity index regclass or NULL |
//! | `pg_get_statisticsobjdef*` | NULL (no extended statistics) |
//! | `pg_describe_object(oid, oid, int)` | `describe::describe(ObjectAddress{class from classid, oid, sub_id})`; unknown -> NULL |
//! | `pg_identify_object(oid, oid, int)` | record `(type text, schema text, name text, identity text)` using `describe::type_description` and the PG identity text (`public.t`, `public.t.col`, `postgres`) |
//! | `pg_identify_object_as_address(oid, oid, int)` | record `(type, object_names text[], object_args text[])` |
//! | `pg_get_object_address(text, text[], text[])` | `Err(unimplemented)` |
//! | `obj_description(oid, name)`, `obj_description(oid)`, `col_description(oid, int)`, `shobj_description(oid, name)` | `snapshot.description(DescriptionKey{..})` with classoid from the catalog NAME (`pg_class` 1259, `pg_type` 1247, `pg_proc` 1255, `pg_namespace` 2615, `pg_constraint` 2606, `pg_database` 1262, `pg_authid` 1260); one-arg `obj_description` is deprecated and returns NULL; missing -> NULL |
use super::CatalogCallCtx;
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::Datum;

/// Scalar procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "pg_get_expr",
    "pg_get_constraintdef",
    "pg_get_indexdef",
    "pg_get_viewdef",
    "pg_get_ruledef",
    "pg_get_triggerdef",
    "pg_get_function_arguments",
    "pg_get_function_identity_arguments",
    "pg_get_function_result",
    "pg_get_functiondef",
    "pg_get_function_sqlbody",
    "pg_get_function_arg_default",
    "pg_get_serial_sequence",
    "pg_get_userbyid",
    "pg_get_partkeydef",
    "pg_get_partition_constraintdef",
    "pg_get_replica_identity_index",
    "pg_get_statisticsobjdef",
    "pg_get_statisticsobjdef_columns",
    "pg_get_statisticsobjdef_expressions",
    "pg_describe_object",
    "pg_identify_object",
    "pg_identify_object_as_address",
    "pg_get_object_address",
    "obj_description",
    "col_description",
    "shobj_description",
];

/// Set-returning procs among [`NAMES`].
// CONTRACT
pub const SET_NAMES: &[&str] = &[
];

/// Scalar dispatch on `proc.name` (module docs table).
// CONTRACT
#[allow(unused_variables)]
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("CAT-46", "definition and introspection functions"))
}

/// Set-returning dispatch: this file has no set-returning procs ([`SET_NAMES`] is empty), so any
/// call is an internal error.
// CONTRACT
#[allow(unused_variables)]
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::internal(format!("function {} is not a set-returning definition function", proc.name)))
}
