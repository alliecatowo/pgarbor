//! PG-exact error and notice constructors of the executor. Fully implemented, frozen (v1).
//!
//! Every message below was verified against the pinned PostgreSQL 18.6 oracle (see the byte-exact
//! fixtures in each function's docs). Workers MUST build executor errors with these functions
//! instead of formatting messages themselves, so wording stays in one place.
//!
//! ## `Failing row contains (...)` / `Key (...)=(...)` rendering
//! PG source: `executor/execMain.c` `ExecBuildSlotValueDescription`, `access/index/genam.c`
//! `BuildIndexValueDescription`. Rules, implemented by [`row_description`] / [`key_description`]:
//! * values are rendered with the column type's text OUTPUT function under the session `IoConfig`
//!   (`numeric(5,2)` 1.5 -> `1.50`, timestamptz `2020-01-01 00:00:00+00`, text raw: NO quoting, so
//!   `'a b c'` renders as `a b c` and `'it''s'` as `it's`);
//! * SQL NULL renders as `null` (lower case);
//! * dropped columns are skipped; a VIRTUAL generated column renders `virtual`;
//! * every value longer than 64 BYTES is cut at a character boundary <= 64 bytes and `...` appended;
//! * values are joined with `, ` and wrapped in `(` `)`.
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::context::IoCtx;
use pgarbor_types::{io, Datum, TypeId};

/// Maximum number of bytes of one value in `Failing row contains` / `Key` details (PG `maxfieldlen`).
pub const DETAIL_MAX_FIELD_LEN: usize = 64;

/// One column of a row being described (see [`row_description`]).
// CONTRACT
#[derive(Clone, Copy, Debug)]
pub struct DescCol {
    pub ty: TypeId,
    /// Dropped column: not rendered at all.
    pub dropped: bool,
    /// VIRTUAL generated column: rendered as `virtual`.
    pub virtual_generated: bool,
}

fn clip(val: &str) -> String {
    if val.len() <= DETAIL_MAX_FIELD_LEN {
        return val.to_string();
    }
    let mut end = DETAIL_MAX_FIELD_LEN;
    while end > 0 && !val.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &val[..end])
}

fn render_one(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    if d.is_null() {
        return Ok("null".to_string());
    }
    Ok(clip(&io::output(ctx, ty, d)?))
}

/// `(v1, v2, ...)` description of a whole table row (module docs). `cols` and `row` are parallel
/// (`row.len() >= cols.len()`; extra row slots are ignored).
pub fn row_description(ctx: &IoCtx, cols: &[DescCol], row: &[Datum]) -> Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(cols.len());
    for (i, c) in cols.iter().enumerate() {
        if c.dropped {
            continue;
        }
        if c.virtual_generated {
            parts.push("virtual".to_string());
            continue;
        }
        let null = Datum::Null;
        let d = row.get(i).unwrap_or(&null);
        parts.push(render_one(ctx, c.ty, d)?);
    }
    Ok(format!("({})", parts.join(", ")))
}

/// `Key (a, b)=(1, 2)` text for index/foreign-key details: column names joined by `, ` in
/// parentheses, `=`, then the rendered values `(v1, v2)`. `names[i]` may be an expression text for
/// expression indexes (`lower(name)`), exactly as PG prints it.
pub fn key_description(ctx: &IoCtx, names: &[String], types: &[TypeId], values: &[Datum]) -> Result<String> {
    let mut vals: Vec<String> = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        let ty = types.get(i).copied().unwrap_or(TypeId::TEXT);
        vals.push(render_one(ctx, ty, v)?);
    }
    Ok(format!("Key ({})=({})", names.join(", "), vals.join(", ")))
}

/// 23502. Oracle: `null value in column "name" of relation "t" violates not-null constraint`
/// DETAIL `Failing row contains (2, null, 1.50, y, null).` (note the trailing period), schema/table/
/// column fields set. `row_desc` comes from [`row_description`].
pub fn not_null_violation(schema: &str, table: &str, column: &str, row_desc: &str) -> PgError {
    PgError::new(
        SqlState::NOT_NULL_VIOLATION,
        format!("null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"),
    )
    .with_detail(format!("Failing row contains {row_desc}."))
    .with_table(schema, table)
    .with_column(column)
}

/// 23514. Oracle: `new row for relation "t" violates check constraint "t_v_check"` DETAIL
/// `Failing row contains (5, b, -1.00, q, null).` constraint field set.
pub fn check_violation(schema: &str, table: &str, constraint: &str, row_desc: &str) -> PgError {
    PgError::new(
        SqlState::CHECK_VIOLATION,
        format!("new row for relation \"{table}\" violates check constraint \"{constraint}\""),
    )
    .with_detail(format!("Failing row contains {row_desc}."))
    .with_table(schema, table)
    .with_constraint(constraint)
}

