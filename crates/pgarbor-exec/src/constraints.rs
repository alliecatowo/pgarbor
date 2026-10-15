//! Row-level constraint enforcement (lease EXE-31). PG: `executor/execMain.c` (`ExecConstraints`,
//! `ExecBuildSlotValueDescription`). UNIQUE/PRIMARY KEY are enforced by `index_maint`, FOREIGN KEY by
//! `fk`, domain constraints inside expressions (`eval_cast::eval_coerce_to_domain`).
//!
//! * [`check_not_null`]: for every non-dropped column with `not_null` whose value in the NEW row is
//!   NULL (columns in attnum order, FIRST violation wins) -> `error::not_null_violation(schema, table,
//!   column, error::row_description(..))` (23502). Message and DETAIL fixtures are in `error`:
//!   `null value in column "name" of relation "t" violates not-null constraint` / `Failing row
//!   contains (2, null, 1.50, y, null).`
//! * [`check_constraints`]: evaluate each `td.checks` expression over the full new row
//!   (`eval::eval_bool3`); FALSE -> `error::check_violation(schema, table, constraint_name,
//!   row_description)` (23514); TRUE or NULL pass. Constraints are checked in NAME order (PG:
//!   alphabetical by constraint name) after NOT NULL.
//! * [`enforce`]: `check_not_null` then `check_constraints` - the order PG uses for INSERT and UPDATE.
//! Row descriptions use `td.desc_cols()` and the session `IoCtx` (`cx.se.io_ctx()`).
use pgarbor_common::{PgError, Result};
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

#[allow(unused_variables)]
pub fn check_not_null(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-31", "NOT NULL enforcement (constraints::check_not_null)"))
}

#[allow(unused_variables)]
pub fn check_constraints(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-31", "CHECK enforcement (constraints::check_constraints)"))
}

#[allow(unused_variables)]
pub fn enforce(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-31", "row constraint enforcement (constraints::enforce)"))
}
