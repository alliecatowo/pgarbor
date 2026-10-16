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
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::registry;
use pgarbor_types::{oids, Datum, TypeEnv, TypeId};
use sqlparser::ast::{AccessExpr, DataType, DateTimeField, Expr, Ident, IntervalFields, Subscript};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::{bind_arg, bind_column_ref};
use super::expr_ops::make_op;
use super::literal::bind_typed_string;
use crate::diag;
use crate::ir::{
    ArrayExpr, BoundExpr, CaseArm, CaseExpr, CoercionForm, CollateExpr, ExprKind, FieldSelect, MinMaxExpr,
    NullIfExpr, OpExpr, RowExpr, SubscriptExpr, SubscriptItem,
};

/// `Expr::Case`.
pub fn bind_case(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let Expr::Case { operand, conditions, else_result, .. } = e else {
        return Err(PgError::internal("bind_case: not a CASE expression"));
    };
    // Simple form: the operand is evaluated once and read through CaseTestValue placeholders.
    // An unknown operand (bare string literal) is resolved to text first.
    let arg: Option<BoundExpr> = match operand {
        Some(op) => {
            let a = bind_arg(b, op, ec)?;
            Some(if a.ty().oid == oids::UNKNOWN { coerce::resolve_unknown_output(b, &a)? } else { a.expr })
        }
        None => None,
    };

    let mut conds: Vec<BoundExpr> = Vec::with_capacity(conditions.len());
    let mut results: Vec<BoundArg> = Vec::with_capacity(conditions.len() + 1);
    for cw in conditions {
        let cond = match &arg {
            Some(op_expr) => {
                let w = bind_arg(b, &cw.condition, ec)?;
                let p = b.pos(&cw.condition);
                let placeholder = BoundExpr::new(ExprKind::CaseTestValue, op_expr.ty);
                make_op(b, "=", Some(BoundArg::new(placeholder, None)), w, p)?
            }
            None => {
                let w = bind_arg(b, &cw.condition, ec)?;
                coerce::coerce_to_boolean(b, &w, "CASE/WHEN")?
            }
        };
        conds.push(cond);
        results.push(bind_arg(b, &cw.result, ec)?);
    }
    let has_else = else_result.is_some();
    if let Some(el) = else_result {
        results.push(bind_arg(b, el, ec)?);
    }
    let (mut exprs, common) = coerce::coerce_to_common(b, results, "CASE")?;
    let else_expr = if has_else { exprs.pop() } else { None };
    // PG always fills the default: a typed NULL when there is no ELSE.
    let default = else_expr.unwrap_or_else(|| BoundExpr::null(common));
    let arms: Vec<CaseArm> = conds
        .into_iter()
        .zip(exprs)
        .map(|(cond, result)| CaseArm { cond, result })
        .collect();
    Ok(BoundExpr::new(
        ExprKind::Case(Box::new(CaseExpr { arg: arg.map(Box::new), arms, default: Some(Box::new(default)) })),
        common,
    ))
}

/// `coalesce(args)`.
pub fn bind_coalesce(b: &mut Binder, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    if args.is_empty() {
        return Err(diag::syntax_error_near(")", pos));
    }
    let bound = bind_args(b, args, ec)?;
    let (exprs, ty) = coerce::coerce_to_common(b, bound, "COALESCE")?;
    Ok(BoundExpr::new(ExprKind::Coalesce(exprs), ty))
}

