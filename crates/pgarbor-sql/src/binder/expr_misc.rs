//! CASE, COALESCE/NULLIF/GREATEST/LEAST, ARRAY[], ROW, subscripts, field selection, COLLATE,
//! INTERVAL literals (SQL-14). PG references: `parse_expr.c` (`transformCaseExpr`,
//! `transformArrayExpr`, `transformRowExpr`, `transformCoalesceExpr`, `transformMinMaxExpr`,
//! `transformIndirection`, `transformCollateClause`), `parse_coerce.c` (`select_common_type`,
//! `coerce_to_common_type`), `parse_node.c` (`transformContainerSubscripts`, `transformContainerType`),
//! `parse_target.c` (`ParseComplexProjection`).
//!
//! * **CASE** (`Expr::Case { operand, conditions: Vec<CaseWhen>, else_result }`): simple form
//!   `CASE x WHEN v THEN r`: bind `x` once as `CaseExpr.arg`; each `cond` becomes `Op(=)(CaseTestValue,
//!   v)` resolved with the operand type; searched form: each condition `coerce_to_boolean("CASE/WHEN")`.
//!   Results unified with `coerce_to_common(results + else, "CASE")`; no ELSE -> `default: Some(typed NULL constant)` (PG's `transformCaseExpr` always
//!   fills the default; the view definition prints `ELSE NULL::text`, oracle).
//!   Type error: 42804 `CASE types integer and text cannot be matched`. Result type of all-unknown
//!   branches: `text`.
//! * **COALESCE(a, b, ...)** -> `ExprKind::Coalesce` with the common type (context `COALESCE`;
//!   42804 `COALESCE types integer and text cannot be matched`). **NULLIF(a, b)** -> `NullIf` with
//!   `=` resolved on the (a, b) operand types; result type = type of `a` after unknown resolution.
//!   **GREATEST/LEAST** -> `MinMax` with common type (context `GREATEST`/`LEAST`).
//! * **ARRAY[a, b]** (`Expr::Array(Array { elem, named })`): element type = common type of the
//!   elements (context `ARRAY`); nested `ARRAY[ARRAY[1,2],ARRAY[3,4]]` -> multidimensional
//!   `ArrayExpr{multidims: true}` (sub-arrays must have the same dims: 2202E `multidimensional
//!   arrays must have array expressions with matching dimensions`); empty `ARRAY[]` requires a cast
//!   (42P18 `cannot determine type of empty array` HINT `Explicitly cast to the desired type, for
//!   example ARRAY[]::integer[].`); `ARRAY[..]::type[]` with explicit cast coerces each element.
//!   Array-of-array element types: arrays whose element is an array type are merged to
//!   multidimensional. Type of the node = array type of the element (`TypeEnv::type_info.array_oid`
//!   or `catalog_data` lookup); no array type -> 42704 `could not find array type for data type X`.
//! * **ROW(a, b)** / `(a, b)` (`Expr::Tuple`) -> `Row` with `ty = record` and field names `f1`, `f2`...
//!   `ROW()` with no fields is allowed. A parenthesized single expression is NOT a row.
//! * **Subscripts** `a[i]`, `a[i][j]`, `a[lo:hi]` (sqlparser `CompoundFieldAccess { root, access_chain:
//!   Vec<AccessExpr> }` with `AccessExpr::Subscript(Subscript::Index|Slice)` and `Dot(expr)`): `root` must
//!   be an array type (or `int2vector`/`oidvector`, json/jsonb subscripting `j['k']` -> operator
//!   `->`-style `SubscriptExpr` is NOT supported: 0A000 `jsonb subscripting`): else 42804 `cannot
//!   subscript type integer because it does not support subscripting`. Index expressions are coerced to
//!   `int4`. A slice anywhere makes every subscript a slice (`[2]` becomes `[1:2]`). Result type: element
//!   type for plain subscripts; the array type for slices.
//! * **Field selection** `(expr).field` and `(expr).*`: `expr` must be composite (a named composite,
//!   `record` from a function with column definitions, or a whole-row `Column`); field looked up in
//!   `TypeEnv::composite_fields` -> `FieldSelect { field: 1-based index }`; unknown field -> 42703
//!   `column "x" not found in data type t`; non-composite -> 42809 `type integer is not composite`.
//!   [`expand_composite`] expands `(expr).*` to `(name, FieldSelect)` pairs.
//! * **COLLATE** (`Expr::Collate { expr, collation }`): collation name resolved with
//!   `env.collation_oid(schema, name)` (42704 `collation "x" for encoding "UTF8" does not exist`);
//!   the operand must be collatable (text-ish): 42804 `collations are not supported by type integer`.
//!   -> `ExprKind::Collate`. v1 treats all collations as code-point order.
//! * **INTERVAL** (`Expr::Interval(Interval { value, leading_field, leading_precision,
//!   last_field, fractional_seconds_precision })`): `INTERVAL '1 day'` and `INTERVAL '1' DAY` /
//!   `INTERVAL '1-2' YEAR TO MONTH`: build a typed literal of type `interval` with the `TypeName`
//!   interval range/precision (same as `literal::bind_typed_string` with an `interval` type);
//!   non-literal `value` expressions -> 0A000 (PG's grammar only allows string literals there).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{Expr, Ident};

