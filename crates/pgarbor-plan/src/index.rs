//! Access path selection for base tables: seq scan vs btree index scan (PLN-05). PG references:
//! `optimizer/path/indxpath.c` (`create_index_paths`, `match_clause_to_index`,
//! `match_clause_to_indexcol`, `build_index_paths`, `index_pathkeys_are_useful`),
//! `optimizer/path/pathkeys.c` (`build_index_pathkeys`), `plancat.c` (`get_relation_info`),
//! `access/nbtree/nbtutils.c` (`_bt_preprocess_keys` for range/equality combination rules).
//!
//! ## `choose_scan(st, scan, wanted_order)`
//! Input: a [`LogicalScan`] over `ScanSource::Table` (virtual relations: `VirtualScan` with
//! `eq_filters` for `col = const` conjuncts, no indexes), its `quals`, and optionally the sort order the
//! parent would like (`wanted_order`: ORDER BY keys when this scan is the only/driving relation).
//! Output: a `PlanNode` of kind `SeqScan` or `IndexScan` (bind-time exprs; `needed` = all true until
//! setrefs narrows it).
//!
//! ### Qual matching (a conjunct matches key column `k` of btree index `I` when)
//! * shape `key OP value` or `value OP key` (commute with the operator's commutator for the second form),
//!   `OP` one of `=`, `<`, `<=`, `>`, `>=` (by operator name AND being a btree comparison operator of
//!   the key's type family), also `key = ANY(array)` / `key IN (...)` (ScalarArrayOp `use_or`), `key IS
//!   NULL`/`IS NOT NULL` are NOT used (NULLs are indexed but keep it simple: skip);
//! * `key` is exactly `Column{levels_up: 0, rt_index == scan.rt_index, attnum == I.keys[k].column}`
//!   (relabel-only implicit casts allowed); expression-key indexes are not matched in v1;
//! * `value` contains no columns of the same level and no volatile functions (constants, params, outer
//!   references, stable functions allowed; evaluated once at scan start);
//! * operand types: same type, or same builtin btree opfamily pair (int2/int4/int8; float4/float8; text/
//!   varchar/bpchar-with-text; date/timestamp/timestamptz; numeric only with numeric); otherwise skip.
//!   When the value type differs, wrap the value in a `Cast` to the key type (planner-created implicit
//!   cast plan via `cast::plan_cast`) so the executor can encode it.
//! * Prefix rule: usable equality quals must cover a LEADING prefix `k = 0..m-1`; after the equality
//!   prefix at most one range column (`lower` and/or `upper` from `>`/`>=`/`<`/`<=` conjuncts - keep the
//!   tightest constant pair only if both are literals, else keep the first of each side and leave the
//!   others as filter). Anything not consumed stays in `IndexScan::filter`.
//! * Partial indexes (`Index::predicate`): usable only if the predicate text equals a conjunct of the
//!   query quals after deparse (exact structural match); v1 may skip partial indexes.
//! * Only `valid`, btree, non-expression-only indexes. `Index::keys[k].column == None` ends matching.
//!
//! ### Choice (no statistics available; rule based)
//! 1. If `options.enable_indexscan` is off or no usable index: `SeqScan`.
//! 2. Candidates = every index with at least one matching qual, or whose key order satisfies
//!    `wanted_order` (prefix match incl. direction/NULLS; a fully reversed order uses `Backward`).
//! 3. Score: unique index fully covered by equality quals = best (1 row); else number of consumed
//!    equality columns (more is better) then presence of a range; an index that only provides ordering
//!    ranks last and is chosen over seq scan only when `wanted_order` is set AND a LIMIT is present above
//!    (planner passes `want_ordered_scan_for_limit`).
//! 4. An index scan with no bounds at all and no ordering benefit is never chosen.
//! 5. Estimated rows: unique full match 1; equality prefix on non-unique index `table_rows * 0.005 ^
//!    cols`; range `1/3`; see `cost`.
//!
//! ## Output node details
//! `IndexScan.index_cond` lists the consumed conjuncts (EXPLAIN `Index Cond`), `filter` the rest;
//! `recheck_index_cond = false` for exact bounds. `ordering` of the node is set from the index keys
//! (for the equality-prefix columns the remaining order columns) so `sort_limit` can skip a Sort.
use pgarbor_catalog::model::{IndexKey, IndexMethod};
use pgarbor_catalog::{CatalogEnv, Index};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundExpr, CastExpr, CoercionForm, ExprKind};
use pgarbor_types::cast::{plan_cast, CastPlan};
use pgarbor_types::catalog_data::{CastContext, Volatility};
use pgarbor_types::{oids, Datum, TypeId};

use crate::context::PlannerState;
use crate::logical::{LogicalScan, ScanSource};
use crate::plan::{
    Estimate, IndexBounds, IndexKeyRef, IndexRef, IndexScan, KeyEq, NodeKind, OutCol, PlanNode, RangeEnd,
    ScanDirection, SeqScan, SortKeySpec, TableRef, VirtualScan,
};
use crate::simplify::make_and;

