//! UNION / INTERSECT / EXCEPT (SQL-23). PG references: `analyze.c` (`transformSetOperationStmt`,
//! `transformSetOperationTree`, `determineRecursiveColTypes`, `coerce_setop_columns`/
//! `select_common_type`/`select_common_typmod` use), `parse_coerce.c`.
//!
//! `bind_set_operation(b, op, quantifier, left, right, qctx)`:
//! 1. Bind both sides as independent queries (`select::bind_query` on a synthesized `Query`, or the
//!    block binder for `SetExpr::Select`); the sides see the OUTER scopes but not each other;
//!    parenthesized sides keep their own ORDER BY/LIMIT.
//! 2. Column counts must match: 42601 `each UNION query must have the same number of columns`
//!    (`INTERSECT` / `EXCEPT` likewise; `diag::setop_column_count`), position = the first column of
//!    the offending side.
//! 3. Per column: `coerce::coerce_to_common([l_i, r_i], "UNION"|"INTERSECT"|"EXCEPT")`
//!    (`UNION types integer and text cannot be matched`, 42804, `diag::setop_type_mismatch`);
//!    all-unknown columns resolve to text. Insert casts into each child's output: for a SELECT body
//!    wrap the target expression; for VALUES coerce the cells; for a nested set operation re-type its
//!    `types` and cast through an enclosing projection (build a `Subquery` SELECT over it if needed).
//!    Typmods: `select_common_typmod` keeps a typmod only when both sides agree.
//! 4. Result: `BoundQuery { body: SetOp(SetOpBody { op, all: quantifier == All, left, right, types }),
//!    output: one OutputColumn per common column named after the LEFT child's names (origin None),
//!    ctes: [], order_by: [] , limit: None }` — the caller (`select::bind_query`) attaches ORDER BY /
//!    LIMIT of the enclosing query.
//! 5. Quantifiers: `ALL` -> all = true; `DISTINCT`/none -> false; `BY NAME`/`ALL BY NAME` (DuckDB) ->
//!    42601. Operators: `Union`, `Intersect`, `Except`; `Minus` -> 42601.
//! 6. Recursive CTE support: `cte::bind_with` calls [`bind_recursive_union`] with the non-recursive
//!    term already typed; the recursive term's columns are coerced to the non-recursive term's types
//!    (not the other way): mismatch -> 42804 `recursive query "x" column 1 has type integer in
//!    non-recursive term but type numeric overall` HINT `Cast the output of the non-recursive term to
//!    the correct type.`
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::TypeId;
use sqlparser::ast::{Query, SetExpr, SetOperator, SetQuantifier};

use super::coerce;
use super::context::{Binder, BoundArg};
use super::select::{self, QueryCtx};
use super::values;
use crate::diag;
use crate::ir::{
    AccessNeeded, BoundExpr, BoundQuery, BoundSelect, CoercionForm, FromItem, OutputColumn, QueryBody,
    RangeColumn, RangeEntry, RangeKind, SetOpBody, SetOpKind, TargetEntry,
};

/// Operator kind and its PG keyword (`Minus` is not PostgreSQL: syntax error).
fn operator_kind(op: &SetOperator) -> Result<(SetOpKind, &'static str)> {
    match op {
        SetOperator::Union => Ok((SetOpKind::Union, "UNION")),
        SetOperator::Intersect => Ok((SetOpKind::Intersect, "INTERSECT")),
        SetOperator::Except => Ok((SetOpKind::Except, "EXCEPT")),
        SetOperator::Minus => Err(PgError::syntax("syntax error at or near \"MINUS\"", None)),
    }
}

/// `ALL` keeps duplicates; no quantifier or `DISTINCT` removes them. The `BY NAME` forms are not
/// PostgreSQL syntax.
fn quantifier_all(quantifier: &SetQuantifier) -> Result<bool> {
    match quantifier {
        SetQuantifier::All => Ok(true),
        SetQuantifier::Distinct | SetQuantifier::None => Ok(false),
        SetQuantifier::ByName | SetQuantifier::AllByName | SetQuantifier::DistinctByName => {
            Err(PgError::syntax("syntax error at or near \"BY\"", None))
        }
    }
}

