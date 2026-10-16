//! INSERT / UPDATE / DELETE / CREATE TABLE AS planning (PLN-17, sonnet). PG references:
//! `planner.c` (`inheritance_planner` not needed), `plan/createplan.c` (`create_modifytable_plan`),
//! `rewrite/rewriteHandler.c` (`rewriteTargetListIU`, `rewriteTargetListUD`, `build_column_default`,
//! `ApplyRetrieveRule` n/a), `prep/preptlist.c` (`preprocess_targetlist`, `expand_insert_targetlist`),
//! `parser/analyze.c`, `nodeModifyTable.c` (`ExecInsert`/`ExecUpdate`/`ExecOnConflictUpdate` semantics).
//!
//! Output: `Plan` pieces assembled by `planner::plan_statement`; each function returns the root
//! `PlanNode` (a `ModifyTable`, or for CTAS a plain query node) plus nothing else.
//!
//! ## INSERT (`plan_insert`, `BoundInsert`)
//! * Input node: `InsertSource::Values(rows)` -> `ValuesScan` (cells simplified; sub-links processed;
//!   `SetToDefault{attnum}` cells replaced by the column's default expression `TargetColumn::default` or
//!   `Const(NULL)` of the column type; identity defaults are `nextval(..)` calls and remain per-row);
//!   `InsertSource::Query(q)` -> `planner::plan_query_node`; `DefaultValues` -> a `Result` node (zero
//!   columns, one row).
//! * `InsertSpec.columns = BoundInsert::columns`; `defaults` for every live attnum not in `columns` and
//!   not a stored generated column: `TargetColumn::default` simplified, else `Const(NULL)`; with
//!   `OVERRIDING USER VALUE` the identity columns listed in `columns` are moved to `defaults` (supplied
//!   value discarded); `generated` = stored generated columns' expressions rewritten from
//!   `Column{rt_index: 1, attnum}` to `InputRef(attnum-1)` over the new row. Identity/generated
//!   ALWAYS checks for a non-DEFAULT value were done by the binder for VALUES; for query sources the
//!   planner raises 428C9 `cannot insert a non-DEFAULT value into column "id"` DETAIL `Column "id" is an
//!   identity column defined as GENERATED ALWAYS.` HINT `Use OVERRIDING SYSTEM VALUE to override.` unless
//!   `overriding == Some(System)`.
//! * `ModifyTarget.checks`: each `TargetCheck.expr` rewritten to `InputRef(attnum-1)` form (these are
//!   already executable forms; setrefs leaves `InputRef` alone).
//! * `ON CONFLICT`: `ConflictSpec { arbiter_indexes, any_unique, action }`; `DoUpdate` assignments/where
//!   rewritten over `existing ++ excluded` layout: `Column{rt_index: target_rt}` -> `InputRef(attnum-1)`,
//!   `Column{rt_index: excluded_rt}` -> `InputRef(ncols + attnum-1)`; `SetToDefault` in assignments ->
//!   default expression.
//! * RETURNING: `returning` target entries' expressions over the RETURNING input row (new row,
//!   `ncols` slots): `Column{rt_index: 1, attnum}` -> `InputRef(attnum-1)`; `returning_extra_from = None`.
//!
//! ## UPDATE (`plan_update`)
//! * Input = the join of the target scan (`LogicalScan` with `with_rowid = true`, quals = WHERE conjuncts
//!   on the target) and the `FROM` items via `build::build_from_item` / `join::order_join_tree` +
//!   `pushdown`, WHERE conjuncts distributed; sub-links processed; the input node's row layout MUST be
//!   `target table row (ncols) ++ rowid ++ remaining FROM columns` (project explicitly if the join order
//!   put another relation first: use a `Project` to restore the layout). `rowid_slot = ncols`.
//! * `UpdateSpec.assignments`: `(attnum, value)` per target column per `Assignment` (multi-column
//!   `SET (a,b) = (SELECT ..)`: evaluate the `SubLink{Row}` once per row via a `Project` that adds the row
//!   value and `FieldSelect` slots - or one `SubPlan` per target column when uncorrelated); `SetToDefault`
//!   -> column default; values are bind-time expressions translated by setrefs over the input row.
//!   `generated`: stored generated columns recomputed over the NEW row (`InputRef(attnum-1)` form).
//! * RETURNING over `[new target row] ++ [input slots from rowid_slot+1]`: `returning_extra_from =
//!   Some(ncols + 1)`; `Column{rt_index: target}` -> `InputRef(attnum-1)`, FROM-table columns ->
//!   `InputRef(ncols + k)` where k is the extra-column index.
//! ## DELETE (`plan_delete`): like UPDATE without assignments; `USING` items are the extra columns.
//!
//! ## CREATE TABLE AS (`plan_create_table_as`): `PlanKind::Populate`: the plan root is the query
//! node; `Plan::output` = the new table's columns (names after the column list, types from the query).
//!
//! ## Common
//! * `PlanKind` per statement; data-modifying CTE bodies are planned with the same functions via
//!   [`plan_dml_statement`].
//! * `Plan::deps` includes the target table, referenced sequences (from defaults) and functions.
use pgarbor_catalog::model::{GeneratedKind, IdentityKind};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{
    Assignment, BoundCreateTableAs, BoundCte, BoundDelete, BoundExpr, BoundInsert, BoundQuery, BoundStatement, BoundUpdate,
    ColumnRef, ConflictAction, ExprKind, FieldSelect, FromItem, InsertSource, Overriding, RangeEntry, TargetColumn,
    TargetEntry, TargetTable,
};
use pgarbor_types::{Datum, TypeId};