/// Cost constants (PG `costsize.c` defaults, see the `cost` module docs).
const CPU_TUPLE_COST: f64 = 0.01;
const CPU_INDEX_TUPLE_COST: f64 = 0.005;
const SEQ_PAGE_COST: f64 = 1.0;
const RANDOM_PAGE_COST: f64 = 4.0;
/// PG `DEFAULT_EQ_SEL`.
const DEFAULT_EQ_SEL: f64 = 0.005;
/// Row estimate of a virtual catalog relation (cost module docs).
const VIRTUAL_ROWS: f64 = 100.0;
/// Average width estimate per column in bytes.
const WIDTH_PER_COLUMN: u32 = 8;

/// Comparison of a qual against one key column, normalized to `key OP value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    /// `key = ANY (array)`.
    AnyEq,
}

impl Cmp {
    fn from_op(name: &str) -> Option<Cmp> {
        match name {
            "=" => Some(Cmp::Eq),
            "<" => Some(Cmp::Lt),
            "<=" => Some(Cmp::Le),
            ">" => Some(Cmp::Gt),
            ">=" => Some(Cmp::Ge),
            _ => None,
        }
    }

    /// The operator with its operands swapped (`value < key` is `key > value`).
    fn commute(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            other => other,
        }
    }
}

/// One conjunct that can be used as an index condition on a column of the scanned relation.
#[derive(Clone, Debug)]
struct QualInfo {
    /// Position of the conjunct in the quals it was taken from.
    pos: usize,
    /// Attribute number of the key column.
    attnum: i16,
    /// Type of the key column.
    key_ty: TypeId,
    cmp: Cmp,
    /// The value side (for `AnyEq`: the array expression), not yet coerced to the key type.
    value: BoundExpr,
}

/// Interesting type families for cross-type comparisons (same family, implicit cast of the value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Int,
    Float,
    Text,
    DateTime,
    Numeric,
}

fn family(oid: Oid) -> Option<Family> {
    match oid {
        oids::INT2 | oids::INT4 | oids::INT8 => Some(Family::Int),
        oids::FLOAT4 | oids::FLOAT8 => Some(Family::Float),
        oids::TEXT | oids::VARCHAR | oids::BPCHAR => Some(Family::Text),
        oids::DATE | oids::TIMESTAMP | oids::TIMESTAMPTZ => Some(Family::DateTime),
        oids::NUMERIC => Some(Family::Numeric),
        _ => None,
    }
}

/// Strip relabel-only casts (`CastPlan::is_noop`) around an expression.
fn strip_relabel(e: &BoundExpr) -> &BoundExpr {
    let mut cur = e;
    while let ExprKind::Cast(c) = &cur.kind {
        if !c.plan.is_noop() {
            break;
        }
        cur = &c.arg;
    }
    cur
}

/// `(attnum, type)` when `e` is a plain attribute of this scan (level 0, range-table index `rt`).
fn scan_column(e: &BoundExpr, rt: u32) -> Option<(i16, TypeId)> {
    let inner = strip_relabel(e);
    match &inner.kind {
        ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == rt && c.attnum > 0 => Some((c.attnum, inner.ty)),
        _ => None,
    }
}

/// A value usable by an index bound: no same-level columns (other than the scanned relation when
/// `allow_other_rels` is false: nothing at level 0 at all), no volatile functions and no sub-plans
/// or aggregate-like nodes. Stable and immutable expressions, parameters and outer references pass.
fn value_ok(e: &BoundExpr, rt: u32, allow_other_rels: bool) -> bool {
    if e.volatility() == Volatility::Volatile {
        return false;
    }
    !e.any(&mut |n| match &n.kind {
        ExprKind::Column(c) => c.levels_up == 0 && (!allow_other_rels || c.rt_index == rt),
        ExprKind::Aggref(_)
        | ExprKind::WindowFunc(_)
        | ExprKind::Grouping(_)
        | ExprKind::CaseTestValue
        | ExprKind::InputRef(_)
        | ExprKind::OuterRef(_)
        | ExprKind::SubLink(_)
        | ExprKind::SubPlan(_)
        | ExprKind::SetToDefault { .. } => true,
        _ => false,
    })
}

/// Classify one conjunct as `key OP value` (or `key = ANY(array)`) on this scan.
fn classify(e: &BoundExpr, rt: u32, pos: usize, allow_other_rels: bool) -> Option<QualInfo> {
    match &e.kind {
        ExprKind::Op(o) if o.args.len() == 2 => {
            let cmp = Cmp::from_op(&o.op.name)?;
            let l = &o.args[0];
            let r = &o.args[1];
            if let Some((attnum, key_ty)) = scan_column(l, rt) {
                if !value_ok(r, rt, allow_other_rels) {
                    return None;
                }
                return Some(QualInfo { pos, attnum, key_ty, cmp, value: r.clone() });
            }
            if let Some((attnum, key_ty)) = scan_column(r, rt) {
                if !value_ok(l, rt, allow_other_rels) {
                    return None;
                }
                return Some(QualInfo { pos, attnum, key_ty, cmp: cmp.commute(), value: l.clone() });
            }
            None
        }
        ExprKind::ScalarArrayOp(s) if s.use_or && s.op.name == "=" => {
            let (attnum, key_ty) = scan_column(&s.left, rt)?;
            let elem_type = match &s.right.kind {
                ExprKind::Array(arr) => arr.elem_type,
                _ => return None,
            };
            if elem_type != key_ty.oid || !value_ok(&s.right, rt, allow_other_rels) {
                return None;
            }
            Some(QualInfo { pos, attnum, key_ty, cmp: Cmp::AnyEq, value: s.right.clone() })
        }
        _ => None,
    }
}

