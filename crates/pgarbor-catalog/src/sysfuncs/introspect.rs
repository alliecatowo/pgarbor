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
use crate::errors;
use crate::session::SizeKind;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::{io, names, registry, Datum, IoCtx};

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
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    match proc.name {
        "pg_sequence_parameters" => sequence_parameters(ctx, args),
        "pg_sequence_last_value" => sequence_last_value(ctx, args),
        "pg_get_sequence_data" => sequence_data(ctx, args),
        "pg_relation_is_updatable" => relation_is_updatable(ctx, args),
        "pg_column_is_updatable" => column_is_updatable(ctx, args),
        "pg_relation_filenode" => relation_filenode(ctx, args),
        "pg_relation_size" => relation_size_fn(ctx, args),
        "pg_table_size" => size_of_oid_arg(ctx, args, SizeKind::Table),
        "pg_indexes_size" => size_of_oid_arg(ctx, args, SizeKind::Indexes),
        "pg_total_relation_size" => size_of_oid_arg(ctx, args, SizeKind::Total),
        "pg_database_size" => database_size(ctx, proc, args),
        "pg_tablespace_location" => tablespace_location(args),
        "pg_indexam_has_property" => indexam_has_property(args),
        "pg_index_has_property" => index_has_property(ctx, args),
        "pg_index_column_has_property" => index_column_has_property(ctx, args),
        "_pg_index_position" => index_position(ctx, args),
        "pg_collation_actual_version" => collation_actual_version(args),
        "pg_input_is_valid" => input_is_valid(ctx, args),
        "pg_input_error_info" => input_error_info(ctx, args),
        "_pg_truetypid" => {
            true_type(ctx, args).map(|found| found.map_or(Datum::Null, |(oid, _)| Datum::Oid(oid)))
        }
        "_pg_truetypmod" => {
            true_type(ctx, args).map(|found| found.map_or(Datum::Null, |(_, typmod)| Datum::Int4(typmod)))
        }
        // No partitions exist, so no relation is a partition.
        "pg_partition_root" => Ok(Datum::Null),
        other => Err(PgError::internal(format!("function {other} is not a scalar introspection function"))),
    }
}

/// Set-returning dispatch.
// CONTRACT
#[allow(unused_variables)]
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    match proc.name {
        "pg_get_keywords" => keyword_rows(),
        "pg_options_to_table" => options_rows(args),
        // No partitions exist: the tree and the ancestry of any relation are empty.
        "pg_partition_tree" | "pg_partition_ancestors" => Ok(Vec::new()),
        "pg_get_catalog_foreign_keys" => {
            Err(PgError::unimplemented("CAT-49", "pg_get_catalog_foreign_keys (catalog foreign-key metadata)"))
        }
        other => Err(PgError::internal(format!("function {other} is not a set-returning introspection function"))),
    }
}

// ---------------------------------------------------------------- constants

/// pg_am OID of btree (the only access method whose properties are reported).
const BTREE_AM_OID: Oid = 403;
/// `DEFAULT_COLLATION_OID` (pg_collation `default`).
const DEFAULT_COLLATION_OID: Oid = 100;
/// `DEFAULTTABLESPACE_OID` (pg_default).
const DEFAULT_TABLESPACE_OID: Oid = 1663;
/// `GLOBALTABLESPACE_OID` (pg_global).
const GLOBAL_TABLESPACE_OID: Oid = 1664;
/// `name` type OID: selects the `pg_database_size(name)` overload.
const NAME_TYPE_OID: Oid = 19;
/// `pg_relation_is_updatable` bits: INSERT (4) | UPDATE (8) | DELETE (16).
const UPDATABLE_BITS: i32 = 4 | 8 | 16;
/// Keyword table compiled in from the PG 18 reference.
const KEYWORDS_CSV: &str = include_str!("../../../../contracts/reference/pg18/pg_keywords.csv");

// ---------------------------------------------------------------- argument helpers

fn oid_arg(args: &[Datum], i: usize) -> Option<Oid> {
    args.get(i).and_then(Datum::as_oid)
}

fn text_arg(args: &[Datum], i: usize) -> Option<&str> {
    args.get(i).and_then(Datum::as_str)
}