/// Source position of the first column of a query expression (error position for column counts).
fn first_column_pos(b: &Binder, set: &SetExpr) -> Option<u32> {
    match set {
        SetExpr::Select(sel) => sel.projection.first().and_then(|item| b.pos(item)),
        SetExpr::Values(v) => v.rows.first().and_then(|row| row.content.first()).and_then(|e| b.pos(e)),
        SetExpr::Query(q) => first_column_pos(b, &q.body),
        SetExpr::SetOperation { left, .. } => first_column_pos(b, left),
        _ => None,
    }
}

/// Source positions of the `width` output columns of a query expression, read from the AST. Only
/// an unexpanded select list (no `*`) or the first VALUES row gives per-column positions; anything
/// else reports `None` rather than a wrong position.
pub(super) fn column_positions(b: &Binder, set: &SetExpr, width: usize) -> Vec<Option<u32>> {
    match set {
        SetExpr::Select(sel) if sel.projection.len() == width => {
            sel.projection.iter().map(|item| b.pos(item)).collect()
        }
        SetExpr::Values(v) => match v.rows.first() {
            Some(row) if row.content.len() == width => row.content.iter().map(|e| b.pos(e)).collect(),
            _ => vec![None; width],
        },
        SetExpr::Query(q) => column_positions(b, &q.body, width),
        SetExpr::SetOperation { left, .. } => column_positions(b, left, width),
        _ => vec![None; width],
    }
}

/// Context for the two sides: unknown output literals stay unknown so that the set operation can
/// coerce them to the other side's type.
fn child_ctx() -> QueryCtx {
    QueryCtx { keep_unknown_outputs: true, ..QueryCtx::default() }
}

/// A `Query` whose body is `body` and which has no clauses of its own (used to hand a bare set
/// expression to `select::bind_query`).
fn synth_query(body: &SetExpr) -> Query {
    Query {
        with: None,
        body: Box::new(body.clone()),
        order_by: None,
        limit_clause: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: Vec::new(),
    }
}

/// A bound query with no CTEs, ORDER BY, LIMIT, OFFSET or locking clause.
fn plain_query(body: QueryBody, output: Vec<OutputColumn>) -> BoundQuery {
    BoundQuery {
        ctes: Vec::new(),
        body,
        output,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        limit_with_ties: false,
        locking: Vec::new(),
    }
}

/// Bind one side of a set operation as an independent query.
fn bind_child(b: &mut Binder, set: &SetExpr) -> Result<BoundQuery> {
    match set {
        SetExpr::Select(sel) => {
            let block = select::bind_select_block(b, sel, &[], true)?;
            Ok(plain_query(QueryBody::Select(Box::new(block.select)), block.output))
        }
        SetExpr::Values(v) => {
            let bound = values::bind_values_body(b, v, None)?;
            let output = values::values_output_columns(&bound, &[]);
            Ok(plain_query(QueryBody::Values(bound), output))
        }
        SetExpr::SetOperation { op, set_quantifier, left, right } => {
            bind_set_operation(b, op, set_quantifier, left, right, &child_ctx())
        }
        SetExpr::Query(q) => select::bind_query(b, q, &child_ctx()),
        other => select::bind_query(b, &synth_query(other), &child_ctx()),
    }
}

/// Bind both sides, left first.
fn bind_children(b: &mut Binder, left: &SetExpr, right: &SetExpr) -> Result<(BoundQuery, BoundQuery)> {
    let l = bind_child(b, left)?;
    let r = bind_child(b, right)?;
    Ok((l, r))
}

