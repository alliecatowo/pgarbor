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
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{self, CastContext};
use pgarbor_types::ops::{self, ResolvedOperator};
use pgarbor_types::{oids, registry, TypeEnv, TypeId};
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator, Value, ValueWithSpan};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::bind_arg;
use super::expr_func;
use super::literal;
use crate::diag;
use crate::ir::{
    ArrayExpr, BoolExpr, BoolOp, BoolTestKind, BooleanTest, BoundExpr, CoercionForm, DistinctFrom,
    ExprKind, NullTest, OpExpr, OpRef, RowCompare, RowCompareKind, ScalarArrayOp,
};

/// PG operator name for a binary operator that is a plain `make_op` call (`None` for the forms
/// handled separately: AND/OR, OVERLAPS, user-defined and dialect-only operators).
fn binary_op_name(op: &BinaryOperator) -> Option<&'static str> {
    use BinaryOperator as B;
    Some(match op {
        B::Plus => "+",
        B::Minus => "-",
        B::Multiply => "*",
        B::Divide => "/",
        B::Modulo => "%",
        B::StringConcat => "||",
        B::Gt => ">",
        B::Lt => "<",
        B::GtEq => ">=",
        B::LtEq => "<=",
        B::Eq => "=",
        B::NotEq => "<>",
        B::BitwiseOr => "|",
        B::BitwiseAnd => "&",
        B::BitwiseXor | B::PGBitwiseXor => "#",
        B::PGBitwiseShiftLeft => "<<",
        B::PGBitwiseShiftRight => ">>",
        B::PGExp => "^",
        B::PGOverlap => "&&",
        B::PGRegexMatch => "~",
        B::PGRegexIMatch => "~*",
        B::PGRegexNotMatch => "!~",
        B::PGRegexNotIMatch => "!~*",
        B::PGLikeMatch => "~~",
        B::PGILikeMatch => "~~*",
        B::PGNotLikeMatch => "!~~",
        B::PGNotILikeMatch => "!~~*",
        B::PGStartsWith => "^@",
        B::Arrow => "->",
        B::LongArrow => "->>",
        B::HashArrow => "#>",
        B::HashLongArrow => "#>>",
        B::AtAt => "@@",
        B::AtArrow => "@>",
        B::ArrowAt => "<@",
        B::HashMinus => "#-",
        B::AtQuestion => "@?",
        B::Question => "?",
        B::QuestionAnd => "?&",
        B::QuestionPipe => "?|",
        B::DoubleHash => "##",
        B::LtDashGt => "<->",
        B::AndLt => "&<",
        B::AndGt => "&>",
        B::LtLtPipe => "<<|",
        B::PipeGtGt => "|>>",
        B::AndLtPipe => "&<|",
        B::PipeAndGt => "|&>",
        B::LtCaret => "<^",
        B::GtCaret => ">^",
        B::QuestionHash => "?#",
        B::QuestionDash => "?-",
        B::QuestionDashPipe => "?-|",
        B::QuestionDoublePipe => "?||",
        B::At => "@",
        B::TildeEq => "~=",
        _ => return None,
    })
}

/// PG operator name for a prefix operator handled by `make_op` (`None` for NOT, `-` of a literal
/// and the removed factorial operators).
fn unary_op_name(op: &UnaryOperator) -> Option<&'static str> {
    use UnaryOperator as U;
    Some(match op {
        U::Plus => "+",
        U::Minus => "-",
        U::BitwiseNot => "~",
        U::PGSquareRoot => "|/",
        U::PGCubeRoot => "||/",
        U::PGAbs => "@",
        U::AtDashAt => "@-@",
        U::DoubleAt => "@@",
        U::Hash => "#",
        U::QuestionDash => "?-",
        U::QuestionPipe => "?|",
        _ => return None,
    })
}

/// Operator name of `OPERATOR(schema.op)`; `pg_catalog` qualification is transparent.
fn custom_operator_name(parts: &[String]) -> String {
    if parts.len() == 2 && parts[0] == "pg_catalog" {
        parts[1].clone()
    } else {
        parts.join(".")
    }
}

/// Peel parentheses (`Expr::Nested`) off an AST node.
fn strip_nested(e: &Expr) -> &Expr {
    match e {
        Expr::Nested(inner) => strip_nested(inner),
        other => other,
    }
}

