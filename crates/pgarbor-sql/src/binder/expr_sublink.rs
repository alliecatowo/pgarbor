//! Sub-select expressions (SQL-15). PG references: `parse_expr.c` (`transformSubLink`),
//! `parse_clause.c`, `parse_oper.c` (`make_scalar_array_op`), `analyze.c` (`transformSelectStmt` for
//! the nested query), `parse_expr.c` (`transformArrayExpr` for `ARRAY(select)`).
//!
//! The sub-query is bound with `select::bind_query` WITHOUT popping the enclosing scopes: it opens a
//! new scope on `b.scopes`, so references to enclosing queries become `Column{levels_up >= 1}`
//! (correlation). Set `has_sublinks` on the enclosing scope. Sub-links are rejected where
//! `!ec.sublinks_ok` (0A000 `cannot use subquery in <clause>`; PG wording for DEFAULT/CHECK:
//! `cannot use subquery in DEFAULT expression` / `cannot use subquery in check constraint`).
//!
//! | SQL | `SubLinkKind` | rules |
//! |---|---|---|
//! | `EXISTS (q)` | `Exists` | `ty` boolean; the query's select list is irrelevant (not validated for type); `NOT EXISTS` -> `Bool(Not)` |
//! | `(q)` scalar | `Scalar` | exactly one output column else 42601 `subquery must return only one column` (`diag::subquery_one_column`); `ty` = that column's type (unknown -> text) |
//! | `x IN (q)` / `x = ANY (q)` / `x <op> ANY\|SOME (q)` | `Any` | `test` = x; per-column operator resolved with `expr_ops::resolve_named_op(op, test_type, column_type)`; IN uses `=`; `NOT IN` -> `Bool(Not)` over `Any(=)`; row tests `(a,b) IN (select x,y)` need matching width: 42601 `subquery has too many columns` / `subquery has too few columns` (`diag::subquery_column_count`) |
//! | `x <op> ALL (q)` | `All` | like ANY |
//! | `ARRAY(q)` | `Array` | one column; `ty` = array of the column type |
//! | `(a,b) = (q)` | `Row` | width must match; comparison expanded like row comparison |
//! The nested `BoundQuery` is stored in `SubLink::subquery`. LATERAL-ish references to the enclosing
//! FROM items of the SAME level are not visible (a sub-select in WHERE sees the enclosing query's
//! full namespace as outer scope, which is what `levels_up` means).
//! `InUnnest { expr, array_expr }` (BigQuery) -> 42601.
//!
//! ## Implementation notes
//! * Order of checks follows PG `transformSubLink`: the sub-link context check, then the sub-select
//!   analysis, then the single-column check (EXPR/ARRAY), then the left operand, then the width check
//!   and the per-column operators.
//! * Operator resolution goes through `expr_ops::make_op` with a placeholder right operand typed as
//!   the sub-select column. `resolve_named_op` returns only the `OpRef`, but the sub-link needs the
//!   coerced left operand and the operator's right operand type as well, and `make_op` carries both
//!   (`args[0]` / `args[1].ty`). The `OpRef` of the resulting `Op` node is the one stored in `ops`.
//! * When the operator's right operand type differs from the sub-select column type, the column
//!   itself is cast in the sub-select's target list (`coerce_output_column`). Set-operation and
//!   VALUES bodies have no single target list to rewrite, so that case is 0A000.

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::registry::array_type_of;
use pgarbor_types::{oids, TypeEnv, TypeId};
use sqlparser::ast::{BinaryOperator, Expr, Query};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr;
use super::expr_ops;
use super::select::{self, QueryCtx};
use crate::diag::{self, ExprClause};
use crate::ir::{BoundExpr, BoundQuery, CoercionForm, ExprKind, OpRef, QueryBody, RowExpr, SubLink, SubLinkKind};

