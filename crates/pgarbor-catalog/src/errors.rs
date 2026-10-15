//! Every user-facing catalog error and notice, worded exactly as PostgreSQL 18.6 (each constructor
//! was checked against the oracle; the comment shows the oracle output). ALL catalog leases build
//! errors through these constructors so wording stays consistent. Fully implemented, frozen (v1).
//!
//! Conventions:
//! - Object names in messages are the bare (unquoted) names PG prints inside `"..."`.
//! - Object descriptions (`table t`, `view zz_scratch.v`, `column b of table t`,
//!   `default value for column id of table t`, `constraint t_pkey on table t`) come from
//!   `crate::describe` (PG `getObjectDescription`): schema-qualified only when the object is not
//!   visible in the search path.
//! - Notices are `PgError`s with severity Notice (SQLSTATE as shown by PG: `00000` for the
//!   "does not exist, skipping" family, the duplicate-object code for "already exists, skipping").
use pgarbor_common::{PgError, Severity, SqlState};

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

fn notice(state: SqlState, msg: String) -> PgError {
    PgError::new(state, msg).with_severity(Severity::Notice)
}

// ---------------------------------------------------------------- relations

/// 42P01 `relation "nosuch" does not exist` / `relation "nos.x" does not exist`.
pub fn relation_does_not_exist(schema: Option<&str>, name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{}\" does not exist", qualified(schema, name)))
}

/// Kind-specific "does not exist" used by DROP/ALTER <kind>:
/// `table "nosuch" does not exist` (42P01), `view ...` (42P01), `materialized view ...` (42P01),
/// `sequence ...` (42P01), `index "nosuch" does not exist` (**42704**), `type "nosuch" does not exist`
/// (42704; also used for DROP DOMAIN), `foreign table ...` (42P01).
pub fn kind_does_not_exist(kind_word: &str, schema: Option<&str>, name: &str) -> PgError {
    let state = match kind_word {
        "index" | "type" => SqlState::UNDEFINED_OBJECT,
        _ => SqlState::UNDEFINED_TABLE,
    };
    PgError::new(state, format!("{kind_word} \"{}\" does not exist", qualified(schema, name)))
}

/// NOTICE 00000 `table "nosuch" does not exist, skipping` (DROP ... IF EXISTS; also `index`,
/// `view`, `sequence`, `type`, `materialized view`). When the SCHEMA is missing PG instead says
/// [`schema_does_not_exist_skipping`].
pub fn kind_does_not_exist_skipping(kind_word: &str, schema: Option<&str>, name: &str) -> PgError {
    notice(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("{kind_word} \"{}\" does not exist, skipping", qualified(schema, name)),
    )
}

/// 42P07 `relation "t" already exists`.
pub fn relation_already_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_TABLE, format!("relation \"{name}\" already exists"))
}

/// NOTICE 42P07 `relation "t" already exists, skipping` (CREATE ... IF NOT EXISTS).
pub fn relation_already_exists_skipping(name: &str) -> PgError {
    notice(SqlState::DUPLICATE_TABLE, format!("relation \"{name}\" already exists, skipping"))
}

/// 42809 `"t" is not a view` + HINT `Use DROP TABLE to remove a table.`
/// `expected_word`: `table`, `view`, `materialized view`, `index`, `sequence`, `composite type`,
/// `foreign table`. `actual`: the object's real kind, giving the hint
/// (`DROP TABLE`/`a table`, `DROP VIEW`/`a view`, `DROP MATERIALIZED VIEW`/`a materialized view`,
/// `DROP INDEX`/`an index`, `DROP SEQUENCE`/`a sequence`, `DROP TYPE`/`a type` for composite types,
/// `DROP FOREIGN TABLE`/`a foreign table`).
/// Oracle: `"zz_i" is not a table` HINT `Use DROP INDEX to remove an index.`;
/// `"zz_c" is not a table` HINT `Use DROP TYPE to remove a type.`
pub fn wrong_object_type_for_drop(name: &str, expected_word: &str, actual: crate::model::RelKind) -> PgError {
    use crate::model::RelKind::*;
    let (cmd, what) = match actual {
        Table | PartitionedTable => ("DROP TABLE", "a table"),
        View => ("DROP VIEW", "a view"),
        MatView => ("DROP MATERIALIZED VIEW", "a materialized view"),
        Index | PartitionedIndex => ("DROP INDEX", "an index"),
        Sequence => ("DROP SEQUENCE", "a sequence"),
        CompositeType => ("DROP TYPE", "a type"),
        ForeignTable => ("DROP FOREIGN TABLE", "a foreign table"),
        Toast => ("DROP TABLE", "a table"),
    };
    let article_word = if expected_word.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not {article_word} {expected_word}"))
        .with_hint(format!("Use {cmd} to remove {what}."))
}