/// All classifiable conjuncts of `quals`, keeping their positions.
fn collect_infos(quals: &[BoundExpr], rt: u32, allow_other_rels: bool) -> Vec<QualInfo> {
    quals.iter().enumerate().filter_map(|(i, q)| classify(q, rt, i, allow_other_rels)).collect()
}

/// Wrap `value` in a cast node to `target` (typmod -1: the key's own storage type).
fn cast_node(value: &BoundExpr, plan: CastPlan) -> BoundExpr {
    let ty = plan.to;
    BoundExpr::new(
        ExprKind::Cast(Box::new(CastExpr { arg: value.clone(), plan, form: CoercionForm::Implicit })),
        ty,
    )
}

/// Integer value of an integer constant.
fn int_const(e: &BoundExpr) -> Option<i64> {
    match &e.kind {
        ExprKind::Const(Datum::Int2(v)) => Some(i64::from(*v)),
        ExprKind::Const(Datum::Int4(v)) => Some(i64::from(*v)),
        ExprKind::Const(Datum::Int8(v)) => Some(*v),
        _ => None,
    }
}

/// Whether integer `v` is representable in the integer type `oid`.
fn fits(v: i64, oid: Oid) -> bool {
    match oid {
        oids::INT2 => i16::try_from(v).is_ok(),
        oids::INT4 => i32::try_from(v).is_ok(),
        _ => true,
    }
}

/// The value as the index key type. Same type: unchanged. Same family: a cast in the implicit
/// direction (which cannot fail), or an assignment cast of an integer constant that fits the key
/// type. `None` when the comparison cannot be turned into a key bound with identical semantics.
fn coerce_value(env: &CatalogEnv, value: &BoundExpr, key_ty: TypeId) -> Option<BoundExpr> {
    let from = value.ty;
    if from.oid == key_ty.oid {
        return Some(value.clone());
    }
    let fam = family(from.oid)?;
    if family(key_ty.oid)? != fam {
        return None;
    }
    // bpchar values compare as text after trailing blanks are dropped; only text-like keys take that path.
    if fam == Family::Text && !matches!(key_ty.oid, oids::TEXT | oids::VARCHAR) {
        return None;
    }
    let target = TypeId::of(key_ty.oid);
    if let Ok(plan) = plan_cast(env, from, target, CastContext::Implicit) {
        return Some(cast_node(value, plan));
    }
    if fam == Family::Int {
        if let Some(v) = int_const(value) {
            if fits(v, key_ty.oid) {
                if let Ok(plan) = plan_cast(env, from, target, CastContext::Assignment) {
                    return Some(cast_node(value, plan));
                }
            }
        }
    }
    None
}

/// Bound value of a range condition (a NULL bound never yields a range).
fn range_value(env: &CatalogEnv, q: &QualInfo) -> Option<BoundExpr> {
    if q.value.is_null_const() {
        return None;
    }
    coerce_value(env, &q.value, q.key_ty)
}

/// Key columns of `idx` usable for matching: the key columns before the first expression key.
fn plain_key_columns(idx: &Index) -> &[IndexKey] {
    let keys = idx.key_columns();
    let n = keys.iter().take_while(|k| k.column.is_some()).count();
    &keys[..n]
}

/// Whether `idx` can serve scans of `table_oid` in v1 (valid btree, no partial predicate, at least one
/// plain key column).
fn index_usable(idx: &Index, table_oid: Oid) -> bool {
    idx.table == table_oid
        && idx.valid
        && idx.method == IndexMethod::Btree
        && idx.predicate.is_none()
        && !plain_key_columns(idx).is_empty()
}

/// Match the classified quals against the key columns of `idx`: equality prefix, then one range
/// on the next key column. `None` when neither an equality nor a range is consumed.
fn build_match(env: &CatalogEnv, idx: &Index, quals: &[BoundExpr], infos: &[QualInfo]) -> Option<IndexMatch> {
    let keys = plain_key_columns(idx);
    let mut used: Vec<usize> = Vec::new();
    let mut eq: Vec<KeyEq> = Vec::new();

    for key in keys {
        let attnum = match key.column {
            Some(a) => a,
            None => break,
        };
        let mut hit: Option<(usize, KeyEq)> = None;
        for q in infos {
            if q.attnum != attnum || used.contains(&q.pos) {
                continue;
            }
            match q.cmp {
                Cmp::Eq => {
                    if let Some(v) = coerce_value(env, &q.value, q.key_ty) {
                        hit = Some((q.pos, KeyEq::Value(v)));
                        break;
                    }
                }
                Cmp::AnyEq => {
                    hit = Some((q.pos, KeyEq::AnyOf(q.value.clone())));
                    break;
                }
                _ => {}
            }
        }
        match hit {
            Some((pos, ke)) => {
                used.push(pos);
                eq.push(ke);
            }
            None => break,
        }
    }

    let mut lower: Option<RangeEnd> = None;
    let mut upper: Option<RangeEnd> = None;
    if let Some(attnum) = keys.get(eq.len()).and_then(|k| k.column) {
        for q in infos {
            if q.attnum != attnum || used.contains(&q.pos) {
                continue;
            }
            match q.cmp {
                Cmp::Gt | Cmp::Ge if lower.is_none() => {
                    if let Some(v) = range_value(env, q) {
                        used.push(q.pos);
                        lower = Some(RangeEnd { value: v, inclusive: q.cmp == Cmp::Ge });
                    }
                }
                Cmp::Lt | Cmp::Le if upper.is_none() => {
                    if let Some(v) = range_value(env, q) {
                        used.push(q.pos);
                        upper = Some(RangeEnd { value: v, inclusive: q.cmp == Cmp::Le });
                    }
                }
                _ => {}
            }
        }
    }

    if eq.is_empty() && lower.is_none() && upper.is_none() {
        return None;
    }

    used.sort_unstable();
    let consumed: Vec<BoundExpr> = used.iter().filter_map(|&p| quals.get(p).cloned()).collect();
    let remaining: Vec<BoundExpr> = quals
        .iter()
        .enumerate()
        .filter(|(p, _)| used.binary_search(p).is_err())
        .map(|(_, q)| q.clone())
        .collect();
    let eq_cols = eq.len();
    let all_value = eq.iter().all(|e| matches!(e, KeyEq::Value(_)));
    let unique_point = idx.unique
        && keys.len() == idx.n_key_columns as usize
        && eq_cols == keys.len()
        && all_value;

    Some(IndexMatch {
        index_oid: idx.oid,
        eq,
        lower,
        upper,
        consumed,
        remaining,
        unique_point,
        eq_cols,
    })
}