/// Bind a nested query as an expression sub-link body (opens/closes its scope; result type checks
/// are the callers').
pub fn bind_nested_query(b: &mut Binder, q: &Query) -> Result<BoundQuery> {
    select::bind_query(b, q, &QueryCtx::default())
}

/// `Expr::Exists { subquery, negated }`.
pub fn bind_exists(b: &mut Binder, q: &Query, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(q);
    check_sublinks_allowed(ec, pos)?;
    let sub = bind_nested_query(b, q)?;
    b.scope_mut()?.has_sublinks = true;
    let link = sub_link_expr(SubLinkKind::Exists, None, Vec::new(), sub, TypeId::BOOL);
    Ok(if negated { BoundExpr::not(link) } else { link })
}

/// `Expr::Subquery(q)` scalar sub-select.
pub fn bind_scalar_subquery(b: &mut Binder, q: &Query, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(q);
    check_sublinks_allowed(ec, pos)?;
    let mut sub = bind_nested_query(b, q)?;
    if sub.width() != 1 {
        return Err(diag::subquery_one_column(pos));
    }
    if sub.output[0].ty.oid == oids::UNKNOWN {
        coerce_output_column(b, &mut sub, 0, TypeId::TEXT, pos)?;
    }
    let ty = sub.output[0].ty;
    b.scope_mut()?.has_sublinks = true;
    Ok(sub_link_expr(SubLinkKind::Scalar, None, Vec::new(), sub, ty))
}

/// `Expr::InSubquery { expr, subquery, negated }`.
pub fn bind_in_subquery(b: &mut Binder, e: &Expr, q: &Query, negated: bool, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(e);
    check_sublinks_allowed(ec, pos)?;
    let mut sub = bind_nested_query(b, q)?;
    let (lefts, is_row) = bind_test_side(b, e, ec)?;
    let (test, ops) = compare_to_subquery(b, lefts, is_row, "=", &mut sub, pos)?;
    b.scope_mut()?.has_sublinks = true;
    let link = sub_link_expr(SubLinkKind::Any, Some(test), ops, sub, TypeId::BOOL);
    Ok(if negated { BoundExpr::not(link) } else { link })
}

/// `AnyOp`/`AllOp` whose right side is `Expr::Subquery`.
pub fn bind_any_all_subquery(b: &mut Binder, left: &Expr, op: &BinaryOperator, q: &Query, is_all: bool, op_pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    check_sublinks_allowed(ec, op_pos)?;
    let op_name = operator_name(op, op_pos)?;
    let mut sub = bind_nested_query(b, q)?;
    let (lefts, is_row) = bind_test_side(b, left, ec)?;
    let (test, ops) = compare_to_subquery(b, lefts, is_row, &op_name, &mut sub, op_pos)?;
    b.scope_mut()?.has_sublinks = true;
    let kind = if is_all { SubLinkKind::All } else { SubLinkKind::Any };
    Ok(sub_link_expr(kind, Some(test), ops, sub, TypeId::BOOL))
}

/// `ARRAY(select ...)`.
pub fn bind_array_subquery(b: &mut Binder, q: &Query, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(q);
    check_sublinks_allowed(ec, pos)?;
    let mut sub = bind_nested_query(b, q)?;
    if sub.width() != 1 {
        return Err(diag::subquery_one_column(pos));
    }
    if sub.output[0].ty.oid == oids::UNKNOWN {
        coerce_output_column(b, &mut sub, 0, TypeId::TEXT, pos)?;
    }
    let elem = sub.output[0].ty;
    let env: &dyn TypeEnv = b.env;
    let array_oid = match array_type_of(env, elem.oid) {
        Some(oid) => oid,
        None => {
            let mut e = PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("could not find array type for data type {}", b.type_name(elem)),
            );
            e.position = pos;
            return Err(e);
        }
    };
    b.scope_mut()?.has_sublinks = true;
    Ok(sub_link_expr(SubLinkKind::Array, None, Vec::new(), sub, TypeId { oid: array_oid, typmod: -1 }))
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Build the `SubLink` expression node.
fn sub_link_expr(kind: SubLinkKind, test: Option<BoundExpr>, ops: Vec<OpRef>, sub: BoundQuery, ty: TypeId) -> BoundExpr {
    let link = SubLink { kind, test: test.map(Box::new), ops, subquery: Box::new(sub) };
    BoundExpr::new(ExprKind::SubLink(Box::new(link)), ty)
}