/// `nullif(a, b)` (exactly two args; any other count is a grammar error, oracle `select nullif(1)`: 42601 syntax
/// error from the grammar; use 42601 `syntax error at or near ")"`-style via `diag::syntax`).
pub fn bind_nullif(b: &mut Binder, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    if args.len() != 2 {
        // PG grammar: NULLIF '(' a_expr ',' a_expr ')'. Too few args stop at ")", too many at ",".
        let token = if args.len() > 2 { "," } else { ")" };
        return Err(diag::syntax_error_near(token, pos));
    }
    let left = bind_arg(b, &args[0], ec)?;
    let right = bind_arg(b, &args[1], ec)?;
    // PG transformAExprNullIf: make_op("=") coerces both operands; the result type is the first operand's.
    let eq = make_op(b, "=", Some(left), right, pos)?;
    match eq.kind {
        ExprKind::Op(op) => {
            let OpExpr { op: eq_ref, args: operands } = *op;
            let mut it = operands.into_iter();
            let (Some(l), Some(r), None) = (it.next(), it.next(), it.next()) else {
                return Err(PgError::internal("NULLIF: operator node without two operands"));
            };
            let l = if l.ty.oid == oids::UNKNOWN {
                coerce::resolve_unknown_output(b, &BoundArg::new(l, pos))?
            } else {
                l
            };
            let ty = l.ty;
            Ok(BoundExpr::new(ExprKind::NullIf(Box::new(NullIfExpr { left: l, right: r, eq: eq_ref })), ty))
        }
        _ => Err(PgError::internal("NULLIF: operator resolution did not produce an operator node")),
    }
}

/// `greatest(args)` / `least(args)`.
pub fn bind_minmax(b: &mut Binder, greatest: bool, args: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    if args.is_empty() {
        return Err(diag::syntax_error_near(")", pos));
    }
    let bound = bind_args(b, args, ec)?;
    let context = if greatest { "GREATEST" } else { "LEAST" };
    let (exprs, ty) = coerce::coerce_to_common(b, bound, context)?;
    Ok(BoundExpr::new(ExprKind::MinMax(Box::new(MinMaxExpr { greatest, args: exprs })), ty))
}

/// `Expr::Array`.
pub fn bind_array(b: &mut Binder, elems: &[Expr], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    if elems.is_empty() {
        return Err(empty_array_error(pos));
    }
    let bound = bind_array_elems(b, elems, ec)?;
    // PG transformArrayExpr: the element type is the common type of the elements (context ARRAY).
    let (exprs, common) = coerce::coerce_to_common(b, bound, "ARRAY")?;
    build_array(b, exprs, common, pos)
}

/// Elements of `ARRAY[..]`. A nested `[..]` element (`ARRAY[[1,2],[3,4]]`) is an array constructor without
/// the keyword: sqlparser yields it as an unnamed array expression, which only an element list may hold
/// (a bare `[` anywhere else is a syntax error, as in PG gram.y).
fn bind_array_elems(b: &mut Binder, elems: &[Expr], ec: ExprCtx) -> Result<Vec<BoundArg>> {
    let mut out: Vec<BoundArg> = Vec::with_capacity(elems.len());
    for e in elems {
        let arg = match e {
            Expr::Array(inner) if !inner.named => {
                let inner_pos = b.pos(e);
                let nested = bind_array(b, &inner.elem, inner_pos, ec)?;
                BoundArg::new(nested, inner_pos)
            }
            _ => bind_arg(b, e, ec)?,
        };
        out.push(arg);
    }
    Ok(out)
}

/// `ROW(..)` / `Expr::Tuple`.
pub fn bind_row(b: &mut Binder, elems: &[Expr], ec: ExprCtx) -> Result<BoundExpr> {
    let mut bound: Vec<BoundArg> = Vec::with_capacity(elems.len());
    for e in elems {
        bound.push(bind_arg(b, e, ec)?);
    }
    row_from_args(b, bound)
}

/// A `ROW` constructor whose unknown-typed fields stay unknown (row-wise comparisons resolve each
/// field pair like any other operator call; callers that keep the row as a value must resolve it).
pub fn bind_row_keep_unknown(b: &mut Binder, elems: &[Expr], ec: ExprCtx) -> Result<BoundExpr> {
    let mut args: Vec<BoundExpr> = Vec::with_capacity(elems.len());
    for e in elems {
        args.push(bind_arg(b, e, ec)?.expr);
    }
    let names: Vec<String> = (1..=args.len()).map(|i| format!("f{i}")).collect();
    Ok(BoundExpr::new(ExprKind::Row(Box::new(RowExpr { args, names })), TypeId::of(oids::RECORD)))
}

