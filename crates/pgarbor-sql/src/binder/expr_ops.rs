//! Operators and operator-like expression forms (SQL-11).
//! PG references: `parse_oper.c` (`make_op`, `oper`, `right_oper`/`left_oper`, `op_error`),
//! `parse_expr.c` (`transformAExprOp`, `transformAExprOpAny/All`, `transformAExprDistinct`,
//! `transformAExprNullIf`, `transformAExprIn`, `transformAExprBetween`, `transformAExprSimilar`,
//! `make_row_comparison_op`, `make_row_distinct_op`, `transformBoolExpr`), `parse_func.c`.
//!
//! ## Operator resolution core: [`make_op`]
//! 1. Candidates: `ops::builtin_operator_candidates(name)` (+ user operators once the catalog has
//!    them) ; call `pgarbor_types::ops::resolve_operator(env, name, left_oid, right_oid, &cands)`
//!    with `unknown` for untyped literals/params. Errors come from there with the PG text
//!    (`operator does not exist: integer + text` 42883 + HINT `No operator matches the given name and
//!    argument types. You might need to add explicit type casts.`; `operator is not unique:
//!    unknown + unknown` 42725). Attach `pos` = the operator token position to errors lacking one.
//! 2. Coerce each argument to the resolved operand types with `coerce::try_coerce(.., Implicit)`
//!    (an `unknown` literal converts via input, so `1 + 'x'` fails with 22P02 at the literal).
//! 3. Result: `ExprKind::Op(OpExpr{op: OpRef{..}, args})` typed `ResolvedOperator::result` (for
//!    polymorphic operators the actual result type). `OpRef.strict/volatility` come from
//!    `catalog_data::builtin_proc(proc)`.
//! 4. Set-returning operands are not allowed (0A000 `set-returning functions are not allowed in X`).
//!
//! ## Operator spelling
//! `sqlparser::ast::BinaryOperator` -> PG operator name: Plus `+`, Minus `-`, Multiply `*`,
//! Divide `/`, Modulo `%`, StringConcat `||`, Gt `>`, Lt `<`, GtEq `>=`, LtEq `<=`, Eq `=`, NotEq
//! `<>`, BitwiseOr `|`, BitwiseAnd `&`, BitwiseXor `#`, PGBitwiseXor `#`, PGBitwiseShiftLeft `<<`,
//! PGBitwiseShiftRight `>>`, PGExp `^`, PGOverlap `&&`, PGRegexMatch `~`, PGRegexIMatch `~*`,
//! PGRegexNotMatch `!~`, PGRegexNotIMatch `!~*`, PGLikeMatch `~~`, PGILikeMatch `~~*`,
//! PGNotLikeMatch `!~~`, PGNotILikeMatch `!~~*`, PGStartsWith `^@`, Arrow `->`, LongArrow `->>`,
//! HashArrow `#>`, HashLongArrow `#>>`, AtAt `@@`, AtArrow `@>`, ArrowAt `<@`, Question `?`,
//! QuestionAnd `?&`, QuestionPipe `?|`, HashMinus `#-`, AtQuestion `@?`, DoubleHash `##`,
//! LtDashGt `<->`, AndLt `&<`, AndGt `&>`, QuestionDashPipe `?-|`, QuestionDoublePipe `?||`,
//! `PGCustomBinaryOperator(parts)` -> the joined text (user-defined operators; 42883 if absent),
//! `Spaceship`/`DuckIntegerDivide`/`MyIntegerDivide`/`Xor`/`Overlaps`/`Assignment`/... -> 42601
//! (other dialects) except `Overlaps` (`(a,b) OVERLAPS (c,d)` -> function `overlaps`).
//! `And`/`Or` -> `BoolExpr` (flatten LEFT-nested same-op chains only, like `makeAndExpr`/`makeOrExpr`: `a AND b AND c`
//! is one node, but a BETWEEN expansion or a parenthesized AND on the right stays nested (oracle viewdef
//! `((a) AND (b) AND ((c >= 1) AND (c <= 3)))`); operands via
//! `coerce::coerce_to_boolean` with `what` = `AND`/`OR`). Unary: Plus `+`, Minus `-` (a literal
//! operand is folded by `literal`), Not -> `BoolExpr(Not)` (what = `NOT`), PGBitwiseNot `~`,
//! PGSquareRoot `|/`, PGCubeRoot `||/`, PGPostfixFactorial `!` (removed in PG14: 42601),
//! PGPrefixFactorial `!!` (removed), PGAbs `@`.
//!
//! ## Comparison of rows
//! `(a,b) = (c,d)` -> AND of pairwise `=`; `<>` -> OR of pairwise `<>`; `<`,`<=`,`>`,`>=` ->
//! `ExprKind::RowCompare`; unequal arity -> 42601 `unequal number of entries in row expressions`
//! (`diag::unequal_row_entries`); `IS [NOT] DISTINCT FROM` on rows expands to AND/OR of
//! `DistinctFrom`.
//!
//! ## `x IN (list)` -> `ScalarArrayOp{use_or: true, op "="}` over an `ArrayExpr` of the coerced list
//! (common type via `coerce_to_common`, context `IN`); `NOT IN` -> `use_or: false, op "<>"`
//! (oracle EXPLAIN: `(a <> ALL ('{1,2}'::integer[]))`, i.e. `<> ALL`, not `NOT (= ANY)`). A single-element list becomes plain `x = e` / `x <> e`.
//! Row-valued left side with a list -> OR of row equalities.
//!
//! ## `BETWEEN`: `x BETWEEN a AND b` -> `x >= a AND x <= b` (the left operand is bound once and
//! cloned; volatile left operand is evaluated twice exactly like PG); `NOT BETWEEN` ->
//! `x < a OR x > b`; `BETWEEN SYMMETRIC` -> `(x>=a AND x<=b) OR (x>=b AND x<=a)`.
//!
//! ## `LIKE`/`ILIKE`/`SIMILAR TO`/`RLIKE`
//! `a LIKE b` -> operator `~~` (`NOT LIKE` -> `!~~`); `ILIKE` -> `~~*`/`!~~*`; `SIMILAR TO` ->
//! function call `similar_to_escape(b[, esc])` then operator `~` (negated `!~`) exactly as
//! `transformAExprSimilar`; `LIKE ... ESCAPE c` -> function `like_escape(pattern, escape)` on the
//! pattern; `RLIKE`/`REGEXP` are MySQL: 42601. Operand types: text-ish; `bytea LIKE bytea`
//! resolves to `~~` over bytea; unknown literals become text.
//!
//! ## `= ANY (array)` / `<op> ALL (array)` (`AnyOp`/`AllOp` with a non-subquery right side)
//! -> `ScalarArrayOp`. Right operand must be an array: else 42809 `op ANY/ALL (array) requires
//! array on right side` (`diag::any_all_needs_array`). The operator is resolved with left type and
//! the ARRAY ELEMENT type. Subquery right sides are `expr_sublink`.
//!
//! ## `IS [NOT] NULL` -> `NullTest` (`row_semantics` true when the argument is a composite/row);
//! `IS [NOT] TRUE|FALSE|UNKNOWN` -> `BooleanTest` with the argument coerced to boolean.
//! ## `AT TIME ZONE` -> function `timezone(zone, value)` (arguments swapped!): resolve via
//! `expr_func::call_function_by_name("timezone", ..)`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator};