/// 23505. Oracle: `duplicate key value violates unique constraint "t_pkey"` DETAIL
/// `Key (id)=(1) already exists.`; `key_desc` comes from [`key_description`]. `constraint` is the
/// CONSTRAINT name when the index backs one, else the index name (PG reports the index name).
pub fn unique_violation(schema: &str, table: &str, constraint: &str, key_desc: &str) -> PgError {
    PgError::new(
        SqlState::UNIQUE_VIOLATION,
        format!("duplicate key value violates unique constraint \"{constraint}\""),
    )
    .with_detail(format!("{key_desc} already exists."))
    .with_table(schema, table)
    .with_constraint(constraint)
}

/// 23505 raised while BUILDING a unique index over existing rows (CREATE UNIQUE INDEX, ADD PRIMARY
/// KEY/UNIQUE). Oracle: `could not create unique index "dx"` DETAIL `Key (a)=(1) is duplicated.`
pub fn unique_build_violation(schema: &str, table: &str, index: &str, key_desc: &str) -> PgError {
    PgError::new(SqlState::UNIQUE_VIOLATION, format!("could not create unique index \"{index}\""))
        .with_detail(format!("{key_desc} is duplicated."))
        .with_table(schema, table)
        .with_constraint(index)
}

/// 23503, child side (INSERT/UPDATE of the referencing row, or validation of ADD FOREIGN KEY).
/// Oracle: `insert or update on table "c2" violates foreign key constraint "c2_a_b_fkey"` DETAIL
/// `Key (a, b)=(1, 2) is not present in table "p".` TABLE field = the REFERENCING table.
pub fn fk_missing_parent(schema: &str, fk_table: &str, constraint: &str, key_desc: &str, pk_table: &str) -> PgError {
    PgError::new(
        SqlState::FOREIGN_KEY_VIOLATION,
        format!("insert or update on table \"{fk_table}\" violates foreign key constraint \"{constraint}\""),
    )
    .with_detail(format!("{key_desc} is not present in table \"{pk_table}\"."))
    .with_table(schema, fk_table)
    .with_constraint(constraint)
}

/// 23503, parent side (DELETE/UPDATE of a referenced row with NO ACTION / RESTRICT).
/// Oracle: `update or delete on table "p" violates foreign key constraint "c2_a_b_fkey" on table
/// "c2"` DETAIL `Key (a, b)=(1, 1) is still referenced from table "c2".` TABLE field = the
/// REFERENCING table (`c2`), not the table being modified.
pub fn fk_still_referenced(
    schema: &str,
    pk_table: &str,
    constraint: &str,
    fk_table: &str,
    key_desc: &str,
) -> PgError {
    PgError::new(
        SqlState::FOREIGN_KEY_VIOLATION,
        format!(
            "update or delete on table \"{pk_table}\" violates foreign key constraint \"{constraint}\" on table \"{fk_table}\""
        ),
    )
    .with_detail(format!("{key_desc} is still referenced from table \"{fk_table}\"."))
    .with_table(schema, fk_table)
    .with_constraint(constraint)
}

/// 23502 raised by `ALTER TABLE ... SET NOT NULL` / ADD PRIMARY KEY over existing NULLs.
/// Oracle: `column "a" of relation "dd3" contains null values`.
pub fn column_contains_nulls(schema: &str, table: &str, column: &str) -> PgError {
    PgError::new(
        SqlState::NOT_NULL_VIOLATION,
        format!("column \"{column}\" of relation \"{table}\" contains null values"),
    )
    .with_table(schema, table)
    .with_column(column)
}

/// 23514 raised by `ALTER TABLE ... ADD CHECK` / VALIDATE CONSTRAINT over violating rows.
/// Oracle: `check constraint "cc2" of relation "dd" is violated by some row`.
pub fn check_violated_by_existing(schema: &str, table: &str, constraint: &str) -> PgError {
    PgError::new(
        SqlState::CHECK_VIOLATION,
        format!("check constraint \"{constraint}\" of relation \"{table}\" is violated by some row"),
    )
    .with_table(schema, table)
    .with_constraint(constraint)
}

/// 21000. Oracle: `ON CONFLICT DO UPDATE command cannot affect row a second time` HINT `Ensure that
/// no rows proposed for insertion within the same command have duplicate constrained values.`
pub fn on_conflict_affects_row_twice() -> PgError {
    PgError::new(SqlState::CARDINALITY_VIOLATION, "ON CONFLICT DO UPDATE command cannot affect row a second time")
        .with_hint("Ensure that no rows proposed for insertion within the same command have duplicate constrained values.")
}

/// 21000. Oracle: `more than one row returned by a subquery used as an expression`.
pub fn scalar_subquery_multiple_rows() -> PgError {
    PgError::new(SqlState::CARDINALITY_VIOLATION, "more than one row returned by a subquery used as an expression")
}

/// 27000 `tuple to be updated was already modified by an operation triggered by the current
/// command` (an FK cascade action tries to change a row the same command already changed).
/// NOTE: UPDATE/DELETE ... FROM matching a target row several times is NOT an error in PG (first
/// match wins), so nothing raises this for it.
pub fn tuple_already_modified() -> PgError {
    PgError::new(
        SqlState(*b"27000"),
        "tuple to be updated was already modified by an operation triggered by the current command",
    )
    .with_hint("Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.")
}

