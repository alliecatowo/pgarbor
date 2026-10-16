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
use pgarbor_catalog::errors::cannot_alter_type_column_uses_it;
use pgarbor_catalog::FollowUp;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_core::sequence::SequenceParams;
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::{Datum, TypeEnv};

use crate::constraints;
use crate::context::{ExecContext, SessionEnv};
use crate::ctas;
use crate::error;
use crate::eval;
use crate::fk;
use crate::index_maint;
use crate::rowstore::{self, HeapCursor};
use crate::runtime::{Executor, Runtime};
use crate::tabledesc;

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
/// whose table matches `rewrite_table_oid`; any other `RewriteTable` rebuilds its rows and indexes
/// without column rewrites.
pub fn run_followups(
    cx: &mut ExecContext,
    follow_ups: &[FollowUp],
    rewrite_table_oid: Option<Oid>,
    rewrites: &[ColumnRewrite],
) -> Result<()> {
    for fu in follow_ups {
        match fu {
            // These two take the `ExecContext` itself, so they run outside an `Executor`.
            FollowUp::PopulateMatView { relation } => {
                ctas::populate_matview(&mut *cx, *relation)?;
            }
            FollowUp::RewriteTable { table } => {
                let rw: &[ColumnRewrite] = if rewrite_table_oid == Some(*table) { rewrites } else { &[] };
                rewrite_table(&mut *cx, *table, rw)?;
            }
            other => with_executor(&mut *cx, |ex| run_one(ex, other))?,
        }
    }
    Ok(())
}

/// Rewrite all rows of `table` (module docs, "rewrite_table") and rebuild its indexes.
pub fn rewrite_table(cx: &mut ExecContext, table: Oid, rewrites: &[ColumnRewrite]) -> Result<()> {
    with_executor(cx, |ex| rewrite_table_in(ex, table, rewrites))
}

/// Run `f` inside an `Executor` over a throw-away `Runtime`.
fn with_executor<T>(cx: &mut ExecContext, f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
    let mut rt = Runtime::default();
    let mut ex = Executor::new(&mut rt, cx);
    f(&mut ex)
}

/// Execute one follow-up that needs only the executor (not `PopulateMatView` / `RewriteTable`).
fn run_one(ex: &mut Executor, fu: &FollowUp) -> Result<()> {
    match fu {
        FollowUp::BuildIndex { index, table } => {
            let td = tabledesc::build_table_desc(ex.se(), *table)?;
            let idx = td.index(*index).ok_or_else(|| {
                PgError::internal(format!("index {index} of relation {table} is not a maintained index"))
            })?;
            index_maint::build_index(ex, &td, idx)?;
        }
        FollowUp::ValidateNotNull { table, attnum } => {
            let td = tabledesc::build_table_desc(ex.se(), *table)?;
            let col = td.col(*attnum).ok_or_else(|| {
                PgError::internal(format!("attnum {attnum} of relation {table} does not exist"))
            })?;
            let slot = slot_index(*attnum)?;
            let mut cursor = HeapCursor::new(*table);
            let mut counter: u32 = 0;
            while let Some((_rowid, row)) = cursor.next(ex, &td, None)? {
                ex.tick(&mut counter)?;
                let is_null = match row.get(slot) {
                    Some(d) => d.is_null(),
                    None => true,
                };
                if is_null {
                    return Err(error::column_contains_nulls(&td.schema, &td.name, &col.name));
                }
            }
        }
        FollowUp::ValidateCheck { table, constraint } => {
            let td = tabledesc::build_table_desc(ex.se(), *table)?;
            let check = td.checks.iter().find(|c| c.oid == *constraint).ok_or_else(|| {
                PgError::internal(format!("check constraint {constraint} of relation {table} is not enforced"))
            })?;
            let mut cursor = HeapCursor::new(*table);
            let mut counter: u32 = 0;
            while let Some((_rowid, row)) = cursor.next(ex, &td, None)? {
                ex.tick(&mut counter)?;
                if eval::eval_bool3(ex, &check.expr, &row)? == Some(false) {
                    return Err(error::check_violated_by_existing(&td.schema, &td.name, &check.name));
                }
            }
        }
        FollowUp::ValidateForeignKey { table, constraint } => {
            fk::validate_foreign_key(ex, *table, *constraint)?;
        }
        FollowUp::DropRelationData { relation } => {
            rowstore::delete_relation_data(ex, *relation)?;
        }
        FollowUp::CreateSequenceState { sequence } => {
            let params = sequence_params(ex.se(), *sequence)?;
            ex.txn().seq_create(*sequence, &params)?;
        }
        FollowUp::DropSequenceState { sequence } => {
            ex.txn().seq_drop(*sequence)?;
        }
        FollowUp::RestartSequence { sequence, value } => {
            ex.txn().seq_restart(*sequence, *value)?;
        }
        FollowUp::RequireEmptyTable { table, attnum, type_oid } => {
            let td = tabledesc::build_table_desc(ex.se(), *table)?;
            let mut cursor = HeapCursor::new(*table);
            if cursor.next(ex, &td, None)?.is_some() {
                let col = td.col(*attnum).ok_or_else(|| {
                    PgError::internal(format!("attnum {attnum} of relation {table} does not exist"))
                })?;
                let type_name = match ex.se().env.type_info(*type_oid) {
                    Some(t) => t.name.into_owned(),
                    None => return Err(PgError::internal(format!("cache lookup failed for type {type_oid}"))),
                };
                let qualified = format!("{}.{}", td.name, col.name);
                return Err(cannot_alter_type_column_uses_it(&type_name, &qualified));
            }
        }
        FollowUp::PopulateMatView { .. } | FollowUp::RewriteTable { .. } => {
            return Err(PgError::internal("follow-up must run outside an executor"));
        }
    }
    Ok(())
}

