//! WINDOW clause and `OVER (...)` specifications (SQL-22). PG references: `parse_clause.c`
//! (`transformWindowDefinitions`, `transformFrameOffset`, `findWindowClause`), `parse_agg.c`
//! (`transformWindowFuncCall`), `parse_expr.c`.
//!
//! * `bind_window_clause(b, named)` binds `WINDOW w AS (...), v AS (w ORDER BY ...)` into
//!   `QueryScope::windows` (named entries, in definition order). Duplicate names: 42P20 `window
//!   "w" is already defined`. References to later-defined names: 42704 `window "x" does not exist`.
//! * `bind_window_spec(b, over)` is called for each `OVER` (`WindowType::WindowSpec(spec)` or
//!   `NamedWindow(ident)`) and returns an index into `QueryScope::windows`, de-duplicating
//!   structurally equal definitions (PG shares one `WindowClause` between identical OVER clauses).
//!   `OVER w` with no changes reuses window `w`. `OVER (w ORDER BY ...)` copies `w` and adds ordering:
//!   errors 42P20 `cannot override PARTITION BY clause of window "w"`, `cannot override ORDER BY
//!   clause of window "w"` (when w already has one), `cannot copy window "w" because it has a frame
//!   clause` HINT `Omit the parentheses in this OVER clause.`
//!
//! ## Specification content (`WindowSpec { window_name, partition_by, order_by, window_frame }`)
//! * `partition_by`: expressions bound with `ExprCtx::window_def(ExprClause::PartitionBy)`; need
//!   equality ops (42883 `could not identify an equality operator for type X`).
//! * `order_by`: bound like ORDER BY items but as `SortExpr`s (no target-list indirection) with
//!   `ExprCtx::window_def(ExprClause::WindowOrderBy)`.
//! * frame (`WindowFrame { units, start_bound, end_bound }`): units ROWS / RANGE / GROUPS; bounds
//!   `UNBOUNDED PRECEDING`, `n PRECEDING`, `CURRENT ROW`, `n FOLLOWING`, `UNBOUNDED FOLLOWING`.
//!   Rules (all 42P20 unless noted): frame start cannot be UNBOUNDED FOLLOWING (`frame start cannot be
//!   UNBOUNDED FOLLOWING`), frame end cannot be UNBOUNDED PRECEDING, frame starting from following row
//!   cannot end with current row / frame starting from current row cannot have preceding rows (the
//!   ordering rule `start <= end`), GROUPS and RANGE with offset require ORDER BY (`GROUPS mode
//!   requires an ORDER BY clause`, `RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY
//!   column`), offset expressions: ROWS/GROUPS -> `int8` (negative constant: 22013 `frame starting
//!   offset must not be negative`/`frame ending offset must not be negative`), RANGE -> the ordering
//!   column's offset type (`in_range` support: int types -> same type, date/timestamp(tz) ->
//!   `interval`, numeric -> numeric, float -> float8); no offset support: 42P20 `RANGE with offset
//!   PRECEDING/FOLLOWING is not supported for column type X`. Offsets must not contain variables,
//!   aggregates, windows (`ExprClause::WindowFrameRows/Range/Groups`). `EXCLUDE` clause supported
//!   (`FrameExclusion`); sqlparser 0.63 may not expose it -> 0A000.
//! * `frame_is_default` is set when no frame was written (`RANGE UNBOUNDED PRECEDING` to `CURRENT
//!   ROW` with ORDER BY, whole partition without).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{NamedWindowDefinition, WindowType};

use super::context::Binder;

/// Bind the `WINDOW` clause definitions into the innermost scope.
#[allow(unused_variables)]
pub fn bind_window_clause(b: &mut Binder, named: &[NamedWindowDefinition]) -> Result<()> {
    Err(PgError::unimplemented("SQL-22", "WINDOW clause binding (window::bind_window_clause)"))
}

/// Bind one `OVER` specification; returns its index in `QueryScope::windows`.
#[allow(unused_variables)]
pub fn bind_window_spec(b: &mut Binder, over: &WindowType) -> Result<usize> {
    Err(PgError::unimplemented("SQL-22", "OVER binding (window::bind_window_spec)"))
}