/// A `ROW` constructor over already bound arguments (unknown literals resolve to text).
pub fn row_from_args(b: &mut Binder, bound: Vec<BoundArg>) -> Result<BoundExpr> {
    let mut args: Vec<BoundExpr> = Vec::with_capacity(bound.len());
    for a in bound {
        let expr = if a.ty().oid == oids::UNKNOWN { coerce::resolve_unknown_output(b, &a)? } else { a.expr };
        args.push(expr);
    }
    let names: Vec<String> = (1..=args.len()).map(|i| format!("f{i}")).collect();
    Ok(BoundExpr::new(ExprKind::Row(Box::new(RowExpr { args, names })), TypeId::of(oids::RECORD)))
}

/// `Expr::CompoundFieldAccess` (subscripts and `.field` chains) and `Expr::Subscript`-like forms.
///
/// The root of `t.col[1]` is an identifier followed by a `.col` access: PG reads `t.col` as a column
/// reference, so the first identifier access after a bare identifier root joins the column reference.
pub fn bind_field_access(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let Expr::CompoundFieldAccess { root, access_chain } = e else {
        return Err(PgError::internal("bind_field_access: not a compound field access"));
    };
    let root_pos = b.pos(&**root);
    let mut rest: &[AccessExpr] = access_chain.as_slice();
    let mut cur: BoundArg = match &**root {
        Expr::Identifier(id) => {
            let mut parts: Vec<Ident> = vec![id.clone()];
            if let Some(AccessExpr::Dot(Expr::Identifier(field))) = rest.first() {
                parts.push(field.clone());
                rest = &rest[1..];
            }
            let expr = bind_column_ref(b, &parts, root_pos, ec)?;
            BoundArg::new(expr, root_pos)
        }
        Expr::CompoundIdentifier(ids) => {
            let expr = bind_column_ref(b, ids, root_pos, ec)?;
            BoundArg::new(expr, root_pos)
        }
        other => bind_arg(b, other, ec)?,
    };

    let mut idx = 0usize;
    while idx < rest.len() {
        if matches!(rest[idx], AccessExpr::Subscript(_)) {
            let start = idx;
            while idx < rest.len() && matches!(rest[idx], AccessExpr::Subscript(_)) {
                idx += 1;
            }
            cur = bind_subscripts(b, cur, &rest[start..idx], ec)?;
        } else {
            if let AccessExpr::Dot(field_expr) = &rest[idx] {
                cur = bind_dot(b, cur, field_expr)?;
            }
            idx += 1;
        }
    }
    Ok(cur.expr)
}

/// `(composite_expr).field`.
pub fn select_field(b: &mut Binder, arg: BoundExpr, field: &Ident, pos: Option<u32>) -> Result<BoundExpr> {
    let name = b.ident(field);
    let attrs = composite_attrs(b, &arg, &name, pos)?;
    match attrs.iter().position(|a| !a.dropped && a.name == name) {
        Some(i) => {
            let ty = attrs[i].ty;
            let number = field_number(i)?;
            Ok(BoundExpr::new(
                ExprKind::FieldSelect(Box::new(FieldSelect { arg, field: number, field_name: name })),
                ty,
            ))
        }
        None => {
            let msg = if arg.ty.oid == oids::RECORD {
                format!("could not identify column \"{}\" in record data type", name)
            } else {
                format!("column \"{}\" not found in data type {}", name, b.type_name(arg.ty))
            };
            Err(diag::with_position(PgError::new(SqlState::UNDEFINED_COLUMN, msg), pos))
        }
    }
}