/// One rewrite action with its expression already bound.
enum RewriteStep {
    Using { attnum: i16, expr: BoundExpr },
    /// `None` = the column has no default in the new catalog: NULL.
    Default { attnum: i16, expr: Option<BoundExpr> },
    Generated { attnum: i16, expr: BoundExpr },
}

fn rewrite_table_in(ex: &mut Executor, table: Oid, rewrites: &[ColumnRewrite]) -> Result<()> {
    // Bind every default / generation expression of the NEW catalog once.
    let mut steps: Vec<RewriteStep> = Vec::with_capacity(rewrites.len());
    for rw in rewrites {
        match rw {
            ColumnRewrite::Using { attnum, expr } => {
                // The USING expression is bound over the table (rt_index 1): make it executable over the old row.
                let expr = tabledesc::columns_to_input_refs(expr.clone())?;
                steps.push(RewriteStep::Using { attnum: *attnum, expr });
            }
            ColumnRewrite::FromDefault { attnum } => {
                let expr = column_expr(ex.se(), table, *attnum)?;
                steps.push(RewriteStep::Default { attnum: *attnum, expr });
            }
            ColumnRewrite::Generated { attnum } => {
                let expr = column_expr(ex.se(), table, *attnum)?.ok_or_else(|| {
                    PgError::internal(format!("generated column {attnum} of relation {table} has no expression"))
                })?;
                steps.push(RewriteStep::Generated { attnum: *attnum, expr });
            }
        }
    }

    let td = tabledesc::build_table_desc(ex.se(), table)?;
    let mut cursor = HeapCursor::new(table);
    let mut counter: u32 = 0;
    while let Some((rowid, old)) = cursor.next(ex, &td, None)? {
        ex.tick(&mut counter)?;
        let mut new_row: Vec<Datum> = old.clone();
        for step in &steps {
            match step {
                RewriteStep::Using { attnum, expr } => {
                    let v = eval::eval(ex, expr, &old)?;
                    set_slot(&mut new_row, *attnum, v)?;
                }
                RewriteStep::Default { attnum, expr } => {
                    let v = match expr {
                        Some(e) => eval::eval(ex, e, &old)?,
                        None => Datum::Null,
                    };
                    set_slot(&mut new_row, *attnum, v)?;
                }
                RewriteStep::Generated { .. } => {}
            }
        }
        // Generated columns see the rewritten row (computed after all other slots).
        for step in &steps {
            if let RewriteStep::Generated { attnum, expr } = step {
                let v = eval::eval(ex, expr, &new_row)?;
                set_slot(&mut new_row, *attnum, v)?;
            }
        }
        constraints::enforce(ex, &td, &new_row)?;
        rowstore::write_row(ex, &td, rowid, &new_row)?;
    }

    for idx in &td.indexes {
        rowstore::delete_relation_data(ex, idx.oid)?;
    }
    for idx in &td.indexes {
        index_maint::build_index(ex, &td, idx)?;
    }
    Ok(())
}

/// Parameters of a sequence from the snapshot of the session environment.
fn sequence_params(se: &SessionEnv, oid: Oid) -> Result<SequenceParams> {
    let seq = se.env.snapshot().sequence(oid).ok_or_else(|| error::relation_vanished(oid))?;
    Ok(SequenceParams {
        name: seq.name.clone(),
        increment: seq.increment,
        min_value: seq.min,
        max_value: seq.max,
        start: seq.start,
        cache: seq.cache,
        cycle: seq.cycle,
    })
}

