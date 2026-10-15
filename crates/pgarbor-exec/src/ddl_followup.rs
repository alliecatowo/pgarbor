//! Data-level consequences of DDL (lease EXE-34, sonnet): `FollowUp` execution and table rewrites.
//! PG: `commands/tablecmds.c` (`ATRewriteTable`, `ATRewriteTables`), `commands/indexcmds.c`,
//! `catalog/index.c` (`index_build`). Called by the `ddl_*` handlers right after `run_ddl`, with the
//! statement's `DdlOutcome::follow_ups` in order. `cx.se.env` already shows the NEW catalog.
//!
//! | `FollowUp` | action |
//! |---|---|
//! | `BuildIndex{index, table}` | `index_maint::build_index(td, idx)` (unique duplicates -> 23505 `could not create unique index "i"` / `Key (a)=(1) is duplicated.`) |
//! | `ValidateNotNull{table, attnum}` | scan the table; first row with NULL in `attnum` -> `error::column_contains_nulls` (23502) |
//! | `ValidateCheck{table, constraint}` | scan; evaluate the CHECK; FALSE -> `error::check_violated_by_existing` (23514); the catalog marks the constraint validated already |
//! | `ValidateForeignKey{table, constraint}` | `fk::validate_foreign_key` |
//! | `RewriteTable{table}` | [`rewrite_table`] with the `ColumnRewrite`s collected by the handler |
//! | `DropRelationData{relation}` | `rowstore::delete_relation_data(relation)` |
//! | `CreateSequenceState{sequence}` | `ExecTxn::seq_create(oid, params)` with `SequenceParams` from the new catalog row (`snapshot.sequence(oid)`; `start` value is the initial `last_value`, `is_called = false`) |
//! | `DropSequenceState{sequence}` | `seq_drop` |
//! | `RestartSequence{sequence, value}` | `seq_restart(oid, value)` |
//! | `PopulateMatView{relation}` | `ctas::populate_matview(cx, relation)` |
//! | `RequireEmptyTable{table, attnum, type_oid}` | if the table has any row -> `pgarbor_catalog::errors::cannot_alter_type_column_uses_it(..)` (0A000) |
//! Validation scans run under the DDL's relation lock; they read the transaction view and take no row
//! locks. Every loop polls `check_interrupts` per 1024 rows.
//!
//! ## `rewrite_table`
//! ALTER COLUMN TYPE / ADD COLUMN with a volatile default / STORED generated column changes: rewrite
//! EVERY heap row of `table` once. For each row (old image decoded with the OLD column layout = the
//! stored tuple; the tuple is self-describing and positional): start from the old values; for each
//! [`ColumnRewrite`] set slot `attnum - 1`: `Using{expr}` -> `eval_standalone(expr, old_row)` (the USING
//! expression is bound over the OLD columns and already cast to the new type; assignment-cast errors
//! such as 22P02 / 22003 / `column "a" cannot be cast automatically` surface here), `FromDefault` -> the
//! column's default expression of the NEW catalog evaluated per row (`nextval`, `random()` get a value
//! per row), `Generated` -> the generation expression over the row; then write the full new row at the
//! same rowid (`rowstore::write_row`) after `constraints::enforce`; NOT NULL / CHECK failures abort the
//! statement. Finally rebuild ALL indexes of the table: `delete_relation_data(index)` for each, then
//! `index_maint::build_index`. Dropped columns stay NULL slots.
use pgarbor_catalog::FollowUp;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::ExecContext;

/// How a column of a rewritten table obtains its new value (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub enum ColumnRewrite {
    /// `ALTER COLUMN TYPE ... USING expr` (expr over the OLD row, result already of the new type).
    Using { attnum: i16, expr: BoundExpr },
    /// ADD COLUMN with a volatile DEFAULT: evaluate the column default for every existing row.
    FromDefault { attnum: i16 },
    /// STORED generated column added/changed: evaluate its generation expression.
    Generated { attnum: i16 },
}

/// Execute `follow_ups` in order (module docs). `rewrites` applies to every `RewriteTable` among them
/// whose table matches `rewrite_table_oid`.
#[allow(unused_variables)]
pub fn run_followups(
    cx: &mut ExecContext,
    follow_ups: &[FollowUp],
    rewrite_table_oid: Option<Oid>,
    rewrites: &[ColumnRewrite],
) -> Result<()> {
    Err(PgError::unimplemented("EXE-34", "DDL follow-ups (ddl_followup::run_followups)"))
}

/// Rewrite all rows of `table` (module docs, "rewrite_table") and rebuild its indexes.
#[allow(unused_variables)]
pub fn rewrite_table(cx: &mut ExecContext, table: Oid, rewrites: &[ColumnRewrite]) -> Result<()> {
    Err(PgError::unimplemented("EXE-34", "table rewrite (ddl_followup::rewrite_table)"))
}