/// `(composite_expr).*` -> (field name, FieldSelect) pairs; used by `target`.
pub fn expand_composite(b: &mut Binder, arg: BoundExpr, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    let attrs = composite_attrs(b, &arg, "*", pos)?;
    let mut out: Vec<(String, BoundExpr)> = Vec::with_capacity(attrs.len());
    for (i, attr) in attrs.iter().enumerate() {
        if attr.dropped {
            continue;
        }
        let number = field_number(i)?;
        let expr = BoundExpr::new(
            ExprKind::FieldSelect(Box::new(FieldSelect {
                arg: arg.clone(),
                field: number,
                field_name: attr.name.clone(),
            })),
            attr.ty,
        );
        out.push((attr.name.clone(), expr));
    }
    Ok(out)
}

/// `Expr::Collate`.
pub fn bind_collate(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let Expr::Collate { expr, collation } = e else {
        return Err(PgError::internal("bind_collate: not a COLLATE expression"));
    };
    let arg = bind_arg(b, expr, ec)?;
    let name = b.qual(collation)?;
    let name_pos = name.pos;
    if let Some(catalog) = &name.catalog {
        if *catalog != b.ctx.session.database {
            let qualified = [name.catalog.as_deref(), name.schema.as_deref(), Some(name.name.as_str())]
                .into_iter()
                .flatten()
                .collect::<Vec<&str>>()
                .join(".");
            return Err(diag::cross_database(&qualified, name_pos));
        }
    }
    // PG transformCollateClause: the collation is looked up first, then the operand type is checked.
    // Both errors are reported at the COLLATE keyword (`CollateClause.location`).
    let clause_pos = super::expr::collate_keyword_pos(b.src, expr).or(name_pos);
    let oid = b
        .env
        .collation_oid(name.schema.as_deref(), &name.name)
        .map_err(|err| diag::with_position(err, clause_pos))?;
    let arg_ty = arg.ty();
    let collatable = arg_ty.oid == oids::UNKNOWN || {
        let env: &dyn TypeEnv = b.env;
        env.type_info(arg_ty.oid).map(|t| t.is_collatable()).unwrap_or(false)
    };
    if !collatable {
        let msg = format!("collations are not supported by type {}", b.type_name(arg_ty));
        return Err(diag::with_position(PgError::new(SqlState::DATATYPE_MISMATCH, msg), clause_pos));
    }
    Ok(BoundExpr::new(ExprKind::Collate(Box::new(CollateExpr { arg: arg.expr, collation: oid })), arg_ty))
}

/// `COLLATION FOR (expr)` and `pg_collation_for(expr)` (PG `transformCollationFor`): the name of the collation
/// of `expr`, as text. The argument's type must be collatable (42804 otherwise). An argument written
/// `x COLLATE c` gives `c`. An expression with no collation-bearing node has the default collation, named
/// `default`. The collation of a column, or of a collation inside a larger expression, is not tracked by the
/// expression binder, so those are 0A000.
pub fn bind_collation_for(b: &mut Binder, e: &Expr, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    let arg = bind_arg(b, e, ec)?;
    let arg_ty = arg.ty();
    let collatable = arg_ty.oid == oids::UNKNOWN || {
        let env: &dyn TypeEnv = b.env;
        env.type_info(arg_ty.oid).map(|t| t.is_collatable()).unwrap_or(false)
    };
    if !collatable {
        let msg = format!("collations are not supported by type {}", b.type_name(arg_ty));
        return Err(diag::with_position(PgError::new(SqlState::DATATYPE_MISMATCH, msg), pos));
    }
    let name = match &arg.expr.kind {
        ExprKind::Collate(c) => match pgarbor_catalog::builtin::builtin_collation(c.collation) {
            Some(coll) => coll.name.to_string(),
            None => return Err(PgError::unimplemented("SQL-14", "collation name of a non-builtin collation")),
        },
        _ if arg.expr.any(&mut |x| matches!(x.kind, ExprKind::Collate(_) | ExprKind::Column(_))) => {
            return Err(PgError::unimplemented("SQL-14", "COLLATION FOR of a column or of an expression with a collation"));
        }
        _ => "default".to_string(),
    };
    Ok(BoundExpr::constant(Datum::Text(name), TypeId::TEXT))
}

