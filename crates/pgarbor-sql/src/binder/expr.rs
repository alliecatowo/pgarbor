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
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use sqlparser::ast::{BinaryOperator, CastKind, DataType, Expr, Ident, Spanned, ValueWithSpan};

use super::context::{Binder, BoundArg, ExprCtx};
use super::{coerce, expr_func, expr_misc, expr_ops, expr_sublink, literal, scope, types};
use crate::diag::{self, ExprClause};
use crate::ir::{BoundExpr, CoercionForm};
use crate::source::SourceMap;

/// Bind an expression (dispatch table in the module docs).
pub fn bind_expr(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    b.enter()?;
    let result = dispatch(b, e, ec);
    b.leave();
    result
}

/// The dispatch table itself; `bind_expr` wraps it in `enter`/`leave`.
fn dispatch(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(e);
    match e {
        Expr::Identifier(id) => {
            let p = b.pos_span(id.span);
            bind_column_ref(b, std::slice::from_ref(id), p, ec)
        }
        Expr::CompoundIdentifier(parts) => {
            let p = parts.first().and_then(|id| b.pos_span(id.span));
            bind_column_ref(b, parts, p, ec)
        }
        Expr::Value(v) => literal::bind_value(b, v, false),
        Expr::TypedString(ts) => literal::bind_typed_string(b, &ts.data_type, &ts.value),
        Expr::Nested(inner) => bind_expr(b, inner, ec),
        Expr::Cast { kind, expr, data_type, format } => match kind {
            CastKind::Cast | CastKind::DoubleColon => {
                if format.is_some() {
                    return Err(diag::syntax_error_near("FORMAT", pos));
                }
                bind_cast(b, expr, data_type, pos, ec)
            }
            CastKind::TryCast => Err(diag::syntax_error_near("TRY_CAST", pos)),
            CastKind::SafeCast => Err(diag::syntax_error_near("SAFE_CAST", pos)),
        },
        Expr::Convert { is_try, expr, data_type, charset, styles, .. } => match data_type {
            Some(dt) if !*is_try && charset.is_none() && styles.is_empty() => bind_cast(b, expr, dt, pos, ec),
            _ => Err(PgError::unimplemented("SQL-10", "CONVERT(... USING ...) and TRY_CONVERT")),
        },
        Expr::IsNull(x) => expr_ops::bind_is_test(b, x, "null", false, ec),
        Expr::IsNotNull(x) => expr_ops::bind_is_test(b, x, "null", true, ec),
        Expr::IsTrue(x) => expr_ops::bind_is_test(b, x, "true", false, ec),
        Expr::IsNotTrue(x) => expr_ops::bind_is_test(b, x, "true", true, ec),
        Expr::IsFalse(x) => expr_ops::bind_is_test(b, x, "false", false, ec),
        Expr::IsNotFalse(x) => expr_ops::bind_is_test(b, x, "false", true, ec),
        Expr::IsUnknown(x) => expr_ops::bind_is_test(b, x, "unknown", false, ec),
        Expr::IsNotUnknown(x) => expr_ops::bind_is_test(b, x, "unknown", true, ec),
        Expr::IsDistinctFrom(l, r) => expr_ops::bind_distinct_from(b, l, r, false, ec),
        Expr::IsNotDistinctFrom(l, r) => expr_ops::bind_distinct_from(b, l, r, true, ec),
        Expr::InList { expr, list, negated } => expr_ops::bind_in_list(b, expr, list, *negated, ec),
        Expr::InSubquery { expr, subquery, negated } => {
            check_sublink(ec, pos)?;
            expr_sublink::bind_in_subquery(b, expr, subquery, *negated, ec)
        }
        Expr::Between { expr, negated, low, high } => expr_ops::bind_between(b, expr, *negated, low, high, ec),
        Expr::BinaryOp { left, op, right } => {
            let op_pos = binary_op_pos(b.src, left);
            expr_ops::bind_binary_op(b, left, op, right, op_pos, ec)
        }
        Expr::Like { negated, any, expr, pattern, escape_char } => {
            if *any {
                return Err(PgError::unimplemented("SQL-10", "LIKE ANY"));
            }
            let esc = escape_value(escape_char.as_deref())?;
            expr_ops::bind_like(b, 0, *negated, expr, pattern, esc, ec)
        }
        Expr::ILike { negated, any, expr, pattern, escape_char } => {
            if *any {
                return Err(PgError::unimplemented("SQL-10", "ILIKE ANY"));
            }
            let esc = escape_value(escape_char.as_deref())?;
            expr_ops::bind_like(b, 1, *negated, expr, pattern, esc, ec)
        }
        Expr::SimilarTo { negated, expr, pattern, escape_char } => {
            let esc = escape_value(escape_char.as_deref())?;
            expr_ops::bind_like(b, 2, *negated, expr, pattern, esc, ec)
        }
        Expr::RLike { regexp, .. } => Err(diag::syntax_error_near(if *regexp { "REGEXP" } else { "RLIKE" }, pos)),
        Expr::AnyOp { left, compare_op, right, .. } => bind_any_all(b, left, compare_op, right, false, ec),
        Expr::AllOp { left, compare_op, right } => bind_any_all(b, left, compare_op, right, true, ec),
        Expr::UnaryOp { op, expr } => {
            let op_pos = unary_op_pos(b.src, expr);
            expr_ops::bind_unary_op(b, op, expr, op_pos, ec)
        }
        Expr::AtTimeZone { timestamp, time_zone } => expr_ops::bind_at_time_zone(b, timestamp, time_zone, ec),
        Expr::Extract { .. }
        | Expr::Ceil { .. }
        | Expr::Floor { .. }
        | Expr::Position { .. }
        | Expr::Substring { .. }
        | Expr::Trim { .. }
        | Expr::Overlay { .. } => expr_func::bind_special_syntax(b, e, ec),
        Expr::Collate { .. } => expr_misc::bind_collate(b, e, ec),
        Expr::Function(f) => expr_func::bind_function_call(b, f, ec),
        Expr::Case { .. } => expr_misc::bind_case(b, e, ec),
        Expr::Exists { subquery, negated } => {
            check_sublink(ec, pos)?;
            expr_sublink::bind_exists(b, subquery, *negated, ec)
        }
        Expr::Subquery(q) => {
            check_sublink(ec, pos)?;
            expr_sublink::bind_scalar_subquery(b, q, ec)
        }
        Expr::Tuple(elems) => expr_misc::bind_row(b, elems, ec),
        Expr::Array(arr) => {
            if !arr.named {
                return Err(diag::syntax_error_near("[", pos));
            }
            expr_misc::bind_array(b, &arr.elem, pos, ec)
        }
        Expr::Interval(_) => expr_misc::bind_interval(b, e, ec),
        Expr::CompoundFieldAccess { .. } => expr_misc::bind_field_access(b, e, ec),
        Expr::JsonAccess { .. } => Err(PgError::unimplemented("SQL-10", "JSON path access (-> shorthand)")),
        Expr::IsJson { .. } => Err(PgError::unimplemented("SQL-10", "IS [NOT] JSON")),
        Expr::IsNormalized { .. } => Err(PgError::unimplemented("SQL-10", "IS [NOT] NORMALIZED")),
        Expr::Prefixed { .. } => Err(PgError::unimplemented("SQL-10", "prefixed expression")),
        Expr::InUnnest { .. } => Err(diag::syntax_error_near("UNNEST", pos)),
        Expr::GroupingSets(_) => Err(diag::syntax_error_near("GROUPING SETS", pos)),
        Expr::Cube(_) => Err(diag::syntax_error_near("CUBE", pos)),
        Expr::Rollup(_) => Err(diag::syntax_error_near("ROLLUP", pos)),
        Expr::Wildcard(_) | Expr::QualifiedWildcard(_, _) => Err(diag::syntax_error_near("*", pos)),
        Expr::Struct { .. } => Err(diag::syntax_error_near("STRUCT", pos)),
        Expr::Named { .. } => Err(diag::syntax_error_near("=>", pos)),
        Expr::Dictionary(_) => Err(diag::syntax_error_near("{", pos)),
        Expr::Map(_) => Err(diag::syntax_error_near("MAP", pos)),
        Expr::MatchAgainst { .. } => Err(diag::syntax_error_near("MATCH", pos)),
        Expr::OuterJoin(_) => Err(diag::syntax_error_near("(+)", pos)),
        Expr::Prior(_) => Err(diag::syntax_error_near("PRIOR", pos)),
        Expr::Lambda(_) => Err(diag::syntax_error_near("->", pos)),
        Expr::MemberOf(_) => Err(diag::syntax_error_near("MEMBER", pos)),
    }
}