/// 42809 `"v" is not a table` (generic, no hint): used outside DROP, e.g. `TRUNCATE v`,
/// `CREATE INDEX ON v` (PG: `cannot create index on relation "v"` + DETAIL
/// `This operation is not supported for views.` — use [`operation_not_supported_for`] there).
pub fn not_a(name: &str, expected_word: &str) -> PgError {
    let article = if expected_word.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not {article} {expected_word}"))
}

/// 42809 `ALTER action ADD COLUMN cannot be performed on relation "v"` + DETAIL
/// `This operation is not supported for views.` (`kind_plural`: `views`, `sequences`, `indexes`,
/// `materialized views`, `composite types`, `tables`, `partitioned tables`, `foreign tables`).
pub fn alter_action_not_supported(action: &str, rel: &str, kind_plural: &str) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("ALTER action {action} cannot be performed on relation \"{rel}\""))
        .with_detail(format!("This operation is not supported for {kind_plural}."))
}

/// 42809 `cannot create index on relation "v"` style (`what` = `cannot create index on relation "v"`)
/// + DETAIL `This operation is not supported for views.`
pub fn operation_not_supported_for(what: &str, kind_plural: &str) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, what.to_string())
        .with_detail(format!("This operation is not supported for {kind_plural}."))
}

/// 42501 `permission denied: "pg_class" is a system catalog` (ALTER/DROP of a system relation).
pub fn system_catalog_permission(name: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied: \"{name}\" is a system catalog"))
}

/// 42501 `permission denied to create "pg_catalog.x"` + DETAIL
/// `System catalog modifications are currently disallowed.`
pub fn system_catalog_create_denied(schema: &str, name: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to create \"{schema}.{name}\""))
        .with_detail("System catalog modifications are currently disallowed.")
}

// ---------------------------------------------------------------- schemas

/// 3F000 `schema "nos" does not exist`.
pub fn schema_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_SCHEMA_NAME, format!("schema \"{name}\" does not exist"))
}

/// NOTICE 00000 `schema "nos" does not exist, skipping` (DROP SCHEMA IF EXISTS, and
/// DROP TABLE IF EXISTS nos.x when the schema is missing).
pub fn schema_does_not_exist_skipping(name: &str) -> PgError {
    notice(SqlState::SUCCESSFUL_COMPLETION, format!("schema \"{name}\" does not exist, skipping"))
}

/// 42P06 `schema "s" already exists`.
pub fn schema_already_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_SCHEMA, format!("schema \"{name}\" already exists"))
}

/// NOTICE 42P06 `schema "s" already exists, skipping`.
pub fn schema_already_exists_skipping(name: &str) -> PgError {
    notice(SqlState::DUPLICATE_SCHEMA, format!("schema \"{name}\" already exists, skipping"))
}

/// 3F000 `no schema has been selected to create in`.
pub fn no_schema_selected() -> PgError {
    PgError::new(SqlState::INVALID_SCHEMA_NAME, "no schema has been selected to create in")
}

/// 42939 `unacceptable schema name "pg_x"` + DETAIL `The prefix "pg_" is reserved for system schemas.`
pub fn reserved_schema_name(name: &str) -> PgError {
    PgError::new(SqlState::RESERVED_NAME, format!("unacceptable schema name \"{name}\""))
        .with_detail("The prefix \"pg_\" is reserved for system schemas.")
}

// ---------------------------------------------------------------- types

/// 42704 `type "nosuchtype" does not exist`.
pub fn type_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("type \"{name}\" does not exist"))
}

/// 42710 `type "mood" already exists`. With `relation_hint` (the name clashes with a relation's
/// rowtype) adds HINT `A relation has an associated type of the same name, so you must use a name
/// that doesn't conflict with any existing type.`
pub fn type_already_exists(name: &str, relation_hint: bool) -> PgError {
    let e = PgError::new(SqlState::DUPLICATE_OBJECT, format!("type \"{name}\" already exists"));
    if relation_hint {
        e.with_hint("A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.")
    } else {
        e
    }
}

/// 42710 `enum label "a" already exists`.
pub fn enum_label_already_exists(label: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("enum label \"{label}\" already exists"))
}

/// NOTICE 42710 `enum label "a" already exists, skipping`.
pub fn enum_label_already_exists_skipping(label: &str) -> PgError {
    notice(SqlState::DUPLICATE_OBJECT, format!("enum label \"{label}\" already exists, skipping"))
}

/// 22023 `"zz" is not an existing enum label`.
pub fn not_an_existing_enum_label(label: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("\"{label}\" is not an existing enum label"))
}

/// 42602 `invalid enum label "<label>"` + DETAIL `Labels must be 63 bytes or less.` (oracle)
pub fn invalid_enum_label(label: &str) -> PgError {
    PgError::new(SqlState::INVALID_NAME, format!("invalid enum label \"{label}\""))
        .with_detail("Labels must be 63 bytes or less.")
}

// ---------------------------------------------------------------- columns

