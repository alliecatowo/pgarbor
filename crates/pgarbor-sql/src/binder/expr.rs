//! Expression binding entry point and the simple expression forms (SQL-10).
//! PG references: `parse_expr.c` (`transformExprRecurse` is the dispatch; `transformColumnRef`,
//! `transformParamRef`, `transformTypeCast`, `transformCollateClause`, `transformBooleanTest`),
//! `parse_node.c`.
//!
//! [`bind_expr`] recursively converts one `sqlparser::ast::Expr` into a [`BoundExpr`] with a fully
//! resolved type. It is a dispatcher: each variant has an owner function, listed here so the
//! worker of THIS file only writes the "simple" arms and calls the others (their signatures are
//! frozen in the other modules).
//!
//! | `Expr` variant | handled by |
//! |---|---|
//! | `Identifier`, `CompoundIdentifier` | [`bind_column_ref`] (this file) |
//! | `Value` | `literal::bind_value` (placeholders -> `params::bind_param`) |
//! | `TypedString` | `literal::bind_typed_string` |
//! | `Nested(e)` | `bind_expr(e)` |
//! | `Cast{kind, expr, data_type, format}` / `Convert` | [`bind_cast`] (this file) |
//! | `IsNull`/`IsNotNull`/`IsTrue`/`IsNotTrue`/`IsFalse`/`IsNotFalse`/`IsUnknown`/`IsNotUnknown` | `expr_ops::bind_is_test` |
//! | `IsDistinctFrom`/`IsNotDistinctFrom` | `expr_ops::bind_distinct_from` |
//! | `BinaryOp`, `UnaryOp`, `Between`, `InList`, `Like`/`ILike`/`SimilarTo`/`RLike`, `AnyOp`/`AllOp`, `AtTimeZone` | `expr_ops::*` |
//! | `Function`, `Extract`, `Ceil`, `Floor`, `Position`, `Substring`, `Trim`, `Overlay` | `expr_func::*` |
//! | `Case`, `Array`, `Tuple`, `Collate`, `CompoundFieldAccess`/subscripts, `Interval` | `expr_misc::*` |
//! | `Exists`, `Subquery`, `InSubquery`, `InUnnest` | `expr_sublink::*` |
//! | `Wildcard`/`QualifiedWildcard` | only legal as `count(*)`/target list: 42601 `syntax error` here |
//! | `GroupingSets`/`Cube`/`Rollup` | only legal in GROUP BY: 42601 |
//! | all other variants (JSON access `->` shorthand variants, `Lambda`, `Struct`, `Map`, `Dictionary`, `MatchAgainst`, `OuterJoin`, `Prior`, `MemberOf`, `IsJson`, `IsNormalized`, ...) | 0A000 via `PgError::unimplemented("SQL-10", "<construct>")` naming the construct (or 42601 for non-PostgreSQL syntax such as `Prior`/`OuterJoin`/`MatchAgainst`/`Lambda`/`Struct`/`Map`/`Dictionary`) |
//!
//! ## Cross-cutting duties of `bind_expr`
//! * `b.enter()` / `b.leave()` around recursion (54001).
//! * Enforce `ExprCtx`: with `!ec.columns_ok`: DEFAULT expressions raise 42P10 `cannot use column reference in
//!   DEFAULT expression` at the column (oracle); LIMIT/OFFSET/VALUES bind with an EMPTY namespace, so
//!   `select 1 limit a` is the ordinary 42703 `column "a" does not exist` (oracle). Build the DEFAULT error
//!   with `PgError::new(SqlState::INVALID_COLUMN_REFERENCE, "cannot use column reference in DEFAULT expression")`.
//! * Sub-links when `!ec.sublinks_ok` -> 0A000 `cannot use subquery in {clause}`.
//!
//! ## Column references ([`bind_column_ref`])
//! `parts` = identifier parts (folded with `Binder::ident`):
//! 1. Candidate resolutions in PG order (`transformColumnRef`): 1 part `c`: column, else range-entry
//!    name (whole-row reference) ; 2 parts `a.b`: column `b` of table `a`; else column `a` then field
//!    `b` of its composite type (`ParseComplexProjection`); else whole-row `a` then field `b`;
//!    3 parts `s.t.c`; 4 parts `db.s.t.c` (db must equal the current database, else 0A000
//!    `cross-database references are not implemented`). Uses `scope::resolve_column`.
//! 2. A column reference found in a scope `levels_up > 0` is a correlated reference: the result is
//!    `Column{levels_up}`; mark the query level `has_sublinks` handling is not needed here.
//! 3. The Column node is created with `BoundExpr::column_at(.., pos)` (position kept for the
//!    grouping error).
//!
//! ## Casts ([`bind_cast`])
//! `expr::type` / `CAST(expr AS type)`: resolve the type (`types::resolve_data_type`), bind the
//! operand, `coerce::coerce_strict(.., CastContext::Explicit, CoercionForm::Explicit)`.
//! Special cases: `'literal'::type` converts the literal immediately; `NULL::type` is a typed NULL;
//! `$1::type` pins the parameter type to `type`; `expr::regclass` on a string constant resolves the
//! relation NOW to an OID (`'t'::regclass` -> 42P01 `relation "t" does not exist`); casts to array
//! types; `CAST(... FORMAT ...)`/`TRY_CAST`/`SAFE_CAST` -> 42601/0A000.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{DataType, Expr, Ident};

use super::context::{Binder, BoundArg, ExprCtx};
use crate::ir::BoundExpr;

/// Bind an expression (dispatch table in the module docs).
#[allow(unused_variables)]
pub fn bind_expr(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-10", "expression binding dispatch (expr::bind_expr)"))
}

/// Bind and keep the source position (`Spanned::span()` start of `e`; for `Nested` use the inner
/// expression's position, for `BinaryOp` PG uses the OPERATOR position for operator errors, which
/// `expr_ops` computes itself).
#[allow(unused_variables)]
pub fn bind_arg(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundArg> {
    Err(PgError::unimplemented("SQL-10", "argument binding (expr::bind_arg)"))
}

/// Bind and coerce to boolean for clause `what` (`WHERE`, `HAVING`, `JOIN/ON`, `FILTER`, ...):
/// `bind_expr` + `coerce::coerce_to_boolean`.
#[allow(unused_variables)]
pub fn bind_boolean(b: &mut Binder, e: &Expr, ec: ExprCtx, what: &str) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-10", "boolean expression binding (expr::bind_boolean)"))
}

/// Column reference (module docs). `pos` is the position of the first identifier.
#[allow(unused_variables)]
pub fn bind_column_ref(b: &mut Binder, parts: &[Ident], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-10", "column reference binding (expr::bind_column_ref)"))
}

/// `CAST(e AS t)` / `e::t` (module docs).
#[allow(unused_variables)]
pub fn bind_cast(b: &mut Binder, operand: &Expr, data_type: &DataType, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-10", "cast binding (expr::bind_cast)"))
}