/// `Expr::Interval`.
pub fn bind_interval(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    // The literal does not depend on the expression context.
    let _ = ec;
    let Expr::Interval(iv) = e else {
        return Err(PgError::internal("bind_interval: not an INTERVAL expression"));
    };
    let pos = b.pos(e);
    let Expr::Value(value) = &*iv.value else {
        // PG grammar: INTERVAL takes a string constant (`ConstInterval Sconst`), so `interval(...)` is a
        // syntax error at the first token of the argument (`interval('1 day')` -> near "'1 day'").
        let mut first = &*iv.value;
        while let Expr::Nested(inner) = first {
            first = &**inner;
        }
        let token = first.to_string();
        return Err(diag::syntax_error_near(&token, b.pos(first)));
    };
    if iv.leading_precision.is_some() {
        return Err(diag::unsupported_construct("INTERVAL leading field precision", pos));
    }
    let fields = interval_fields(iv.leading_field.as_ref(), iv.last_field.as_ref(), pos)?;
    let data_type = DataType::Interval { fields, precision: iv.fractional_seconds_precision };
    bind_typed_string(b, &data_type, value)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn bind_args(b: &mut Binder, args: &[Expr], ec: ExprCtx) -> Result<Vec<BoundArg>> {
    let mut out: Vec<BoundArg> = Vec::with_capacity(args.len());
    for a in args {
        out.push(bind_arg(b, a, ec)?);
    }
    Ok(out)
}

/// 42P18 `cannot determine type of empty array` with the PG hint.
fn empty_array_error(pos: Option<u32>) -> PgError {
    let e = PgError::new(SqlState::INDETERMINATE_DATATYPE, "cannot determine type of empty array").with_hint(
        "Explicitly cast to the desired type, for example ARRAY[]::integer[].",
    );
    diag::with_position(e, pos)
}

/// 2202E `multidimensional arrays must have array expressions with matching dimensions`.
fn matching_dims_error(pos: Option<u32>) -> PgError {
    diag::with_position(
        PgError::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "multidimensional arrays must have array expressions with matching dimensions",
        ),
        pos,
    )
}

/// Build the array node for `ARRAY[..]` once the elements are coerced to `common`.
fn build_array(b: &mut Binder, elements: Vec<BoundExpr>, common: TypeId, pos: Option<u32>) -> Result<BoundExpr> {
    let env: &dyn TypeEnv = b.env;
    if registry::is_array_type(env, common.oid) {
        // Elements are arrays: PG builds a multidimensional array of the common array type
        // (arrays of arrays are the same type as their multidimensional form).
        let elem_type = registry::element_type_of(env, common.oid).ok_or_else(|| {
            let msg = format!("could not find element type for data type {}", b.type_name(common));
            diag::with_position(PgError::new(SqlState::UNDEFINED_OBJECT, msg), pos)
        })?;
        sibling_dims(&elements, pos)?;
        return Ok(BoundExpr::new(
            ExprKind::Array(Box::new(ArrayExpr { elements, elem_type, multidims: true })),
            common,
        ));
    }
    let array_oid = registry::array_type_of(env, common.oid).ok_or_else(|| {
        let msg = format!("could not find array type for data type {}", b.type_name(common));
        diag::with_position(PgError::new(SqlState::UNDEFINED_OBJECT, msg), pos)
    })?;
    Ok(BoundExpr::new(
        ExprKind::Array(Box::new(ArrayExpr { elements, elem_type: common.oid, multidims: false })),
        TypeId::of(array_oid),
    ))
}

/// `ARRAY[]::T[]` (PG transformTypeCast: an empty A_ArrayExpr takes the cast's array type, since it has no
/// element to infer one from). `Some` when `target` is an array type, `None` otherwise.
pub fn cast_empty_array(b: &Binder, target: TypeId) -> Option<BoundExpr> {
    let env: &dyn TypeEnv = b.env;
    let elem_type = registry::element_type_of(env, target.oid)?;
    Some(BoundExpr::new(
        ExprKind::Array(Box::new(ArrayExpr { elements: Vec::new(), elem_type, multidims: false })),
        target,
    ))
}

