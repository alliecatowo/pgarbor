//! Window function planning (PLN-08). PG references: `optimizer/plan/planner.c`
//! (`create_window_paths`, `select_active_windows`, `make_pathkeys_for_window`),
//! `plan/createplan.c` (`create_windowagg_plan`), `nodeWindowAgg.c` for semantics.
//!
//! ## Logical build (`build_windows`)
//! For each `WindowDef` in `sel.windows` (in index order) that has at least one `WindowFunc`
//! referencing it (`WindowFunc::window == index`) in the target list / ORDER BY expressions, stack a
//! [`LogicalWindow`] over the input: `partition_by`, `order_by` (from `WindowDef`), frame, name
//! (`WindowDef::name` or `w{index+1}`), `funcs` = the distinct `WindowFunc` expressions for that window.
//! Stacking order: windows with the same sort requirement adjacent; windows whose
//! (partition, order) is a prefix of another's come first (PG `select_active_windows` ordering) so a
//! single Sort can serve several windows.
//! Output `vars = input.vars ++ funcs`.
//!
//! ## Physical (`plan_window`)
//! * Required input order = `partition_by` (asc, nulls last) then `order_by`. If the child's `ordering`
//!   already satisfies it (prefix match incl. direction), no Sort; otherwise add `Sort`.
//! * Produce `WindowAggNode { partition_by, order_by, frame, window_name, funcs: WindowCall.. }`
//!   (`WindowFunc` -> `WindowCall`: copy name/args/filter/ignore_nulls/is_agg/star, `result_type`).
//! * `frame_is_default` frames are passed as the PG default (RANGE UNBOUNDED PRECEDING..CURRENT ROW with
//!   ORDER BY, ROWS whole partition otherwise); the executor derives behavior from `FrameSpec`.
//! * `ordering` of the node = its input order.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundSelect;

use crate::context::PlannerState;
use crate::logical::{LogicalPlan, LogicalWindow};
use crate::plan::PlanNode;

/// Stack windows over `input` (module docs).
#[allow(unused_variables)]
pub fn build_windows(st: &mut PlannerState, input: LogicalPlan, sel: &BoundSelect) -> Result<LogicalPlan> {
    Err(PgError::unimplemented("PLN-08", "window logical plan (window::build_windows)"))
}

/// Physical plan of a `LogicalWindow`.
#[allow(unused_variables)]
pub fn plan_window(st: &mut PlannerState, w: &LogicalWindow) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-08", "window planning (window::plan_window)"))
}