/// 42703 `column "nosuch" does not exist` (RENAME COLUMN, key lists in PG use other wording).
pub fn column_does_not_exist(col: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{col}\" does not exist"))
}

/// 42703 `column "nosuch" of relation "t" does not exist` (ALTER TABLE DROP/ALTER COLUMN).
pub fn column_of_relation_does_not_exist(col: &str, rel: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{col}\" of relation \"{rel}\" does not exist"))
}

/// NOTICE 00000 `column "nosuch" of relation "t" does not exist, skipping`.
pub fn column_of_relation_does_not_exist_skipping(col: &str, rel: &str) -> PgError {
    notice(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("column \"{col}\" of relation \"{rel}\" does not exist, skipping"),
    )
}

/// 42701 `column "a" of relation "t" already exists`.
pub fn column_already_exists(col: &str, rel: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{col}\" of relation \"{rel}\" already exists"))
}

/// NOTICE 42701 `column "a" of relation "t" already exists, skipping`.
pub fn column_already_exists_skipping(col: &str, rel: &str) -> PgError {
    notice(SqlState::DUPLICATE_COLUMN, format!("column \"{col}\" of relation \"{rel}\" already exists, skipping"))
}

/// 42701 `column "a" specified more than once` (CREATE TABLE).
pub fn column_specified_more_than_once(col: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{col}\" specified more than once"))
}

/// 42703 `column "zz" named in key does not exist`.
pub fn key_column_does_not_exist(col: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{col}\" named in key does not exist"))
}

/// 54011 `tables can have at most 1600 columns` (SQLSTATE too_many_columns).
pub fn too_many_columns() -> PgError {
    PgError::new(SqlState(*b"54011"), "tables can have at most 1600 columns")
}

// ---------------------------------------------------------------- constraints

/// 42704 `constraint "nosuch" of relation "t" does not exist`.
pub fn constraint_does_not_exist(con: &str, rel: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("constraint \"{con}\" of relation \"{rel}\" does not exist"))
}

/// NOTICE 00000 `constraint "nosuch" of relation "t" does not exist, skipping`.
pub fn constraint_does_not_exist_skipping(con: &str, rel: &str) -> PgError {
    notice(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("constraint \"{con}\" of relation \"{rel}\" does not exist, skipping"),
    )
}

/// 42710 `constraint "c" for relation "t" already exists`.
pub fn constraint_already_exists(con: &str, rel: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("constraint \"{con}\" for relation \"{rel}\" already exists"))
}

/// 42P16 `multiple primary keys for table "g" are not allowed`.
pub fn multiple_primary_keys(table: &str) -> PgError {
    PgError::new(SqlState::INVALID_TABLE_DEFINITION, format!("multiple primary keys for table \"{table}\" are not allowed"))
}

/// 42830 `there is no unique constraint matching given keys for referenced table "t"`.
pub fn no_unique_constraint_matching(table: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_FOREIGN_KEY,
        format!("there is no unique constraint matching given keys for referenced table \"{table}\""),
    )
}

/// 42704 `there is no primary key for referenced table "zz_t"` (`REFERENCES t` without columns).
pub fn no_primary_key_for_referenced_table(table: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("there is no primary key for referenced table \"{table}\""))
}

/// 42804 `foreign key constraint "f_x_fkey" cannot be implemented` + DETAIL
/// `Key columns "x" of the referencing table and "a" of the referenced table are of incompatible types: text and integer.`
pub fn fk_cannot_be_implemented(con: &str, col: &str, ref_col: &str, col_type: &str, ref_type: &str) -> PgError {
    PgError::new(SqlState::DATATYPE_MISMATCH, format!("foreign key constraint \"{con}\" cannot be implemented")).with_detail(
        format!(
            "Key columns \"{col}\" of the referencing table and \"{ref_col}\" of the referenced table are of incompatible types: {col_type} and {ref_type}."
        ),
    )
}

/// 42830 `number of referencing and referenced columns for foreign key disagree`.
pub fn fk_column_count_mismatch() -> PgError {
    PgError::new(SqlState::INVALID_FOREIGN_KEY, "number of referencing and referenced columns for foreign key disagree")
}

// ---------------------------------------------------------------- dependencies / DROP

/// 2BP01 `cannot drop table t because other objects depend on it` + DETAIL one line per dependent
/// (`view v depends on table t`, newline-separated, in PG's reporting order) + HINT
/// `Use DROP ... CASCADE to drop the dependent objects too.`
/// PG caps the DETAIL at 100 lines (`MAX_REPORTED_DEPS`); beyond that the last line is
/// `and N other objects (see server log for list)`. `obj` is the dropped object's description
/// (`table t`, `column b of table t`, `schema zz_scratch`).
pub fn dependent_objects_still_exist(obj: &str, detail_lines: &[String]) -> PgError {
    const MAX: usize = 100;
    let mut lines: Vec<String> = detail_lines.iter().take(MAX).cloned().collect();
    if detail_lines.len() > MAX {
        lines.push(format!("and {} other objects (see server log for list)", detail_lines.len() - MAX));
    }
    PgError::new(SqlState::DEPENDENT_OBJECTS_STILL_EXIST, format!("cannot drop {obj} because other objects depend on it"))
        .with_detail(lines.join("\n"))
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
}