/// Whether the index keys, read from the start (or fully reversed), satisfy `wanted`. Key columns
/// fixed by an equality on a constant are ordered trivially and may be skipped.
fn ordering_match(keys: &[IndexKey], eq: &[KeyEq], rt: u32, wanted: &[SortKeySpec], reverse: bool) -> bool {
    let mut p = 0usize;
    for w in wanted {
        if w.using_op.is_some() {
            return false;
        }
        let col = match scan_column(&w.expr, rt) {
            Some((col, _)) => col,
            None => return false,
        };
        let fixed = eq
            .iter()
            .zip(keys.iter())
            .any(|(e, k)| matches!(e, KeyEq::Value(_)) && k.column == Some(col));
        if fixed {
            continue;
        }
        while p < eq.len() && matches!(eq[p], KeyEq::Value(_)) {
            p += 1;
        }
        let k = match keys.get(p) {
            Some(k) => k,
            None => return false,
        };
        if k.column != Some(col) {
            return false;
        }
        let (desc, nulls_first) = if reverse {
            (!k.descending, !k.nulls_first)
        } else {
            (k.descending, k.nulls_first)
        };
        if w.descending != desc || w.nulls_first != nulls_first {
            return false;
        }
        p += 1;
    }
    true
}

/// Scan direction under which the index provides the wanted order (forward preferred).
fn wanted_direction(
    keys: &[IndexKey],
    eq: &[KeyEq],
    rt: u32,
    wanted: Option<&[SortKeySpec]>,
) -> Option<ScanDirection> {
    let wanted = wanted?;
    if wanted.is_empty() {
        return None;
    }
    if ordering_match(keys, eq, rt, wanted, false) {
        Some(ScanDirection::Forward)
    } else if ordering_match(keys, eq, rt, wanted, true) {
        Some(ScanDirection::Backward)
    } else {
        None
    }
}

/// The order an index scan in direction `dir` produces: the key columns in index order after the
/// `skip` leading equality-on-constant columns (constant columns are trivially ordered).
fn index_ordering(keys: &[IndexKey], skip: usize, rt: u32, dir: ScanDirection, table: &TableRef) -> Vec<SortKeySpec> {
    let mut out = Vec::with_capacity(keys.len());
    for k in keys.iter().skip(skip) {
        let attnum = match k.column {
            Some(a) => a,
            None => break,
        };
        let (descending, nulls_first) = match dir {
            ScanDirection::Forward => (k.descending, k.nulls_first),
            ScanDirection::Backward => (!k.descending, !k.nulls_first),
        };
        let idx0 = usize::try_from(attnum - 1).unwrap_or(0);
        out.push(SortKeySpec {
            expr: BoundExpr::column(0, rt, attnum, col_type(table, idx0)),
            descending,
            nulls_first,
            collation: k.collation,
            using_op: None,
        });
    }
    out
}

/// Type of attribute slot `i` (0-based) of a table reference.
fn col_type(table: &TableRef, i: usize) -> TypeId {
    table.col_types.get(i).copied().unwrap_or(TypeId::of(0))
}

/// Placeholder names of dropped attributes (PG `........pg.dropped.N........`).
fn dropped_names(ncols: usize) -> Vec<String> {
    (1..=ncols).map(|i| format!("........pg.dropped.{i}........")).collect()
}

/// Column names of a base table, by slot (dropped columns get PG's placeholder names).
fn table_names(env: &CatalogEnv, oid: Oid, ncols: usize) -> Result<Vec<String>> {
    let rel = env
        .snapshot()
        .relation(oid)
        .ok_or_else(|| PgError::internal(format!("relation {oid} is not in the catalog snapshot")))?;
    let mut names = dropped_names(ncols);
    for c in &rel.columns {
        if c.attnum >= 1 && (c.attnum as usize) <= ncols {
            names[c.attnum as usize - 1] = c.name.clone();
        }
    }
    Ok(names)
}