/// Positional items of a row constructor `(a, b)` written at the AST level.
fn tuple_items(e: &Expr) -> Option<&[Expr]> {
    match strip_nested(e) {
        Expr::Tuple(v) => Some(v.as_slice()),
        _ => None,
    }
}

fn is_row(e: &BoundExpr) -> bool {
    matches!(e.kind, ExprKind::Row(_))
}

/// Field expressions of a bound row constructor.
fn take_row(e: BoundExpr) -> Result<Vec<BoundExpr>> {
    match e.kind {
        ExprKind::Row(row) => Ok((*row).args),
        _ => Err(PgError::internal("binder: row expression expected")),
    }
}

/// `AND`/`OR` flattening: a flattenable left operand contributes its arguments.
fn push_flat(args: &mut Vec<BoundExpr>, e: BoundExpr, op: BoolOp, flatten: bool) {
    let ty = e.ty;
    match e.kind {
        ExprKind::Bool(be) if flatten && be.op == op => {
            let be = *be;
            args.extend(be.args);
        }
        kind => args.push(BoundExpr::new(kind, ty)),
    }
}

/// Whether the left operand's AST is a binary connective of the same kind (`makeAndExpr` /
/// `makeOrExpr` only flatten the RAW left-nested chain; a BETWEEN expansion is not flattened).
fn same_connective(e: &Expr, is_and: bool) -> bool {
    match strip_nested(e) {
        Expr::BinaryOp {
            op: BinaryOperator::And,
            ..
        } => is_and,
        Expr::BinaryOp {
            op: BinaryOperator::Or,
            ..
        } => !is_and,
        _ => false,
    }
}

/// Convert an operand to the resolved operand type (`try_coerce(.., Implicit)`; the no-path case is
/// reported by `coerce_strict`).
fn coerce_operand(b: &mut Binder, arg: BoundArg, target: Oid) -> Result<BoundExpr> {
    if arg.ty().oid == target {
        return Ok(arg.expr);
    }
    coerce::coerce_strict(
        b,
        &arg,
        TypeId::of(target),
        CastContext::Implicit,
        CoercionForm::Implicit,
    )
}