/// 2BP01 `cannot drop index t_pkey because constraint t_pkey on table t requires it` + HINT
/// `You can drop constraint t_pkey on table t instead.` (dropping an INTERNAL dependent directly;
/// also `cannot drop type zz_t3 because table zz_t3 requires it` / `You can drop table zz_t3 instead.`)
pub fn required_by(obj: &str, owner: &str) -> PgError {
    PgError::new(SqlState::DEPENDENT_OBJECTS_STILL_EXIST, format!("cannot drop {obj} because {owner} requires it"))
        .with_hint(format!("You can drop {owner} instead."))
}

/// 2BP01 `cannot drop schema pg_catalog because it is required by the database system`.
pub fn required_by_system(obj: &str) -> PgError {
    PgError::new(
        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
        format!("cannot drop {obj} because it is required by the database system"),
    )
}

/// The CASCADE notice. One object → NOTICE `drop cascades to table zz_scratch.t` (no DETAIL).
/// Several → NOTICE `drop cascades to 2 other objects` + DETAIL lines `drop cascades to view v`
/// (capped at 100 lines like [`dependent_objects_still_exist`], then
/// `and N other objects (see server log for list)`). None for zero objects.
pub fn drop_cascades_notice(descs: &[String]) -> Option<PgError> {
    const MAX: usize = 100;
    match descs.len() {
        0 => None,
        1 => Some(notice(SqlState::SUCCESSFUL_COMPLETION, format!("drop cascades to {}", descs[0]))),
        n => {
            let mut lines: Vec<String> = descs.iter().take(MAX).map(|d| format!("drop cascades to {d}")).collect();
            if n > MAX {
                lines.push(format!("and {} other objects (see server log for list)", n - MAX));
            }
            Some(
                notice(SqlState::SUCCESSFUL_COMPLETION, format!("drop cascades to {n} other objects"))
                    .with_detail(lines.join("\n")),
            )
        }
    }
}

/// 0A000 `cannot alter type of a column used by a view or rule` + DETAIL
/// `rule _RETURN on view v depends on column "b"`.
pub fn cannot_alter_type_used_by_view(view: &str, col: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot alter type of a column used by a view or rule")
        .with_detail(format!("rule _RETURN on view {view} depends on column \"{col}\""))
}

// ---------------------------------------------------------------- sequences

/// 22023 `INCREMENT must not be zero`.
pub fn sequence_increment_zero() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "INCREMENT must not be zero")
}

/// 22023 `MINVALUE (10) must be less than MAXVALUE (5)`.
pub fn sequence_min_not_less_than_max(min: i64, max: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("MINVALUE ({min}) must be less than MAXVALUE ({max})"))
}

/// 22023 `START value (0) cannot be less than MINVALUE (1)`.
pub fn sequence_start_below_min(start: i64, min: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("START value ({start}) cannot be less than MINVALUE ({min})"))
}

/// 22023 `START value (100) cannot be greater than MAXVALUE (10)`.
pub fn sequence_start_above_max(start: i64, max: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("START value ({start}) cannot be greater than MAXVALUE ({max})"))
}

/// 22023 `RESTART value (0) cannot be less than MINVALUE (1)`.
pub fn sequence_restart_below_min(v: i64, min: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("RESTART value ({v}) cannot be less than MINVALUE ({min})"))
}

/// 22023 `RESTART value (100) cannot be greater than MAXVALUE (10)`.
pub fn sequence_restart_above_max(v: i64, max: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("RESTART value ({v}) cannot be greater than MAXVALUE ({max})"))
}

/// 22023 `sequence type must be smallint, integer, or bigint`.
pub fn sequence_bad_type() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "sequence type must be smallint, integer, or bigint")
}

/// 22023 `MAXVALUE (100000) is out of range for sequence data type smallint` (`which` = `MAXVALUE`
/// or `MINVALUE`; type name as `format_type`).
pub fn sequence_bound_out_of_range(which: &str, v: i64, type_name: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("{which} ({v}) is out of range for sequence data type {type_name}"),
    )
}

/// 22023 `CACHE (0) must be greater than zero`.
pub fn sequence_cache_not_positive(v: i64) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("CACHE ({v}) must be greater than zero"))
}

// ---------------------------------------------------------------- roles / databases / functions

/// 42704 `role "nosuch" does not exist`.
pub fn role_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist"))
}

/// NOTICE 00000 `role "nosuch" does not exist, skipping`.
pub fn role_does_not_exist_skipping(name: &str) -> PgError {
    notice(SqlState::SUCCESSFUL_COMPLETION, format!("role \"{name}\" does not exist, skipping"))
}