/// `pg_relation_size`-style size of a class OID; NULL for an OID that is not in the catalog.
fn size_of(ctx: &CatalogCallCtx, oid: Oid, kind: SizeKind) -> Result<Datum> {
    if ctx.env.snapshot().class(oid).is_none() {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int8(ctx.session.relation_size(oid, kind)?))
}

fn size_of_oid_arg(ctx: &CatalogCallCtx, args: &[Datum], kind: SizeKind) -> Result<Datum> {
    match oid_arg(args, 0) {
        Some(oid) => size_of(ctx, oid, kind),
        None => Ok(Datum::Null),
    }
}

// ---------------------------------------------------------------- sequences

fn sequence_parameters(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    let Some(seq) = ctx.env.snapshot().sequence(oid) else {
        return Err(errors::cache_lookup_failed("sequence", oid));
    };
    Ok(Datum::Record(vec![
        Datum::Int8(seq.start),
        Datum::Int8(seq.min),
        Datum::Int8(seq.max),
        Datum::Int8(seq.increment),
        Datum::Bool(seq.cycle),
        Datum::Int8(seq.cache),
        Datum::Oid(seq.type_oid),
    ]))
}

fn sequence_last_value(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    if ctx.env.snapshot().sequence(oid).is_none() {
        return Ok(Datum::Null);
    }
    match ctx.session.sequence_state(oid)? {
        Some(state) => Ok(state.last_value.map_or(Datum::Null, Datum::Int8)),
        None => Ok(Datum::Null),
    }
}

/// `pg_get_sequence_data(oid)`: `(last_value bigint, is_called boolean)`; all NULL for a missing sequence.
fn sequence_data(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    let Some(seq) = ctx.env.snapshot().sequence(oid) else {
        return Ok(Datum::Record(vec![Datum::Null, Datum::Null]));
    };
    let (last, called) = match ctx.session.sequence_state(oid)? {
        // A sequence that was never called reports its start value with is_called = false (PG).
        Some(state) => (state.last_value.unwrap_or(state.start_value), state.is_called),
        None => (seq.start, false),
    };
    Ok(Datum::Record(vec![Datum::Int8(last), Datum::Bool(called)]))
}

// ---------------------------------------------------------------- updatability

fn relation_is_updatable(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    let Some((_, _, kind)) = ctx.env.snapshot().class_identity(oid) else {
        return Ok(Datum::Null);
    };
    // Ordinary and partitioned tables accept every DML; views, matviews, indexes, sequences do not.
    let bits = match kind.as_char() {
        b'r' | b'p' => UPDATABLE_BITS,
        _ => 0,
    };
    Ok(Datum::Int4(bits))
}

fn column_is_updatable(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(oid), Some(attnum)) = (oid_arg(args, 0), args.get(1).and_then(Datum::as_i16)) else {
        return Ok(Datum::Null);
    };
    let snap = ctx.env.snapshot();
    let Some((_, _, kind)) = snap.class_identity(oid) else {
        return Ok(Datum::Null);
    };
    let is_table = matches!(kind.as_char(), b'r' | b'p');
    let visible = snap.relation(oid).and_then(|rel| rel.column(attnum)).is_some_and(|col| col.is_visible());
    Ok(Datum::Bool(is_table && visible))
}

// ---------------------------------------------------------------- filenodes and sizes

fn relation_filenode(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    let Some((_, _, kind)) = ctx.env.snapshot().class_identity(oid) else {
        return Ok(Datum::Null);
    };
    if kind.has_storage() {
        Ok(Datum::Oid(oid))
    } else {
        Ok(Datum::Null)
    }
}

/// `pg_relation_size(oid [, fork])`.
fn relation_size_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    // PG validates the fork name before looking at the relation.
    let fork = match args.get(1) {
        None => "main",
        Some(d) => match d.as_str() {
            Some(name) => name,
            None => return Ok(Datum::Null),
        },
    };
    match fork {
        "main" => size_of(ctx, oid, SizeKind::Relation),
        // PGArbor keeps only main forks, so the free-space, visibility and init forks are absent (size 0).
        "fsm" | "vm" | "init" => {
            if ctx.env.snapshot().class(oid).is_none() {
                Ok(Datum::Null)
            } else {
                Ok(Datum::Int8(0))
            }
        }
        _ => Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid fork name")
            .with_hint("Valid fork names are \"main\", \"fsm\", \"vm\", and \"init\".")),
    }
}