use crate::build;
use crate::cte;
use crate::context::PlannerState;
use crate::join;
use crate::logical::{LogicalJoin, LogicalNode, LogicalPlan};
use crate::plan::{
    ConflictPlanAction, ConflictSpec, InsertSpec, JoinType, ModifyOp, ModifyTableNode, ModifyTarget, NodeKind, OutCol,
    PlanKind, PlanNode, ProjectNode, ResultNode, TableRef, UpdateSpec, ValuesScan,
};
use crate::{planner, pushdown, simplify, subplan};

/// SQLSTATE 428C9 (`ERRCODE_GENERATED_ALWAYS`).
const GENERATED_ALWAYS: SqlState = SqlState(*b"428C9");
/// SQLSTATE 0A000 (`ERRCODE_FEATURE_NOT_SUPPORTED`).
const FEATURE_NOT_SUPPORTED: SqlState = SqlState(*b"0A000");
/// SQLSTATE 42601 (`ERRCODE_SYNTAX_ERROR`), used for column-count mismatches in multi-column SET.
const SYNTAX_ERROR: SqlState = SqlState(*b"42601");

/// Plan any DML/CTAS statement; returns the root node and the plan kind.
pub fn plan_dml_statement(st: &mut PlannerState, stmt: &BoundStatement) -> Result<(PlanNode, PlanKind)> {
    match stmt {
        BoundStatement::Insert(ins) => Ok((plan_insert(st, ins)?, PlanKind::Insert)),
        BoundStatement::Update(upd) => Ok((plan_update(st, upd)?, PlanKind::Update)),
        BoundStatement::Delete(del) => Ok((plan_delete(st, del)?, PlanKind::Delete)),
        BoundStatement::Merge(m) => Ok((crate::build_merge::plan_merge(st, m)?, PlanKind::Merge)),
        BoundStatement::CreateTableAs(ctas) => Ok((plan_create_table_as(st, ctas)?, PlanKind::Populate)),
        _ => Err(PgError::internal("statement is not plannable")),
    }
}

/// INSERT.
pub fn plan_insert(st: &mut PlannerState, ins: &BoundInsert) -> Result<PlanNode> {
    with_ctes(st, &ins.ctes, |st| in_level(st, &ins.range_table, |st| plan_insert_inner(st, ins)))
}

/// UPDATE.
pub fn plan_update(st: &mut PlannerState, upd: &BoundUpdate) -> Result<PlanNode> {
    with_ctes(st, &upd.ctes, |st| in_level(st, &upd.range_table, |st| plan_update_inner(st, upd)))
}

/// DELETE.
pub fn plan_delete(st: &mut PlannerState, del: &BoundDelete) -> Result<PlanNode> {
    with_ctes(st, &del.ctes, |st| in_level(st, &del.range_table, |st| plan_delete_inner(st, del)))
}