/// Dimensions of a sibling list when they are statically known (nested `ARRAY[..]` constructors).
/// Siblings with different known dimensions are 2202E. Unknown siblings (column values) are skipped.
fn sibling_dims(elements: &[BoundExpr], pos: Option<u32>) -> Result<Option<Vec<usize>>> {
    let mut common: Option<Vec<usize>> = None;
    for el in elements {
        if let Some(d) = static_dims(el, pos)? {
            let mismatch = match &common {
                Some(c) => *c != d,
                None => false,
            };
            if mismatch {
                return Err(matching_dims_error(pos));
            }
            if common.is_none() {
                common = Some(d);
            }
        }
    }
    Ok(common)
}

fn static_dims(e: &BoundExpr, pos: Option<u32>) -> Result<Option<Vec<usize>>> {
    let ExprKind::Array(a) = &e.kind else {
        return Ok(None);
    };
    let mut dims: Vec<usize> = vec![a.elements.len()];
    if a.multidims {
        if let Some(sub) = sibling_dims(&a.elements, pos)? {
            dims.extend(sub);
        }
    }
    Ok(Some(dims))
}

/// Composite attribute as seen by field selection.
struct CompositeAttr {
    name: String,
    ty: TypeId,
    dropped: bool,
}

/// Attributes of a composite-valued expression (`ROW(..)` values carry their own names).
fn composite_attrs(b: &Binder<'_>, arg: &BoundExpr, what: &str, pos: Option<u32>) -> Result<Vec<CompositeAttr>> {
    if let ExprKind::Row(row) = &arg.kind {
        return Ok(row
            .names
            .iter()
            .zip(row.args.iter())
            .map(|(name, a)| CompositeAttr { name: name.clone(), ty: a.ty, dropped: false })
            .collect());
    }
    let env: &dyn TypeEnv = b.env;
    let base = registry::base_type(env, arg.ty.oid);
    if base == oids::RECORD {
        return Err(diag::unsupported_construct("field selection from a record of unknown structure", pos));
    }
    let is_composite = env.type_info(base).map(|t| t.is_composite()).unwrap_or(false);
    if !is_composite {
        let msg = format!(
            "column notation .{} applied to type {}, which is not a composite type",
            what,
            b.type_name(arg.ty)
        );
        return Err(diag::with_position(PgError::new(SqlState::WRONG_OBJECT_TYPE, msg), pos));
    }
    let Some(fields) = env.composite_fields(base) else {
        return Err(PgError::internal(format!("cache lookup failed for composite type {base}")));
    };
    Ok(fields
        .iter()
        .map(|f| CompositeAttr { name: f.name.clone(), ty: f.type_id, dropped: f.dropped })
        .collect())
}

/// 1-based attribute number of the `i`-th (0-based) attribute, as `FieldSelect::field` stores it.
fn field_number(i: usize) -> Result<i16> {
    i16::try_from(i + 1).map_err(|_| PgError::internal("composite attribute number out of range"))
}

/// `.name` / `.*` / `.1` after a value (one step of a field chain).
fn bind_dot(b: &mut Binder, cur: BoundArg, field_expr: &Expr) -> Result<BoundArg> {
    match field_expr {
        Expr::Identifier(id) => {
            let pos = b.pos_span(id.span);
            let expr = select_field(b, cur.expr, id, pos)?;
            Ok(BoundArg::new(expr, pos))
        }
        Expr::Wildcard(_) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "row expansion via \"*\" is not supported here",
        )),
        Expr::Value(v) => Err(diag::syntax_error_near(&v.value.to_string(), cur.pos)),
        other => {
            let text = other.to_string();
            Err(diag::syntax_error_near(&text, cur.pos))
        }
    }
}

/// One subscript position as an int4 expression (PG transformContainerSubscripts, COERCION_ASSIGNMENT).
fn bind_index(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let arg = bind_arg(b, e, ec)?;
    match coerce::try_coerce(b, &arg, TypeId::INT4, CastContext::Assignment, CoercionForm::Implicit)? {
        Some(expr) => Ok(expr),
        None => Err(diag::with_position(
            PgError::new(SqlState::DATATYPE_MISMATCH, "array subscript must have type integer"),
            arg.pos,
        )),
    }
}