/// Bind an operand expression and reject a set-returning call where the context forbids it.
fn bind_operand(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundArg> {
    let arg = bind_arg(b, e, ec)?;
    if !ec.srf_ok {
        if let ExprKind::FuncCall(f) = &arg.expr.kind {
            if f.retset {
                return Err(diag::srf_not_allowed(ec.clause, arg.pos));
            }
        }
    }
    Ok(arg)
}

/// Resolve `name` for the given operand types (`left` `None` for prefix operators) and coerce both
/// operands to the resolved types. Returns the resolution and the coerced operands (left first).
fn resolve_coerce(
    b: &mut Binder,
    name: &str,
    left: Option<BoundArg>,
    right: BoundArg,
    pos: Option<u32>,
) -> Result<(ResolvedOperator, Vec<BoundExpr>)> {
    let cands = ops::builtin_operator_candidates(name)?;
    let env: &dyn TypeEnv = b.env;
    let lt = left.as_ref().map(|l| l.ty().oid);
    let resolved = ops::resolve_operator(env, name, lt, right.ty().oid, &cands)
        .map_err(|e| diag::with_position(e, pos))?;
    let mut args = Vec::with_capacity(2);
    if let Some(l) = left {
        let target = resolved.left.ok_or_else(|| {
            PgError::internal("binder: binary operator resolved without a left operand type")
        })?;
        args.push(coerce_operand(b, l, target)?);
    }
    args.push(coerce_operand(b, right, resolved.right)?);
    Ok((resolved, args))
}

/// Resolve an operator for a pair of operand types (no expressions needed).
fn resolve_full(
    b: &Binder,
    name: &str,
    left: Option<Oid>,
    right: Oid,
    pos: Option<u32>,
) -> Result<(ResolvedOperator, OpRef)> {
    let cands = ops::builtin_operator_candidates(name)?;
    let env: &dyn TypeEnv = b.env;
    let resolved = ops::resolve_operator(env, name, left, right, &cands)
        .map_err(|e| diag::with_position(e, pos))?;
    let op = op_ref(b, resolved.oid)?;
    Ok((resolved, op))
}

/// Resolve and build one operator application (`left` is `None` for prefix operators). `name` is the
/// PG operator name. `pos` is the position of the operator token (falls back to the left operand's).
pub fn make_op(
    b: &mut Binder,
    name: &str,
    left: Option<BoundArg>,
    right: BoundArg,
    pos: Option<u32>,
) -> Result<BoundExpr> {
    let pos = pos.or_else(|| left.as_ref().and_then(|l| l.pos));
    let (resolved, args) = resolve_coerce(b, name, left, right, pos)?;
    let op = op_ref(b, resolved.oid)?;
    Ok(BoundExpr::new(
        ExprKind::Op(Box::new(OpExpr { op, args })),
        resolved.result,
    ))
}

/// Build an [`OpRef`] for an operator OID found by resolution: name from `catalog_data`, strictness
/// and volatility from the implementing proc. 42883 never (OID is valid by construction); XX000 if
/// the OID is unknown.
pub fn op_ref(_b: &Binder, op_oid: Oid) -> Result<OpRef> {
    let op = catalog_data::builtin_operator(op_oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for operator {op_oid}")))?;
    let proc = catalog_data::builtin_proc(op.proc).ok_or_else(|| {
        PgError::internal(format!("cache lookup failed for function {}", op.proc))
    })?;
    Ok(OpRef {
        op: op_oid,
        proc: op.proc,
        name: op.name.to_string(),
        volatility: proc.volatility,
        strict: proc.strict,
    })
}

/// Resolve the `=` / `<` ... operator for a given operand type pair (planner-independent helper for
/// IN lists, row comparison, NULLIF, DISTINCT, ANY/ALL, GROUP BY, IN-subquery).
pub fn resolve_named_op(
    b: &mut Binder,
    name: &str,
    left: pgarbor_types::TypeId,
    right: pgarbor_types::TypeId,
    pos: Option<u32>,
) -> Result<OpRef> {
    resolve_full(b, name, Some(left.oid), right.oid, pos).map(|(_, op)| op)
}

/// `BinaryOp` (arithmetic, comparison, string, JSON, AND/OR, ...). See "Operator spelling".
pub fn bind_binary_op(
    b: &mut Binder,
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    match op {
        BinaryOperator::And => bind_connective(b, left, right, true, ec),
        BinaryOperator::Or => bind_connective(b, left, right, false, ec),
        BinaryOperator::Overlaps => bind_overlaps(b, left, right, op_pos, ec),
        BinaryOperator::PGCustomBinaryOperator(parts) => {
            let name = custom_operator_name(parts);
            bind_plain_binary(b, &name, left, right, op_pos, ec)
        }
        other => match binary_op_name(other) {
            Some(name) => bind_plain_binary(b, name, left, right, op_pos, ec),
            None => Err(diag::syntax_error_near(&other.to_string(), op_pos)),
        },
    }
}

/// `a AND b` / `a OR b` (flattening per [`same_connective`]).
fn bind_connective(
    b: &mut Binder,
    left: &Expr,
    right: &Expr,
    is_and: bool,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let what = if is_and { "AND" } else { "OR" };
    let op = if is_and { BoolOp::And } else { BoolOp::Or };
    let l = bind_operand(b, left, ec)?;
    let lb = coerce::coerce_to_boolean(b, &l, what)?;
    let r = bind_operand(b, right, ec)?;
    let rb = coerce::coerce_to_boolean(b, &r, what)?;
    let flatten = same_connective(left, is_and);
    let mut args = Vec::with_capacity(2);
    push_flat(&mut args, lb, op, flatten);
    args.push(rb);
    Ok(BoundExpr::new(
        ExprKind::Bool(Box::new(BoolExpr { op, args })),
        TypeId::BOOL,
    ))
}

/// Binary operator with both operands bound; rows take the row-comparison forms.
fn bind_plain_binary(
    b: &mut Binder,
    name: &str,
    left: &Expr,
    right: &Expr,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let l = bind_operand(b, left, ec)?;
    let r = bind_operand(b, right, ec)?;
    if is_row(&l.expr) && is_row(&r.expr) {
        match name {
            "=" | "<>" => return row_equality(b, name, l, r, op_pos),
            "<" | "<=" | ">" | ">=" => return row_order(b, name, l, r, op_pos),
            _ => {}
        }
    }
    make_op(b, name, Some(l), r, op_pos)
}

/// `(a1,..,an) = (b1,..,bn)` is the AND of pairwise `=`; `<>` the OR of pairwise `<>`. Operands that
/// are not both rows are a plain operator call.
fn row_equality(
    b: &mut Binder,
    name: &str,
    l: BoundArg,
    r: BoundArg,
    pos: Option<u32>,
) -> Result<BoundExpr> {
    if !(is_row(&l.expr) && is_row(&r.expr)) {
        return make_op(b, name, Some(l), r, pos);
    }
    let pos = pos.or(l.pos);
    let la = take_row(l.expr)?;
    let ra = take_row(r.expr)?;
    if la.len() != ra.len() {
        return Err(diag::unequal_row_entries(pos));
    }
    let mut parts = Vec::with_capacity(la.len());
    for (x, y) in la.into_iter().zip(ra) {
        parts.push(make_op(
            b,
            name,
            Some(BoundArg::new(x, pos)),
            BoundArg::new(y, pos),
            pos,
        )?);
    }
    Ok(if name == "=" {
        BoundExpr::and_all(parts)
    } else {
        BoundExpr::or_all(parts)
    })
}

/// `(a1,..,an) < (b1,..,bn)` and friends: `ExprKind::RowCompare` with the `<`-family operator per
/// column pair and the `=` operator per column pair.
fn row_order(
    b: &mut Binder,
    name: &str,
    l: BoundArg,
    r: BoundArg,
    pos: Option<u32>,
) -> Result<BoundExpr> {
    let pos = pos.or(l.pos);
    let kind = match name {
        "<" => RowCompareKind::Lt,
        "<=" => RowCompareKind::Le,
        ">" => RowCompareKind::Gt,
        _ => RowCompareKind::Ge,
    };
    let la = take_row(l.expr)?;
    let ra = take_row(r.expr)?;
    if la.len() != ra.len() {
        return Err(diag::unequal_row_entries(pos));
    }
    let mut lefts = Vec::with_capacity(la.len());
    let mut rights = Vec::with_capacity(la.len());
    let mut ops_v = Vec::with_capacity(la.len());
    let mut eq_ops = Vec::with_capacity(la.len());
    for (x, y) in la.into_iter().zip(ra) {
        let (resolved, args) = resolve_coerce(
            b,
            name,
            Some(BoundArg::new(x, pos)),
            BoundArg::new(y, pos),
            pos,
        )?;
        let mut it = args.into_iter();
        let ll = it
            .next()
            .ok_or_else(|| PgError::internal("binder: row comparison operand missing"))?;
        let rr = it
            .next()
            .ok_or_else(|| PgError::internal("binder: row comparison operand missing"))?;
        let (lt, rt) = (ll.ty.oid, rr.ty.oid);
        let op = op_ref(b, resolved.oid)?;
        let (_, eq) = resolve_full(b, "=", Some(lt), rt, pos)?;
        lefts.push(ll);
        rights.push(rr);
        ops_v.push(op);
        eq_ops.push(eq);
    }
    Ok(BoundExpr::new(
        ExprKind::RowCompare(Box::new(RowCompare {
            kind,
            left: lefts,
            right: rights,
            ops: ops_v,
            eq_ops,
        })),
        TypeId::BOOL,
    ))
}

/// `(a, b) OVERLAPS (c, d)` is the function call `overlaps(a, b, c, d)`.
fn bind_overlaps(
    b: &mut Binder,
    left: &Expr,
    right: &Expr,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let lhs = match tuple_items(left) {
        Some(v) if v.len() == 2 => v,
        _ => {
            return Err(diag::syntax(
                "wrong number of parameters on left side of OVERLAPS expression",
                op_pos,
            ))
        }
    };
    let rhs = match tuple_items(right) {
        Some(v) if v.len() == 2 => v,
        _ => {
            return Err(diag::syntax(
                "wrong number of parameters on right side of OVERLAPS expression",
                op_pos,
            ))
        }
    };
    let mut args = Vec::with_capacity(4);
    for e in lhs.iter().chain(rhs.iter()) {
        args.push(bind_operand(b, e, ec)?);
    }
    expr_func::call_function_by_name(b, "overlaps", args, op_pos, ec)
}

/// `UnaryOp` including `NOT`.
pub fn bind_unary_op(
    b: &mut Binder,
    op: &UnaryOperator,
    operand: &Expr,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    match op {
        UnaryOperator::Not => {
            let a = bind_operand(b, operand, ec)?;
            let arg = coerce::coerce_to_boolean(b, &a, "NOT")?;
            Ok(BoundExpr::not(arg))
        }
        UnaryOperator::Minus => {
            // `-5` is a negative constant (PG `doNegate`), not a call to the prefix `-` operator.
            if let Expr::Value(v) = strip_nested(operand) {
                if matches!(v.value, Value::Number(..)) {
                    return literal::bind_value(b, v, true);
                }
            }
            prefix_op(b, "-", operand, op_pos, ec)
        }
        UnaryOperator::BangNot | UnaryOperator::PGPostfixFactorial => {
            Err(diag::syntax_error_near("!", op_pos))
        }
        UnaryOperator::PGPrefixFactorial => Err(diag::syntax_error_near("!!", op_pos)),
        other => match unary_op_name(other) {
            Some(name) => prefix_op(b, name, operand, op_pos, ec),
            None => Err(diag::syntax_error_near(&other.to_string(), op_pos)),
        },
    }
}

fn prefix_op(
    b: &mut Binder,
    name: &str,
    operand: &Expr,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let a = bind_operand(b, operand, ec)?;
    let pos = op_pos.or_else(|| b.pos(operand));
    make_op(b, name, None, a, pos)
}

/// `Between { expr, negated, low, high }`: `x >= a AND x <= b`, `NOT BETWEEN` is `x < a OR x > b`.
/// The left operand is bound once and cloned. (sqlparser 0.63 has no SYMMETRIC flag.)
pub fn bind_between(
    b: &mut Binder,
    e: &Expr,
    negated: bool,
    low: &Expr,
    high: &Expr,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let pos = b.pos(e);
    let x = bind_operand(b, e, ec)?;
    let lo = bind_operand(b, low, ec)?;
    let hi = bind_operand(b, high, ec)?;
    let x2 = x.clone();
    if negated {
        let lt = make_op(b, "<", Some(x), lo, pos)?;
        let gt = make_op(b, ">", Some(x2), hi, pos)?;
        Ok(BoundExpr::or_all(vec![lt, gt]))
    } else {
        let ge = make_op(b, ">=", Some(x), lo, pos)?;
        let le = make_op(b, "<=", Some(x2), hi, pos)?;
        Ok(BoundExpr::and_all(vec![ge, le]))
    }
}

/// `InList { expr, list, negated }`.
pub fn bind_in_list(
    b: &mut Binder,
    e: &Expr,
    list: &[Expr],
    negated: bool,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let pos = b.pos(e);
    let x = bind_operand(b, e, ec)?;
    if list.is_empty() {
        return Err(diag::syntax_error_near(")", pos));
    }
    let mut items = Vec::with_capacity(list.len());
    for it in list {
        items.push(bind_operand(b, it, ec)?);
    }
    bind_in_items(b, x, items, negated, pos)
}

/// `x IN (items)` (`negated`: `x NOT IN (items)`, operator `<>` combined with AND).
///
/// Mirrors PG `transformAExprIn`: a row-valued left side is an OR (AND) of row comparisons; a single
/// item is a plain comparison; otherwise the non-column items become a `ScalarArrayOp` over an
/// `ArrayExpr` of the common type (`x = ANY (ARRAY[..])`) and column items stay separate terms. When
/// no array type exists, every item is its own comparison.
fn bind_in_items(
    b: &mut Binder,
    x: BoundArg,
    items: Vec<BoundArg>,
    negated: bool,
    pos: Option<u32>,
) -> Result<BoundExpr> {
    let name = if negated { "<>" } else { "=" };
    if is_row(&x.expr) {
        let mut parts = Vec::with_capacity(items.len());
        for it in items {
            parts.push(row_equality(b, name, x.clone(), it, pos)?);
        }
        return Ok(if negated {
            BoundExpr::and_all(parts)
        } else {
            BoundExpr::or_all(parts)
        });
    }
    let mut items = items;
    if items.len() == 1 {
        let it = items
            .pop()
            .ok_or_else(|| PgError::internal("binder: IN list item missing"))?;
        return row_equality(b, name, x, it, pos);
    }
    let mut all = Vec::with_capacity(items.len() + 1);
    all.push(x.clone());
    all.extend(items.iter().cloned());
    let (_, common) = coerce::coerce_to_common(b, all, "IN")?;
    let env: &dyn TypeEnv = b.env;
    let n_var = items
        .iter()
        .filter(|it| matches!(it.expr.kind, ExprKind::Column(_)))
        .count();
    let n_non = items.len() - n_var;
    if n_non >= 2
        && common.oid != oids::RECORD
        && registry::array_type_of(env, common.oid).is_some()
    {
        let cands = ops::builtin_operator_candidates(name)?;
        let resolved = ops::resolve_operator(env, name, Some(x.ty().oid), common.oid, &cands)
            .map_err(|e| diag::with_position(e, pos))?;
        if let Some(arr) = registry::array_type_of(env, resolved.right) {
            let lt = resolved.left.unwrap_or(x.ty().oid);
            let op = op_ref(b, resolved.oid)?;
            let left_expr = coerce_operand(b, x.clone(), lt)?;
            let mut elements = Vec::with_capacity(n_non);
            let mut vars = Vec::with_capacity(n_var);
            for it in items {
                if matches!(it.expr.kind, ExprKind::Column(_)) {
                    vars.push(it);
                } else {
                    elements.push(coerce_operand(b, it, resolved.right)?);
                }
            }
            let array = BoundExpr::new(
                ExprKind::Array(Box::new(ArrayExpr {
                    elements,
                    elem_type: resolved.right,
                    multidims: false,
                })),
                TypeId::of(arr),
            );
            let sao = BoundExpr::new(
                ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp {
                    op,
                    use_or: !negated,
                    left: left_expr,
                    right: array,
                })),
                TypeId::BOOL,
            );
            let mut parts = vec![sao];
            for v in vars {
                parts.push(make_op(b, name, Some(x.clone()), v, pos)?);
            }
            return Ok(if negated {
                BoundExpr::and_all(parts)
            } else {
                BoundExpr::or_all(parts)
            });
        }
    }
    let mut parts = Vec::with_capacity(items.len());
    for it in items {
        parts.push(make_op(b, name, Some(x.clone()), it, pos)?);
    }
    Ok(if negated {
        BoundExpr::and_all(parts)
    } else {
        BoundExpr::or_all(parts)
    })
}