/// A nested set operation has no target list to cast, so it is put under a derived SELECT
/// (`SELECT #1.c1, .., #1.cN FROM (<set operation>) unnamed_subquery`) whose target entries can be
/// cast like any SELECT body. Other bodies are returned unchanged.
fn wrap_set_operation(child: BoundQuery) -> BoundQuery {
    if !matches!(child.body, QueryBody::SetOp(_)) {
        return child;
    }
    let output = child.output.clone();
    let target_list: Vec<TargetEntry> = child
        .output
        .iter()
        .enumerate()
        .map(|(i, col)| TargetEntry {
            expr: BoundExpr::column(0, 1, i16::try_from(i + 1).unwrap_or(i16::MAX), col.ty),
            name: col.name.clone(),
            junk: false,
            origin: None,
        })
        .collect();
    let columns: Vec<RangeColumn> = child
        .output
        .iter()
        .map(|col| RangeColumn { name: col.name.clone(), ty: col.ty, dropped: false })
        .collect();
    let mut select = BoundSelect::empty();
    select.range_table.push(RangeEntry {
        kind: RangeKind::Subquery { query: Box::new(child) },
        alias: "unnamed_subquery".to_string(),
        columns,
        lateral: false,
        visible: true,
        access: AccessNeeded::default(),
    });
    select.from.push(FromItem::Range(1));
    select.target_list = target_list;
    plain_query(QueryBody::Select(Box::new(select)), output)
}

/// Index into `target_list` of the `i`-th non-junk entry.
fn visible_index(sel: &BoundSelect, i: usize) -> Result<usize> {
    sel.target_list
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.junk)
        .nth(i)
        .map(|(idx, _)| idx)
        .ok_or_else(|| PgError::internal("set operation: missing output column"))
}

/// The expression of output column `i` of a child, for the common-type decision. A SELECT body
/// gives its real expression (it is written back by [`write_column`]); a VALUES body gives a typed
/// placeholder because its cells are cast individually later.
fn column_rep(child: &mut BoundQuery, i: usize) -> Result<BoundExpr> {
    match &mut child.body {
        QueryBody::Select(sel) => {
            let idx = visible_index(&**sel, i)?;
            let target = sel
                .target_list
                .get_mut(idx)
                .ok_or_else(|| PgError::internal("set operation: missing target entry"))?;
            let placeholder = BoundExpr::null(target.expr.ty);
            Ok(std::mem::replace(&mut target.expr, placeholder))
        }
        QueryBody::Values(v) => {
            let ty = v
                .types
                .get(i)
                .copied()
                .ok_or_else(|| PgError::internal("set operation: missing VALUES column type"))?;
            Ok(BoundExpr::null(ty))
        }
        QueryBody::SetOp(_) => Err(PgError::internal("set operation: child was not wrapped")),
    }
}

/// Store the coerced column `i` into a child and retype the child's output column to `ty`. For a
/// SELECT body `expr` replaces the target expression; for VALUES every cell is cast to `ty`
/// (`expr` is then only the decision placeholder and is not used).
fn write_column(
    b: &mut Binder,
    child: &mut BoundQuery,
    i: usize,
    expr: BoundExpr,
    ty: TypeId,
    op: &str,
) -> Result<()> {
    match &mut child.body {
        QueryBody::Select(sel) => {
            let idx = visible_index(&**sel, i)?;
            let target = sel
                .target_list
                .get_mut(idx)
                .ok_or_else(|| PgError::internal("set operation: missing target entry"))?;
            target.expr = expr;
        }
        QueryBody::Values(v) => {
            for row in v.rows.iter_mut() {
                let slot = row
                    .get_mut(i)
                    .ok_or_else(|| PgError::internal("set operation: VALUES row is too short"))?;
                if slot.ty == ty {
                    continue;
                }
                let from_ty = slot.ty;
                let cell = std::mem::replace(&mut *slot, BoundExpr::null(ty));
                let coerced =
                    coerce::try_coerce(b, &BoundArg::new(cell, None), ty, CastContext::Implicit, CoercionForm::Implicit)?;
                match coerced {
                    Some(e) => *slot = e,
                    None => {
                        return Err(diag::setop_type_mismatch(op, &b.type_name(from_ty), &b.type_name(ty), None));
                    }
                }
            }
            let slot = v
                .types
                .get_mut(i)
                .ok_or_else(|| PgError::internal("set operation: missing VALUES column type"))?;
            *slot = ty;
        }
        QueryBody::SetOp(_) => return Err(PgError::internal("set operation: child was not wrapped")),
    }
    let col = child
        .output
        .get_mut(i)
        .ok_or_else(|| PgError::internal("set operation: missing output column"))?;
    col.ty = ty;
    col.origin = None;
    Ok(())
}