/// Reject sub-links in expression contexts that forbid them (PG `transformSubLink` switch on
/// `p_expr_kind`). The message names the clause with PG's wording for the sub-link check.
fn check_sublinks_allowed(ec: ExprCtx, pos: Option<u32>) -> Result<()> {
    if ec.sublinks_ok {
        return Ok(());
    }
    let mut e = PgError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("cannot use subquery in {}", sublink_clause_name(ec.clause)),
    );
    e.position = pos;
    Err(e)
}

/// Clause wording of `cannot use subquery in <clause>` (PG `transformSubLink`, one string per kind).
fn sublink_clause_name(clause: ExprClause) -> &'static str {
    match clause {
        ExprClause::CheckConstraint | ExprClause::DomainCheck => "check constraint",
        ExprClause::ColumnDefault => "DEFAULT expression",
        ExprClause::IndexExpression => "index expression",
        ExprClause::IndexPredicate => "index predicate",
        ExprClause::ExecuteParameter => "EXECUTE parameter",
        ExprClause::CallArgument => "CALL argument",
        ExprClause::CopyWhere => "COPY FROM WHERE condition",
        ExprClause::GeneratedColumn => "column generation expression",
        ExprClause::PolicyExpression => "policy expression",
        ExprClause::AlterColumnType => "transform expression",
        other => other.name(),
    }
}

/// PG operator spelling of the comparison operator of `ANY`/`ALL` (`expr_ops` module docs).
/// Operators that cannot be used as a sub-link comparison are 0A000.
fn operator_name(op: &BinaryOperator, pos: Option<u32>) -> Result<String> {
    let name: &str = match op {
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::Gt => ">",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::Plus => "+",
        BinaryOperator::Minus => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Modulo => "%",
        BinaryOperator::StringConcat => "||",
        BinaryOperator::BitwiseOr => "|",
        BinaryOperator::BitwiseAnd => "&",
        BinaryOperator::BitwiseXor => "#",
        BinaryOperator::PGBitwiseXor => "#",
        BinaryOperator::PGBitwiseShiftLeft => "<<",
        BinaryOperator::PGBitwiseShiftRight => ">>",
        BinaryOperator::PGExp => "^",
        BinaryOperator::PGOverlap => "&&",
        BinaryOperator::PGRegexMatch => "~",
        BinaryOperator::PGRegexIMatch => "~*",
        BinaryOperator::PGRegexNotMatch => "!~",
        BinaryOperator::PGRegexNotIMatch => "!~*",
        BinaryOperator::PGLikeMatch => "~~",
        BinaryOperator::PGILikeMatch => "~~*",
        BinaryOperator::PGNotLikeMatch => "!~~",
        BinaryOperator::PGNotILikeMatch => "!~~*",
        BinaryOperator::PGStartsWith => "^@",
        BinaryOperator::Arrow => "->",
        BinaryOperator::LongArrow => "->>",
        BinaryOperator::HashArrow => "#>",
        BinaryOperator::HashLongArrow => "#>>",
        BinaryOperator::AtAt => "@@",
        BinaryOperator::AtArrow => "@>",
        BinaryOperator::ArrowAt => "<@",
        BinaryOperator::HashMinus => "#-",
        BinaryOperator::AtQuestion => "@?",
        BinaryOperator::Question => "?",
        BinaryOperator::QuestionAnd => "?&",
        BinaryOperator::QuestionPipe => "?|",
        BinaryOperator::DoubleHash => "##",
        BinaryOperator::LtDashGt => "<->",
        BinaryOperator::AndLt => "&<",
        BinaryOperator::AndGt => "&>",
        BinaryOperator::PGCustomBinaryOperator(parts) => return Ok(parts.join(".")),
        _ => return Err(diag::unsupported_construct("this operator in ANY/ALL (subquery)", pos)),
    };
    Ok(name.to_string())
}