/// A run of consecutive `[..]` accesses applied to `cur` (PG groups them into one SubscriptingRef).
fn bind_subscripts(b: &mut Binder, cur: BoundArg, chain: &[AccessExpr], ec: ExprCtx) -> Result<BoundArg> {
    let pos = cur.pos;
    let env: &dyn TypeEnv = b.env;
    let base = registry::base_type(env, cur.ty().oid);
    if base == oids::JSONB {
        return Err(diag::unsupported_construct("jsonb subscripting", pos));
    }
    let elem = match registry::element_type_of(env, base) {
        Some(elem) => elem,
        None => {
            let msg = format!(
                "cannot subscript type {} because it does not support subscripting",
                b.type_name(TypeId::of(base))
            );
            return Err(diag::with_position(PgError::new(SqlState::DATATYPE_MISMATCH, msg), pos));
        }
    };
    // A slice anywhere makes every subscript a slice; a plain index then becomes [1:idx].
    let is_slice = chain.iter().any(|a| matches!(a, AccessExpr::Subscript(Subscript::Slice { .. })));
    let mut items: Vec<SubscriptItem> = Vec::with_capacity(chain.len());
    for access in chain {
        let AccessExpr::Subscript(sub) = access else {
            return Err(PgError::internal("bind_subscripts: non-subscript access in a subscript run"));
        };
        match sub {
            Subscript::Index { index } => {
                let ix = bind_index(b, index, ec)?;
                if is_slice {
                    let one = BoundExpr::constant(Datum::Int4(1), TypeId::INT4);
                    items.push(SubscriptItem { lower: Some(one), upper: Some(ix) });
                } else {
                    items.push(SubscriptItem { lower: Some(ix), upper: None });
                }
            }
            Subscript::Slice { lower_bound, upper_bound, stride } => {
                if stride.is_some() {
                    return Err(diag::syntax_error_near(":", pos));
                }
                let lower = match lower_bound {
                    Some(x) => Some(bind_index(b, x, ec)?),
                    None => None,
                };
                let upper = match upper_bound {
                    Some(x) => Some(bind_index(b, x, ec)?),
                    None => None,
                };
                items.push(SubscriptItem { lower, upper });
            }
        }
    }
    // A domain over an array is read through its base array type.
    let array = if cur.ty().oid == base {
        cur.expr
    } else {
        match coerce::try_coerce(b, &cur, TypeId::of(base), CastContext::Implicit, CoercionForm::Implicit)? {
            Some(expr) => expr,
            None => return Err(PgError::internal("subscript: domain does not coerce to its base array type")),
        }
    };
    let ty = if is_slice { TypeId::of(base) } else { TypeId::of(elem) };
    let node = SubscriptExpr { array, subscripts: items, is_slice };
    Ok(BoundArg::new(BoundExpr::new(ExprKind::Subscript(Box::new(node)), ty), pos))
}

fn single_interval_field(f: &DateTimeField) -> Option<IntervalFields> {
    match f {
        DateTimeField::Year => Some(IntervalFields::Year),
        DateTimeField::Month => Some(IntervalFields::Month),
        DateTimeField::Day => Some(IntervalFields::Day),
        DateTimeField::Hour => Some(IntervalFields::Hour),
        DateTimeField::Minute => Some(IntervalFields::Minute),
        DateTimeField::Second => Some(IntervalFields::Second),
        _ => None,
    }
}