use super::context::{Binder, BoundArg, ExprCtx};
use crate::ir::{BoundExpr, OpRef};

/// Resolve and build one operator application (`left` is `None` for prefix operators). `name` is the
/// PG operator name. `pos` is the position of the operator token (falls back to the left operand's).
#[allow(unused_variables)]
pub fn make_op(b: &mut Binder, name: &str, left: Option<BoundArg>, right: BoundArg, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "operator resolution (expr_ops::make_op)"))
}

/// Build an [`OpRef`] for an operator OID found by resolution: name from `catalog_data`, strictness
/// and volatility from the implementing proc. 42883 never (OID is valid by construction); XX000 if
/// the OID is unknown.
#[allow(unused_variables)]
pub fn op_ref(b: &Binder, op_oid: pgarbor_common::Oid) -> Result<OpRef> {
    Err(PgError::unimplemented("SQL-11", "OpRef construction (expr_ops::op_ref)"))
}

/// Resolve the `=` / `<` ... operator for a given operand type pair (planner-independent helper for
/// IN lists, row comparison, NULLIF, DISTINCT, ANY/ALL, GROUP BY, IN-subquery).
#[allow(unused_variables)]
pub fn resolve_named_op(b: &mut Binder, name: &str, left: pgarbor_types::TypeId, right: pgarbor_types::TypeId, pos: Option<u32>) -> Result<OpRef> {
    Err(PgError::unimplemented("SQL-11", "named operator lookup (expr_ops::resolve_named_op)"))
}

