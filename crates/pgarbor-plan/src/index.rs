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
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;

use crate::context::PlannerState;
use crate::logical::LogicalScan;
use crate::plan::{PlanNode, SortKeySpec};

/// Pick the access path for a base-table scan (module docs).
#[allow(unused_variables)]
pub fn choose_scan(
    st: &mut PlannerState,
    scan: &LogicalScan,
    wanted_order: Option<&[SortKeySpec]>,
    want_ordered_scan_for_limit: bool,
) -> Result<PlanNode> {
    Err(PgError::unimplemented("PLN-05", "scan path selection (index::choose_scan)"))
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
#[allow(unused_variables)]
pub fn match_index(
    st: &PlannerState,
    table_oid: pgarbor_common::Oid,
    rt_index: u32,
    index_oid: pgarbor_common::Oid,
    quals: &[BoundExpr],
) -> Result<Option<IndexMatch>> {
    Err(PgError::unimplemented("PLN-05", "index qual matching (index::match_index)"))
}

/// Best equality-lookup index for a join inner side: given equi-join conjuncts `inner.col = outer_expr`
/// (`outer_exprs` are bind-time expressions over the outer relations, to become `OuterRef` parameters),
/// find a btree index of the inner table whose leading key columns are all covered. Returns the index
/// match with `eq` values referring to the outer expressions (the join planner rewrites them to
/// `OuterRef`s).
#[allow(unused_variables)]
pub fn find_param_index(
    st: &PlannerState,
    table_oid: pgarbor_common::Oid,
    rt_index: u32,
    equi_quals: &[BoundExpr],
) -> Result<Option<IndexMatch>> {
    Err(PgError::unimplemented("PLN-05", "parameterized index lookup (index::find_param_index)"))
}