/// CREATE TABLE AS source query.
pub fn plan_create_table_as(st: &mut PlannerState, ctas: &BoundCreateTableAs) -> Result<PlanNode> {
    let mut root = planner::plan_query_node(st, &ctas.query)?;
    if root.width() != ctas.table.columns.len() {
        return Err(PgError::internal("CREATE TABLE AS column count does not match the query"));
    }
    // The column list (or the query's own names) becomes the new table's column names.
    for (out, col) in root.out.iter_mut().zip(ctas.table.columns.iter()) {
        out.name = col.name.clone();
    }
    if ctas.with_data {
        return Ok(root);
    }
    // WITH NO DATA: the query is planned (for its output shape) but never runs; the root yields no rows.
    let exprs: Vec<BoundExpr> = root.out.iter().map(|c| BoundExpr::null(c.ty)).collect();
    let out = root.out.clone();
    let kind = NodeKind::Result(ResultNode { exprs, one_time_filter: Some(BoundExpr::bool_const(false)) });
    Ok(st.new_node(kind, out))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Range-table names of a DML statement, registered as one query level (EXPLAIN names).
fn level_names(entries: &[RangeEntry]) -> Vec<(String, Vec<String>)> {
    entries
        .iter()
        .map(|e| (e.alias.clone(), e.columns.iter().map(|c| c.name.clone()).collect()))
        .collect()
}

/// Run `f` inside a fresh query level built from the statement's range table.
/// Register the statement's top-level WITH list (if any) around planning the statement, so CTE
/// references (`levels_up` counts WITH frames) resolve and data-modifying CTEs are planned.
pub(crate) fn with_ctes<'a, T>(
    st: &mut PlannerState<'a>,
    ctes: &[BoundCte],
    f: impl FnOnce(&mut PlannerState<'a>) -> Result<T>,
) -> Result<T> {
    if ctes.is_empty() {
        return f(st);
    }
    cte::register_ctes(st, ctes)?;
    let result = f(&mut *st);
    cte::unregister_ctes(st);
    result
}

pub(crate) fn in_level<'a, T>(
    st: &mut PlannerState<'a>,
    entries: &[RangeEntry],
    f: impl FnOnce(&mut PlannerState<'a>) -> Result<T>,
) -> Result<T> {
    let prev = st.cur_level;
    let level = st.enter_level(level_names(entries));
    st.cur_level = level;
    let result = f(&mut *st);
    st.cur_level = prev;
    st.leave_level();
    result
}

pub(crate) fn column_of(target: &TargetTable, attnum: i16) -> Result<&TargetColumn> {
    target
        .column(attnum)
        .ok_or_else(|| PgError::internal(format!("attnum {attnum} is not a column of the DML target")))
}

pub(crate) fn table_ref(target: &TargetTable, alias: &str) -> TableRef {
    TableRef {
        oid: target.oid,
        schema: target.schema.clone(),
        name: target.name.clone(),
        alias: alias.to_string(),
        ncols: target.columns.len() as u32,
        col_types: target.columns.iter().map(|c| if c.dropped { TypeId::of(0) } else { c.ty }).collect(),
    }
}

/// Alias of range-table entry `rt` (1-based), or `fallback` when the entry is missing.
pub(crate) fn target_alias(entries: &[RangeEntry], rt: u32, fallback: &str) -> String {
    match (rt as usize).checked_sub(1).and_then(|i| entries.get(i)) {
        Some(e) => e.alias.clone(),
        None => fallback.to_string(),
    }
}

/// Record the functions and sequences an expression depends on (plan invalidation).
pub(crate) fn note_expr_deps(st: &mut PlannerState, e: &BoundExpr) {
    let mut funcs: Vec<Oid> = Vec::new();
    let mut seqs: Vec<Oid> = Vec::new();
    e.walk(&mut |x: &BoundExpr| -> bool {
        if let ExprKind::FuncCall(f) = &x.kind {
            funcs.push(f.func);
            if f.name == "nextval" {
                if let Some(ExprKind::Const(Datum::Oid(seq))) = f.args.first().map(|a| &a.kind) {
                    seqs.push(*seq);
                }
            }
        }
        true
    });
    for f in funcs {
        if !st.dep_functions.contains(&f) {
            st.dep_functions.push(f);
        }
    }
    for s in seqs {
        st.note_relation(s);
    }
}

/// `tableoid` system column attnum.
const TABLEOID_ATTNUM: i16 = -6;

/// Slot of a column of the DML target row (`attnum - 1`). Whole-row and system columns are not
/// supported in DML expressions yet.
pub(crate) fn row_slot(c: &ColumnRef, ncols: u32) -> Result<u32> {
    if c.attnum < 1 {
        return Err(PgError::unimplemented("PLN-17", "whole-row or system column reference in a DML expression"));
    }
    let a = c.attnum as u32;
    if a > ncols {
        return Err(PgError::internal(format!("column reference {a} is outside the DML target row")));
    }
    Ok(a - 1)
}

/// Slot of a reference to the target relation `rt` (level 0) in a row laid out as the table row.
fn target_slot(c: &ColumnRef, rt: u32, ncols: u32) -> Result<u32> {
    if c.levels_up != 0 || c.rt_index != rt {
        return Err(PgError::internal("unexpected column reference in a DML expression"));
    }
    row_slot(c, ncols)
}

/// Slot of a FROM/USING column: `from_base + k`, where `k` is the position of the column in the
/// flattened FROM output (`from_vars`, the planner's bind-time vars of the FROM plan).
fn from_slot(c: &ColumnRef, from_vars: &[BoundExpr], from_base: u32) -> Result<u32> {
    if c.levels_up != 0 {
        return Err(PgError::internal("unexpected outer column reference in a DML expression"));
    }
    match from_vars.iter().position(|v| matches!(&v.kind, ExprKind::Column(vc) if vc == c)) {
        Some(k) => Ok(from_base + k as u32),
        None => Err(PgError::internal("column reference not found in the DML input")),
    }
}

/// UPDATE/DELETE input row: `table row (ncols) ++ rowid ++ FROM columns`.
fn dml_input_slot(c: &ColumnRef, target_rt: u32, ncols: u32, from_vars: &[BoundExpr]) -> Result<u32> {
    if c.levels_up == 0 && c.rt_index == target_rt {
        return row_slot(c, ncols);
    }
    from_slot(c, from_vars, ncols + 1)
}

/// UPDATE/DELETE RETURNING input row: `new/deleted table row (ncols) ++ FROM columns` (rowid dropped).
fn dml_returning_slot(c: &ColumnRef, target_rt: u32, ncols: u32, from_vars: &[BoundExpr]) -> Result<u32> {
    if c.levels_up == 0 && c.rt_index == target_rt && c.attnum == TABLEOID_ATTNUM {
        // The executor appends the row's relation OID after the RETURNING input row.
        return Ok(ncols + from_vars.len() as u32);
    }
    if c.levels_up == 0 && c.rt_index == target_rt {
        return row_slot(c, ncols);
    }
    from_slot(c, from_vars, ncols)
}

/// INSERT ON CONFLICT: `existing row (ncols) ++ excluded row (ncols)`.
fn conflict_slot(c: &ColumnRef, excluded_rt: u32, ncols: u32) -> Result<u32> {
    if c.levels_up != 0 {
        return Err(PgError::internal("unexpected outer column reference in ON CONFLICT"));
    }
    if c.rt_index == 1 {
        row_slot(c, ncols)
    } else if c.rt_index == excluded_rt {
        Ok(ncols + row_slot(c, ncols)?)
    } else {
        Err(PgError::internal("unexpected column reference in ON CONFLICT"))
    }
}

/// Rewrite every level-0 `Column` of `e` into `InputRef(map(column))`, bottom-up. Sub-plan
/// arguments are rewritten too (they are evaluated over the same input row).
pub(crate) fn remap_columns(e: BoundExpr, map: &dyn Fn(&ColumnRef) -> Result<u32>) -> Result<BoundExpr> {
    e.transform(&mut |x: BoundExpr| -> Result<BoundExpr> {
        let slot = match &x.kind {
            ExprKind::Column(c) => Some(map(c)?),
            _ => None,
        };
        match slot {
            Some(s) => Ok(BoundExpr::input_ref(s, x.ty)),
            None => Ok(x),
        }
    })
}

/// `SetToDefault` must have been replaced by the time an expression reaches the plan.
pub(crate) fn reject_default(e: &BoundExpr) -> Result<()> {
    if e.any(&mut |x: &BoundExpr| matches!(x.kind, ExprKind::SetToDefault { .. })) {
        return Err(PgError::new(FEATURE_NOT_SUPPORTED, "DEFAULT is not allowed in this context"));
    }
    Ok(())
}

/// The value a column gets when the statement says DEFAULT (or supplies nothing): the column's default
/// expression (over no input row) or NULL.
pub(crate) fn default_expr(st: &mut PlannerState, target: &TargetTable, attnum: i16) -> Result<BoundExpr> {
    let col = column_of(target, attnum)?;
    match &col.default {
        Some(d) => {
            if d.any(&mut |x: &BoundExpr| matches!(x.kind, ExprKind::Column(_))) {
                return Err(PgError::internal("column default references a column"));
            }
            note_expr_deps(st, d);
            Ok(d.clone())
        }
        None => {
            let null = BoundExpr::null(col.ty);
            // rewriteHandler.c `build_column_default`: a domain-typed column without a default gets
            // `coerce_to_domain(NULL)`, so the domain's NOT NULL / CHECK constraints still apply.
            let is_domain =
                pgarbor_types::TypeEnv::type_info(st.ctx.env, col.ty.oid).is_some_and(|t| t.typtype == b'd');
            if !is_domain {
                return Ok(null);
            }
            Ok(BoundExpr::new(
                ExprKind::CoerceToDomain(Box::new(pgarbor_sql::ir::CoerceToDomain {
                    arg: null,
                    domain: col.ty.oid,
                    form: pgarbor_sql::ir::CoercionForm::Implicit,
                })),
                col.ty,
            ))
        }
    }
}

/// Lower one value of a DML statement for column `attnum`: DEFAULT becomes the column default, then
/// constant folding and sub-link planning. Column references are still bind-time (remapped later).
pub(crate) fn lower_value(st: &mut PlannerState, e: BoundExpr, target: &TargetTable, attnum: i16) -> Result<BoundExpr> {
    let e = if matches!(e.kind, ExprKind::SetToDefault { .. }) { default_expr(st, target, attnum)? } else { e };
    let e = simplify::simplify_expr(st, e)?;
    let e = subplan::process_sublinks(st, e)?;
    reject_default(&e)?;
    Ok(e)
}

/// `(attnum, is_default)` for every target column an assignment sets (PG `SetToDefault` detection).
fn assignment_defaults(a: &Assignment) -> Vec<(i16, bool)> {
    let is_default = |e: &BoundExpr| matches!(e.kind, ExprKind::SetToDefault { .. });
    if let [only] = a.targets.as_slice() {
        return vec![(only.attnum, is_default(&a.value))];
    }
    match &a.value.kind {
        ExprKind::Row(row) => a.targets.iter().zip(row.args.iter()).map(|(t, e)| (t.attnum, is_default(e))).collect(),
        _ => a.targets.iter().map(|t| (t.attnum, false)).collect(),
    }
}

/// PG `rewriteTargetListIU` for UPDATE and ON CONFLICT DO UPDATE: identity GENERATED ALWAYS and generated
/// columns can only be assigned DEFAULT. Checked in column order, like PG.
pub(crate) fn check_update_targets(target: &TargetTable, assigns: &[Assignment]) -> Result<()> {
    let mut assigned: Vec<(i16, bool)> = Vec::new();
    for a in assigns {
        assigned.extend(assignment_defaults(a));
    }
    for col in target.columns.iter() {
        let non_default = assigned.iter().any(|(n, d)| *n == col.attnum && !*d);
        if !non_default {
            continue;
        }
        if matches!(col.identity, Some(IdentityKind::Always)) {
            return Err(PgError::new(
                GENERATED_ALWAYS,
                format!("column \"{}\" can only be updated to DEFAULT", col.name),
            )
            .with_detail(format!("Column \"{}\" is an identity column defined as GENERATED ALWAYS.", col.name)));
        }
        if col.generated.is_some() {
            return Err(PgError::new(
                GENERATED_ALWAYS,
                format!("column \"{}\" can only be updated to DEFAULT", col.name),
            )
            .with_detail(format!("Column \"{}\" is a generated column.", col.name)));
        }
    }
    Ok(())
}

/// PG `rewriteTargetListIU` for INSERT ... SELECT: identity GENERATED ALWAYS (unless OVERRIDING) and
/// generated columns cannot receive a non-DEFAULT value. VALUES sources are checked by the binder.
fn check_query_insert(target: &TargetTable, columns: &[i16], overriding: Option<Overriding>) -> Result<()> {
    for col in target.columns.iter() {
        if col.dropped || !columns.contains(&col.attnum) {
            continue;
        }
        if matches!(col.identity, Some(IdentityKind::Always))
            && overriding != Some(Overriding::System)
            && overriding != Some(Overriding::User)
        {
            return Err(PgError::new(
                GENERATED_ALWAYS,
                format!("cannot insert a non-DEFAULT value into column \"{}\"", col.name),
            )
            .with_detail(format!(
                "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
                col.name
            ))
            .with_hint("Use OVERRIDING SYSTEM VALUE to override."));
        }
        if col.generated.is_some() {
            return Err(PgError::new(
                GENERATED_ALWAYS,
                format!("cannot insert a non-DEFAULT value into column \"{}\"", col.name),
            )
            .with_detail(format!("Column \"{}\" is a generated column.", col.name)));
        }
    }
    Ok(())
}

/// Lower one assignment into `(attnum, value)` pairs (values still bind-time). A multi-column SET takes
/// a row constructor (one value per column) or a row sub-link (one field selection per column).
pub(crate) fn lower_assignment(st: &mut PlannerState, target: &TargetTable, a: &Assignment) -> Result<Vec<(i16, BoundExpr)>> {
    if let [only] = a.targets.as_slice() {
        let v = lower_value(st, a.value.clone(), target, only.attnum)?;
        return Ok(vec![(only.attnum, v)]);
    }
    match &a.value.kind {
        ExprKind::Row(row) => {
            if row.args.len() != a.targets.len() {
                return Err(PgError::new(SYNTAX_ERROR, "number of columns does not match number of values"));
            }
            let mut out = Vec::with_capacity(a.targets.len());
            for (t, arg) in a.targets.iter().zip(row.args.iter()) {
                let v = lower_value(st, arg.clone(), target, t.attnum)?;
                out.push((t.attnum, v));
            }
            Ok(out)
        }
        ExprKind::SubLink(_) => {
            // One sub-plan evaluates the row; each target column selects its field from that row.
            let whole = simplify::simplify_expr(st, a.value.clone())?;
            let whole = subplan::process_sublinks(st, whole)?;
            reject_default(&whole)?;
            let mut out = Vec::with_capacity(a.targets.len());
            for (i, t) in a.targets.iter().enumerate() {
                let sel = FieldSelect { arg: whole.clone(), field: (i + 1) as i16, field_name: t.name.clone() };
                out.push((t.attnum, BoundExpr::new(ExprKind::FieldSelect(Box::new(sel)), t.ty)));
            }
            Ok(out)
        }
        _ => Err(PgError::internal("multi-column assignment value is neither a row nor a sub-link")),
    }
}

/// Rewrite the stored generated columns of the target as expressions over the NEW row (`InputRef(attnum-1)`).
pub(crate) fn generated_exprs(st: &mut PlannerState, target: &TargetTable) -> Result<Vec<(i16, BoundExpr)>> {
    let ncols = target.columns.len() as u32;
    let mut out = Vec::new();
    for col in target.columns.iter() {
        if col.dropped {
            continue;
        }
        if let Some((GeneratedKind::Stored, expr)) = &col.generated {
            let e = simplify::simplify_expr(st, expr.clone())?;
            note_expr_deps(st, &e);
            let e = remap_columns(e, &|c: &ColumnRef| target_slot(c, 1, ncols))?;
            out.push((col.attnum, e));
        }
    }
    Ok(out)
}

/// CHECK constraints over the NEW row (`InputRef(attnum-1)`).
pub(crate) fn target_checks(target: &TargetTable) -> Result<Vec<(Oid, String, BoundExpr)>> {
    let ncols = target.columns.len() as u32;
    let mut out = Vec::with_capacity(target.checks.len());
    for c in target.checks.iter() {
        let e = remap_columns(c.expr.clone(), &|cr: &ColumnRef| target_slot(cr, 1, ncols))?;
        out.push((c.constraint, c.name.clone(), e));
    }
    Ok(out)
}

/// WITH CHECK OPTIONs over the NEW row (`InputRef(attnum-1)`), sub-links planned.
pub(crate) fn target_wco(st: &mut PlannerState, target: &TargetTable) -> Result<Vec<pgarbor_sql::ir::TargetWco>> {
    let ncols = target.columns.len() as u32;
    let mut out = Vec::with_capacity(target.wco.len());
    for w in target.wco.iter() {
        let e = simplify::simplify_expr(st, w.expr.clone())?;
        let e = subplan::process_sublinks(st, e)?;
        let e = remap_columns(e, &|cr: &ColumnRef| target_slot(cr, 1, ncols))?;
        out.push(pgarbor_sql::ir::TargetWco { kind: w.kind, relname: w.relname.clone(), polname: w.polname.clone(), expr: e });
    }
    Ok(out)
}

/// Lower a RETURNING list: simplify, plan sub-links, map columns with `map`. Returns the expressions and
/// the RowDescription-style output slots.
pub(crate) fn lower_returning(
    st: &mut PlannerState,
    entries: &[TargetEntry],
    map: &dyn Fn(&ColumnRef) -> Result<u32>,
) -> Result<(Vec<BoundExpr>, Vec<OutCol>)> {
    let mut exprs = Vec::with_capacity(entries.len());
    let mut out = Vec::with_capacity(entries.len());
    for t in entries {
        let e = simplify::simplify_expr(st, t.expr.clone())?;
        let e = subplan::process_sublinks(st, e)?;
        reject_default(&e)?;
        let e = remap_columns(e, map)?;
        out.push(OutCol { name: t.name.clone(), ty: e.ty });
        exprs.push(e);
    }
    Ok((exprs, out))
}

/// Plan the input of UPDATE/DELETE: the target scan (with rowid) joined with the FROM/USING items, with the
/// WHERE conjuncts distributed by pushdown. The join output is `target row ++ rowid ++ FROM columns`
/// (joins output `left ++ right`). Returns the physical input and the FROM plan's bind-time vars
/// (their order is the FROM column order used by [`dml_input_slot`]).
fn dml_input(
    st: &mut PlannerState,
    entries: &[RangeEntry],
    target_rt: u32,
    from: &[FromItem],
    where_: Option<&BoundExpr>,
) -> Result<(PlanNode, Vec<BoundExpr>)> {
    let scan = build::build_range_scan(st, entries, target_rt, true)?;
    let target_lp = build::scan_to_plan(st, entries, scan)?;
    let quals = match where_ {
        Some(w) => simplify::simplify_quals(st, simplify::split_conjuncts(w.clone()))?,
        None => Vec::new(),
    };
    if from.is_empty() {
        let lp = pushdown::push_down_quals(st, target_lp, quals)?;
        let node = planner::plan_logical(st, &lp)?;
        return Ok((node, Vec::new()));
    }
    let mut items = Vec::with_capacity(from.len());
    for item in from {
        items.push(build::build_from_item(st, entries, item, &[])?);
    }
    let (from_lp, join_quals) = join::order_join_tree(st, items, Vec::new())?;
    let from_vars = from_lp.vars.clone();
    let target_vars = target_lp.vars.clone();
    let target_out = target_lp.out.clone();
    let mut vars = target_lp.vars.clone();
    vars.extend(from_lp.vars.iter().cloned());
    let mut out = target_lp.out.clone();
    out.extend(from_lp.out.iter().cloned());
    let mut rels = target_lp.rels.clone();
    rels.extend(from_lp.rels.iter().copied());
    rels.sort_unstable();
    rels.dedup();
    let joined = LogicalPlan {
        node: LogicalNode::Join(Box::new(LogicalJoin {
            join_type: JoinType::Inner,
            left: target_lp,
            right: from_lp,
            quals: join_quals,
            lateral: false,
        })),
        vars,
        out,
        rels,
        level: st.cur_level,
    };
    let lp = pushdown::push_down_quals(st, joined, quals)?;
    let node = planner::plan_logical(st, &lp)?;
    // The join planner may put the FROM side first: restore `target row ++ rowid ++ FROM columns`.
    let mut expected = target_vars;
    expected.push(BoundExpr::column(0, target_rt, -1, TypeId::INT8));
    expected.extend(from_vars.iter().cloned());
    if node.vars != expected && expected.iter().all(|e| node.vars.contains(e)) {
        let mut out = target_out;
        out.push(OutCol { name: "ctid".to_string(), ty: TypeId::INT8 });
        for v in &from_vars {
            let i = node.vars.iter().position(|x| x == v).unwrap_or(0);
            out.push(node.out.get(i).cloned().unwrap_or(OutCol { name: "?column?".to_string(), ty: v.ty }));
        }
        let est = node.est;
        let ordering = node.ordering.clone();
        let mut projected = st.new_node(NodeKind::Project(Box::new(ProjectNode { input: node, exprs: expected.clone() })), out);
        projected.vars = expected;
        projected.est = est;
        projected.ordering = ordering;
        return Ok((projected, from_vars));
    }
    Ok((node, from_vars))
}

// ---------------------------------------------------------------------------------------------
// INSERT
// ---------------------------------------------------------------------------------------------

fn plan_insert_inner(st: &mut PlannerState, ins: &BoundInsert) -> Result<PlanNode> {
    let target = &ins.target;
    let ncols = target.columns.len() as u32;
    st.note_relation(target.oid);

    // Supplied columns that OVERRIDING USER VALUE discards: their defaults are used instead.
    let mut kept: Vec<usize> = Vec::with_capacity(ins.columns.len());
    for (pos, attnum) in ins.columns.iter().enumerate() {
        let col = column_of(target, *attnum)?;
        if ins.overriding == Some(Overriding::User) && col.identity.is_some() {
            continue;
        }
        kept.push(pos);
    }
    let kept_attnums: Vec<i16> = kept.iter().map(|&p| ins.columns[p]).collect();

    let input = match &ins.source {
        InsertSource::Values(rows) => values_input(st, ins, rows, &kept)?,
        InsertSource::Query(q) => {
            check_query_insert(target, &ins.columns, ins.overriding)?;
            query_input(st, q, ins, &kept)?
        }
        InsertSource::DefaultValues => {
            st.new_node(NodeKind::Result(ResultNode { exprs: Vec::new(), one_time_filter: None }), Vec::new())
        }
    };

    // Defaults for every live column the input does not supply (stored generated columns are computed below).
    let mut defaults = Vec::new();
    for col in target.columns.iter() {
        if col.dropped || kept_attnums.contains(&col.attnum) {
            continue;
        }
        if matches!(col.generated, Some((GeneratedKind::Stored, _))) {
            continue;
        }
        let e = default_expr(st, target, col.attnum)?;
        let e = simplify::simplify_expr(st, e)?;
        defaults.push((col.attnum, e));
    }

    let generated = generated_exprs(st, target)?;

    let on_conflict = match &ins.on_conflict {
        None => None,
        Some(oc) => {
            if oc.arbiter.index_predicate.is_some() {
                return Err(PgError::unimplemented("PLN-17", "ON CONFLICT with an arbiter index predicate"));
            }
            let action = match &oc.action {
                ConflictAction::DoNothing => ConflictPlanAction::DoNothing,
                ConflictAction::DoUpdate { assignments, where_ } => {
                    check_update_targets(target, assignments)?;
                    let mut pairs = Vec::new();
                    for a in assignments {
                        pairs.extend(lower_assignment(st, target, a)?);
                    }
                    let excluded_rt = oc.excluded_rt;
                    let map = move |c: &ColumnRef| conflict_slot(c, excluded_rt, ncols);
                    let mut mapped = Vec::with_capacity(pairs.len());
                    for (attnum, e) in pairs {
                        mapped.push((attnum, remap_columns(e, &map)?));
                    }
                    let where_ = match where_ {
                        Some(w) => {
                            let w = simplify::simplify_expr(st, w.clone())?;
                            let w = subplan::process_sublinks(st, w)?;
                            reject_default(&w)?;
                            Some(remap_columns(w, &map)?)
                        }
                        None => None,
                    };
                    ConflictPlanAction::DoUpdate { assignments: mapped, where_ }
                }
            };
            Some(ConflictSpec {
                arbiter_indexes: oc.arbiter.indexes.clone(),
                any_unique: oc.arbiter.any_unique,
                action,
            })
        }
    };

    // Target relation is range table entry 1 (`ncols`-slot table row for RETURNING).
    // `tableoid` reads the slot the executor appends after the RETURNING input (the relation
    // that stored the row, e.g. the leaf partition; amendment pgarbor-plan-05).
    let ret_map = |c: &ColumnRef| {
        if c.levels_up == 0 && c.rt_index == 1 && c.attnum == TABLEOID_ATTNUM {
            return Ok(ncols);
        }
        target_slot(c, 1, ncols)
    };
    let (returning, out) = lower_returning(st, &ins.returning, &ret_map)?;

    let table = table_ref(target, &target_alias(&ins.range_table, 1, &target.name));
    let node = ModifyTableNode {
        op: ModifyOp::Insert,
        target: ModifyTarget { table, meta: target.clone(), checks: target_checks(target)?, wco: target_wco(st, target)? },
        input,
        rowid_slot: None,
        insert: Some(InsertSpec { columns: kept_attnums, defaults, generated, on_conflict }),
        update: None,
        returning,
        returning_extra_from: None,
        merge: None,
    };
    Ok(st.new_node(NodeKind::ModifyTable(Box::new(node)), out))
}

/// INSERT ... VALUES: a `ValuesScan` over the kept columns, DEFAULT cells replaced by the column default.
fn values_input(st: &mut PlannerState, ins: &BoundInsert, rows: &[Vec<BoundExpr>], kept: &[usize]) -> Result<PlanNode> {
    let target = &ins.target;
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != ins.columns.len() {
            return Err(PgError::internal("INSERT VALUES row does not match the column list"));
        }
        let mut cells = Vec::with_capacity(kept.len());
        for &p in kept {
            let attnum = ins.columns[p];
            let cell = row[p].clone();
            cells.push(lower_value(st, cell, target, attnum)?);
        }
        out_rows.push(cells);
    }
    let mut out = Vec::with_capacity(kept.len());
    for &p in kept {
        let col = column_of(target, ins.columns[p])?;
        out.push(OutCol { name: col.name.clone(), ty: col.ty });
    }
    // PG: a single-row INSERT ... VALUES has no VALUES range table entry; it plans as a Result over the
    // row's expressions (EXPLAIN shows `Result`, not `Values Scan`).
    if out_rows.len() == 1 {
        let exprs = out_rows.remove(0);
        return Ok(st.new_node(NodeKind::Result(ResultNode { exprs, one_time_filter: None }), out));
    }
    Ok(st.new_node(NodeKind::ValuesScan(ValuesScan { rows: out_rows, filter: None }), out))
}