/// Bind and keep the source position (`Spanned::span()` start of `e`; for `Nested` use the inner
/// expression's position, for `BinaryOp` PG uses the OPERATOR position for operator errors, which
/// `expr_ops` computes itself).
pub fn bind_arg(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundArg> {
    let pos = arg_pos(b, e);
    let expr = bind_expr(b, e, ec)?;
    Ok(BoundArg::new(expr, pos))
}

/// Bind and coerce to boolean for clause `what` (`WHERE`, `HAVING`, `JOIN/ON`, `FILTER`, ...):
/// `bind_expr` + `coerce::coerce_to_boolean`.
pub fn bind_boolean(b: &mut Binder, e: &Expr, ec: ExprCtx, what: &str) -> Result<BoundExpr> {
    let arg = bind_arg(b, e, ec)?;
    coerce::coerce_to_boolean(b, &arg, what)
}

/// Column reference (module docs). `pos` is the position of the first identifier.
///
/// Candidate order follows PG `transformColumnRef`: `c` is a column (any level) or a whole-row
/// reference to a range entry; `a.b` is column `b` of range entry `a`; `s.t.c` is column `c` of
/// `s.t`; `db.s.t.c` checks the database name first. Every probe uses `want_error = false`; when
/// nothing matches, the same lookup is repeated with `want_error = true` so that the error is the
/// one of the whole-name interpretation (42703 / 42P01 / 42702 as produced by `scope`).
pub fn bind_column_ref(b: &mut Binder, parts: &[Ident], pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    if !ec.columns_ok && ec.clause == ExprClause::ColumnDefault {
        return Err(diag::with_position(
            PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot use column reference in DEFAULT expression"),
            pos,
        ));
    }
    let mut names: Vec<String> = Vec::with_capacity(parts.len());
    for part in parts {
        let name = b.ident(part);
        names.push(name);
    }
    match names.len() {
        1 => resolve_name(b, &names[..0], &names[0], pos),
        2 => resolve_name(b, &names[..1], &names[1], pos),
        3 => resolve_name(b, &names[..2], &names[2], pos),
        4 => {
            if names[0] != b.ctx.session.database {
                return Err(diag::cross_database(&names.join("."), pos));
            }
            resolve_name(b, &names[1..3], &names[3], pos)
        }
        0 => Err(PgError::internal("binder: empty column reference")),
        _ => Err(diag::syntax(
            format!("improper qualified name (too many dotted names): {}", names.join(".")),
            pos,
        )),
    }
}

/// Resolve `qualifier.name` with the probe-then-final-error protocol described on
/// [`bind_column_ref`].
fn resolve_name(b: &mut Binder, qualifier: &[String], name: &str, pos: Option<u32>) -> Result<BoundExpr> {
    if let Some(found) = scope::resolve_column(b, qualifier, name, pos, false)? {
        return Ok(found.expr);
    }
    match scope::resolve_column(b, qualifier, name, pos, true)? {
        Some(found) => Ok(found.expr),
        None => Err(diag::undefined_column(qualifier.last().map(|q| q.as_str()), name, pos)),
    }
}

/// `CAST(e AS t)` / `e::t` (module docs). The operand is bound first (as PG `transformTypeCast`
/// does), then the type is resolved, then the explicit coercion is applied.
pub fn bind_cast(b: &mut Binder, operand: &Expr, data_type: &DataType, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    // `ARRAY[]::T[]`: the empty array takes the cast's type (see `expr_misc::cast_empty_array`).
    if let Expr::Array(arr) = operand {
        if !arr.named && arr.elem.is_empty() {
            let type_pos = type_pos_after(b.src, operand).or(pos);
            let target = types::resolve_data_type(b, data_type, type_pos)?.id;
            if let Some(empty) = expr_misc::cast_empty_array(b, target) {
                return Ok(empty);
            }
        }
    }
    let operand_expr = bind_expr(b, operand, ec)?;
    let arg = BoundArg::new(operand_expr, arg_pos(b, operand));
    let type_pos = type_pos_after(b.src, operand).or(pos);
    let resolved = types::resolve_data_type(b, data_type, type_pos)?;
    coerce::coerce_strict(b, &arg, resolved.id, CastContext::Explicit, CoercionForm::Explicit)
}

/// Sub-links are forbidden in some clauses (`cannot use subquery in {clause}`, 0A000).
fn check_sublink(ec: ExprCtx, pos: Option<u32>) -> Result<()> {
    if ec.sublinks_ok {
        return Ok(());
    }
    Err(diag::with_position(
        PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot use subquery in {}", ec.clause.name())),
        pos,
    ))
}

/// `left op ANY (..)` / `left op ALL (..)`: a subquery right side goes to `expr_sublink`, anything
/// else to the array form in `expr_ops`.
fn bind_any_all(
    b: &mut Binder,
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    is_all: bool,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let op_pos = binary_op_pos(b.src, left);
    if let Expr::Subquery(q) = right {
        check_sublink(ec, b.pos(right))?;
        return expr_sublink::bind_any_all_subquery(b, left, op, q, is_all, op_pos, ec);
    }
    expr_ops::bind_any_all_array(b, left, op, right, is_all, op_pos, ec)
}

/// The escape character of LIKE/ILIKE/SIMILAR TO: only a literal is supported.
fn escape_value(esc: Option<&Expr>) -> Result<Option<&ValueWithSpan>> {
    match esc {
        None => Ok(None),
        Some(Expr::Value(v)) => Ok(Some(v)),
        Some(_) => Err(PgError::unimplemented("SQL-10", "LIKE ... ESCAPE with a non-literal escape character")),
    }
}

/// Position of the first character of `e`, looking through parentheses.
fn arg_pos(b: &Binder, e: &Expr) -> Option<u32> {
    let mut inner = e;
    while let Expr::Nested(x) = inner {
        inner = &**x;
    }
    b.pos(inner)
}

fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// Byte index of the character just after the (inclusive) end of the span of `e`.
fn byte_after_end(src: &SourceMap, e: &Expr) -> Option<usize> {
    let off = src.byte_offset(e.span().end)?;
    let text = src.text();
    Some(match text.get(off..).and_then(|rest| rest.chars().next()) {
        Some(c) => off + c.len_utf8(),
        None => off,
    })
}

/// Skip blanks and closing parentheses starting at byte index `i` (ASCII only, so `i` stays on a
/// character boundary).
fn skip_blank(text: &str, mut i: usize) -> usize {
    let bytes = text.as_bytes();
    while i < bytes.len() && (is_blank(bytes[i]) || bytes[i] == b')') {
        i += 1;
    }
    i
}

/// Position of a binary operator token: the first token after the left operand's text.
fn binary_op_pos(src: &SourceMap, left: &Expr) -> Option<u32> {
    let i = skip_blank(src.text(), byte_after_end(src, left)?);
    Some(src.pos_of_byte(i))
}

/// Position of a prefix operator token: the run of non-blank text that ends just before the
/// operand (skipping blanks and opening parentheses).
fn unary_op_pos(src: &SourceMap, operand: &Expr) -> Option<u32> {
    let bytes = src.text().as_bytes();
    let mut i = src.byte_offset(operand.span().start)?.min(bytes.len());
    while i > 0 && (is_blank(bytes[i - 1]) || bytes[i - 1] == b'(') {
        i -= 1;
    }
    let end = i;
    while i > 0 && !(is_blank(bytes[i - 1]) || bytes[i - 1] == b'(') {
        i -= 1;
    }
    if i == end {
        None
    } else {
        Some(src.pos_of_byte(i))
    }
}

/// Position of the `COLLATE` keyword of `operand COLLATE name`: the first token after the operand's
/// text (PG reports collation conflicts at the COLLATE clause, `CollateClause.location`).
pub(super) fn collate_keyword_pos(src: &SourceMap, operand: &Expr) -> Option<u32> {
    let text = src.text();
    let i = skip_blank(text, byte_after_end(src, operand)?);
    let tok = text.get(i..i + "COLLATE".len())?;
    if tok.eq_ignore_ascii_case("COLLATE") {
        Some(src.pos_of_byte(i))
    } else {
        None
    }
}

/// Position of the type name of a cast: after `::`, or after `AS` in `CAST(e AS t)`.
fn type_pos_after(src: &SourceMap, operand: &Expr) -> Option<u32> {
    let text = src.text();
    let mut i = skip_blank(text, byte_after_end(src, operand)?);
    let tok = text.get(i..i + 2)?;
    if tok == "::" || tok.eq_ignore_ascii_case("AS") {
        i += 2;
    } else {
        return None;
    }
    Some(src.pos_of_byte(skip_blank(text, i)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::oids;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse(sql: &str) -> Expr {
        Parser::new(&PostgreSqlDialect {})
            .try_with_sql(sql)
            .and_then(|mut p| p.parse_expr())
            .expect("test expression parses")
    }

    /// Run `f` with a binder over an empty bootstrapped catalog; `f` gets the parsed `sql`.
    fn with_binder<T>(sql: &str, f: impl FnOnce(&mut Binder, &Expr) -> T) -> T {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new(sql);
        let expr = parse(sql);
        let mut b = Binder::new(&ctx, &src);
        f(&mut b, &expr)
    }

    #[test]
    fn unsupported_variants_are_rejected() {
        with_binder("1", |b, _| {
            let prior = Expr::Prior(Box::new(parse("a")));
            let err = bind_expr(b, &prior, ExprCtx::select_list()).err().expect("PRIOR is rejected");
            assert!(err.sqlstate == SqlState::SYNTAX_ERROR);

            let named = Expr::Named { expr: Box::new(parse("1")), name: Ident::new("x") };
            let err = bind_expr(b, &named, ExprCtx::select_list()).err().expect("named arg is rejected");
            assert!(err.sqlstate == SqlState::SYNTAX_ERROR);

            let sets = Expr::GroupingSets(Vec::new());
            let err = bind_expr(b, &sets, ExprCtx::select_list()).err().expect("grouping sets are rejected");
            assert!(err.sqlstate == SqlState::SYNTAX_ERROR);
        });
    }

    #[test]
    fn column_reference_in_default_is_rejected() {
        with_binder("a", |b, _| {
            let parts = [Ident::new("a")];
            let err = bind_column_ref(b, &parts, Some(1), ExprCtx::column_default()).err().expect("rejected");
            assert!(err.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
            assert_eq!(err.message, "cannot use column reference in DEFAULT expression");
        });
    }

    #[test]
    fn operator_positions_follow_source_text() {
        let src = SourceMap::new("a + b");
        match parse("a + b") {
            Expr::BinaryOp { left, .. } => assert_eq!(binary_op_pos(&src, &left), Some(3)),
            _ => unreachable!("test input is a binary operation"),
        }

        let src = SourceMap::new("(a)+b");
        match parse("(a)+b") {
            Expr::BinaryOp { left, .. } => assert_eq!(binary_op_pos(&src, &left), Some(4)),
            _ => unreachable!("test input is a binary operation"),
        }

        let src = SourceMap::new("NOT x");
        match parse("NOT x") {
            Expr::UnaryOp { expr, .. } => assert_eq!(unary_op_pos(&src, &expr), Some(1)),
            _ => unreachable!("test input is a unary operation"),
        }
    }

    #[test]
    fn cast_type_position_follows_double_colon() {
        let src = SourceMap::new("1 :: int");
        match parse("1 :: int") {
            Expr::Cast { expr, .. } => assert_eq!(type_pos_after(&src, &expr), Some(6)),
            _ => unreachable!("test input is a cast"),
        }
    }

    #[test]
    #[ignore = "needs SQL-08 (literal binding)"]
    fn nested_literal_is_int4() {
        with_binder("(1)", |b, e| {
            let out = bind_expr(b, e, ExprCtx::select_list()).expect("binds");
            assert_eq!(out.ty.oid, oids::INT4);
        });
    }

    #[test]
    #[ignore = "needs SQL-08 (literal binding) and SQL-09 (coercion)"]
    fn unknown_literal_cast_to_bigint() {
        with_binder("('1')::bigint", |b, e| {
            let out = bind_expr(b, e, ExprCtx::select_list()).expect("binds");
            assert_eq!(out.ty.oid, oids::INT8);
        });
    }

    #[test]
    #[ignore = "needs SQL-07 (type resolution)"]
    fn unknown_cast_type_is_42704() {
        with_binder("1::notatype", |b, e| {
            let err = bind_expr(b, e, ExprCtx::select_list()).err().expect("rejected");
            assert!(err.sqlstate == SqlState::UNDEFINED_OBJECT);
        });
    }

    #[test]
    #[ignore = "needs SQL-05 (column resolution)"]
    fn three_part_name_without_relation_is_42p01() {
        with_binder("a.b.c", |b, e| {
            let err = bind_expr(b, e, ExprCtx::select_list()).err().expect("rejected");
            assert!(err.sqlstate == SqlState::UNDEFINED_TABLE);
        });
    }
}