/// `Like`/`ILike`/`SimilarTo` (module docs). `kind`: 0 = LIKE, 1 = ILIKE, 2 = SIMILAR TO.
///
/// `escape` is a literal ESCAPE character (the only form the signature can carry).
pub fn bind_like(
    b: &mut Binder,
    kind: u8,
    negated: bool,
    e: &Expr,
    pattern: &Expr,
    escape: Option<&ValueWithSpan>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let pos = b.pos(e);
    let subject = bind_operand(b, e, ec)?;
    let pat = bind_operand(b, pattern, ec)?;
    let esc = match escape {
        Some(v) => Some(bind_operand(b, &Expr::Value(v.clone()), ec)?),
        None => None,
    };
    let (opname, pattern_arg) = match kind {
        0 | 1 => {
            let name = match (kind, negated) {
                (0, false) => "~~",
                (0, true) => "!~~",
                (_, false) => "~~*",
                (_, true) => "!~~*",
            };
            let p = match esc {
                Some(x) => {
                    let f =
                        expr_func::call_function_by_name(b, "like_escape", vec![pat, x], pos, ec)?;
                    BoundArg::new(f, pos)
                }
                None => pat,
            };
            (name, p)
        }
        2 => {
            let mut args = vec![pat];
            if let Some(x) = esc {
                args.push(x);
            }
            let f = expr_func::call_function_by_name(b, "similar_to_escape", args, pos, ec)?;
            (if negated { "!~" } else { "~" }, BoundArg::new(f, pos))
        }
        _ => return Err(PgError::internal("binder: unknown LIKE kind")),
    };
    make_op(b, opname, Some(subject), pattern_arg, pos)
}