/// 2201W. Oracle: `LIMIT must not be negative`.
pub fn limit_negative() -> PgError {
    PgError::new(SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, "LIMIT must not be negative")
}

/// 2201X. Oracle: `OFFSET must not be negative`.
pub fn offset_negative() -> PgError {
    PgError::new(SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE, "OFFSET must not be negative")
}

/// 55P03 for `SELECT ... FOR UPDATE NOWAIT`: `could not obtain lock on row in relation "t"`.
pub fn row_lock_not_available(table: &str) -> PgError {
    PgError::new(SqlState::LOCK_NOT_AVAILABLE, format!("could not obtain lock on row in relation \"{table}\""))
}

/// 55P03 for `LOCK TABLE ... NOWAIT`: `could not obtain lock on relation "t"`.
pub fn relation_lock_not_available(table: &str) -> PgError {
    PgError::new(SqlState::LOCK_NOT_AVAILABLE, format!("could not obtain lock on relation \"{table}\""))
}

/// Message of [`catalog_retry`].
pub const CATALOG_RETRY_MESSAGE: &str = "could not serialize access due to concurrent catalog change";

/// 40001 `could not serialize access due to concurrent catalog change`: raised by the DDL driver
/// when another transaction changed the catalog after this statement's snapshot (contract
/// `pgarbor-catalog.md`, "Transactions, concurrency and invalidation" step 2). Under READ COMMITTED
/// the embedding session MUST treat it as "re-take the snapshot, re-bind and re-run the statement"
/// (see [`is_catalog_retry`]); under REPEATABLE READ / SERIALIZABLE it is the final error.
pub fn catalog_retry() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, CATALOG_RETRY_MESSAGE)
}

/// True for the error produced by [`catalog_retry`] (also what `CatalogTxn::begin` raises on a
/// version mismatch: same SQLSTATE and message).
pub fn is_catalog_retry(e: &PgError) -> bool {
    e.sqlstate == SqlState::SERIALIZATION_FAILURE && e.message == CATALOG_RETRY_MESSAGE
}

/// 42P01 for a relation OID the plan references but the catalog no longer has (stale plan):
/// `cache lookup failed for relation 123456` (XX000 in PG).
pub fn relation_vanished(oid: Oid) -> PgError {
    PgError::internal(format!("cache lookup failed for relation {oid}"))
}

/// XX000 for a plan node/expression the executor cannot run because the planner left a bind-time
/// node in an executable plan (`Column`, `SubLink`, `Aggref`, `WindowFunc`; plan.rs module docs).
pub fn not_executable(what: &str) -> PgError {
    PgError::internal(format!("plan contains a non-executable node: {what}"))
}

/// 0A000 for user-defined (non-builtin) function calls inside expressions: PGArbor stores SQL
/// functions but does not execute them yet.
pub fn user_function_unsupported(lease: &str, name: &str) -> PgError {
    PgError::unimplemented(lease, format!("calling user-defined function {name}"))
}

/// A WARNING-severity diagnostic (`PgError` with `Severity::Warning`).
pub fn warning(sqlstate: SqlState, message: impl Into<String>) -> PgError {
    PgError::new(sqlstate, message).with_severity(Severity::Warning)
}

/// 57014 raised when `ExecContext::check_interrupts` sees a cancel or statement timeout (the text
/// comes from `pgarbor_core::error`; this helper only exists so tests can build the same value).
pub fn query_canceled_by_user() -> PgError {
    PgError::new(SqlState::QUERY_CANCELED, "canceling statement due to user request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_types::context::IoConfig;

    #[test]
    fn clip_cuts_at_char_boundary() {
        let s = "é".repeat(40); // 80 bytes
        let c = clip(&s);
        assert!(c.ends_with("..."));
        assert!(c.len() <= DETAIL_MAX_FIELD_LEN + 3);
        assert_eq!(clip("abc"), "abc");
    }

    #[test]
    fn row_description_renders_null_and_skips_dropped() {
        let env = pgarbor_types::registry::builtin_env();
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(env, &cfg);
        let cols = [
            DescCol { ty: TypeId::INT4, dropped: false, virtual_generated: false },
            DescCol { ty: TypeId::INT4, dropped: true, virtual_generated: false },
            DescCol { ty: TypeId::TEXT, dropped: false, virtual_generated: false },
            DescCol { ty: TypeId::TEXT, dropped: false, virtual_generated: true },
        ];
        let row = vec![Datum::Int4(2), Datum::Null, Datum::Null, Datum::Null];
        assert_eq!(row_description(&ctx, &cols, &row).unwrap(), "(2, null, virtual)");
    }

    #[test]
    fn catalog_retry_roundtrip() {
        assert!(is_catalog_retry(&catalog_retry()));
        assert!(!is_catalog_retry(&limit_negative()));
    }

    #[test]
    fn unique_detail_shape() {
        let e = unique_violation("public", "t", "t_pkey", "Key (id)=(1)");
        assert_eq!(e.detail.as_deref(), Some("Key (id)=(1) already exists."));
        assert_eq!(e.sqlstate.code(), "23505");
    }
}