/// Bind the left side of `x IN (q)` / `x op ANY (q)`. A row constructor `(a, b)` gives one
/// argument per element and `true`; anything else is a single argument and `false`.
fn bind_test_side(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<(Vec<BoundArg>, bool)> {
    match e {
        Expr::Tuple(items) => {
            let mut lefts: Vec<BoundArg> = Vec::with_capacity(items.len());
            for item in items {
                lefts.push(expr::bind_arg(b, item, ec)?);
            }
            Ok((lefts, true))
        }
        _ => Ok((vec![expr::bind_arg(b, e, ec)?], false)),
    }
}

/// Compare the bound left operands with the columns of the sub-select (ANY/ALL/IN test). Checks the
/// width (PG `transformSubLink`: 42601 `subquery has too many columns` when the left side is
/// narrower, `subquery has too few columns` when it is wider), resolves one operator per column
/// pair, and returns the test expression (the single coerced left operand, or a `Row` of the coerced
/// operands for a row constructor) together with the per-column operators.
fn compare_to_subquery(
    b: &mut Binder,
    lefts: Vec<BoundArg>,
    is_row: bool,
    op_name: &str,
    sub: &mut BoundQuery,
    pos: Option<u32>,
) -> Result<(BoundExpr, Vec<OpRef>)> {
    let width = sub.width();
    if lefts.len() != width {
        return Err(diag::subquery_column_count(lefts.len() < width, pos));
    }
    let mut test_args: Vec<BoundExpr> = Vec::with_capacity(width);
    let mut ops: Vec<OpRef> = Vec::with_capacity(width);
    let mut right_casts: Vec<(usize, TypeId)> = Vec::new();
    for (i, left) in lefts.into_iter().enumerate() {
        let col_ty = sub.output[i].ty;
        let probe = BoundArg::new(BoundExpr::input_ref(0, col_ty), None);
        let resolved = expr_ops::make_op(b, op_name, Some(left), probe, pos)?;
        let op_expr = match resolved.kind {
            ExprKind::Op(op_expr) => *op_expr,
            _ => return Err(PgError::internal("sub-link: operator resolution built no operator node")),
        };
        let mut args = op_expr.args;
        let right = args.pop().ok_or_else(|| PgError::internal("sub-link: operator without right operand"))?;
        let left_arg = args.pop().ok_or_else(|| PgError::internal("sub-link: operator without left operand"))?;
        if right.ty.oid != col_ty.oid {
            right_casts.push((i, right.ty));
        }
        test_args.push(left_arg);
        ops.push(op_expr.op);
    }
    for (index, target) in right_casts {
        coerce_output_column(b, sub, index, target, pos)?;
    }
    let test = if is_row {
        let names: Vec<String> = (1..=width).map(|i| format!("f{i}")).collect();
        BoundExpr::new(ExprKind::Row(Box::new(RowExpr { args: test_args, names })), TypeId::RECORD)
    } else {
        test_args
            .pop()
            .ok_or_else(|| PgError::internal("sub-link: missing test expression"))?
    };
    Ok((test, ops))
}

/// Coerce output column `index` of a sub-select to `target` by casting its target-list entry, and
/// update the output type. Only SELECT bodies have a target list to rewrite; other bodies are 0A000.
fn coerce_output_column(b: &mut Binder, sub: &mut BoundQuery, index: usize, target: TypeId, pos: Option<u32>) -> Result<()> {
    if sub.output[index].ty.oid == target.oid {
        return Ok(());
    }
    let sel = match &mut sub.body {
        QueryBody::Select(sel) => sel,
        _ => {
            return Err(diag::unsupported_construct(
                "a sub-select of VALUES or a set operation whose column needs a cast",
                pos,
            ))
        }
    };
    let entry = sel
        .target_list
        .iter_mut()
        .filter(|t| !t.junk)
        .nth(index)
        .ok_or_else(|| PgError::internal("sub-link: output column without a target entry"))?;
    let arg = BoundArg::new(entry.expr.clone(), pos);
    let coerced = coerce::coerce_strict(b, &arg, target, CastContext::Implicit, CoercionForm::Implicit)?;
    entry.expr = coerced;
    sub.output[index].ty = target;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BoundStatement;
    use pgarbor_catalog::testing;
    use crate::{BindContext, BindSession};
    use pgarbor_types::context::IoConfig;

    #[test]
    fn operator_spelling_for_any_all() {
        assert_eq!(operator_name(&BinaryOperator::Eq, None).unwrap(), "=");
        assert_eq!(operator_name(&BinaryOperator::NotEq, None).unwrap(), "<>");
        assert_eq!(operator_name(&BinaryOperator::GtEq, None).unwrap(), ">=");
        assert_eq!(operator_name(&BinaryOperator::PGLikeMatch, None).unwrap(), "~~");
        assert_eq!(
            operator_name(&BinaryOperator::PGCustomBinaryOperator(vec!["pg_catalog".into(), "+".into()]), None)
                .unwrap(),
            "pg_catalog.+"
        );
        let err = operator_name(&BinaryOperator::And, Some(7)).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(err.position, Some(7));
    }

    #[test]
    fn sublink_context_messages() {
        let err = check_sublinks_allowed(ExprCtx::check(), Some(3)).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(err.message, "cannot use subquery in check constraint");
        assert_eq!(err.position, Some(3));

        let err = check_sublinks_allowed(ExprCtx::column_default(), None).unwrap_err();
        assert_eq!(err.message, "cannot use subquery in DEFAULT expression");

        assert!(check_sublinks_allowed(ExprCtx::where_(), None).is_ok());
        assert!(check_sublinks_allowed(ExprCtx::select_list(), None).is_ok());
    }

    /// Bind one statement against the bootstrapped catalog. Needs the parser and the binder
    /// lease bodies (SQL-01, SQL-05, SQL-10, SQL-11, SQL-16) to be implemented.
    fn bind_text(sql: &str) -> Result<BoundStatement> {
        let (_kv, env) = testing::bootstrapped()?;
        let stmts = crate::parse(sql)?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        Ok(crate::bind(&stmts[0], &ctx)?.statement)
    }

    fn first_target_kind(stmt: &BoundStatement) -> Option<&ExprKind> {
        match stmt {
            BoundStatement::Query(q) => q.as_select().and_then(|s| s.target_list.first()).map(|t| &t.expr.kind),
            _ => None,
        }
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10, SQL-11, SQL-16"]
    fn exists_binds_to_sublink() {
        let stmt = bind_text("select exists(select 1)").unwrap();
        match first_target_kind(&stmt) {
            Some(ExprKind::SubLink(link)) => assert_eq!(link.kind, SubLinkKind::Exists),
            other => panic!("expected an EXISTS sub-link, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10, SQL-11, SQL-16"]
    fn scalar_subquery_rejects_two_columns() {
        let err = bind_text("select (select 1, 2)").unwrap_err();
        assert_eq!(err.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(err.message, "subquery must return only one column");
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10, SQL-11, SQL-16"]
    fn in_subquery_rejects_too_many_columns() {
        let err = bind_text("select 1 in (select 1, 2)").unwrap_err();
        assert_eq!(err.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(err.message, "subquery has too many columns");
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10, SQL-11, SQL-16"]
    fn in_subquery_without_common_operator_is_42883() {
        let err = bind_text("select 1 in (select 'x')").unwrap_err();
        assert_eq!(err.sqlstate, SqlState::UNDEFINED_FUNCTION);
    }
}