/// `AnyOp`/`AllOp` against an array expression (`compare_op`, `right`, `is_some`).
///
/// The operator is resolved for (left type, array element type). An `unknown` right side (a string
/// literal such as `'{1,2}'`) is converted to the array type of the left operand.
pub fn bind_any_all_array(
    b: &mut Binder,
    left: &Expr,
    compare_op: &BinaryOperator,
    right: &Expr,
    is_all: bool,
    op_pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let name = match binary_op_name(compare_op) {
        Some(n) => n,
        None => return Err(diag::syntax_error_near(&compare_op.to_string(), op_pos)),
    };
    let l = bind_operand(b, left, ec)?;
    let r = bind_operand(b, right, ec)?;
    let pos = op_pos.or(l.pos);
    let rpos = r.pos;
    let r_expr = if r.ty().oid == oids::UNKNOWN {
        let base = if l.is_unknown() {
            oids::TEXT
        } else {
            l.ty().oid
        };
        let env: &dyn TypeEnv = b.env;
        let arr =
            registry::array_type_of(env, base).ok_or_else(|| diag::any_all_needs_array(rpos))?;
        coerce_operand(b, r, arr)?
    } else {
        r.expr
    };
    let r_oid = r_expr.ty.oid;
    let env: &dyn TypeEnv = b.env;
    let elem = match registry::element_type_of(env, r_oid) {
        Some(e) => e,
        None => return Err(diag::any_all_needs_array(rpos)),
    };
    let cands = ops::builtin_operator_candidates(name)?;
    let resolved = ops::resolve_operator(env, name, Some(l.ty().oid), elem, &cands)
        .map_err(|e| diag::with_position(e, pos))?;
    if resolved.result.oid != oids::BOOL {
        return Err(diag::with_position(
            PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                "op ANY/ALL (array) requires operator to yield boolean",
            ),
            pos,
        ));
    }
    let lt = resolved.left.unwrap_or(l.ty().oid);
    let op = op_ref(b, resolved.oid)?;
    let left_expr = coerce_operand(b, l, lt)?;
    let right_expr = if elem == resolved.right {
        r_expr
    } else {
        let arr = registry::array_type_of(env, resolved.right)
            .ok_or_else(|| PgError::internal("binder: operator right type has no array type"))?;
        coerce_operand(b, BoundArg::new(r_expr, rpos), arr)?
    };
    Ok(BoundExpr::new(
        ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp {
            op,
            use_or: !is_all,
            left: left_expr,
            right: right_expr,
        })),
        TypeId::BOOL,
    ))
}