/// `pg_database_size(name | oid)`: the sum of the sizes of every storage object of the current database.
fn database_size(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    let snap = ctx.env.snapshot();
    let db_oid = if proc.arg_types.first().copied() == Some(NAME_TYPE_OID) {
        let Some(name) = text_arg(args, 0) else {
            return Ok(Datum::Null);
        };
        match snap.database_by_name(name) {
            Some(db) => db.oid,
            None => return Err(errors::database_does_not_exist(name)),
        }
    } else {
        let Some(oid) = oid_arg(args, 0) else {
            return Ok(Datum::Null);
        };
        match snap.database(oid) {
            Some(db) => db.oid,
            None => return Ok(Datum::Null),
        }
    };
    if db_oid != ctx.env.database() {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "database size is only available for the current database",
        ));
    }
    let data = snap.data();
    let storage = data
        .relations
        .values()
        .filter(|rel| rel.kind.has_storage())
        .map(|rel| rel.oid)
        .chain(data.indexes.keys().copied())
        .chain(data.sequences.keys().copied());
    let mut total: i64 = 0;
    for oid in storage {
        let size = ctx.session.relation_size(oid, SizeKind::Relation)?;
        total = total
            .checked_add(size)
            .ok_or_else(|| PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?;
    }
    Ok(Datum::Int8(total))
}

fn tablespace_location(args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    // Zero means the database's default tablespace (pg_default for database postgres).
    let oid = if oid == 0 { DEFAULT_TABLESPACE_OID } else { oid };
    if oid == DEFAULT_TABLESPACE_OID || oid == GLOBAL_TABLESPACE_OID {
        return Ok(Datum::text(""));
    }
    Err(PgError::new(
        SqlState::UNDEFINED_FILE,
        format!("could not stat file \"pg_tblspc/{oid}\": No such file or directory"),
    ))
}

// ---------------------------------------------------------------- index access method properties

/// AM-wide properties (`pg_indexam_has_property`). Only btree is reported; other access methods have
/// no property data in PGArbor and yield NULL.
fn am_property(am: Oid, prop: &str) -> Option<bool> {
    if am != BTREE_AM_OID {
        return None;
    }
    match prop {
        "can_order" | "can_unique" | "can_multi_col" | "can_exclude" | "can_include" => Some(true),
        _ => None,
    }
}

/// Index-wide properties (`pg_index_has_property`, and `attno = 0` of the column variant).
fn index_wide_property(am: Oid, prop: &str) -> Option<bool> {
    if am != BTREE_AM_OID {
        return None;
    }
    match prop {
        "clusterable" | "index_scan" | "bitmap_scan" | "backward_scan" => Some(true),
        _ => None,
    }
}

/// One index attribute as the column properties see it.
#[derive(Clone, Copy)]
struct KeyPart {
    /// False for INCLUDE columns.
    key: bool,
    descending: bool,
    nulls_first: bool,
}

/// Column properties (`pg_index_column_has_property`) following PG `amutils.c` for btree.
fn column_property(am: Oid, part: KeyPart, prop: &str) -> Option<bool> {
    if am != BTREE_AM_OID {
        return None;
    }
    match prop {
        "asc" => part.key.then_some(!part.descending),
        "desc" => part.key.then_some(part.descending),
        "nulls_first" => part.key.then_some(part.nulls_first),
        "nulls_last" => part.key.then_some(!part.nulls_first),
        "orderable" => Some(part.key),
        "distance_orderable" => Some(false),
        "returnable" => Some(true),
        "search_array" | "search_nulls" => part.key.then_some(true),
        _ => None,
    }
}