/// Column names of a virtual catalog relation, by slot.
fn virtual_names(oid: Oid, ncols: usize) -> Result<Vec<String>> {
    let rel = pgarbor_catalog::vrel::lookup(oid)
        .ok_or_else(|| PgError::internal(format!("virtual relation {oid} is not registered")))?;
    let sys = rel.schema()?;
    let mut names = dropped_names(ncols);
    for c in sys.columns.iter() {
        if c.attnum >= 1 && (c.attnum as usize) <= ncols {
            names[c.attnum as usize - 1] = c.name.to_string();
        }
    }
    Ok(names)
}

/// Base row count: virtual relations a fixed size, tables the planner hint or the default.
fn base_rows(st: &PlannerState, oid: Oid, is_virtual: bool) -> f64 {
    if is_virtual {
        return VIRTUAL_ROWS;
    }
    st.ctx.table_rows.get(&oid).copied().unwrap_or(st.ctx.options.default_table_rows)
}

fn estimate(rows: f64, total_cost: f64, width: u32) -> Estimate {
    Estimate { startup_cost: 0.0, total_cost, rows: rows.max(1.0), width }
}

/// Rows an index scan reads: unique point 1; equality prefix `base * 0.005^cols`; range `1/3`;
/// no bounds (ordering only) the whole table.
fn index_rows(base: f64, m: Option<&IndexMatch>) -> f64 {
    match m {
        Some(x) if x.unique_point => 1.0,
        Some(x) if x.eq_cols > 0 => base * DEFAULT_EQ_SEL.powi(x.eq_cols as i32),
        Some(_) => base / 3.0,
        None => base,
    }
}

/// Equality hints of a virtual scan: `column = value` with the column's own type, no NULLs.
fn virtual_eq_filters(quals: &[BoundExpr], rt: u32) -> Vec<(i16, BoundExpr)> {
    collect_infos(quals, rt, false)
        .into_iter()
        .filter(|q| q.cmp == Cmp::Eq && q.value.ty.oid == q.key_ty.oid && !q.value.is_null_const())
        .map(|q| (q.attnum, q.value))
        .collect()
}

/// Sequential scan node over `scan`.
fn seq_scan_node(st: &PlannerState, scan: &LogicalScan, table: &TableRef, out: Vec<OutCol>, vars: Vec<BoundExpr>, base: f64) -> PlanNode {
    let ncols = out.len();
    let width = (ncols as u32).saturating_mul(WIDTH_PER_COLUMN);
    let pages = (base * f64::from(width) / 8192.0).max(1.0);
    let node = SeqScan {
        table: table.clone(),
        filter: make_and(scan.quals.clone()),
        with_rowid: scan.with_rowid,
        needed: vec![true; ncols],
    };
    let mut n = st.new_node(NodeKind::SeqScan(node), out);
    n.vars = vars;
    n.est = estimate(base, base * CPU_TUPLE_COST + pages * SEQ_PAGE_COST, width);
    n
}

/// Index scan node over `idx` with the chosen match (or none: an ordered full index scan).
#[allow(clippy::too_many_arguments)]
fn index_scan_node(
    st: &PlannerState,
    scan: &LogicalScan,
    table: &TableRef,
    idx: &Index,
    m: Option<IndexMatch>,
    dir: Option<ScanDirection>,
    out: Vec<OutCol>,
    vars: Vec<BoundExpr>,
    base: f64,
) -> PlanNode {
    let direction = dir.unwrap_or(ScanDirection::Forward);
    let ncols = out.len();
    let width = (ncols as u32).saturating_mul(WIDTH_PER_COLUMN);
    let index = IndexRef {
        oid: idx.oid,
        name: idx.name.clone(),
        unique: idx.unique,
        primary: idx.primary,
        nulls_not_distinct: idx.nulls_not_distinct,
        keys: idx
            .key_columns()
            .iter()
            .map(|k| IndexKeyRef { column: k.column, ty: k.type_id, descending: k.descending, nulls_first: k.nulls_first })
            .collect(),
    };
    let leading_const = m
        .as_ref()
        .map_or(0, |x| x.eq.iter().take_while(|e| matches!(e, KeyEq::Value(_))).count());
    let ordering = index_ordering(plain_key_columns(idx), leading_const, scan.rt_index, direction, table);
    let rows = index_rows(base, m.as_ref()).max(1.0);
    let (bounds, index_cond, remaining) = match m {
        Some(mm) => (
            IndexBounds { eq: mm.eq, lower: mm.lower, upper: mm.upper },
            mm.consumed,
            mm.remaining,
        ),
        None => (IndexBounds::default(), Vec::new(), scan.quals.clone()),
    };
    let node = IndexScan {
        table: table.clone(),
        index,
        bounds,
        index_cond,
        recheck_index_cond: false,
        filter: make_and(remaining),
        direction,
        with_rowid: scan.with_rowid,
        needed: vec![true; ncols],
    };
    let mut n = st.new_node(NodeKind::IndexScan(node), out);
    n.vars = vars;
    n.ordering = ordering;
    n.est = estimate(rows, rows * (RANDOM_PAGE_COST + CPU_INDEX_TUPLE_COST), width);
    n
}