/// `BinaryOp` (arithmetic, comparison, string, JSON, AND/OR, ...). See "Operator spelling".
#[allow(unused_variables)]
pub fn bind_binary_op(b: &mut Binder, left: &Expr, op: &BinaryOperator, right: &Expr, op_pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "binary operators (expr_ops::bind_binary_op)"))
}

/// `UnaryOp` including `NOT`.
#[allow(unused_variables)]
pub fn bind_unary_op(b: &mut Binder, op: &UnaryOperator, operand: &Expr, op_pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "unary operators (expr_ops::bind_unary_op)"))
}

/// `Between { expr, negated, low, high }` (incl. SYMMETRIC when sqlparser exposes it).
#[allow(unused_variables)]
pub fn bind_between(b: &mut Binder, e: &Expr, negated: bool, low: &Expr, high: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "BETWEEN (expr_ops::bind_between)"))
}

/// `InList { expr, list, negated }`.
#[allow(unused_variables)]
pub fn bind_in_list(b: &mut Binder, e: &Expr, list: &[Expr], negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "IN (list) (expr_ops::bind_in_list)"))
}

/// `Like`/`ILike`/`SimilarTo` (module docs). `kind`: 0 = LIKE, 1 = ILIKE, 2 = SIMILAR TO.
#[allow(unused_variables)]
pub fn bind_like(b: &mut Binder, kind: u8, negated: bool, e: &Expr, pattern: &Expr, escape: Option<&sqlparser::ast::ValueWithSpan>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "LIKE/ILIKE/SIMILAR TO (expr_ops::bind_like)"))
}

/// `AnyOp`/`AllOp` against an array expression (`compare_op`, `right`, `is_some`).
#[allow(unused_variables)]
pub fn bind_any_all_array(b: &mut Binder, left: &Expr, compare_op: &BinaryOperator, right: &Expr, is_all: bool, op_pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "ANY/ALL (array) (expr_ops::bind_any_all_array)"))
}

/// `IS [NOT] DISTINCT FROM`.
#[allow(unused_variables)]
pub fn bind_distinct_from(b: &mut Binder, left: &Expr, right: &Expr, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "IS DISTINCT FROM (expr_ops::bind_distinct_from)"))
}

/// `IS [NOT] NULL|TRUE|FALSE|UNKNOWN`. `test`: "null", "true", "false", "unknown".
#[allow(unused_variables)]
pub fn bind_is_test(b: &mut Binder, e: &Expr, test: &str, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "IS tests (expr_ops::bind_is_test)"))
}

/// `AtTimeZone { timestamp, time_zone }`.
#[allow(unused_variables)]
pub fn bind_at_time_zone(b: &mut Binder, timestamp: &Expr, time_zone: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-11", "AT TIME ZONE (expr_ops::bind_at_time_zone)"))
}
