//! INSERT / UPDATE / DELETE / CREATE TABLE AS planning (PLN-17, sonnet). PG references:
//! `planner.c` (`inheritance_planner` not needed), `plan/createplan.c` (`create_modifytable_plan`),
//! `rewrite/rewriteHandler.c` (`rewriteTargetListIU`, `rewriteTargetListUD`, `build_column_default`,
//! `ApplyRetrieveRule` n/a), `prep/preptlist.c` (`preprocess_targetlist`, `expand_insert_targetlist`),
//! `parser/analyze.c`, `nodeModifyTable.c` (`ExecInsert`/`ExecUpdate`/`ExecOnConflictUpdate` semantics).
//!
//! Output: `Plan` pieces assembled by `planner::plan_statement`; each function returns the root
//! `PlanNode` (a `ModifyTable`, or for CTAS a plain query node) plus nothing else.
//!
//! ## INSERT (`plan_insert`, `BoundInsert`)
//! * Input node: `InsertSource::Values(rows)` -> `ValuesScan` (cells simplified; sub-links processed;
//!   `SetToDefault{attnum}` cells replaced by the column's default expression `TargetColumn::default` or
//!   `Const(NULL)` of the column type; identity defaults are `nextval(..)` calls and remain per-row);
//!   `InsertSource::Query(q)` -> `planner::plan_query_node`; `DefaultValues` -> a `Result` node (zero
//!   columns, one row).
//! * `InsertSpec.columns = BoundInsert::columns`; `defaults` for every live attnum not in `columns` and
//!   not a stored generated column: `TargetColumn::default` simplified, else `Const(NULL)`; with
//!   `OVERRIDING USER VALUE` the identity columns listed in `columns` are moved to `defaults` (supplied
//!   value discarded); `generated` = stored generated columns' expressions rewritten from
//!   `Column{rt_index: 1, attnum}` to `InputRef(attnum-1)` over the new row. Identity/generated
//!   ALWAYS checks for a non-DEFAULT value were done by the binder for VALUES; for query sources the
//!   planner raises 428C9 `cannot insert a non-DEFAULT value into column "id"` DETAIL `Column "id" is an
//!   identity column defined as GENERATED ALWAYS.` HINT `Use OVERRIDING SYSTEM VALUE to override.` unless
//!   `overriding == Some(System)`.
//! * `ModifyTarget.checks`: each `TargetCheck.expr` rewritten to `InputRef(attnum-1)` form (these are
//!   already executable forms; setrefs leaves `InputRef` alone).
//! * `ON CONFLICT`: `ConflictSpec { arbiter_indexes, any_unique, action }`; `DoUpdate` assignments/where
//!   rewritten over `existing ++ excluded` layout: `Column{rt_index: target_rt}` -> `InputRef(attnum-1)`,
//!   `Column{rt_index: excluded_rt}` -> `InputRef(ncols + attnum-1)`; `SetToDefault` in assignments ->
//!   default expression.
//! * RETURNING: `returning` target entries' expressions over the RETURNING input row (new row,
//!   `ncols` slots): `Column{rt_index: 1, attnum}` -> `InputRef(attnum-1)`; `returning_extra_from = None`.
//!
//! ## UPDATE (`plan_update`)
//! * Input = the join of the target scan (`LogicalScan` with `with_rowid = true`, quals = WHERE conjuncts
//!   on the target) and the `FROM` items via `build::build_from_item` / `join::order_join_tree` +
//!   `pushdown`, WHERE conjuncts distributed; sub-links processed; the input node's row layout MUST be
//!   `target table row (ncols) ++ rowid ++ remaining FROM columns` (project explicitly if the join order
//!   put another relation first: use a `Project` to restore the layout). `rowid_slot = ncols`.
//! * `UpdateSpec.assignments`: `(attnum, value)` per target column per `Assignment` (multi-column
//!   `SET (a,b) = (SELECT ..)`: evaluate the `SubLink{Row}` once per row via a `Project` that adds the row
//!   value and `FieldSelect` slots - or one `SubPlan` per target column when uncorrelated); `SetToDefault`
//!   -> column default; values are bind-time expressions translated by setrefs over the input row.
//!   `generated`: stored generated columns recomputed over the NEW row (`InputRef(attnum-1)` form).
//! * RETURNING over `[new target row] ++ [input slots from rowid_slot+1]`: `returning_extra_from =
//!   Some(ncols + 1)`; `Column{rt_index: target}` -> `InputRef(attnum-1)`, FROM-table columns ->
//!   `InputRef(ncols + k)` where k is the extra-column index.
//! ## DELETE (`plan_delete`): like UPDATE without assignments; `USING` items are the extra columns.
//!
//! ## CREATE TABLE AS (`plan_create_table_as`): `PlanKind::Populate`: the plan root is the query
//! node; `Plan::output` = the new table's columns (names after the column list, types from the query).
//!
//! ## Common
//! * `PlanKind` per statement; data-modifying CTE bodies are planned with the same functions via
//!   [`plan_dml_statement`].
//! * `Plan::deps` includes the target table, referenced sequences (from defaults) and functions.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundCreateTableAs, BoundDelete, BoundInsert, BoundStatement, BoundUpdate};

use crate::context::PlannerState;
use crate::plan::{PlanKind, PlanNode};

/// Plan any DML/CTAS statement; returns the root node and the plan kind.
#[allow(unused_variables)]
pub fn plan_dml_statement(st: &mut PlannerState, stmt: &BoundStatement) -> Result<(PlanNode, PlanKind)> {
    Err(PgError::unimplemented("PLN-17", "DML dispatch (build_dml::plan_dml_statement)"))
}

/// INSERT.
#[allow(unused_variables)]
pub fn plan_insert(st: &mut PlannerState, ins: &BoundInsert) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-17", "INSERT planning (build_dml::plan_insert)"))
}

/// UPDATE.
#[allow(unused_variables)]
pub fn plan_update(st: &mut PlannerState, upd: &BoundUpdate) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-17", "UPDATE planning (build_dml::plan_update)"))
}

/// DELETE.
#[allow(unused_variables)]
pub fn plan_delete(st: &mut PlannerState, del: &BoundDelete) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-17", "DELETE planning (build_dml::plan_delete)"))
}

/// CREATE TABLE AS source query.
#[allow(unused_variables)]
pub fn plan_create_table_as(st: &mut PlannerState, ctas: &BoundCreateTableAs) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-17", "CREATE TABLE AS planning (build_dml::plan_create_table_as)"))
}