/// Pick the access path for a base-table scan (module docs).
pub fn choose_scan(
    st: &mut PlannerState,
    scan: &LogicalScan,
    wanted_order: Option<&[SortKeySpec]>,
    want_ordered_scan_for_limit: bool,
) -> Result<PlanNode> {
    let (table, is_virtual) = match &scan.source {
        ScanSource::Table { table, is_virtual } => (table, *is_virtual),
        _ => return Err(PgError::internal("access path selection needs a base table scan")),
    };
    let rt = scan.rt_index;
    let ncols = table.ncols as usize;
    let names = if is_virtual && st.ctx.env.snapshot().sequence(table.oid).is_some() {
        // `SELECT * FROM seq` (sequence.c): last_value, log_cnt, is_called.
        vec!["last_value".to_string(), "log_cnt".to_string(), "is_called".to_string()]
    } else if is_virtual {
        virtual_names(table.oid, ncols)?
    } else {
        table_names(st.ctx.env, table.oid, ncols)?
    };
    let out: Vec<OutCol> = names
        .into_iter()
        .enumerate()
        .map(|(i, name)| OutCol { name, ty: col_type(table, i) })
        .collect();
    let vars: Vec<BoundExpr> = out
        .iter()
        .enumerate()
        .map(|(i, c)| BoundExpr::column(0, rt, (i + 1) as i16, c.ty))
        .collect();
    let base = base_rows(st, table.oid, is_virtual);
    let quals = &scan.quals;

    if is_virtual {
        let width = (ncols as u32).saturating_mul(WIDTH_PER_COLUMN);
        let node = VirtualScan {
            table: table.clone(),
            eq_filters: virtual_eq_filters(quals, rt),
            filter: make_and(quals.clone()),
        };
        let mut n = st.new_node(NodeKind::VirtualScan(node), out);
        n.vars = vars;
        n.est = estimate(base, base * CPU_TUPLE_COST, width);
        return Ok(n);
    }

    if !st.ctx.options.enable_indexscan {
        return Ok(seq_scan_node(st, scan, table, out, vars, base));
    }

    let env: &CatalogEnv = st.ctx.env;
    let infos = collect_infos(quals, rt, false);

    // Score: (rank, equality columns, has range, provides order). Rank 3 unique point, 2 bounds,
    // 1 ordering only (only when the planner asks for an ordered scan for a LIMIT).
    type Score = (u8, usize, bool, bool);
    let mut best: Option<(Score, &Index, Option<IndexMatch>, Option<ScanDirection>)> = None;
    for idx in env.snapshot().indexes_of(table.oid) {
        if !index_usable(idx, table.oid) {
            continue;
        }
        let keys = plain_key_columns(idx);
        let m = build_match(env, idx, quals, &infos);
        let eq: &[KeyEq] = match &m {
            Some(x) => &x.eq,
            None => &[],
        };
        let dir = wanted_direction(keys, eq, rt, wanted_order);
        let rank: u8 = match &m {
            Some(x) if x.unique_point => 3,
            Some(_) => 2,
            None if dir.is_some() && want_ordered_scan_for_limit => 1,
            None => continue,
        };
        let (eq_cols, ranged) = match &m {
            Some(x) => (x.eq_cols, x.lower.is_some() || x.upper.is_some()),
            None => (0, false),
        };
        let score: Score = (rank, eq_cols, ranged, dir.is_some());
        let better = match &best {
            None => true,
            Some((s, _, _, _)) => score > *s,
        };
        if better {
            best = Some((score, idx, m, dir));
        }
    }

    match best {
        Some((_, idx, m, dir)) => Ok(index_scan_node(st, scan, table, idx, m, dir, out, vars, base)),
        None => Ok(seq_scan_node(st, scan, table, out, vars, base)),
    }
}

/// Result of matching quals against one index (exposed for the join planner's parameterized
/// inner-index lookups, which calls it with `outer_params` quals pre-bound).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMatch {
    pub index_oid: pgarbor_common::Oid,
    /// Equality conditions on the leading key prefix (value expressions).
    pub eq: Vec<crate::plan::KeyEq>,
    pub lower: Option<crate::plan::RangeEnd>,
    pub upper: Option<crate::plan::RangeEnd>,
    /// Conjuncts consumed by the index.
    pub consumed: Vec<BoundExpr>,
    /// Conjuncts left for the filter.
    pub remaining: Vec<BoundExpr>,
    /// Whole unique index covered by equalities (<= 1 row).
    pub unique_point: bool,
    /// Number of equality-covered key columns.
    pub eq_cols: usize,
}

/// Match `quals` against index `index_oid` of table `table_oid` for range-table entry `rt_index`.
/// `None` when nothing matches. Pure function over the catalog snapshot (`st.ctx.env`).
pub fn match_index(
    st: &PlannerState,
    table_oid: pgarbor_common::Oid,
    rt_index: u32,
    index_oid: pgarbor_common::Oid,
    quals: &[BoundExpr],
) -> Result<Option<IndexMatch>> {
    let env = st.ctx.env;
    let idx = match env.snapshot().index(index_oid) {
        Some(idx) => idx,
        None => return Ok(None),
    };
    if !index_usable(idx, table_oid) {
        return Ok(None);
    }
    let infos = collect_infos(quals, rt_index, false);
    Ok(build_match(env, idx, quals, &infos))
}

