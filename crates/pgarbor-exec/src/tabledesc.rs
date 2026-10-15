//! [`TableDesc`]: everything the executor needs to know about ONE table, derived from the catalog
//! snapshot once per statement and cached in `StmtState::tables` (lease EXE-01).
//!
//! The catalog model (`pgarbor_catalog::Relation`, `Index`, `Constraint`) stores expressions as TEXT;
//! DML needs bound, executable expressions (partial-index predicates, expression-index keys, CHECK
//! bodies) and decoded defaults, so this module binds them once and keeps the result.
//!
//! ## Building (`table_desc`)
//! `table_desc(cx, oid)`: cache hit -> clone the `Rc`. Otherwise read `cx.se.env.snapshot()`:
//! * `relation(oid)` -> `ColDesc` per attnum slot (dropped columns included, index = attnum - 1).
//!   `missing` = the column's `missing_value` text parsed with `io::input(column type)` (ADD COLUMN
//!   fast default: stored rows written before the column existed have fewer values);
//! * `indexes_of(oid)` -> every `valid` btree `Index` as [`IndexDesc`], PRIMARY KEY first, then unique,
//!   then others, each group by OID. Expression keys / predicates are bound with [`bind_stored_text`]
//!   (+ [`columns_to_input_refs`]) so they evaluate over a full table row; `label` of an expression
//!   key is its stored SQL text (what PG prints in `Key (lower(name))=(x)`), of a column key the
//!   column name;
//! * `constraints_of(oid)`: `Check` (enforced, `validated` flag kept) -> [`CheckDesc`] bound the same
//!   way; `ForeignKey` where `table == oid` -> `fks_out`; `foreign_keys_referencing(oid)` -> `fks_in`
//!   (names resolved through the snapshot; NOT ENFORCED constraints omitted);
//! * non-btree indexes and invalid indexes (`valid == false`) are skipped for maintenance.
//! Errors: unknown OID or a non-table relation kind -> XX000 `cache lookup failed for relation N`
//! (`error::relation_vanished`).
use std::rc::Rc;

use pgarbor_catalog::model::{FkAction, FkMatch, GeneratedKind, IdentityKind, RelKind};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::keycodec::KeyColumn;
use pgarbor_types::{Datum, TypeId};

use crate::context::{ExecContext, SessionEnv};

/// One attribute slot of a table.
// CONTRACT
#[derive(Clone, Debug)]
pub struct ColDesc {
    pub attnum: i16,
    pub name: String,
    pub ty: TypeId,
    pub not_null: bool,
    pub dropped: bool,
    pub generated: Option<GeneratedKind>,
    pub identity: Option<IdentityKind>,
    /// Value to use when a stored row has no slot for this column (module docs); `None` = NULL.
    pub missing: Option<Datum>,
}

/// One key column of an index.
// CONTRACT
#[derive(Clone, Debug)]
pub struct IdxKeyDesc {
    /// Table attnum of a plain column key.
    pub column: Option<i16>,
    /// Bound expression key over the full table row (`InputRef(attnum-1)`), for expression indexes.
    pub expr: Option<BoundExpr>,
    pub ty: TypeId,
    pub descending: bool,
    pub nulls_first: bool,
    /// Text for `Key (...)` details: column name or stored expression text.
    pub label: String,
}

/// A btree index as seen by DML.
// CONTRACT
#[derive(Clone, Debug)]
pub struct IndexDesc {
    pub oid: Oid,
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    pub nulls_not_distinct: bool,
    /// Key columns only (INCLUDE columns are not part of the key and not stored).
    pub keys: Vec<IdxKeyDesc>,
    /// `keycodec` description parallel to `keys` (`ty`, `desc`, `nulls_first`).
    pub key_cols: Vec<KeyColumn>,
    /// Partial index predicate over the full table row; rows for which it is not TRUE have no entry.
    pub predicate: Option<BoundExpr>,
    /// The constraint this index backs (PK / UNIQUE): `(oid, name)`. Errors report the CONSTRAINT
    /// name when present, else the index name.
    pub constraint: Option<(Oid, String)>,
}

impl IndexDesc {
    /// Name PG reports in unique violations: the constraint name if the index backs one, else the
    /// index name.
    pub fn violation_name(&self) -> &str {
        self.constraint.as_ref().map(|c| c.1.as_str()).unwrap_or(&self.name)
    }
}

/// A CHECK constraint.
// CONTRACT
#[derive(Clone, Debug)]
pub struct CheckDesc {
    pub oid: Oid,
    pub name: String,
    /// Boolean expression over the full table row (`InputRef(attnum-1)`). A row passes when the
    /// result is TRUE or NULL (`logic::check_passes`).
    pub expr: BoundExpr,
    /// `NOT VALID` constraints are still enforced for NEW rows but not validated against old ones.
    pub validated: bool,
}

/// A foreign-key constraint (either side).
// CONTRACT
#[derive(Clone, Debug)]
pub struct FkDesc {
    pub oid: Oid,
    pub name: String,
    pub fk_table: Oid,
    pub fk_schema: String,
    pub fk_table_name: String,
    /// Referencing columns (attnums of the FK table).
    pub cols: Vec<i16>,
    pub ref_table: Oid,
    pub ref_schema: String,
    pub ref_table_name: String,
    /// Referenced columns (attnums of the referenced table), same length as `cols`.
    pub ref_cols: Vec<i16>,
    /// The unique index of the referenced table that backs the key.
    pub ref_index: Oid,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    pub match_type: FkMatch,
    pub deferrable: bool,
    pub initially_deferred: bool,
    pub validated: bool,
    /// `ON DELETE SET NULL/DEFAULT (cols)`: subset of `cols` to reset (empty = all).
    pub delete_set_columns: Vec<i16>,
}