/// `IS [NOT] DISTINCT FROM`.
pub fn bind_distinct_from(
    b: &mut Binder,
    left: &Expr,
    right: &Expr,
    negated: bool,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let l = bind_operand(b, left, ec)?;
    let r = bind_operand(b, right, ec)?;
    let pos = b.pos(left);
    if is_row(&l.expr) && is_row(&r.expr) {
        let la = take_row(l.expr)?;
        let ra = take_row(r.expr)?;
        if la.len() != ra.len() {
            return Err(diag::unequal_row_entries(pos));
        }
        let mut parts = Vec::with_capacity(la.len());
        for (x, y) in la.into_iter().zip(ra) {
            parts.push(distinct_pair(
                b,
                BoundArg::new(x, pos),
                BoundArg::new(y, pos),
                negated,
                pos,
            )?);
        }
        return Ok(if negated {
            BoundExpr::and_all(parts)
        } else {
            BoundExpr::or_all(parts)
        });
    }
    distinct_pair(b, l, r, negated, pos)
}

fn distinct_pair(
    b: &mut Binder,
    l: BoundArg,
    r: BoundArg,
    negated: bool,
    pos: Option<u32>,
) -> Result<BoundExpr> {
    let (resolved, args) = resolve_coerce(b, "=", Some(l), r, pos)?;
    if resolved.result.oid != oids::BOOL {
        return Err(diag::with_position(
            PgError::new(
                SqlState::DATATYPE_MISMATCH,
                "IS DISTINCT FROM requires = operator to yield boolean",
            ),
            pos,
        ));
    }
    let eq = op_ref(b, resolved.oid)?;
    let mut it = args.into_iter();
    let left = it
        .next()
        .ok_or_else(|| PgError::internal("binder: DISTINCT operand missing"))?;
    let right = it
        .next()
        .ok_or_else(|| PgError::internal("binder: DISTINCT operand missing"))?;
    Ok(BoundExpr::new(
        ExprKind::DistinctFrom(Box::new(DistinctFrom {
            left,
            right,
            eq,
            negated,
        })),
        TypeId::BOOL,
    ))
}