/// Bind `left <op> right` (module docs).
pub fn bind_set_operation(
    b: &mut Binder,
    op: &SetOperator,
    quantifier: &SetQuantifier,
    left: &SetExpr,
    right: &SetExpr,
    qctx: &QueryCtx,
) -> Result<BoundQuery> {
    // Set operations always resolve their unknown columns (to the other side's type, or text), so
    // the caller's `keep_unknown_outputs` does not change them.
    let _ = qctx;
    let (kind, word) = operator_kind(op)?;
    let all = quantifier_all(quantifier)?;

    b.enter()?;
    let bound = bind_children(b, left, right);
    b.leave();
    let (l, r) = bound?;

    let width = l.width();
    if width != r.width() {
        return Err(diag::setop_column_count(word, first_column_pos(b, right)));
    }
    let lpos = column_positions(b, left, width);
    let rpos = column_positions(b, right, width);

    let mut l = wrap_set_operation(l);
    let mut r = wrap_set_operation(r);
    let mut types: Vec<TypeId> = Vec::with_capacity(width);
    for i in 0..width {
        let lrep = column_rep(&mut l, i)?;
        let rrep = column_rep(&mut r, i)?;
        let args = vec![
            BoundArg::new(lrep, lpos.get(i).copied().flatten()),
            BoundArg::new(rrep, rpos.get(i).copied().flatten()),
        ];
        let (mut exprs, common) = coerce::coerce_to_common(b, args, word)?;
        let rexpr = exprs.pop().ok_or_else(|| PgError::internal("set operation: missing coerced column"))?;
        let lexpr = exprs.pop().ok_or_else(|| PgError::internal("set operation: missing coerced column"))?;
        write_column(b, &mut l, i, lexpr, common, word)?;
        write_column(b, &mut r, i, rexpr, common, word)?;
        types.push(common);
    }

    // Output names come from the LEFT side (PG `transformSetOperationTree`).
    let output: Vec<OutputColumn> = l
        .output
        .iter()
        .zip(types.iter())
        .map(|(col, ty)| OutputColumn { name: col.name.clone(), ty: *ty, origin: None })
        .collect();
    let body = QueryBody::SetOp(SetOpBody {
        op: kind,
        all,
        left: Box::new(l),
        right: Box::new(r),
        types,
    });
    Ok(plain_query(body, output))
}