/// Access method of an index OID; `None` when the OID is not an index. System indexes carry no
/// `pg_index` row here and are treated as btree.
fn index_am(ctx: &CatalogCallCtx, oid: Oid) -> Option<Oid> {
    let snap = ctx.env.snapshot();
    let (_, _, kind) = snap.class_identity(oid)?;
    if kind.as_char() != b'i' {
        return None;
    }
    Some(snap.index(oid).map_or(BTREE_AM_OID, |idx| idx.method.am_oid()))
}

fn indexam_has_property(args: &[Datum]) -> Result<Datum> {
    let (Some(am), Some(prop)) = (oid_arg(args, 0), text_arg(args, 1)) else {
        return Ok(Datum::Null);
    };
    Ok(am_property(am, prop).map_or(Datum::Null, Datum::Bool))
}

fn index_has_property(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(oid), Some(prop)) = (oid_arg(args, 0), text_arg(args, 1)) else {
        return Ok(Datum::Null);
    };
    let Some(am) = index_am(ctx, oid) else {
        return Ok(Datum::Null);
    };
    Ok(index_wide_property(am, prop).map_or(Datum::Null, Datum::Bool))
}

fn index_column_has_property(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(oid), Some(colno), Some(prop)) =
        (oid_arg(args, 0), args.get(1).and_then(Datum::as_i32), text_arg(args, 2))
    else {
        return Ok(Datum::Null);
    };
    let Some(am) = index_am(ctx, oid) else {
        return Ok(Datum::Null);
    };
    if colno < 0 {
        return Ok(Datum::Null);
    }
    if colno == 0 {
        return Ok(index_wide_property(am, prop).map_or(Datum::Null, Datum::Bool));
    }
    let snap = ctx.env.snapshot();
    // System indexes have no `pg_index` row, so their column properties are unknown (NULL).
    let Some(idx) = snap.index(oid) else {
        return Ok(Datum::Null);
    };
    let colno = colno as usize;
    if colno > idx.keys.len() {
        return Ok(Datum::Null);
    }
    let key = &idx.keys[colno - 1];
    let part = KeyPart {
        key: colno <= idx.n_key_columns as usize,
        descending: key.descending,
        nulls_first: key.nulls_first,
    };
    Ok(column_property(am, part, prop).map_or(Datum::Null, Datum::Bool))
}

/// `_pg_index_position(index oid, attnum)`: 1-based position of the attnum in `indkey`, NULL if absent.
fn index_position(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(oid), Some(attnum)) = (oid_arg(args, 0), args.get(1).and_then(Datum::as_i16)) else {
        return Ok(Datum::Null);
    };
    let Some(idx) = ctx.env.snapshot().index(oid) else {
        return Ok(Datum::Null);
    };
    match idx.keys.iter().position(|key| key.column == Some(attnum)) {
        Some(pos) => Ok(Datum::Int4((pos + 1) as i32)),
        None => Ok(Datum::Null),
    }
}

// ---------------------------------------------------------------- collations

/// `pg_collation_actual_version(oid)`: every collation of PGArbor uses the builtin provider, which has
/// no version (NULL). Unknown collation OIDs raise 42704.
fn collation_actual_version(args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0) else {
        return Ok(Datum::Null);
    };
    if oid == DEFAULT_COLLATION_OID || crate::builtin::builtin_collation(oid).is_some() {
        return Ok(Datum::Null);
    }
    Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("collation with OID {oid} does not exist")))
}

// ---------------------------------------------------------------- input validation

/// `Ok(None)` when `value` is valid input for the type, `Ok(Some(err))` with the input error otherwise.
/// Type-name errors and resource / internal / unsupported-feature errors are raised.
fn input_error(ctx: &CatalogCallCtx, value: &str, type_name: &str) -> Result<Option<PgError>> {
    let ty = names::parse_type_string(ctx.env, type_name)?;
    let io_ctx = IoCtx::new(ctx.env, ctx.io_config);
    match io::input(&io_ctx, ty, value) {
        Ok(_) => Ok(None),
        Err(err) if is_hard_error(&err) => Err(err),
        Err(err) => Ok(Some(err)),
    }
}

fn is_hard_error(err: &PgError) -> bool {
    matches!(err.sqlstate.class(), "XX" | "53" | "54" | "57" | "58" | "0A")
}