/// 42710 `role "postgres" already exists`.
pub fn role_already_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("role \"{name}\" already exists"))
}

/// 3D000 `database "nosuchdb" does not exist` (FATAL at connection time; ERROR in DDL).
pub fn database_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_CATALOG_NAME, format!("database \"{name}\" does not exist"))
}

/// 42883 `function nosuch(integer) does not exist` (`signature` from `resolve::func::signature_string`).
pub fn function_does_not_exist(signature: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function {signature} does not exist"))
}

/// NOTICE 00000 `function nosuch(pg_catalog.int4) does not exist, skipping` — NOTE PG prints the
/// argument types with `format_type_be_qualified`-like internal names here (oracle-verified).
pub fn function_does_not_exist_skipping(signature_internal: &str) -> PgError {
    notice(SqlState::SUCCESSFUL_COMPLETION, format!("function {signature_internal} does not exist, skipping"))
}

/// 42723 `function "f" already exists with same argument types` (oracle: the bare quoted NAME, not a
/// signature; procedures too: `function "p" already exists ...`).
pub fn function_already_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_FUNCTION, format!("function \"{name}\" already exists with same argument types"))
}

/// 42725 `function name "f" is not unique` + HINT `Specify the argument list to select the function unambiguously.`
pub fn function_name_not_unique(name: &str) -> PgError {
    PgError::new(SqlState::AMBIGUOUS_FUNCTION, format!("function name \"{name}\" is not unique"))
        .with_hint("Specify the argument list to select the function unambiguously.")
}

/// 42883 `could not find a function named "f"` (DROP FUNCTION f / DROP ROUTINE f without arguments
/// when no function of that name exists; the procedure variant says `could not find a procedure named`).
pub fn no_function_named(kind_word: &str, name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("could not find a {kind_word} named \"{name}\""))
}

/// 42809 `p(integer) is not a function` / `f(integer) is not a procedure` / `... is not an aggregate`
/// (`sig` = `name(argtypes)` via format_type; `expected` = `function`, `procedure`, `aggregate`).
pub fn wrong_routine_kind(sig: &str, expected: &str) -> PgError {
    let article = if expected.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is not {article} {expected}"))
}

/// 42P13 `cannot change name of input parameter "a"` + HINT `Use DROP FUNCTION f(integer) first.`
pub fn cannot_change_param_name(param: &str, signature: &str) -> PgError {
    PgError::new(SqlState(*b"42P13"), format!("cannot change name of input parameter \"{param}\""))
        .with_hint(format!("Use DROP FUNCTION {signature} first."))
}

/// 42704 `access method "foo" does not exist`.
pub fn access_method_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("access method \"{name}\" does not exist"))
}

/// 42704 `collation "x" for encoding "UTF8" does not exist`.
pub fn collation_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("collation \"{name}\" for encoding \"UTF8\" does not exist"))
}

/// 22023 `invalid value for parameter "search_path": "<value>"` + DETAIL `List syntax is invalid.`
pub fn invalid_search_path(value: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"search_path\": \"{value}\""))
        .with_detail("List syntax is invalid.")
}

/// XX000 `cache lookup failed for relation 12345` (internal: an OID that should exist does not).
pub fn cache_lookup_failed(what: &str, oid: pgarbor_common::Oid) -> PgError {
    PgError::internal(format!("cache lookup failed for {what} {oid}"))
}

// ---------------------------------------------------------------- privileges / ownership

/// 42501 `must be owner of table t` (`kind_word`: `table`, `view`, `sequence`, `index`, `schema`,
/// `type`, `function`, `database`, `materialized view`, `domain`, `role`...).
pub fn must_be_owner(kind_word: &str, name: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be owner of {kind_word} {name}"))
}

/// 42501 `permission denied for table t` (`kind_word` as PG `aclcheck_error`: `table`, `schema`,
/// `sequence`, `function`, `type`, `database`, `language`, `relation` ...).
pub fn permission_denied_for(kind_word: &str, name: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied for {kind_word} {name}"))
}

/// 42501 `permission denied to create role` + DETAIL `Only roles with the CREATEROLE attribute may create roles.`
pub fn permission_denied_create_role() -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to create role")
        .with_detail("Only roles with the CREATEROLE attribute may create roles.")
}

/// 42501 `permission denied to <action>` (`action` = `drop role`, `alter role`, `grant role`, `rename
/// role`, ...); the caller adds DETAIL via `with_detail` (e.g. `Only roles with the CREATEROLE
/// attribute and the ADMIN option on role "r" may drop this role.`).
pub fn permission_denied_to(action: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to {action}"))
}

/// 42939 `role name "pg_x" is reserved` + DETAIL `Role names starting with "pg_" are reserved.`
pub fn reserved_role_name(name: &str) -> PgError {
    PgError::new(SqlState::RESERVED_NAME, format!("role name \"{name}\" is reserved"))
        .with_detail("Role names starting with \"pg_\" are reserved.")
}