/// Everything DML needs to know about a table (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub struct TableDesc {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    /// By attnum - 1, dropped slots included.
    pub cols: Vec<ColDesc>,
    pub indexes: Vec<IndexDesc>,
    pub checks: Vec<CheckDesc>,
    /// FKs where this table is the referencing side.
    pub fks_out: Vec<FkDesc>,
    /// FKs where this table is the referenced side.
    pub fks_in: Vec<FkDesc>,
}

impl TableDesc {
    /// Number of attribute slots (dropped included).
    pub fn ncols(&self) -> usize {
        self.cols.len()
    }
    pub fn col(&self, attnum: i16) -> Option<&ColDesc> {
        if attnum < 1 {
            return None;
        }
        self.cols.get(attnum as usize - 1)
    }
    pub fn col_by_name(&self, name: &str) -> Option<&ColDesc> {
        self.cols.iter().find(|c| !c.dropped && c.name == name)
    }
    pub fn live_cols(&self) -> impl Iterator<Item = &ColDesc> {
        self.cols.iter().filter(|c| !c.dropped)
    }
    /// Column types by slot (dropped = `TypeId::of(0)`), what `TableRef::col_types` carries.
    pub fn col_types(&self) -> Vec<TypeId> {
        self.cols.iter().map(|c| if c.dropped { TypeId::of(0) } else { c.ty }).collect()
    }
    pub fn index(&self, oid: Oid) -> Option<&IndexDesc> {
        self.indexes.iter().find(|i| i.oid == oid)
    }
    pub fn unique_indexes(&self) -> impl Iterator<Item = &IndexDesc> {
        self.indexes.iter().filter(|i| i.unique)
    }
    pub fn primary_index(&self) -> Option<&IndexDesc> {
        self.indexes.iter().find(|i| i.primary)
    }
    /// `DescCol`s for `error::row_description`.
    pub fn desc_cols(&self) -> Vec<crate::error::DescCol> {
        self.cols
            .iter()
            .map(|c| crate::error::DescCol {
                ty: c.ty,
                dropped: c.dropped,
                virtual_generated: matches!(c.generated, Some(GeneratedKind::Virtual)),
            })
            .collect()
    }

    /// Complete a decoded stored row to exactly `ncols()` values: truncate surplus values, then fill
    /// missing trailing slots with the column's `missing` value (NULL if none); dropped slots are
    /// always NULL.
    pub fn complete_row(&self, mut row: Vec<Datum>) -> Vec<Datum> {
        row.truncate(self.cols.len());
        for i in row.len()..self.cols.len() {
            let c = &self.cols[i];
            row.push(if c.dropped { Datum::Null } else { c.missing.clone().unwrap_or(Datum::Null) });
        }
        for (i, c) in self.cols.iter().enumerate() {
            if c.dropped {
                row[i] = Datum::Null;
            }
        }
        row
    }
}

/// Cached description of table `oid` (module docs, "Building").
#[allow(unused_variables)]
pub fn table_desc(cx: &ExecContext, oid: Oid) -> Result<Rc<TableDesc>> {
    Err(PgError::unimplemented("EXE-01", "table description (tabledesc::table_desc)"))
}

/// Same as [`table_desc`] but from a snapshot taken from `se.env` without a cache (used while the
/// `ExecContext` is mutably borrowed elsewhere, and by DDL follow-ups right after a catalog change).
#[allow(unused_variables)]
pub fn build_table_desc(se: &SessionEnv, oid: Oid) -> Result<TableDesc> {
    Err(PgError::unimplemented("EXE-01", "table description build (tabledesc::build_table_desc)"))
}

/// Parse and bind catalog expression TEXT (`StoredExpr::sql`) in the scope of relation `rel_oid`:
/// creates a `BindContext` from `se` (env, `BindSession{database: se.services.database_name()}`,
/// `se.io`, `se.statement_ts`, no declared params) and a throwaway `Binder`
/// (`pgarbor_sql::binder::stored::bind_stored_expr`, `ExprCtx::check()`/`column_default()` as
/// appropriate). Columns bind to `Column{levels_up:0, rt_index:1, attnum}`; apply
/// [`columns_to_input_refs`] to make the result executable over a table row. Bind failure of stored
/// text is an internal error XX000 `could not bind stored expression`.
#[allow(unused_variables)]
pub fn bind_stored_text(se: &SessionEnv, sql: &str, rel_oid: Oid) -> Result<BoundExpr> {
    Err(PgError::unimplemented("EXE-01", "stored expression binding (tabledesc::bind_stored_text)"))
}

/// Rewrite bind-time `Column{levels_up: 0, rt_index: 1, attnum}` into `InputRef(attnum - 1)` (uses
/// `BoundExpr::transform` from `pgarbor_sql::ir::expr_walk`). Other column references (outer
/// levels, rt_index != 1, whole-row, system columns) are an error XX000 (they cannot occur in
/// stored table expressions). Sub-links are not allowed in stored expressions either.
#[allow(unused_variables)]
pub fn columns_to_input_refs(expr: BoundExpr) -> Result<BoundExpr> {
    Err(PgError::unimplemented("EXE-01", "column to input-ref rewrite (tabledesc::columns_to_input_refs)"))
}
