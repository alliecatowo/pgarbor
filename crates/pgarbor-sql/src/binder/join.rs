//! JOINs: ON / USING / NATURAL, join columns, namespace maintenance (SQL-18, sonnet).
//! PG references: `parse_clause.c` (`transformFromClauseItem` JoinExpr branch,
//! `buildMergedJoinVar`, `buildJoinNSItem`/`transformJoinUsingClause`, `transformJoinOnClause`,
//! `extractRemainingColumns`, `setNamespaceLateralState`), `parse_relation.c`
//! (`addRangeTableEntryForJoin`).
//!
//! `bind_join(b, left, join)` is called by `from::bind_from_clause` once per `Join` of a
//! `TableWithJoins`, with `left` = the item built so far.
//! 1. Bind the right factor with `from::bind_table_factor(b, &join.relation, lateral_ok)`.
//!    LATERAL/`CROSS APPLY` on the right may see the left side; a plain right side must not
//!    (namespace lateral flags; RIGHT/FULL JOIN with a LATERAL reference to the left side: 0A000?
//!    PG: 42601 `invalid reference to FROM-clause entry for table "x"` DETAIL `The combining JOIN type
//!    must be INNER or LEFT for a LATERAL reference.`).
//! 2. Map `JoinOperator`: `Join`/`Inner` -> Inner; `Left`/`LeftOuter` -> Left; `Right`/`RightOuter` ->
//!    Right; `FullOuter` -> Full; `CrossJoin` -> Inner without quals (constraint must be `None`);
//!    `Semi/LeftSemi/RightSemi/Anti/LeftAnti/RightAnti/CrossApply/OuterApply/AsOf/StraightJoin` -> 42601
//!    (not PostgreSQL syntax).
//! 3. Constraint (`JoinConstraint`):
//!    * `On(expr)`: bind with `ExprCtx::join_on()` in the namespace of left+right (both
//!      `cols_visible`, nothing else of this FROM list), `coerce_to_boolean("JOIN/ON")`.
//!    * `Using(cols)`: each name must exist exactly once on both sides (42703 `column "c" specified in
//!      USING clause does not exist in left table` / `right table` via `diag::using_column_missing`;
//!      42702 `common column name "c" appears more than once in left table` via
//!      `diag::using_column_ambiguous`); quals = AND of `l.c = r.c` (operator resolved on the two
//!      column types; types coerced to the common type, the merged column has the common type);
//!      duplicate names in the USING list: 42701 `column name "c" appears more than once in USING
//!      clause`.
//!    * `Natural`: USING with all common column names (none common: becomes a cross join).
//!    * `None` (plain `JOIN` without ON): 42601 `syntax error at or near ...` (PG grammar requires ON/USING);
//!      `CROSS JOIN` has no constraint.
//! 4. Merged columns for USING/NATURAL: output column list = merged columns (in USING order) then
//!    remaining left columns, then remaining right columns. The merged column's replacement
//!    expression: INNER/LEFT -> the left column; RIGHT -> the right column; FULL ->
//!    `COALESCE(left, right)` (coerced to the common type). Names in the result namespace come from the
//!    left side. `SELECT *` over the join lists the merged column once; `t.*` still lists the table's
//!    own columns (`hidden_from_star` handles the duplicates).
//! 5. Namespace: both input items get `cols_visible = false` (still `rel_visible`), a NEW join
//!    `NsItem` (alias `None` or the `JOIN ... AS j` alias if `TableAlias` present on a NestedJoin) is
//!    added with `cols_visible = true` and the merged column list; `ON` clauses are bound BEFORE this
//!    change (they see both sides' columns).
//! 6. Result: `FromItem::Join(Box<JoinItem { kind, left, right, quals }>)`.
//! Chains `a JOIN b ON .. JOIN c ON ..` are left-deep: the left item of the second join is the first
//! join (the ON of the second sees a, b, c).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Join;

use super::context::Binder;
use crate::ir::FromItem;

/// Combine `left` with the right side of `join` (module docs).
#[allow(unused_variables)]
pub fn bind_join(b: &mut Binder, left: FromItem, join: &Join) -> Result<FromItem> {
    Err(PgError::unimplemented("SQL-18", "JOIN binding (join::bind_join)"))
}