use super::context::{Binder, ExprCtx};
use crate::ir::BoundExpr;

/// `Expr::Case`.
#[allow(unused_variables)]
pub fn bind_case(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "CASE (expr_misc::bind_case)"))
}

/// `coalesce(args)`.
#[allow(unused_variables)]
pub fn bind_coalesce(b: &mut Binder, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "COALESCE (expr_misc::bind_coalesce)"))
}

/// `nullif(a, b)` (exactly two args; any other count is a grammar error, oracle `select nullif(1)`: 42601 syntax
/// error from the grammar; use 42601 `syntax error at or near ")"`-style via `diag::syntax`).
#[allow(unused_variables)]
pub fn bind_nullif(b: &mut Binder, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "NULLIF (expr_misc::bind_nullif)"))
}

/// `greatest(args)` / `least(args)`.
#[allow(unused_variables)]
pub fn bind_minmax(b: &mut Binder, greatest: bool, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "GREATEST/LEAST (expr_misc::bind_minmax)"))
}

/// `Expr::Array`.
#[allow(unused_variables)]
pub fn bind_array(b: &mut Binder, elems: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "ARRAY[] constructor (expr_misc::bind_array)"))
}

/// `ROW(..)` / `Expr::Tuple`.
#[allow(unused_variables)]
pub fn bind_row(b: &mut Binder, elems: &[Expr], ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "ROW constructor (expr_misc::bind_row)"))
}

/// `Expr::CompoundFieldAccess` (subscripts and `.field` chains) and `Expr::Subscript`-like forms.
#[allow(unused_variables)]
pub fn bind_field_access(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "subscripts and field access (expr_misc::bind_field_access)"))
}

/// `(composite_expr).field`.
#[allow(unused_variables)]
pub fn select_field(b: &mut Binder, arg: BoundExpr, field: &Ident, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "field selection (expr_misc::select_field)"))
}

/// `(composite_expr).*` -> (field name, FieldSelect) pairs; used by `target`.
#[allow(unused_variables)]
pub fn expand_composite(b: &mut Binder, arg: BoundExpr, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    Err(PgError::unimplemented("SQL-14", "composite expansion (expr_misc::expand_composite)"))
}

/// `Expr::Collate`.
#[allow(unused_variables)]
pub fn bind_collate(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "COLLATE (expr_misc::bind_collate)"))
}

/// `Expr::Interval`.
#[allow(unused_variables)]
pub fn bind_interval(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-14", "INTERVAL literal (expr_misc::bind_interval)"))
}