/// 55006 `current user cannot be dropped`.
pub fn cannot_drop_current_user() -> PgError {
    PgError::new(SqlState::OBJECT_IN_USE, "current user cannot be dropped")
}

/// 55006 `session user cannot be dropped`.
pub fn cannot_drop_session_user() -> PgError {
    PgError::new(SqlState::OBJECT_IN_USE, "session user cannot be dropped")
}

/// 2BP01 `role "r" cannot be dropped because some objects depend on it` + DETAIL one line per
/// object (`owner of table t`, `privileges for schema s`; capped like [`dependent_objects_still_exist`]).
pub fn role_cannot_be_dropped(role: &str, detail_lines: &[String]) -> PgError {
    const MAX: usize = 100;
    let mut lines: Vec<String> = detail_lines.iter().take(MAX).cloned().collect();
    if detail_lines.len() > MAX {
        lines.push(format!("and {} other objects (see server log for list)", detail_lines.len() - MAX));
    }
    PgError::new(
        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
        format!("role \"{role}\" cannot be dropped because some objects depend on it"),
    )
    .with_detail(lines.join("\n"))
}

/// 0LP01 `role "a" is a member of role "b"` (circular membership: GRANT b TO a while b is already
/// a member of a).
pub fn circular_membership(role: &str, member: &str) -> PgError {
    PgError::new(SqlState(*b"0LP01"), format!("role \"{role}\" is a member of role \"{member}\""))
}

/// 22023 PGArbor guard: `password is not hashed` (catalog stores verifiers only).
pub fn password_not_hashed() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "password is not hashed")
}

// ---------------------------------------------------------------- concurrency

/// 40001 `could not serialize access due to concurrent catalog change` + HINT `Retry the statement.`
/// (PGArbor: the catalog changed between the statement's snapshot and its DDL lock).
pub fn concurrent_catalog_change() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent catalog change")
        .with_hint("Retry the statement.")
}

// ---------------------------------------------------------------- columns / tables (ALTER)

/// 0A000 `cannot rename system column "ctid"`.
pub fn cannot_rename_system_column(col: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot rename system column \"{col}\""))
}

/// 0A000 `cannot alter system column "xmin"`.
pub fn cannot_alter_system_column(col: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot alter system column \"{col}\""))
}

/// 42701 PG 18 `column name "ctid" conflicts with a system column name` (CREATE TABLE / ADD COLUMN
/// using `tableoid`, `xmin`, `cmin`, `xmax`, `cmax`, `ctid`).
pub fn column_conflicts_with_system_column(col: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_COLUMN, format!("column name \"{col}\" conflicts with a system column name"))
}

/// 55000 `column "id" of relation "t" is already an identity column`.
pub fn column_already_identity(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("column \"{col}\" of relation \"{rel}\" is already an identity column"),
    )
}

/// 55000 `column "id" of relation "t" is not an identity column`.
pub fn column_not_identity(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("column \"{col}\" of relation \"{rel}\" is not an identity column"),
    )
}

/// NOTICE 00000 `column "id" of relation "t" is not an identity column, skipping` (DROP IDENTITY IF EXISTS).
pub fn column_not_identity_skipping(col: &str, rel: &str) -> PgError {
    notice(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("column \"{col}\" of relation \"{rel}\" is not an identity column, skipping"),
    )
}

/// 42P16 `column "id" of relation "t" already has a default value` (ADD GENERATED ... AS IDENTITY on
/// a column with a default) - PG: `column "x" of relation "t" already has a default value`.
pub fn column_already_has_default(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("column \"{col}\" of relation \"{rel}\" already has a default value"),
    )
}

/// 42P16 `column "id" of relation "t" is an identity column` + HINT `Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.`
/// (SET/DROP DEFAULT or DROP NOT NULL on an identity column).
pub fn column_is_identity(col: &str, rel: &str, hint: Option<&str>) -> PgError {
    let e = PgError::new(SqlState::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION, format!("column \"{col}\" of relation \"{rel}\" is an identity column"));
    let e = PgError { sqlstate: SqlState::INVALID_TABLE_DEFINITION, ..e };
    match hint {
        Some(h) => e.with_hint(h),
        None => e,
    }
}

/// 42P16 `column "id" is in a primary key` (DROP NOT NULL).
pub fn column_in_primary_key(col: &str) -> PgError {
    PgError::new(SqlState::INVALID_TABLE_DEFINITION, format!("column \"{col}\" is in a primary key"))
}

/// 42P16 `column "x" of relation "t" is a generated column` (+ optional HINT).
pub fn column_is_generated(col: &str, rel: &str, hint: Option<&str>) -> PgError {
    let e = PgError::new(SqlState::INVALID_TABLE_DEFINITION, format!("column \"{col}\" of relation \"{rel}\" is a generated column"));
    match hint {
        Some(h) => e.with_hint(h),
        None => e,
    }
}

