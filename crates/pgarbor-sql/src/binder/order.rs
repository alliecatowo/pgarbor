//! ORDER BY, DISTINCT / DISTINCT ON, LIMIT / OFFSET / FETCH (SQL-21).
//! PG references: `parse_clause.c` (`transformSortClause`, `transformDistinctClause`,
//! `transformDistinctOnClause`, `findTargetlistEntrySQL92`, `findTargetlistEntrySQL99`,
//! `addTargetToSortList`, `transformLimitClause`, `get_sortgroupref_tle`), `parse_oper.c`
//! (`get_ordering_op_properties`), `analyze.c`.
//!
//! ## ORDER BY item resolution (`bind_order_by`, SELECT blocks)
//! For each `OrderByExpr { expr, options: { asc, nulls_first }, with_fill }`:
//! 1. integer literal `n` -> target entry n (1-based, among NON-junk entries): out of range ->
//!    42P10 `ORDER BY position 3 is not in select list` (`diag::position_not_in_select_list`);
//!    a non-integer constant (`ORDER BY 'a'`, `ORDER BY 1.5`) is 42601 `non-integer constant in ORDER BY`
//!    (same for GROUP BY: `non-integer constant in GROUP BY`; DISTINCT ON: `non-integer constant in DISTINCT ON`).
//! 2. bare identifier (no qualifier): if it matches an OUTPUT column name -> that entry (SQL92 rule,
//!    output names win over input columns in ORDER BY; two outputs with that name ->
//!    42702 `ORDER BY "x" is ambiguous`, unless they are the same expression);
//! 3. otherwise bind the expression (`ExprCtx::order_by()`) and look for a structurally equal target
//!    entry (reuse); if none, append a JUNK target entry — except for `SELECT DISTINCT`, where it is
//!    42P10 `for SELECT DISTINCT, ORDER BY expressions must appear in select list`
//!    (`diag::distinct_order_by`).
//! Direction: `ASC`/default ascending; `DESC`; `USING <op>` (not in sqlparser's PG dialect? if exposed,
//! resolve `<`/`>`-family operator and store `using_op`); default null ordering: `NULLS LAST` for ASC,
//! `NULLS FIRST` for DESC unless explicit. Ordering requires an ordering operator: type without
//! `<` -> 42883 `could not identify an ordering operator for type X`. Duplicate keys are kept
//! (PG removes redundant ones later; keep the first occurrence and drop later duplicates of the same
//! (index, direction)).
//! Positions for errors: the item expression start.
//!
//! ## DISTINCT ([`bind_distinct`])
//! `Distinct::Distinct` -> `DistinctSpec::All` (needs an equality operator on every output column;
//! 42883 `could not identify an equality operator for type X`); `Distinct::On(exprs)` -> each expression
//! resolved like an ORDER BY item (ordinal, output name, expression -> maybe junk entry); the DISTINCT ON
//! expressions must match the LEFTMOST ORDER BY expressions: 42P10 `SELECT DISTINCT ON expressions must
//! match initial ORDER BY expressions` (`diag::distinct_on_mismatch`). When ORDER BY is absent the
//! DISTINCT ON keys become the sort keys (planner). The distinct step runs BEFORE sort-key resolution
//! in PG (`transformDistinctOnClause` after `transformSortClause`); implement accordingly.
//!
//! ## Set operation / VALUES ORDER BY ([`bind_order_by_output`])
//! Only output-column references: ordinal or bare/qualified output name; any other expression ->
//! 0A000 `invalid UNION/INTERSECT/EXCEPT ORDER BY clause` DETAIL `Only result column names can be used,
//! not expressions or functions.` HINT `Add the expression/function to every SELECT, or move the UNION into
//! a FROM clause.` (`diag::setop_order_by_expression`).
//!
//! ## LIMIT / OFFSET / FETCH ([`bind_limit_clause`])
//! `LimitClause::LimitOffset { limit, offset, .. }`, `Query::fetch: Fetch { with_ties, percent, quantity }`.
//! Expressions bound with `ExprCtx::limit()` (no columns/aggregates: 42803/42P10 `argument of LIMIT must
//! not contain variables`), coerced to `int8` (`coerce_to_specific(.., "LIMIT")`; `OFFSET` -> "OFFSET";
//! `FETCH` -> "FETCH"). `LIMIT ALL` / `LIMIT NULL` = no limit. A negative value is NOT a bind-time error:
//! PG raises 2201W `LIMIT must not be negative` / 2201X `OFFSET must not be negative` (`diag::negative_limit`,
//! no position) when the executor starts the node, even for constants (oracle: `select 1 limit -1`). `FETCH FIRST n ROWS
//! ONLY|WITH TIES`: `FETCH FIRST ROW ONLY` = 1; `percent` -> 0A000; `FETCH` together with `LIMIT` -> 42601 `syntax error at or near "fetch"` (oracle). Returns
//! `(limit, offset, with_ties)`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{Distinct, Fetch, LimitClause, OrderByExpr};

use super::context::Binder;
use crate::ir::{BoundExpr, DistinctSpec, OutputColumn, SortKey, TargetEntry};

/// ORDER BY of a SELECT block (module docs). May append junk entries to `target_list`.
/// `distinct` is the block's DISTINCT spec (`DistinctSpec::All` forbids junk entries).
#[allow(unused_variables)]
pub fn bind_order_by(
    b: &mut Binder,
    order_by: &[OrderByExpr],
    target_list: &mut Vec<TargetEntry>,
    distinct: &DistinctSpec,
) -> Result<Vec<SortKey>> {
    Err(PgError::unimplemented("SQL-21", "ORDER BY binding (order::bind_order_by)"))
}

/// DISTINCT / DISTINCT ON (module docs). `sort_keys` are the already bound ORDER BY keys (for the
/// prefix check). May append junk entries for DISTINCT ON expressions.
#[allow(unused_variables)]
pub fn bind_distinct(
    b: &mut Binder,
    distinct: Option<&Distinct>,
    target_list: &mut Vec<TargetEntry>,
    sort_keys: &[SortKey],
) -> Result<DistinctSpec> {
    Err(PgError::unimplemented("SQL-21", "DISTINCT binding (order::bind_distinct)"))
}

/// ORDER BY for VALUES / set-operation bodies (module docs).
#[allow(unused_variables)]
pub fn bind_order_by_output(b: &mut Binder, order_by: &[OrderByExpr], output: &[OutputColumn]) -> Result<Vec<SortKey>> {
    Err(PgError::unimplemented("SQL-21", "output ORDER BY binding (order::bind_order_by_output)"))
}

/// LIMIT / OFFSET / FETCH (module docs).
#[allow(unused_variables)]
pub fn bind_limit_clause(
    b: &mut Binder,
    limit_clause: Option<&LimitClause>,
    fetch: Option<&Fetch>,
) -> Result<(Option<BoundExpr>, Option<BoundExpr>, bool)> {
    Err(PgError::unimplemented("SQL-21", "LIMIT/OFFSET/FETCH binding (order::bind_limit_clause)"))
}