/// Combine an already bound non-recursive term with the recursive term for `WITH RECURSIVE`
/// (module docs item 6). `cte_name` is used in the error messages.
pub fn bind_recursive_union(
    b: &mut Binder,
    cte_name: &str,
    non_recursive: BoundQuery,
    recursive: BoundQuery,
    all: bool,
) -> Result<BoundQuery> {
    let width = non_recursive.width();
    if width != recursive.width() {
        return Err(diag::setop_column_count("UNION", None));
    }
    let nonrec = non_recursive;
    let mut rec = wrap_set_operation(recursive);
    let mut types: Vec<TypeId> = Vec::with_capacity(width);
    for i in 0..width {
        let ntype = nonrec
            .output
            .get(i)
            .map(|col| col.ty)
            .ok_or_else(|| PgError::internal("recursive union: missing non-recursive column"))?;
        let rrep = column_rep(&mut rec, i)?;
        let args = vec![BoundArg::new(BoundExpr::null(ntype), None), BoundArg::new(rrep, None)];
        let (mut exprs, common) = coerce::coerce_to_common(b, args, "UNION")?;
        if common.oid != ntype.oid {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "recursive query \"{cte_name}\" column {} has type {} in non-recursive term but type {} overall",
                    i + 1,
                    b.type_name(ntype),
                    b.type_name(common)
                ),
            )
            .with_hint("Cast the output of the non-recursive term to the correct type."));
        }
        let rexpr = exprs.pop().ok_or_else(|| PgError::internal("recursive union: missing coerced column"))?;
        write_column(b, &mut rec, i, rexpr, ntype, "UNION")?;
        types.push(ntype);
    }

    let output: Vec<OutputColumn> = nonrec
        .output
        .iter()
        .zip(types.iter())
        .map(|(col, ty)| OutputColumn { name: col.name.clone(), ty: *ty, origin: None })
        .collect();
    let body = QueryBody::SetOp(SetOpBody {
        op: SetOpKind::Union,
        all,
        left: Box::new(nonrec),
        right: Box::new(rec),
        types,
    });
    Ok(plain_query(body, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BoundValues, SetOpBody};

    fn leaf(ncols: usize) -> BoundQuery {
        let output: Vec<OutputColumn> = (0..ncols)
            .map(|i| OutputColumn { name: format!("c{}", i + 1), ty: TypeId::BOOL, origin: None })
            .collect();
        let types = vec![TypeId::BOOL; ncols];
        plain_query(QueryBody::Values(BoundValues { rows: Vec::new(), types }), output)
    }

    #[test]
    fn operators_map_to_kinds() {
        assert!(matches!(operator_kind(&SetOperator::Union), Ok((SetOpKind::Union, "UNION"))));
        assert!(matches!(operator_kind(&SetOperator::Intersect), Ok((SetOpKind::Intersect, "INTERSECT"))));
        assert!(matches!(operator_kind(&SetOperator::Except), Ok((SetOpKind::Except, "EXCEPT"))));
        assert!(operator_kind(&SetOperator::Minus).is_err());
    }

    #[test]
    fn quantifiers_map_to_all_flag() {
        assert!(matches!(quantifier_all(&SetQuantifier::All), Ok(true)));
        assert!(matches!(quantifier_all(&SetQuantifier::Distinct), Ok(false)));
        assert!(matches!(quantifier_all(&SetQuantifier::None), Ok(false)));
        assert!(quantifier_all(&SetQuantifier::ByName).is_err());
        assert!(quantifier_all(&SetQuantifier::AllByName).is_err());
        assert!(quantifier_all(&SetQuantifier::DistinctByName).is_err());
    }

    #[test]
    fn plain_child_is_not_wrapped() {
        let q = wrap_set_operation(leaf(2));
        assert!(matches!(q.body, QueryBody::Values(_)));
        assert_eq!(q.output.len(), 2);
    }

    #[test]
    fn nested_set_operation_is_wrapped_in_derived_select() {
        let output = leaf(2).output;
        let inner = plain_query(
            QueryBody::SetOp(SetOpBody {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(leaf(2)),
                right: Box::new(leaf(2)),
                types: vec![TypeId::BOOL; 2],
            }),
            output,
        );
        let wrapped = wrap_set_operation(inner);
        match &wrapped.body {
            QueryBody::Select(sel) => {
                assert_eq!(sel.target_list.len(), 2);
                assert_eq!(sel.range_table.len(), 1);
                assert_eq!(sel.from.len(), 1);
            }
            _ => assert!(false, "expected a derived SELECT"),
        }
        assert_eq!(wrapped.output.len(), 2);
        assert_eq!(wrapped.output[1].name, "c2");
    }

    #[test]
    fn visible_index_skips_junk_entries() {
        let mut sel = BoundSelect::empty();
        for (i, junk) in [false, true, false].into_iter().enumerate() {
            sel.target_list.push(TargetEntry {
                expr: BoundExpr::null(TypeId::BOOL),
                name: format!("c{i}"),
                junk,
                origin: None,
            });
        }
        assert_eq!(visible_index(&sel, 0).ok(), Some(0));
        assert_eq!(visible_index(&sel, 1).ok(), Some(2));
        assert!(visible_index(&sel, 2).is_err());
    }
}