/// INSERT ... SELECT: the planned query, projected down to the kept columns when some were overridden.
fn query_input(st: &mut PlannerState, q: &BoundQuery, ins: &BoundInsert, kept: &[usize]) -> Result<PlanNode> {
    let root = planner::plan_query_node(st, q)?;
    if root.width() != ins.columns.len() {
        return Err(PgError::internal("INSERT source width does not match the column list"));
    }
    if kept.len() == ins.columns.len() {
        return Ok(root);
    }
    let mut exprs = Vec::with_capacity(kept.len());
    let mut out = Vec::with_capacity(kept.len());
    for &p in kept {
        let c = root.out.get(p).ok_or_else(|| PgError::internal("INSERT source column is missing"))?;
        exprs.push(BoundExpr::input_ref(p as u32, c.ty));
        out.push(c.clone());
    }
    Ok(st.new_node(NodeKind::Project(Box::new(ProjectNode { input: root, exprs })), out))
}

// ---------------------------------------------------------------------------------------------
// UPDATE / DELETE
// ---------------------------------------------------------------------------------------------

fn plan_update_inner(st: &mut PlannerState, upd: &BoundUpdate) -> Result<PlanNode> {
    let target = &upd.target;
    let ncols = target.columns.len() as u32;
    st.note_relation(target.oid);
    check_update_targets(target, &upd.assignments)?;

    let (input, from_vars) = dml_input(st, &upd.range_table, upd.target_rt, &upd.from, upd.where_.as_ref())?;

    let mut assignments = Vec::new();
    for a in &upd.assignments {
        assignments.extend(lower_assignment(st, target, a)?);
    }
    let input_map = |c: &ColumnRef| dml_input_slot(c, upd.target_rt, ncols, &from_vars);
    let mut assign_out = Vec::with_capacity(assignments.len());
    for (attnum, e) in assignments {
        assign_out.push((attnum, remap_columns(e, &input_map)?));
    }

    let generated = generated_exprs(st, target)?;

    let ret_map = |c: &ColumnRef| dml_returning_slot(c, upd.target_rt, ncols, &from_vars);
    let (returning, out) = lower_returning(st, &upd.returning, &ret_map)?;

    let table = table_ref(target, &target_alias(&upd.range_table, upd.target_rt, &target.name));
    let node = ModifyTableNode {
        op: ModifyOp::Update,
        target: ModifyTarget { table, meta: target.clone(), checks: target_checks(target)?, wco: target_wco(st, target)? },
        input,
        rowid_slot: Some(ncols),
        insert: None,
        update: Some(UpdateSpec { assignments: assign_out, generated }),
        returning,
        returning_extra_from: Some(ncols + 1),
        merge: None,
    };
    Ok(st.new_node(NodeKind::ModifyTable(Box::new(node)), out))
}