/// 42809 `e is not a domain` (oracle: NO quotes; `name` as `format_type_be`, schema-qualified when
/// not visible).
pub fn not_a_domain(name: &str) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{name} is not a domain"))
}

/// 42809 `"t" is not an enum` (ALTER TYPE ADD VALUE on a non-enum).
pub fn not_an_enum(name: &str) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an enum"))
}

/// 42809 `constraint "c" of relation "t" is not a foreign key constraint`.
pub fn constraint_not_foreign_key(con: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("constraint \"{con}\" of relation \"{rel}\" is not a foreign key constraint"),
    )
}

/// 42809 `constraint "c" of relation "t" is not a foreign key or check constraint` (VALIDATE CONSTRAINT).
pub fn constraint_not_fk_or_check(con: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("constraint \"{con}\" of relation \"{rel}\" is not a foreign key or check constraint"),
    )
}

/// 42704 `data type text has no default operator class for access method "btree"` + HINT
/// `You must specify an operator class for the index or define a default operator class for the data type.`
pub fn no_default_opclass(type_name: &str, method: &str) -> PgError {
    PgError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("data type {type_name} has no default operator class for access method \"{method}\""),
    )
    .with_hint("You must specify an operator class for the index or define a default operator class for the data type.")
}

// ---------------------------------------------------------------- views / functions / sequences (ALTER)

/// 42P16 `cannot change name of view column "a" to "b"` + HINT
/// `Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead.`
pub fn cannot_change_view_column_name(old: &str, new: &str) -> PgError {
    PgError::new(SqlState::INVALID_TABLE_DEFINITION, format!("cannot change name of view column \"{old}\" to \"{new}\""))
        .with_hint("Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead.")
}

/// 42P16 `cannot drop columns from view`.
pub fn cannot_drop_view_columns() -> PgError {
    PgError::new(SqlState::INVALID_TABLE_DEFINITION, "cannot drop columns from view")
}

/// 42P16 `cannot change data type of view column "a" from integer to text`.
pub fn cannot_change_view_column_type(col: &str, from: &str, to: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("cannot change data type of view column \"{col}\" from {from} to {to}"),
    )
}

/// 42P13 `cannot change return type of existing function` + DETAIL (e.g. `Row type defined by OUT
/// parameters is different.`) + HINT `Use DROP FUNCTION f(integer) first.` (`signature` as in
/// [`function_already_exists`]).
pub fn cannot_change_return_type(signature: &str, detail: Option<&str>) -> PgError {
    let e = PgError::new(SqlState(*b"42P13"), "cannot change return type of existing function")
        .with_hint(format!("Use DROP FUNCTION {signature} first."));
    match detail {
        Some(d) => e.with_detail(d),
        None => e,
    }
}

/// 55000 `sequence must have same owner as table it is linked to`.
pub fn sequence_owner_mismatch() -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "sequence must have same owner as table it is linked to")
}

/// 55000 `sequence must be in same schema as table it is linked to`.
pub fn sequence_schema_mismatch() -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "sequence must be in same schema as table it is linked to")
}

/// 55000 `cannot change ownership of identity sequence` + DETAIL
/// `Sequence "t_id_seq" is linked to table "t".` (ALTER SEQUENCE OWNER/SET SCHEMA/OWNED BY on identity sequence).
pub fn cannot_alter_identity_sequence(what: &str, seq: &str, table: &str) -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot {what} of identity sequence"))
        .with_detail(format!("Sequence \"{seq}\" is linked to table \"{table}\"."))
}

/// 42501 `permission denied: "pg_class" is a system catalog` (alias of [`system_catalog_permission`]
/// kept for call-site clarity when renaming/altering builtin objects).
pub fn cannot_rename_system_object(name: &str) -> PgError {
    PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied: \"{name}\" is a system catalog"))
}

/// 2BP01 variant for objects that cannot be dropped at all because a built-in depends on them:
/// `cannot drop <obj> because it is required by the database system` (alias of [`required_by_system`]).
pub fn pinned_object(obj: &str) -> PgError {
    required_by_system(obj)
}

// ---------------------------------------------------------------- CREATE TABLE validation

/// 42P16 `column "a" has pseudo-type record` (oracle: also `unknown`, `anyelement`, `void` ...).
pub fn column_has_pseudo_type(col: &str, type_name: &str) -> PgError {
    PgError::new(SqlState::INVALID_TABLE_DEFINITION, format!("column \"{col}\" has pseudo-type {type_name}"))
}