fn pair_interval_field(lead: &DateTimeField, last: &DateTimeField) -> Option<IntervalFields> {
    match (lead, last) {
        (DateTimeField::Year, DateTimeField::Month) => Some(IntervalFields::YearToMonth),
        (DateTimeField::Day, DateTimeField::Hour) => Some(IntervalFields::DayToHour),
        (DateTimeField::Day, DateTimeField::Minute) => Some(IntervalFields::DayToMinute),
        (DateTimeField::Day, DateTimeField::Second) => Some(IntervalFields::DayToSecond),
        (DateTimeField::Hour, DateTimeField::Minute) => Some(IntervalFields::HourToMinute),
        (DateTimeField::Hour, DateTimeField::Second) => Some(IntervalFields::HourToSecond),
        (DateTimeField::Minute, DateTimeField::Second) => Some(IntervalFields::MinuteToSecond),
        _ => None,
    }
}

/// `INTERVAL <lead> [TO <last>]` field qualifier, mapped to the sqlparser form.
fn interval_fields(
    lead: Option<&DateTimeField>,
    last: Option<&DateTimeField>,
    pos: Option<u32>,
) -> Result<Option<IntervalFields>> {
    let unsupported = || diag::unsupported_construct("INTERVAL field qualifier", pos);
    match (lead, last) {
        (None, None) => Ok(None),
        (Some(l), None) => match single_interval_field(l) {
            Some(f) => Ok(Some(f)),
            None => Err(unsupported()),
        },
        (Some(l), Some(r)) => match pair_interval_field(l, r) {
            Some(f) => Ok(Some(f)),
            None => Err(unsupported()),
        },
        (None, Some(_)) => Err(unsupported()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int4_const(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn array_of(n: usize, multidims: bool) -> BoundExpr {
        let elements: Vec<BoundExpr> = (0..n).map(|i| int4_const(i as i32)).collect();
        BoundExpr::new(
            ExprKind::Array(Box::new(ArrayExpr { elements, elem_type: oids::INT4, multidims })),
            TypeId::of(1007),
        )
    }

    #[test]
    fn empty_array_is_42p18_with_cast_hint() {
        let e = empty_array_error(Some(8));
        assert_eq!(e.sqlstate.code(), "42P18");
        assert_eq!(e.message, "cannot determine type of empty array");
        assert_eq!(e.position, Some(8));
        assert_eq!(
            e.hint.as_deref(),
            Some("Explicitly cast to the desired type, for example ARRAY[]::integer[].")
        );
    }

    #[test]
    fn sibling_dims_accepts_equal_and_rejects_different_lengths() {
        let same = [array_of(2, false), array_of(2, false)];
        assert_eq!(sibling_dims(&same, None).ok().flatten(), Some(vec![2]));

        let different = [array_of(2, false), array_of(1, false)];
        match sibling_dims(&different, None) {
            Err(e) => assert_eq!(e.sqlstate.code(), "2202E"),
            Ok(_) => panic!("mismatched sub-array dimensions must be rejected"),
        }
    }

    #[test]
    fn sibling_dims_skips_unknown_siblings() {
        let mixed = [array_of(3, false), int4_const(7)];
        assert_eq!(sibling_dims(&mixed, None).ok().flatten(), Some(vec![3]));
    }

    #[test]
    fn interval_field_qualifiers_map_to_sqlparser_forms() {
        assert!(matches!(interval_fields(None, None, None), Ok(None)));
        assert!(matches!(
            interval_fields(Some(&DateTimeField::Day), None, None),
            Ok(Some(IntervalFields::Day))
        ));
        assert!(matches!(
            interval_fields(Some(&DateTimeField::Year), Some(&DateTimeField::Month), None),
            Ok(Some(IntervalFields::YearToMonth))
        ));
        assert!(matches!(
            interval_fields(Some(&DateTimeField::Hour), Some(&DateTimeField::Second), None),
            Ok(Some(IntervalFields::HourToSecond))
        ));
        // YEAR TO DAY is not a PostgreSQL interval qualifier.
        assert!(interval_fields(Some(&DateTimeField::Year), Some(&DateTimeField::Day), None).is_err());
    }

    #[test]
    fn row_fields_are_numbered_from_one() {
        assert_eq!(field_number(0).ok(), Some(1));
        assert_eq!(field_number(1).ok(), Some(2));
    }
}