/// Bound expression of the default (or generation expression) of column `attnum` of `rel` in the
/// session's catalog, executable over a table row. `None` when the column has no default.
fn column_expr(se: &SessionEnv, rel: Oid, attnum: i16) -> Result<Option<BoundExpr>> {
    let relation = se.env.snapshot().relation(rel).ok_or_else(|| error::relation_vanished(rel))?;
    let sql = match relation.columns.iter().find(|c| c.attnum == attnum).and_then(|c| c.default.as_ref()) {
        Some(d) => d.expr.sql.clone(),
        None => return Ok(None),
    };
    let (name, ty) = relation
        .columns
        .iter()
        .find(|c| c.attnum == attnum)
        .map(|c| (c.name.clone(), c.type_id))
        .ok_or_else(|| PgError::internal(format!("attnum {attnum} of relation {rel} does not exist")))?;
    let bound = tabledesc::bind_column_default_text(se, &sql, rel, &name, ty)?;
    Ok(Some(tabledesc::columns_to_input_refs(bound)?))
}

/// 0-based slot of a 1-based attnum.
fn slot_index(attnum: i16) -> Result<usize> {
    if attnum < 1 {
        return Err(PgError::internal(format!("invalid attribute number {attnum}")));
    }
    Ok(attnum as usize - 1)
}

/// Store `value` into the slot of `attnum` (1-based).
fn set_slot(row: &mut [Datum], attnum: i16, value: Datum) -> Result<()> {
    let slot = slot_index(attnum)?;
    match row.get_mut(slot) {
        Some(target) => {
            *target = value;
            Ok(())
        }
        None => Err(PgError::internal(format!("rewrite target attnum {attnum} out of range"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices};
    use crate::memtxn::MemTxn;
    use crate::txn::ExecTxn;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    fn seq_params(name: &str) -> SequenceParams {
        SequenceParams { name: name.to_string(), increment: 1, min_value: 1, max_value: i64::MAX, start: 1, cache: 1, cycle: false }
    }

    #[test]
    fn restart_then_drop_sequence_state() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(7u64);
        let mut txn = MemTxn::new();
        txn.seq_create(20000, &seq_params("s"))?;
        {
            let se = SessionEnv::new(env.clone(), IoConfig::default(), &guc, &services, &rng, 0, 0);
            let mut cx = ExecContext::new(&mut txn, se);
            run_followups(&mut cx, &[FollowUp::RestartSequence { sequence: 20000, value: 5 }], None, &[])?;
        }
        assert_eq!(txn.seq_state(20000)?.last_value, 5);
        {
            let se = SessionEnv::new(env.clone(), IoConfig::default(), &guc, &services, &rng, 0, 0);
            let mut cx = ExecContext::new(&mut txn, se);
            run_followups(&mut cx, &[FollowUp::DropSequenceState { sequence: 20000 }], None, &[])?;
        }
        assert!(txn.seq_state(20000).is_err());
        Ok(())
    }

    #[test]
    fn create_sequence_state_for_unknown_sequence_is_vanished() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(7u64);
        let mut txn = MemTxn::new();
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let err = match run_followups(&mut cx, &[FollowUp::CreateSequenceState { sequence: 999_999 }], None, &[]) {
            Ok(()) => return Err(PgError::internal("expected a cache lookup failure")),
            Err(e) => e,
        };
        assert_eq!(err.sqlstate.code(), "XX000");
        assert!(err.message.contains("cache lookup failed for relation 999999"));
        Ok(())
    }

    #[test]
    fn slot_index_is_one_based() -> Result<()> {
        assert_eq!(slot_index(1)?, 0);
        assert_eq!(slot_index(3)?, 2);
        assert!(slot_index(0).is_err());
        assert!(slot_index(-1).is_err());
        Ok(())
    }

    #[test]
    fn set_slot_writes_by_attnum_and_rejects_out_of_range() -> Result<()> {
        let mut row = vec![Datum::Null, Datum::Null];
        set_slot(&mut row, 2, Datum::Int4(7))?;
        assert_eq!(row[1], Datum::Int4(7));
        assert_eq!(row[0], Datum::Null);
        assert!(set_slot(&mut row, 3, Datum::Int4(1)).is_err());
        assert!(set_slot(&mut row, 0, Datum::Int4(1)).is_err());
        Ok(())
    }
}