/// 42601 for conflicting column options. `what` completes `both ... specified` / `multiple ...`:
/// - `both_default_and_identity` -> `both default and identity specified for column "a" of table "t"`
/// - `both_default_and_generation` -> `both default and generation expression specified for column "a" of table "t"`
/// - `both_identity_and_generation` -> `both identity and generation expression specified for column "a" of table "t"`
/// - `multiple_defaults` -> `multiple default values specified for column "a" of table "t"`
pub fn column_option_conflict(what: &str, col: &str, table: &str) -> PgError {
    let msg = match what {
        "both_default_and_identity" => "both default and identity specified",
        "both_default_and_generation" => "both default and generation expression specified",
        "both_identity_and_generation" => "both identity and generation expression specified",
        _ => "multiple default values specified",
    };
    PgError::new(SqlState::SYNTAX_ERROR, format!("{msg} for column \"{col}\" of table \"{table}\""))
}

/// 42710 `check constraint "c" already exists` (two CHECKs with one name inside CREATE TABLE).
pub fn check_constraint_already_exists(name: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("check constraint \"{name}\" already exists"))
}

/// 42701 `column "a" appears twice in unique constraint` (`what`: `unique constraint`,
/// `primary key constraint`, `exclusion constraint`).
pub fn column_appears_twice(col: &str, what: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{col}\" appears twice in {what}"))
}

/// 42701 `foreign key referenced-columns list must not contain duplicates`.
pub fn fk_duplicate_referenced_columns() -> PgError {
    PgError::new(SqlState::DUPLICATE_COLUMN, "foreign key referenced-columns list must not contain duplicates")
}

/// 54011 `cannot use more than 32 columns in an index` (PG limit INDEX_MAX_KEYS 32).
pub fn too_many_index_columns() -> PgError {
    PgError::new(SqlState(*b"54011"), "cannot use more than 32 columns in an index")
}

/// 0A000 `cannot drop system column "ctid"`.
pub fn cannot_drop_system_column(col: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot drop system column \"{col}\""))
}

/// 0A000 `cannot alter type of a column used by a generated column` + DETAIL
/// `Column "a" is used by generated column "d".`
pub fn cannot_alter_type_used_by_generated(col: &str, generated: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot alter type of a column used by a generated column")
        .with_detail(format!("Column \"{col}\" is used by generated column \"{generated}\"."))
}

/// 42P16 `column "b" of relation "t" is not a generated column`.
pub fn column_not_generated(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("column \"{col}\" of relation \"{rel}\" is not a generated column"),
    )
}

/// NOTICE 00000 `column "b" of relation "t" is not a generated column, skipping`.
pub fn column_not_generated_skipping(col: &str, rel: &str) -> PgError {
    notice(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("column \"{col}\" of relation \"{rel}\" is not a generated column, skipping"),
    )
}

/// 42P16 `column "a" of relation "t" must be declared NOT NULL before identity can be added`.
pub fn identity_requires_not_null(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("column \"{col}\" of relation \"{rel}\" must be declared NOT NULL before identity can be added"),
    )
}

/// 22023 `identity column type must be smallint, integer, or bigint`.
pub fn identity_bad_type() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "identity column type must be smallint, integer, or bigint")
}

/// 42804 `column data type integer can only have storage PLAIN`.
pub fn bad_storage_for_type(type_name: &str) -> PgError {
    PgError::new(SqlState::DATATYPE_MISMATCH, format!("column data type {type_name} can only have storage PLAIN"))
}

/// 22023 `unrecognized parameter "x"`.
pub fn unrecognized_parameter(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("unrecognized parameter \"{name}\""))
}

/// 22023 `value 5 out of bounds for option "fillfactor"` DETAIL `Valid values are between "10" and "100".`
pub fn option_out_of_bounds(value: &str, option: &str, min: &str, max: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("value {value} out of bounds for option \"{option}\""))
        .with_detail(format!("Valid values are between \"{min}\" and \"{max}\"."))
}

/// 0A000 `cannot alter type "c" because column "u.c" uses it` (composite type change while a
/// non-empty table has a column of that type; `column` is `table.column`).
pub fn cannot_alter_type_column_uses_it(type_name: &str, column: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot alter type \"{type_name}\" because column \"{column}\" uses it"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wording() {
        assert_eq!(relation_does_not_exist(Some("nos"), "x").message, "relation \"nos.x\" does not exist");
        let e = dependent_objects_still_exist("table t", &["view v depends on table t".into()]);
        assert_eq!(e.sqlstate.code(), "2BP01");
        assert_eq!(e.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
        let n = drop_cascades_notice(&["view v".into(), "view v2".into()]).unwrap();
        assert_eq!(n.message, "drop cascades to 2 other objects");
        assert_eq!(n.detail.as_deref(), Some("drop cascades to view v\ndrop cascades to view v2"));
        let w = wrong_object_type_for_drop("zz_i", "table", crate::model::RelKind::Index);
        assert_eq!(w.message, "\"zz_i\" is not a table");
        assert_eq!(w.hint.as_deref(), Some("Use DROP INDEX to remove an index."));
        assert_eq!(kind_does_not_exist("index", None, "x").sqlstate.code(), "42704");
    }
}