fn plan_delete_inner(st: &mut PlannerState, del: &BoundDelete) -> Result<PlanNode> {
    let target = &del.target;
    let ncols = target.columns.len() as u32;
    st.note_relation(target.oid);

    let (input, from_vars) = dml_input(st, &del.range_table, del.target_rt, &del.using, del.where_.as_ref())?;

    let ret_map = |c: &ColumnRef| dml_returning_slot(c, del.target_rt, ncols, &from_vars);
    let (returning, out) = lower_returning(st, &del.returning, &ret_map)?;

    let table = table_ref(target, &target_alias(&del.range_table, del.target_rt, &target.name));
    let node = ModifyTableNode {
        op: ModifyOp::Delete,
        target: ModifyTarget { table, meta: target.clone(), checks: target_checks(target)?, wco: target_wco(st, target)? },
        input,
        rowid_slot: Some(ncols),
        insert: None,
        update: None,
        returning,
        returning_extra_from: Some(ncols + 1),
        merge: None,
    };
    Ok(st.new_node(NodeKind::ModifyTable(Box::new(node)), out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_catalog::model::RelKind;
    use pgarbor_sql::ir::AssignTarget;

    fn int4() -> TypeId {
        TypeId::of(23)
    }

    fn col(attnum: i16, name: &str, identity: Option<IdentityKind>) -> TargetColumn {
        TargetColumn {
            attnum,
            name: name.to_string(),
            ty: int4(),
            dropped: false,
            not_null: false,
            collation: 0,
            default: None,
            identity,
            generated: None,
        }
    }

    fn table(columns: Vec<TargetColumn>) -> TargetTable {
        TargetTable {
            oid: 16400,
            namespace: 2200,
            schema: "public".to_string(),
            name: "t".to_string(),
            kind: RelKind::Table,
            columns,
            checks: Vec::new(),
            wco: Vec::new(),
        }
    }

    fn colref(rt: u32, attnum: i16) -> ColumnRef {
        ColumnRef { levels_up: 0, rt_index: rt, attnum, pos: 0 }
    }

    #[test]
    fn remap_maps_target_columns_to_row_slots() {
        let e = BoundExpr::column(0, 1, 2, int4());
        let mapped = remap_columns(e, &|c: &ColumnRef| target_slot(c, 1, 3)).unwrap();
        assert_eq!(mapped, BoundExpr::input_ref(1, int4()));
    }

    #[test]
    fn update_input_places_from_columns_after_rowid() {
        // Target t(a, b) (ncols = 2); FROM u(d, e) contributes Column(rt 2, attnum 1) and (rt 2, attnum 2).
        let from_vars = vec![BoundExpr::column(0, 2, 1, int4()), BoundExpr::column(0, 2, 2, int4())];
        assert_eq!(dml_input_slot(&colref(1, 2), 1, 2, &from_vars).unwrap(), 1);
        assert_eq!(dml_input_slot(&colref(2, 2), 1, 2, &from_vars).unwrap(), 4);
        // RETURNING drops the rowid slot: the same FROM column is extra column 1 -> slot ncols + 1.
        assert_eq!(dml_returning_slot(&colref(2, 2), 1, 2, &from_vars).unwrap(), 3);
    }

    #[test]
    fn update_of_identity_always_requires_default() {
        let t = table(vec![col(1, "id", Some(IdentityKind::Always)), col(2, "b", None)]);
        let a = Assignment {
            targets: vec![AssignTarget { attnum: 1, name: "id".to_string(), ty: int4() }],
            value: BoundExpr::constant(Datum::Int4(5), int4()),
        };
        let err = check_update_targets(&t, &[a]).unwrap_err();
        assert_eq!(err.sqlstate.0, *b"428C9");
        assert_eq!(err.message, "column \"id\" can only be updated to DEFAULT");
    }

    #[test]
    fn update_to_default_of_identity_column_is_allowed() {
        let t = table(vec![col(1, "id", Some(IdentityKind::Always))]);
        let a = Assignment {
            targets: vec![AssignTarget { attnum: 1, name: "id".to_string(), ty: int4() }],
            value: BoundExpr::new(ExprKind::SetToDefault { attnum: 1 }, int4()),
        };
        assert!(check_update_targets(&t, &[a]).is_ok());
    }

    #[test]
    fn insert_select_rejects_identity_always_without_override() {
        let t = table(vec![col(1, "id", Some(IdentityKind::Always))]);
        let err = check_query_insert(&t, &[1], None).unwrap_err();
        assert_eq!(err.sqlstate.0, *b"428C9");
        assert!(check_query_insert(&t, &[1], Some(Overriding::System)).is_ok());
    }

    #[test]
    fn conflict_layout_is_existing_then_excluded() {
        assert_eq!(conflict_slot(&colref(1, 2), 2, 3).unwrap(), 1);
        assert_eq!(conflict_slot(&colref(2, 2), 2, 3).unwrap(), 4);
        assert!(conflict_slot(&colref(3, 1), 2, 3).is_err());
    }
}
