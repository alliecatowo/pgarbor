//! UPDATE and DELETE (SQL-28), plus the pieces INSERT/ON CONFLICT share: SET assignments and
//! RETURNING. PG references: `analyze.c` (`transformUpdateStmt`, `transformDeleteStmt`,
//! `transformUpdateTargetList`, `transformReturningList`), `parse_target.c`
//! (`transformAssignedExpr`, `transformAssignmentIndirection`, `checkInsertTargets`,
//! `ExpandRowReference`), `rewriteHandler.c` (generated/identity rules).
//!
//! ## UPDATE (`bind_update`, sqlparser `Update { table, assignments, from, selection, returning, .. }`)
//! 1. Reject MySQL/other options: `or`, `order_by`, `limit`, `output`, multi-table `table` with joins
//!    (`UPDATE a JOIN b ...` is MySQL: 42601).
//! 2. `ScopeKind::Update`; target from `table.relation` (`TableFactor::Table { name, alias }`) via
//!    `target_table::open_target_table(.., DmlKind::Update, false)`.
//! 3. `FROM a, b JOIN c ...` (`UpdateTableFromKind::AfterSet`/`BeforeSet`): `from::bind_from_clause`
//!    items follow the target in the range table; the target is visible to the FROM items only through
//!    LATERAL? (PG: not at all; sub-selects in FROM cannot reference the target).
//! 4. `SET` assignments: [`bind_assignments`]. WHERE: `bind_boolean(.., ExprCtx::where_(), "WHERE")`
//!    (`WHERE CURRENT OF cursor`: 0A000). RETURNING: [`bind_returning`].
//! 5. Result `BoundStatement::Update(Box<BoundUpdate { target, target_rt: 1, range_table, ctes, from,
//!    assignments, where_, returning }>)`.
//!
//! ## DELETE (`bind_delete`, sqlparser `Delete { from: FromTable, using, selection, returning, .. }`)
//! `tables` (MySQL multi-table) non-empty -> 42601. Target = the single `FromTable::WithFromKeyword`
//! item; `USING a, b` -> like UPDATE's FROM. `BoundStatement::Delete(..)`.
//!
//! ## Assignments ([`bind_assignments`])
//! * `SET col = expr`: column resolved against the TARGET (42703 `column "x" of relation "t" does not
//!   exist`, `diag::column_of_relation_missing`). A qualified target `SET t.c = 1` parses as column `t`
//!   with a field/subscript indirection (oracle): 42703 `column "t" of relation "t" does not exist` with
//!   HINT `SET target columns cannot be qualified with the relation name.`, position at `t`.
//! * `expr` bound with `ExprCtx::update_set()`; `DEFAULT` keyword -> `SetToDefault{attnum}`; coerced with
//!   `coerce::coerce_assignment`.
//! * `SET (a, b) = (e1, e2)` -> `Assignment { targets: [a, b], value: Row([e1', e2']) }` each coerced;
//!   `SET (a, b) = (SELECT x, y ...)` -> `value: SubLink{Row}` with the sub-query's projection coerced;
//!   `SET (a, b) = ROW(..)` same as the first; arity mismatch 42601 `number of columns does not match
//!   number of values`.
//! * Duplicate targets in one statement: 42601 `multiple assignments to same column "x"`
//!   (`diag::multiple_assignments`; no position in PG's output).
//! * Generated column target: 428C9 `column "g" can only be updated to DEFAULT` DETAIL `Column "g" is a
//!   generated column.`; identity ALWAYS: 428C9 `column "id" can only be updated to DEFAULT` DETAIL
//!   `Column "id" is an identity column defined as GENERATED ALWAYS.`.
//! * Marks `access.update_cols` (and `select_cols` for referenced columns).
//!
//! ## RETURNING ([`bind_returning`])
//! `Vec<SelectItem>` bound in the DML scope with `target::bind_target_list(.., ExprCtx::returning())`:
//! `*` expands the target's columns (and FROM columns for UPDATE/DELETE: `RETURNING *` includes the
//! FROM/USING tables' columns too); aggregates/windows/SRFs are errors (42803 `aggregate functions
//! are not allowed in RETURNING`, 0A000 SRF); unknown-typed outputs resolved to text. Empty -> `vec![]`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{Assignment, Delete, SelectItem, Update};

use super::context::Binder;
use crate::ir::{Assignment as BoundAssignment, BoundStatement, TargetEntry, TargetTable};

/// Bind an UPDATE statement (module docs).
#[allow(unused_variables)]
pub fn bind_update(b: &mut Binder, u: &Update) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-28", "UPDATE binding (update_delete::bind_update)"))
}

/// Bind a DELETE statement (module docs).
#[allow(unused_variables)]
pub fn bind_delete(b: &mut Binder, d: &Delete) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-28", "DELETE binding (update_delete::bind_delete)"))
}

/// Bind `SET` assignments against `target` (range-table index `target_rt` of the current scope);
/// shared by UPDATE and ON CONFLICT DO UPDATE.
#[allow(unused_variables)]
pub fn bind_assignments(b: &mut Binder, assignments: &[Assignment], target: &TargetTable, target_rt: u32) -> Result<Vec<BoundAssignment>> {
    Err(PgError::unimplemented("SQL-28", "SET assignment binding (update_delete::bind_assignments)"))
}

/// Bind a RETURNING list in the current (DML) scope.
#[allow(unused_variables)]
pub fn bind_returning(b: &mut Binder, items: Option<&[SelectItem]>) -> Result<Vec<TargetEntry>> {
    Err(PgError::unimplemented("SQL-28", "RETURNING binding (update_delete::bind_returning)"))
}