fn input_is_valid(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(value), Some(type_name)) = (text_arg(args, 0), text_arg(args, 1)) else {
        return Ok(Datum::Null);
    };
    Ok(Datum::Bool(input_error(ctx, value, type_name)?.is_none()))
}

fn input_error_info(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let (Some(value), Some(type_name)) = (text_arg(args, 0), text_arg(args, 1)) else {
        return Ok(Datum::Null);
    };
    let fields = match input_error(ctx, value, type_name)? {
        None => vec![Datum::Null, Datum::Null, Datum::Null, Datum::Null],
        Some(err) => vec![
            Datum::Text(err.message.clone()),
            err.detail.clone().map_or(Datum::Null, Datum::Text),
            err.hint.clone().map_or(Datum::Null, Datum::Text),
            Datum::text(err.sqlstate.code()),
        ],
    };
    Ok(Datum::Record(fields))
}

// ---------------------------------------------------------------- information_schema helpers

/// `_pg_truetypid(pg_attribute, pg_type)` / `_pg_truetypmod(...)`: base type and typmod of an attribute,
/// following domains through the type environment. The attribute row carries `atttypid` at position 2
/// and `atttypmod` at position 5 (pg_attribute column order); the type row is not needed because
/// `registry::base_type_and_typmod` follows domains itself. `None` for NULL input.
fn true_type(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Option<(Oid, i32)>> {
    if args.iter().take(2).any(Datum::is_null) {
        return Ok(None);
    }
    let Some(Datum::Record(attr)) = args.first() else {
        return Err(PgError::internal("_pg_truetypid: first argument must be a pg_attribute row"));
    };
    let atttypid = attr
        .get(2)
        .and_then(Datum::as_oid)
        .ok_or_else(|| PgError::internal("_pg_truetypid: pg_attribute row lacks atttypid"))?;
    let atttypmod = attr
        .get(5)
        .and_then(Datum::as_i32)
        .ok_or_else(|| PgError::internal("_pg_truetypmod: pg_attribute row lacks atttypmod"))?;
    Ok(Some(registry::base_type_and_typmod(ctx.env, atttypid, atttypmod)))
}

// ---------------------------------------------------------------- set-returning helpers

/// `pg_get_keywords()`: one row per keyword, ordered by word, from the PG 18 keyword table.
fn keyword_rows() -> Result<Vec<Datum>> {
    let mut keywords: Vec<(&str, u8, bool, &str)> = Vec::new();
    for line in KEYWORDS_CSV.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let mut cols = line.splitn(4, ',');
        let (Some(word), Some(catcode), Some(barelabel), Some(catdesc)) =
            (cols.next(), cols.next(), cols.next(), cols.next())
        else {
            return Err(PgError::internal(format!("malformed pg_keywords row: {line}")));
        };
        let code = match catcode.as_bytes() {
            [c] => *c,
            _ => return Err(PgError::internal(format!("malformed pg_keywords category: {line}"))),
        };
        let bare = match barelabel {
            "t" => true,
            "f" => false,
            _ => return Err(PgError::internal(format!("malformed pg_keywords barelabel: {line}"))),
        };
        keywords.push((word, code, bare, catdesc));
    }
    keywords.sort_by(|a, b| a.0.cmp(b.0));
    Ok(keywords
        .into_iter()
        .map(|(word, code, bare, catdesc)| {
            let baredesc = if bare { "can be bare label" } else { "requires AS" };
            Datum::Record(vec![
                Datum::text(word),
                Datum::Char(code),
                Datum::Bool(bare),
                Datum::text(catdesc),
                Datum::text(baredesc),
            ])
        })
        .collect())
}