/// `IS [NOT] NULL|TRUE|FALSE|UNKNOWN`. `test`: "null", "true", "false", "unknown".
pub fn bind_is_test(
    b: &mut Binder,
    e: &Expr,
    test: &str,
    negated: bool,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let a = bind_operand(b, e, ec)?;
    match test {
        "null" => {
            let row_semantics = is_row(&a.expr)
                || a.ty().oid == oids::RECORD
                || registry::type_info(b.env, a.ty().oid)
                    .map(|t| t.is_composite())
                    .unwrap_or(false);
            Ok(BoundExpr::new(
                ExprKind::NullTest(Box::new(NullTest {
                    arg: a.expr,
                    is_null: !negated,
                    row_semantics,
                })),
                TypeId::BOOL,
            ))
        }
        "true" | "false" | "unknown" => {
            let (clause, kind) = match (test, negated) {
                ("true", false) => ("IS TRUE", BoolTestKind::IsTrue),
                ("true", true) => ("IS NOT TRUE", BoolTestKind::IsNotTrue),
                ("false", false) => ("IS FALSE", BoolTestKind::IsFalse),
                ("false", true) => ("IS NOT FALSE", BoolTestKind::IsNotFalse),
                (_, false) => ("IS UNKNOWN", BoolTestKind::IsUnknown),
                (_, true) => ("IS NOT UNKNOWN", BoolTestKind::IsNotUnknown),
            };
            let arg = coerce::coerce_to_boolean(b, &a, clause)?;
            Ok(BoundExpr::new(
                ExprKind::BooleanTest(Box::new(BooleanTest { arg, test: kind })),
                TypeId::BOOL,
            ))
        }
        _ => Err(PgError::internal(format!("binder: unknown IS test {test}"))),
    }
}

