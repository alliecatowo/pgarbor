//! `VALUES (...), (...)` lists (SQL-24). PG references: `analyze.c` (`transformValuesClause`,
//! `transformInsertStmt` for the INSERT variant), `parse_coerce.c`.
//!
//! * Every row must have the same number of expressions: 42601 `VALUES lists must all be the same
//!   length` (`diag::values_length_mismatch`, position = first surplus expression of the offending row).
//! * Cells are bound with `ExprCtx::values()` (no aggregates/windows/SRFs/columns of this query:
//!   a column reference is 42703; outer references allowed in sub-selects).
//! * Per column: `coerce::coerce_to_common(cells, "VALUES")` ; all-unknown columns are text;
//!   `types[i]` = the common type. A standalone `VALUES (1),(2.5)` has columns `column1`, ... of type
//!   numeric.
//! * `DEFAULT` in a VALUES list is legal ONLY as an INSERT source (`InsertTarget` given): produces
//!   `ExprKind::SetToDefault{attnum}` for the matching target column and the column's own type; elsewhere
//!   42601 `DEFAULT is not allowed in this context`.
//! * INSERT source mode: cells are NOT unified into a common type per column; each cell is
//!   assignment-coerced directly to its target column type (`coerce::coerce_assignment`) — PG's
//!   `transformInsertStmt` does the same for a single-row/multi-row VALUES. The caller passes
//!   `insert_targets`.
use pgarbor_common::{PgError, Result};
use pgarbor_types::TypeId;
use sqlparser::ast::Values;

use super::context::Binder;
use crate::ir::{BoundValues, OutputColumn};

/// Target column description for INSERT ... VALUES.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct InsertTargetCol {
    pub attnum: i16,
    pub name: String,
    pub ty: TypeId,
}

/// Bind a VALUES list. `insert_targets`: `Some` when this VALUES is an INSERT source (then rows must
/// have exactly that many cells: 42601 `INSERT has more expressions than target columns` /
/// `INSERT has more target columns than expressions`) and cells are assignment-coerced.
#[allow(unused_variables)]
pub fn bind_values_body(b: &mut Binder, values: &Values, insert_targets: Option<&[InsertTargetCol]>) -> Result<BoundValues> {
    Err(PgError::unimplemented("SQL-24", "VALUES binding (values::bind_values_body)"))
}

/// Output columns of a standalone VALUES: `column1`, `column2`, ... (or the given aliases).
#[allow(unused_variables)]
pub fn values_output_columns(values: &BoundValues, aliases: &[String]) -> Vec<OutputColumn> {
    values
        .types
        .iter()
        .enumerate()
        .map(|(i, ty)| OutputColumn {
            name: aliases.get(i).cloned().unwrap_or_else(|| format!("column{}", i + 1)),
            ty: *ty,
            origin: None,
        })
        .collect()
}