/// `pg_options_to_table(text[])`: `name=value` elements split at the first `=`; a missing value is NULL.
/// NULL elements are skipped (PG `untransformRelOptions`).
fn options_rows(args: &[Datum]) -> Result<Vec<Datum>> {
    let arr = match args.first() {
        None | Some(Datum::Null) => return Ok(Vec::new()),
        Some(Datum::Array(arr)) => arr,
        Some(other) => {
            return Err(PgError::internal(format!(
                "pg_options_to_table: expected text[], got {}",
                other.variant_name()
            )))
        }
    };
    let mut rows = Vec::with_capacity(arr.items.len());
    for item in &arr.items {
        let text = match item {
            Datum::Null => continue,
            Datum::Text(s) => s.as_str(),
            other => {
                return Err(PgError::internal(format!(
                    "pg_options_to_table: expected text elements, got {}",
                    other.variant_name()
                )))
            }
        };
        rows.push(match text.split_once('=') {
            Some((name, value)) => Datum::Record(vec![Datum::text(name), Datum::text(value)]),
            None => Datum::Record(vec![Datum::text(text), Datum::Null]),
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::catalog_data::PROCS;
    use pgarbor_types::{Array, IoConfig};

    fn proc_named(name: &str) -> &'static BuiltinProc {
        PROCS.iter().find(|p| p.name == name).expect("proc is in PROCS")
    }

    fn call_named(env: &CatalogEnv, name: &str, args: &[Datum]) -> Result<Datum> {
        let io_config = IoConfig::default();
        let ctx = CatalogCallCtx { env, session: &NullSessionInfo, io_config: &io_config };
        call(&ctx, proc_named(name), args)
    }

    #[test]
    fn keywords_have_pg_categories_sorted() {
        let rows = keyword_rows().expect("keyword table parses");
        assert_eq!(rows.len(), 494);
        let decimal = rows
            .iter()
            .find(|r| matches!(r, Datum::Record(f) if matches!(&f[0], Datum::Text(w) if w == "decimal")))
            .expect("decimal is a keyword");
        assert!(matches!(decimal, Datum::Record(f) if matches!(
            &f[..],
            [_, Datum::Char(b'C'), Datum::Bool(true), Datum::Text(d), Datum::Text(b)]
                if d == "unreserved (cannot be function or type name)" && b == "can be bare label"
        )));
    }

    #[test]
    fn options_split_name_and_value() {
        let arr = Datum::Array(Box::new(Array::from_vec(25, vec![Datum::text("a=1"), Datum::text("b")])));
        let rows = options_rows(&[arr]).expect("text[] input");
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Datum::Record(f) if matches!(
            &f[..], [Datum::Text(n), Datum::Text(v)] if n == "a" && v == "1"
        )));
        assert!(matches!(&rows[1], Datum::Record(f) if matches!(
            &f[..], [Datum::Text(n), Datum::Null] if n == "b"
        )));
    }

    #[test]
    fn input_validity_and_error_info() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let valid = call_named(&env, "pg_input_is_valid", &[Datum::text("42"), Datum::text("int4")]);
        assert!(matches!(valid, Ok(Datum::Bool(true))));
        let invalid = call_named(&env, "pg_input_is_valid", &[Datum::text("abc"), Datum::text("int4")]);
        assert!(matches!(invalid, Ok(Datum::Bool(false))));
        match call_named(&env, "pg_input_error_info", &[Datum::text("abc"), Datum::text("int4")]) {
            Ok(Datum::Record(f)) => assert!(matches!(&f[3], Datum::Text(c) if c == "22P02")),
            _ => panic!("pg_input_error_info must return a record"),
        }
    }

    #[test]
    fn unknown_type_name_still_raises() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        match call_named(&env, "pg_input_is_valid", &[Datum::text("1"), Datum::text("nosuchtype")]) {
            Err(e) => assert_eq!(e.sqlstate.code(), "42704"),
            Ok(_) => panic!("unknown type must raise"),
        }
    }

    #[test]
    fn unknown_relation_size_is_null() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let v = call_named(&env, "pg_relation_size", &[Datum::Oid(999_999_999)]);
        assert!(matches!(v, Ok(Datum::Null)));
    }

    #[test]
    fn table_is_updatable_with_all_dml_bits() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let v = call_named(&env, "pg_relation_is_updatable", &[Datum::Oid(1259), Datum::Bool(true)]);
        assert!(matches!(v, Ok(Datum::Int4(28))));
    }

    #[test]
    fn tablespace_location_for_default_is_empty() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let v = call_named(&env, "pg_tablespace_location", &[Datum::Oid(0)]);
        assert!(matches!(v, Ok(Datum::Text(s)) if s.is_empty()));
    }
}