/// `AtTimeZone { timestamp, time_zone }` is `timezone(time_zone, timestamp)`.
pub fn bind_at_time_zone(
    b: &mut Binder,
    timestamp: &Expr,
    time_zone: &Expr,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let pos = b.pos(timestamp);
    let zone = bind_operand(b, time_zone, ec)?;
    let ts = bind_operand(b, timestamp, ec)?;
    expr_func::call_function_by_name(b, "timezone", vec![zone, ts], pos, ec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_spelling_common_operators() {
        assert_eq!(binary_op_name(&BinaryOperator::Plus), Some("+"));
        assert_eq!(binary_op_name(&BinaryOperator::NotEq), Some("<>"));
        assert_eq!(binary_op_name(&BinaryOperator::PGLikeMatch), Some("~~"));
        assert_eq!(binary_op_name(&BinaryOperator::PGILikeMatch), Some("~~*"));
        assert_eq!(binary_op_name(&BinaryOperator::PGNotLikeMatch), Some("!~~"));
        assert_eq!(
            binary_op_name(&BinaryOperator::PGNotILikeMatch),
            Some("!~~*")
        );
        assert_eq!(binary_op_name(&BinaryOperator::Spaceship), None);
        assert_eq!(binary_op_name(&BinaryOperator::Xor), None);
    }

    #[test]
    fn unary_spelling() {
        assert_eq!(unary_op_name(&UnaryOperator::Minus), Some("-"));
        assert_eq!(unary_op_name(&UnaryOperator::PGSquareRoot), Some("|/"));
        assert_eq!(unary_op_name(&UnaryOperator::PGPostfixFactorial), None);
        assert_eq!(unary_op_name(&UnaryOperator::Not), None);
    }

    #[test]
    fn custom_operator_names() {
        assert_eq!(
            custom_operator_name(&["pg_catalog".to_string(), "+".to_string()]),
            "+"
        );
        assert_eq!(
            custom_operator_name(&["myschema".to_string(), "+".to_string()]),
            "myschema.+"
        );
    }

    #[test]
    fn left_nested_and_flattens() {
        let inner = BoundExpr::and_all(vec![
            BoundExpr::bool_const(true),
            BoundExpr::bool_const(false),
        ]);
        let mut args = Vec::new();
        push_flat(&mut args, inner, BoolOp::And, true);
        push_flat(&mut args, BoundExpr::bool_const(true), BoolOp::And, true);
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn nested_and_is_not_flattened_when_not_requested() {
        let inner = BoundExpr::and_all(vec![
            BoundExpr::bool_const(true),
            BoundExpr::bool_const(false),
        ]);
        let mut args = Vec::new();
        push_flat(&mut args, inner, BoolOp::And, false);
        assert_eq!(args.len(), 1);
    }
}
