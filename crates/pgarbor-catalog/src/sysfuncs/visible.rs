//! Name-resolution functions: visibility predicates, `to_reg*`, `current_schema(s)`. Lease CAT-45.
//! PG sources: `catalog/namespace.c` (`*IsVisible`, `fetch_search_path`), `utils/adt/regproc.c`
//! (`to_regclass`, `to_regproc`, `to_regprocedure`, `to_regoper`, `to_regoperator`, `to_regnamespace`,
//! `to_regrole`, `to_regcollation`, `regclass(text)`), `utils/adt/misc.c` (`current_schema`,
//! `current_schemas`, `pg_my_temp_schema`).
//!
//! | proc (args) | behavior |
//! |---|---|
//! | `pg_table_is_visible(oid)` | `CatalogEnv::relation_is_visible`; unknown OID -> NULL |
//! | `pg_type_is_visible(oid)` | `type_is_visible`; unknown -> NULL |
//! | `pg_function_is_visible(oid)` | `function_is_visible`; unknown -> NULL |
//! | `pg_operator_is_visible(oid)`, `pg_opclass_is_visible`, `pg_opfamily_is_visible`, `pg_collation_is_visible`, `pg_conversion_is_visible`, `pg_statistics_obj_is_visible`, `pg_ts_config_is_visible`, `pg_ts_dict_is_visible`, `pg_ts_parser_is_visible`, `pg_ts_template_is_visible` | operator/opclass/opfamily/collation: visible iff the object's namespace is in `path().effective` and no earlier same-name object (builtins live in pg_catalog, which is searched first/implicitly); unknown OID -> NULL; conversions, statistics objects and text-search objects do not exist -> NULL |
//! | `to_regclass(text)` | `reg_lookup_impl(RegKind::Class, name)` -> `Datum::Oid` or NULL when missing; NEVER raises 42P01; syntax errors in the name (unterminated quotes, too many dots) are also swallowed to NULL like PG (`to_reg*` catch errors) |
//! | `to_regproc(text)`, `to_regprocedure(text)`, `to_regoper`, `to_regoperator`, `to_regnamespace`, `to_regrole`, `to_regcollation` | same with the matching `RegKind`; `to_regproc` of an ambiguous name -> raises 42725 `more than one function named "f"` (PG raises here) |
//! | `regclass(text)` | the text->regclass conversion (`'t'::regclass` semantics): `reg_lookup_impl(Class, name)` -> OID, missing -> 42P01 `relation "t" does not exist` (`errors::relation_does_not_exist`) |
//! | `current_schema()` | the first existing schema of `path().explicit` that is the creation target: name of `creation_namespace`, NULL if none |
//! | `current_schemas(bool)` | `name[]` of `path().explicit` names (false) or `effective` (true, includes implicit `pg_catalog`; oracle default: `{pg_catalog,public}` for true, `{public}` for false) |
//! | `pg_my_temp_schema()` | `session.temp_namespace()` or 0 |
//! | `pg_is_other_temp_schema(oid)` | false (no temp schemas of other sessions are visible) |
use super::CatalogCallCtx;
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::Datum;

/// Procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "pg_table_is_visible",
    "pg_type_is_visible",
    "pg_function_is_visible",
    "pg_operator_is_visible",
    "pg_opclass_is_visible",
    "pg_opfamily_is_visible",
    "pg_collation_is_visible",
    "pg_conversion_is_visible",
    "pg_statistics_obj_is_visible",
    "pg_ts_config_is_visible",
    "pg_ts_dict_is_visible",
    "pg_ts_parser_is_visible",
    "pg_ts_template_is_visible",
    "to_regclass",
    "to_regproc",
    "to_regprocedure",
    "to_regoper",
    "to_regoperator",
    "to_regnamespace",
    "to_regrole",
    "to_regcollation",
    "regclass",
    "current_schema",
    "current_schemas",
    "pg_my_temp_schema",
    "pg_is_other_temp_schema",
];

/// Dispatch on `proc.name` (module docs table).
// CONTRACT
#[allow(unused_variables)]
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("CAT-45", "visibility and name-resolution functions"))
}
