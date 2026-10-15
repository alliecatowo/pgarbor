//! Introspection helpers: sequence info, updatability, relation/database sizes, index access-method
//! properties, keywords, option parsing, input validation, partition stubs. Lease CAT-49. PG sources:
//! `commands/sequence.c`, `utils/adt/dbsize.c`, `utils/adt/amutils.c`, `utils/adt/misc.c`
//! (`pg_get_keywords`, `pg_input_is_valid`), `utils/adt/regproc.c`, `foreign/foreign.c`
//! (`pg_options_to_table`), `catalog/partition.c`, `utils/adt/ruleutils.c` (`pg_get_sequence_data` is
//! 18's addition). Definition text lives in `deparse` (CAT-26) and is served by `info.rs` (CAT-46).
//!
//! | proc | behavior |
//! |---|---|
//! | `pg_sequence_parameters(oid)` | record `(start_value, minimum_value, maximum_value, increment, cycle_option, cache_size, data_type)` (`Int8`..., `Bool`, `Int8`, `Oid`) from `Sequence` |
//! | `pg_sequence_last_value(oid)`, `pg_get_sequence_data(oid)` | from `session.sequence_state` (NULL when never called) |
//! | `pg_relation_is_updatable(oid, bool)` / `pg_column_is_updatable(oid, int2, bool)` | bitmask 28 (INSERT 4 + UPDATE 8 + DELETE 16) for ordinary tables, 0 for views/matviews/others; column variant `true` for table columns |
//! | `pg_relation_filenode(oid)` | the relation's OID for relkinds with storage, NULL otherwise |
//! | `pg_relation_size(oid [, text])`, `pg_table_size`, `pg_indexes_size`, `pg_total_relation_size` | `session.relation_size(oid, SizeKind)`; unknown OID -> NULL |
//! | `pg_database_size(name\|oid)` | sum over relations via the session (or `session.relation_size` of the database); 42704-style `database "x" does not exist` for unknown name |
//! | `pg_tablespace_location(oid)` | empty string for pg_default/pg_global |
//! | `pg_indexam_has_property(oid, text)`, `pg_index_has_property(regclass, text)`, `pg_index_column_has_property(regclass, int, text)`, `_pg_index_position(oid, int)` | btree properties as PG `amutils.c`: am: `can_order t, can_unique t, can_multi_col t, can_exclude t, can_include t`; index: `clusterable t, index_scan t, bitmap_scan t, backward_scan t`; column: `asc, desc, nulls_first, nulls_last, orderable t, distance_orderable f, returnable t, search_array t, search_nulls t`; unknown property -> NULL |
//! | `pg_get_keywords()` (SRF) | rows `(word text, catcode "char", barelabel bool, catdesc text, baredesc text)` from `contracts/reference/pg18/pg_keywords.csv` (embed with `include_str!`); order by word |
//! | `pg_options_to_table(text[])` (SRF) | rows `(option_name, option_value)` splitting `name=value`; value NULL when absent |
//! | `pg_collation_actual_version(oid)` | NULL for builtin collations |
//! | `pg_input_is_valid(text, text)`, `pg_input_error_info(text, text)` | parse the type name with `names::parse_type_string`, call `io::input`; `is_valid` -> bool; `error_info` -> record `(message, detail, hint, sql_error_code)` (all NULL when valid); type-name errors still raise |
//! | `_pg_truetypid(attribute record, type oid)`, `_pg_truetypmod(...)` | base type OID / typmod through domains (used by information_schema: arguments arrive as (atttypid, atttypmod)-derived values per `information_schema.sql`; implement `registry::base_type_and_typmod`) |
//! | `pg_partition_tree`, `pg_partition_ancestors`, `pg_partition_root` | no partitions: `pg_partition_root(x)` -> NULL; tree/ancestors -> empty sets for tables, NULL root |
//! | `pg_get_catalog_foreign_keys()` (SRF) | `Err(unimplemented)` |
use super::CatalogCallCtx;
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::Datum;

/// Scalar procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "pg_sequence_parameters",
    "pg_sequence_last_value",
    "pg_get_sequence_data",
    "pg_relation_is_updatable",
    "pg_column_is_updatable",
    "pg_relation_filenode",
    "pg_relation_size",
    "pg_table_size",
    "pg_indexes_size",
    "pg_total_relation_size",
    "pg_database_size",
    "pg_tablespace_location",
    "pg_indexam_has_property",
    "pg_index_has_property",
    "pg_index_column_has_property",
    "_pg_index_position",
    "pg_collation_actual_version",
    "pg_input_is_valid",
    "pg_input_error_info",
    "_pg_truetypid",
    "_pg_truetypmod",
    "pg_partition_root",
    "pg_get_keywords",
    "pg_options_to_table",
    "pg_partition_tree",
    "pg_partition_ancestors",
    "pg_get_catalog_foreign_keys",
];

/// Set-returning procs among [`NAMES`].
// CONTRACT
pub const SET_NAMES: &[&str] = &[
    "pg_get_keywords",
    "pg_options_to_table",
    "pg_partition_tree",
    "pg_partition_ancestors",
    "pg_get_catalog_foreign_keys",
];

/// Scalar dispatch on `proc.name` (module docs table).
// CONTRACT
#[allow(unused_variables)]
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("CAT-49", "introspection helper functions"))
}

/// Set-returning dispatch.
// CONTRACT
#[allow(unused_variables)]
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("CAT-49", "set-returning introspection helpers"))
}
