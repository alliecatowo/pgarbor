//! UPDATE row processing (lease EXE-28, sonnet, north-star). PG: `nodeModifyTable.c`
//! (`ExecUpdate`, `ExecUpdateAct`, EvalPlanQual), `execIndexing.c`. Spec: plan.rs `UpdateSpec`.
//!
//! ## Input and identity
//! The ModifyTable input row is `table row (ncols) ++ rowid ++ extra FROM columns`;
//! `rowid = input[rowid_slot]` (`Datum::Int8` -> u64). The first `ncols` slots are the old row image as
//! seen by the statement snapshot.
//!
//! ## `update_one` (one input row)
//! 1. `rowstore::lock_row(ex, td, rowid, ForNoKeyUpdate, Block)` (use `ForUpdate` if any assigned
//!    column is a key of a unique index or referenced by a foreign key). Outcomes:
//!    * `Gone` -> skip the row (concurrently deleted); returns `None`.
//!    * `Current(img)` -> old image `img`.
//!    * `Changed(latest)` (READ COMMITTED) -> EvalPlanQual per [`EpqPolicy`]: if `policy.quals` is `None`
//!      (joins / FROM) return the 40001 error `could not serialize access due to concurrent update`;
//!      else evaluate every qual over `latest` (`eval::eval_qual`); any false -> skip the row (`None`);
//!      otherwise rebuild the input row as `latest ++ rowid ++ <extra columns unchanged>` and continue
//!      with `latest` as the old image (assignments are re-evaluated against it).
//! 2. [`build_new_row`]: evaluate the assignments over the input row; apply to a copy of the old image;
//!    `SetToDefault{attnum}` that survives in an assignment expression is replaced by the evaluated
//!    column default (`ModifyTarget.meta.columns[attnum-1].default`, NULL if none); then
//!    [`recompute_generated`] for `spec.generated`.
//! 3. If the new row is identical to the old row (all slots `not_distinct`) the row is STILL counted and
//!    written (PG writes a new tuple version); skipping index maintenance is allowed.
//! 4. [`apply_update`]: `constraints::enforce` -> `rowstore::write_row` (same rowid) ->
//!    `index_maint::update_entries(.., modified_cols)` -> `fk::after_update`. Returns the new image.
//! A target row matched by SEVERAL input rows is updated once (first match): the caller
//! (`modify`) dedups by rowid before calling.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{ModifyTarget, UpdateSpec};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// How to handle a concurrently changed row under READ COMMITTED (EvalPlanQual lite).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct EpqPolicy {
    /// Qualifications to re-check against the latest row version (expressions over a TABLE row,
    /// `InputRef(attnum-1)`); `None` = EPQ unsupported for this plan shape (joins): 40001.
    pub quals: Option<Vec<BoundExpr>>,
}

#[allow(unused_variables)]
pub fn update_one(
    ex: &mut Executor,
    td: &TableDesc,
    target: &ModifyTarget,
    spec: &UpdateSpec,
    policy: &EpqPolicy,
    input: &[Datum],
    rowid: u64,
) -> Result<Option<Vec<Datum>>> {
    Err(PgError::unimplemented("EXE-28", "UPDATE row (dml_update::update_one)"))
}

#[allow(unused_variables)]
pub fn build_new_row(
    ex: &mut Executor,
    td: &TableDesc,
    target: &ModifyTarget,
    spec: &UpdateSpec,
    old_row: &[Datum],
    input: &[Datum],
) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-28", "UPDATE new row (dml_update::build_new_row)"))
}

/// Recompute stored generated columns `(attnum, expr)` over the (new) row, in order.
#[allow(unused_variables)]
pub fn recompute_generated(ex: &mut Executor, generated: &[(i16, BoundExpr)], row: &mut Vec<Datum>) -> Result<()> {
    Err(PgError::unimplemented("EXE-28", "generated column recompute (dml_update::recompute_generated)"))
}

/// Low-level in-place update used by UPDATE, ON CONFLICT DO UPDATE and foreign-key actions (module
/// docs step 4). `modified_cols`: assigned attnums when known (index maintenance hint).
#[allow(unused_variables)]
pub fn apply_update(
    ex: &mut Executor,
    td: &TableDesc,
    rowid: u64,
    old_row: &[Datum],
    new_row: Vec<Datum>,
    modified_cols: Option<&[i16]>,
) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-28", "row update (dml_update::apply_update)"))
}