/// Best equality-lookup index for a join inner side: given equi-join conjuncts `inner.col = outer_expr`
/// (`outer_exprs` are bind-time expressions over the outer relations, to become `OuterRef` parameters),
/// find a btree index of the inner table whose leading key columns are all covered. Returns the index
/// match with `eq` values referring to the outer expressions (the join planner rewrites them to
/// `OuterRef`s).
pub fn find_param_index(
    st: &PlannerState,
    table_oid: pgarbor_common::Oid,
    rt_index: u32,
    equi_quals: &[BoundExpr],
) -> Result<Option<IndexMatch>> {
    let env = st.ctx.env;
    let infos: Vec<QualInfo> = collect_infos(equi_quals, rt_index, true)
        .into_iter()
        .filter(|q| q.cmp == Cmp::Eq)
        .collect();
    if infos.is_empty() {
        return Ok(None);
    }
    let mut best: Option<IndexMatch> = None;
    for idx in env.snapshot().indexes_of(table_oid) {
        if !index_usable(idx, table_oid) {
            continue;
        }
        let m = match build_match(env, idx, equi_quals, &infos) {
            Some(m) => m,
            None => continue,
        };
        let better = match &best {
            None => true,
            Some(b) => (m.unique_point, m.eq_cols) > (b.unique_point, b.eq_cols),
        };
        if better {
            best = Some(m);
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PlanContext;
    use crate::context::PlanOptions;
    use crate::plan::SortKeySpec;
    use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec, IndexKeySpec, IndexSpec};
    use pgarbor_catalog::model::relation::Persistence;
    use pgarbor_catalog::{testing, RelKind};
    use pgarbor_common::MemKv;
    use pgarbor_sql::ir::{OpExpr, OpRef};
    use std::collections::HashMap;

    /// `public.t(id int4, a int4, b int4, c text)` with unique `t_pkey(id)`, `t_a(a)`, `t_a_b(a, b)`.
    fn fixture() -> Result<(MemKv, CatalogEnv, Oid)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateTableSpec {
            namespace: 2200,
            name: "t".to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![
                ColumnSpec::new("id", TypeId::INT4),
                ColumnSpec::new("a", TypeId::INT4),
                ColumnSpec::new("b", TypeId::INT4),
                ColumnSpec::new("c", TypeId::TEXT),
            ],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        };
        let mut tbl: Option<Oid> = None;
        let (_r, mut env) = testing::run(&mut kv, &env, |t| {
            tbl = t.create_table(&spec)?.map(|c| c.table);
            Ok(())
        })?;
        let tbl = tbl.ok_or_else(|| PgError::internal("fixture table was not created"))?;
        let indexes: Vec<(&str, Vec<&str>, bool)> = vec![
            ("t_pkey", vec!["id"], true),
            ("t_a", vec!["a"], false),
            ("t_a_b", vec!["a", "b"], false),
        ];
        for (name, cols, unique) in indexes {
            let keys: Vec<IndexKeySpec> = cols
                .iter()
                .map(|c| IndexKeySpec {
                    column: Some(c.to_string()),
                    expr: None,
                    expr_func_name: None,
                    expr_type: None,
                    opclass: None,
                    collation: None,
                    descending: false,
                    nulls_first: None,
                })
                .collect();
            let ispec = IndexSpec {
                table: tbl,
                name: Some(name.to_string()),
                unique,
                nulls_not_distinct: false,
                method: IndexMethod::Btree,
                keys,
                include: Vec::new(),
                predicate: None,
                options: Vec::new(),
                if_not_exists: false,
                concurrently: false,
            };
            let (_r, next) = testing::run(&mut kv, &env, |t| t.create_index(&ispec).map(|_| ()))?;
            env = next;
        }
        Ok((kv, env, tbl))
    }

    fn index_oid(env: &CatalogEnv, tbl: Oid, name: &str) -> Option<Oid> {
        env.snapshot().indexes_of(tbl).iter().find(|i| i.name == name).map(|i| i.oid)
    }

    fn col(attnum: i16, ty: TypeId) -> BoundExpr {
        BoundExpr::column(0, 1, attnum, ty)
    }

    fn int4(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    fn op(name: &str, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            ExprKind::Op(Box::new(OpExpr {
                op: OpRef { op: 0, proc: 0, name: name.to_string(), volatility: Volatility::Immutable, strict: true },
                args: vec![l, r],
            })),
            TypeId::BOOL,
        )
    }

    fn table_scan(tbl: Oid, quals: Vec<BoundExpr>) -> LogicalScan {
        LogicalScan {
            rt_index: 1,
            source: ScanSource::Table {
                table: TableRef {
                    oid: tbl,
                    schema: "public".to_string(),
                    name: "t".to_string(),
                    alias: "t".to_string(),
                    ncols: 4,
                    col_types: vec![TypeId::INT4, TypeId::INT4, TypeId::INT4, TypeId::TEXT],
                },
                is_virtual: false,
            },
            quals,
            with_rowid: false,
        }
    }

    fn ctx_for(env: &CatalogEnv, options: PlanOptions) -> PlanContext<'_> {
        PlanContext { env, param_types: Vec::new(), param_values: None, options, table_rows: HashMap::new() }
    }

    fn index_scan_of(node: &PlanNode) -> Result<&IndexScan> {
        match &node.kind {
            NodeKind::IndexScan(s) => Ok(s),
            _ => Err(PgError::internal("test expected an IndexScan node")),
        }
    }

    #[test]
    fn primary_key_equality_is_unique_point() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let quals = vec![op("=", col(1, TypeId::INT4), int4(5))];
        let scan = table_scan(tbl, quals.clone());
        let node = choose_scan(&mut st, &scan, None, false)?;
        let s = index_scan_of(&node)?;
        assert_eq!(s.index.name, "t_pkey");
        assert_eq!(s.bounds.eq, vec![KeyEq::Value(int4(5))]);
        assert!(s.filter.is_none());
        let pk = index_oid(&env, tbl, "t_pkey").ok_or_else(|| PgError::internal("no t_pkey"))?;
        let m = match_index(&st, tbl, 1, pk, &quals)?.ok_or_else(|| PgError::internal("no match"))?;
        assert!(m.unique_point);
        Ok(())
    }

    #[test]
    fn range_bounds_on_indexed_column() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let quals = vec![op(">", col(2, TypeId::INT4), int4(3)), op("<=", col(2, TypeId::INT4), int4(10))];
        let scan = table_scan(tbl, quals);
        let node = choose_scan(&mut st, &scan, None, false)?;
        let s = index_scan_of(&node)?;
        assert!(s.index.name == "t_a" || s.index.name == "t_a_b");
        assert_eq!(s.bounds.lower, Some(RangeEnd { value: int4(3), inclusive: false }));
        assert_eq!(s.bounds.upper, Some(RangeEnd { value: int4(10), inclusive: true }));
        assert!(s.bounds.eq.is_empty());
        assert!(s.filter.is_none());
        Ok(())
    }

    #[test]
    fn composite_prefix_equality_then_range() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let quals = vec![op("=", col(2, TypeId::INT4), int4(1)), op(">", col(3, TypeId::INT4), int4(2))];
        let scan = table_scan(tbl, quals);
        let node = choose_scan(&mut st, &scan, None, false)?;
        let s = index_scan_of(&node)?;
        assert_eq!(s.index.name, "t_a_b");
        assert_eq!(s.bounds.eq, vec![KeyEq::Value(int4(1))]);
        assert_eq!(s.bounds.lower, Some(RangeEnd { value: int4(2), inclusive: false }));
        Ok(())
    }

    #[test]
    fn non_prefix_column_uses_seq_scan() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let scan = table_scan(tbl, vec![op("=", col(3, TypeId::INT4), int4(2))]);
        let node = choose_scan(&mut st, &scan, None, false)?;
        assert!(matches!(node.kind, NodeKind::SeqScan(_)));
        Ok(())
    }

    #[test]
    fn int8_constant_on_int4_key_is_cast_to_key_type() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let int8 = BoundExpr::constant(Datum::Int8(5), TypeId::INT8);
        let scan = table_scan(tbl, vec![op("=", col(2, TypeId::INT4), int8)]);
        let node = choose_scan(&mut st, &scan, None, false)?;
        let s = index_scan_of(&node)?;
        match s.bounds.eq.first() {
            Some(KeyEq::Value(v)) => {
                assert!(matches!(v.kind, ExprKind::Cast(_)));
                assert_eq!(v.ty.oid, oids::INT4);
            }
            _ => return Err(PgError::internal("expected a value equality")),
        }
        Ok(())
    }

    #[test]
    fn order_by_desc_with_limit_scans_backward() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let mut st = PlannerState::new(&ctx);
        let wanted = vec![SortKeySpec {
            expr: col(2, TypeId::INT4),
            descending: true,
            nulls_first: true,
            collation: 0,
            using_op: None,
        }];
        let scan = table_scan(tbl, Vec::new());
        let node = choose_scan(&mut st, &scan, Some(&wanted), true)?;
        let s = index_scan_of(&node)?;
        assert_eq!(s.direction, ScanDirection::Backward);
        assert!(s.filter.is_none());
        assert_eq!(s.index.keys.first().and_then(|k| k.column), Some(2));
        Ok(())
    }

    #[test]
    fn index_scans_disabled_uses_seq_scan() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let options = PlanOptions { enable_indexscan: false, ..PlanOptions::default() };
        let ctx = ctx_for(&env, options);
        let mut st = PlannerState::new(&ctx);
        let scan = table_scan(tbl, vec![op("=", col(1, TypeId::INT4), int4(5))]);
        let node = choose_scan(&mut st, &scan, None, false)?;
        assert!(matches!(node.kind, NodeKind::SeqScan(_)));
        Ok(())
    }

    #[test]
    fn param_index_lookup_on_outer_column() -> Result<()> {
        let (_kv, env, tbl) = fixture()?;
        let ctx = ctx_for(&env, PlanOptions::default());
        let st = PlannerState::new(&ctx);
        let outer = BoundExpr::column(0, 2, 1, TypeId::INT4);
        let quals = vec![op("=", col(2, TypeId::INT4), outer.clone())];
        let m = find_param_index(&st, tbl, 1, &quals)?.ok_or_else(|| PgError::internal("no param index"))?;
        assert_eq!(m.eq_cols, 1);
        assert_eq!(m.eq, vec![KeyEq::Value(outer)]);
        assert!(m.lower.is_none() && m.upper.is_none());
        Ok(())
    }
}
