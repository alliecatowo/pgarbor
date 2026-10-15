//! Aggregate and window function calls, GROUPING() (SQL-13, sonnet).
//! PG references: `parse_agg.c` (`transformAggregateCall`, `transformWindowFuncCall`,
//! `check_agglevels_and_constraints`, `check_agg_arguments`, `transformGroupingFunc`),
//! `parse_func.c` (`ParseFuncOrColumn` aggregate branch), `parse_clause.c` (`transformWindowDefinitions`
//! lives in `window.rs`).
//!
//! ## Aggregates
//! Called by `expr_func::bind_function_call` after resolution says `ProcKind::Aggregate` (so the
//! argument types are final). Build [`crate::ir::Aggref`]:
//! * `agg` = proc OID, `name`, `args` coerced to the declared types (`coerce_to`), `star` for
//!   `count(*)` (zero args), `distinct` (`DISTINCT`), `filter` (bound with `ExprCtx::where_()` and
//!   `coerce_to_boolean("FILTER")`), `order_by` (`agg(x ORDER BY y)`; sort operators resolved as in
//!   `order::sort_expr`), `variadic`, `kind` from `catalog_data::builtin_aggregate(oid).kind`.
//! * Ordered-set (`percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`): direct arguments are the call
//!   args, the WITHIN GROUP sort keys are `order_by` and ALSO the aggregated `args`; hypothetical-set
//!   (`rank(1) WITHIN GROUP (ORDER BY x)`) has matching counts (42883 `function rank(integer) does not
//!   exist`...). A normal aggregate with WITHIN GROUP: 42809 `X is not an ordered-set aggregate, so
//!   it cannot have WITHIN GROUP`; an ordered-set aggregate without it: 42809 `WITHIN GROUP is
//!   required for ordered-set aggregate X`. `DISTINCT` + ordered-set: 42809? (PG: `cannot use
//!   DISTINCT with WITHIN GROUP`); `ORDER BY` in a call to an ordered-set aggregate: error.
//! * `count(DISTINCT a, b)`: multi-arg DISTINCT allowed. `ORDER BY` with `DISTINCT`: each ORDER BY
//!   expression must appear in the argument list (42P10 `in an aggregate with DISTINCT, ORDER BY
//!   expressions must appear in argument list`).
//! * **Level** (`agglevelsup`): `min(levels_up of every Column in args and filter)` (outer-only args
//!   make the aggregate belong to the outer query; no columns at all -> level 0). The node is
//!   returned with `levels_up` set; the owning scope (`scopes[len-1-levels_up]`) gets `has_aggs =
//!   true`. Placement checks (`check_agglevels_and_constraints`) use the clause of the OWNING level:
//!   `aggregate functions are not allowed in WHERE` (42803, also GROUP BY, JOIN conditions,
//!   FROM function arguments, LIMIT, DEFAULT, CHECK, index expressions, ...) via
//!   `diag::agg_not_allowed`; `aggregate function calls cannot be nested` (42803) when the
//!   arguments contain an aggregate of the same level (track `Binder::agg_depth`); `aggregate
//!   function calls cannot contain set-returning function calls`; `aggregate function calls cannot
//!   contain window function calls` (42P20).
//! * Aggregates whose argument types need resolution use the normal function machinery so
//!   `sum(int4)` -> `bigint`, `sum(numeric)` -> `numeric`, `count(*)` -> `bigint`, `avg(int)` ->
//!   `numeric`, `string_agg(text,text)`, `array_agg(x)` -> array, `min(text)`, `max(date)`.
//!
//! ## Window functions
//! `f(args) OVER (spec)` / `OVER name`: resolve `f` as a window function OR an aggregate (becomes
//! `WindowFunc { is_agg: true }`). Build [`crate::ir::WindowFunc`]: coerced `args`, `filter`
//! (aggregates only; FILTER on a pure window function: 42809? `FILTER is not implemented for
//! non-aggregate window functions` 0A000), `distinct` (aggregate windows: 0A000 `DISTINCT is not
//! implemented for window functions`), `ignore_nulls` / `RESPECT NULLS`: 0A000 for functions other
//! than lead/lag/first_value/last_value/nth_value (PG 18 supports IGNORE NULLS for exactly those),
//! `window` = index returned by `window::bind_window_spec`. Placement: `window functions are not
//! allowed in WHERE` / GROUP BY / HAVING / JOIN conditions (42P20, `diag::window_not_allowed`);
//! nested: `window function calls cannot be nested` (42P20); inside an aggregate's argument: same
//! rule. Marks `scope.has_windows`. `OVER` is only legal in the SELECT list and ORDER BY of the
//! owning query.
//!
//! ## GROUPING(a, b, ...)
//! Arguments must be grouping expressions of the owning query (42P20 `arguments to GROUPING must
//! be grouping expressions of the associated query level`); at most 31 (54023 `GROUPING must have
//! fewer than 32 arguments`); result type `int4`. GROUP BY is bound AFTER the select list, so the
//! grouping expressions are not known yet. Protocol: this function binds each argument expression,
//! appends it to the owning scope's `QueryScope::pending_grouping` (dedup by equality) and returns
//! `ExprKind::Grouping(GroupingExpr { args })` where `args[i]` is the index of the i-th argument
//! INTO `pending_grouping`. `group::resolve_pending_groupings` (after GROUP BY is bound) rewrites
//! every `Grouping` node of the query block so `args` index `GroupBy::exprs`, raising the 42P20
//! error for an argument that is not a grouping expression.
use pgarbor_common::{PgError, Result};
use pgarbor_types::resolve::func::ResolvedFunc;
use sqlparser::ast::{Function, WindowType};

use super::context::{Binder, BoundArg, ExprCtx};
use crate::ir::BoundExpr;

/// Build an `Aggref` for an already resolved aggregate call (module docs). `f` is the whole call
/// (filter, within group, order by, distinct are read from it); `args` are the bound arguments in call
/// order (after named-argument handling).
#[allow(unused_variables)]
pub fn bind_aggregate(b: &mut Binder, f: &Function, resolved: &ResolvedFunc, args: Vec<BoundArg>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-13", "aggregate call binding (expr_agg::bind_aggregate)"))
}

/// Build a `WindowFunc` for `f(...) OVER window` (module docs).
#[allow(unused_variables)]
pub fn bind_window_call(b: &mut Binder, f: &Function, resolved: &ResolvedFunc, args: Vec<BoundArg>, over: &WindowType, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-13", "window function binding (expr_agg::bind_window_call)"))
}

/// `GROUPING(args)` (module docs).
#[allow(unused_variables)]
pub fn bind_grouping(b: &mut Binder, f: &Function, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-13", "GROUPING() binding (expr_agg::bind_grouping)"))
}

/// The query level (0 = current) an aggregate with these bound arguments/filter belongs to:
/// the minimum `levels_up` over all `Column` nodes at the aggregate's own nesting (columns inside
/// sub-selects count with their level adjusted by the sub-select depth). No columns -> 0.
/// Pure helper over bound expressions (`BoundExpr::collect_columns`), fully testable.
#[allow(unused_variables)]
pub fn aggregate_level(args: &[BoundExpr], filter: Option<&BoundExpr>) -> Result<u32> {
    Err(PgError::unimplemented("SQL-13", "aggregate level computation (expr_agg::aggregate_level)"))
}
