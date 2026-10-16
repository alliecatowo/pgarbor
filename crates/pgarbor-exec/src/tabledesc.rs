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

use pgarbor_catalog::model::{
    Column, Constraint, ConstraintKind, FkAction, FkMatch, GeneratedKind, IdentityKind, Index, IndexKey, IndexMethod,
    RelKind,
};
use pgarbor_catalog::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::binder::coerce::coerce_assignment;
use pgarbor_sql::binder::context::{BindContext, BindSession, Binder, BoundArg, ExprCtx};
use pgarbor_sql::binder::stored::bind_stored_expr;
use pgarbor_sql::ir::{BoundExpr, ExprKind};
use pgarbor_sql::source::SourceMap;
use pgarbor_types::keycodec::KeyColumn;
use pgarbor_types::{Datum, TypeId};

use crate::context::{ExecContext, SessionEnv};
use crate::error::relation_vanished;

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
    /// INCLUDE columns (plain table columns). Not stored in the entry, but PG counts their values
    /// toward the index tuple size limit (`index_key::check_tuple_size`).
    pub include: Vec<IdxKeyDesc>,
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
    /// NOT NULL VIRTUAL generated columns as (slot, generation expression over the full row): stored
    /// as NULL, so NOT NULL is enforced against the computed value (PG `ExecConstraints`).
    pub virtual_not_null: Vec<(usize, BoundExpr)>,
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
pub fn table_desc(cx: &ExecContext, oid: Oid) -> Result<Rc<TableDesc>> {
    let cached = cx.stmt.tables.borrow().get(&oid).cloned();
    if let Some(hit) = cached {
        return Ok(hit);
    }
    let desc = Rc::new(build_table_desc(&cx.se, oid)?);
    cx.stmt.tables.borrow_mut().insert(oid, Rc::clone(&desc));
    Ok(desc)
}

/// Same as [`table_desc`] but from a snapshot taken from `se.env` without a cache (used while the
/// `ExecContext` is mutably borrowed elsewhere, and by DDL follow-ups right after a catalog change).
pub fn build_table_desc(se: &SessionEnv, oid: Oid) -> Result<TableDesc> {
    let snap = se.env.snapshot();
    let rel = snap.relation(oid).ok_or_else(|| relation_vanished(oid))?;
    if rel.row_security || rel.force_row_security {
        return Err(PgError::unimplemented("EXE-01", "row security policy enforcement"));
    }
    if snap.constraints_of(oid).iter().any(|c| c.deferrable || c.initially_deferred) {
        return Err(PgError::unimplemented("EXE-01", "deferred constraint enforcement"));
    }
    // Row storage is what DML and table-shaped DDL need; views, indexes, sequences, ... have none.
    if !matches!(rel.kind, RelKind::Table | RelKind::MatView) {
        return Err(relation_vanished(oid));
    }
    let schema = namespace_name(snap, rel.namespace)?;
    let cols = rel.columns.iter().map(|c| col_desc(se, c)).collect::<Result<Vec<ColDesc>>>()?;
    let indexes = index_descs(se, snap, oid)?;
    let checks = check_descs(se, snap, oid)?;
    let mut virtual_not_null: Vec<(usize, BoundExpr)> = Vec::new();
    for (slot, c) in rel.columns.iter().enumerate() {
        if c.dropped || !c.not_null || c.generated != Some(GeneratedKind::Virtual) {
            continue;
        }
        if let Some(d) = c.default.as_ref() {
            let g = bind_column_default_text(se, &d.expr.sql, oid, &c.name, c.type_id)?;
            virtual_not_null.push((slot, columns_to_input_refs(expand_virtual_columns(se, snap, oid, g)?)?));
        }
    }
    let fks_out = snap
        .constraints_of(oid)
        .into_iter()
        .filter(|c| c.kind == ConstraintKind::ForeignKey && c.enforced)
        .map(|c| fk_desc(snap, c))
        .collect::<Result<Vec<FkDesc>>>()?;
    let fks_in = snap
        .foreign_keys_referencing(oid)
        .into_iter()
        .filter(|c| c.kind == ConstraintKind::ForeignKey && c.enforced)
        .map(|c| fk_desc(snap, c))
        .collect::<Result<Vec<FkDesc>>>()?;
    Ok(TableDesc {
        oid,
        schema,
        name: rel.name.clone(),
        kind: rel.kind,
        cols,
        indexes,
        checks,
        virtual_not_null,
        fks_out,
        fks_in,
    })
}

/// Schema name of a namespace OID (a missing namespace is catalog corruption: XX000).
fn namespace_name(snap: &CatalogSnapshot, oid: Oid) -> Result<String> {
    snap.namespace(oid)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for namespace {oid}")))
}

/// (schema, relation name) of a relation OID.
fn qualified_rel_name(snap: &CatalogSnapshot, oid: Oid) -> Result<(String, String)> {
    let rel = snap.relation(oid).ok_or_else(|| relation_vanished(oid))?;
    Ok((namespace_name(snap, rel.namespace)?, rel.name.clone()))
}

/// One attribute slot. `missing` is parsed from the stored text with the column type's input function.
fn col_desc(se: &SessionEnv, c: &Column) -> Result<ColDesc> {
    let missing = match (&c.missing_value, c.dropped) {
        (Some(text), false) => Some(pgarbor_types::io::input(&se.io_ctx(), c.type_id, text)?),
        _ => None,
    };
    Ok(ColDesc {
        attnum: c.attnum,
        name: c.name.clone(),
        ty: c.type_id,
        not_null: c.not_null,
        dropped: c.dropped,
        generated: c.generated,
        identity: c.identity.map(|i| i.kind),
        missing,
    })
}

/// Valid btree indexes of `table`: primary key first, then other unique, then the rest; ties by OID
/// (`constraints_of`/`indexes_of` are OID-sorted, and the sort below is stable).
fn index_descs(se: &SessionEnv, snap: &CatalogSnapshot, table: Oid) -> Result<Vec<IndexDesc>> {
    let mut indexes: Vec<&Index> = snap
        .indexes_of(table)
        .into_iter()
        .filter(|i| i.method == IndexMethod::Btree && i.valid)
        .collect();
    indexes.sort_by_key(|i| (index_group(i), i.oid));
    indexes.into_iter().map(|i| index_desc(se, snap, table, i)).collect()
}

fn index_group(i: &Index) -> u8 {
    if i.primary {
        0
    } else if i.unique {
        1
    } else {
        2
    }
}

fn index_desc(se: &SessionEnv, snap: &CatalogSnapshot, table: Oid, i: &Index) -> Result<IndexDesc> {
    let mut keys = Vec::with_capacity(i.key_columns().len());
    let mut key_cols = Vec::with_capacity(i.key_columns().len());
    for k in i.key_columns() {
        keys.push(index_key_desc(se, table, k)?);
        key_cols.push(KeyColumn { ty: k.type_id, desc: k.descending, nulls_first: k.nulls_first });
    }
    let mut include = Vec::with_capacity(i.include_columns().len());
    for k in i.include_columns() {
        include.push(index_key_desc(se, table, k)?);
    }
    let predicate = i.predicate.as_ref().map(|p| bound_row_expr(se, &p.sql, table)).transpose()?;
    let constraint = i.constraint.and_then(|co| snap.constraint(co).map(|c| (co, c.name.clone())));
    Ok(IndexDesc {
        oid: i.oid,
        name: i.name.clone(),
        unique: i.unique,
        primary: i.primary,
        nulls_not_distinct: i.nulls_not_distinct,
        keys,
        include,
        key_cols,
        predicate,
        constraint,
    })
}

fn index_key_desc(se: &SessionEnv, table: Oid, k: &IndexKey) -> Result<IdxKeyDesc> {
    let (expr, label) = match &k.expr {
        Some(stored) => (Some(bound_row_expr(se, &stored.sql, table)?), stored.sql.clone()),
        None => (None, k.name.clone()),
    };
    Ok(IdxKeyDesc { column: k.column, expr, ty: k.type_id, descending: k.descending, nulls_first: k.nulls_first, label })
}

/// Enforced CHECK constraints of `table`, bound over the full row (`NOT NULL` is a column flag).
fn check_descs(se: &SessionEnv, snap: &CatalogSnapshot, table: Oid) -> Result<Vec<CheckDesc>> {
    let mut out = Vec::new();
    for c in snap.constraints_of(table) {
        if c.kind != ConstraintKind::Check || !c.enforced {
            continue;
        }
        let stored = c
            .check
            .as_ref()
            .ok_or_else(|| PgError::internal(format!("cache lookup failed for constraint {}", c.oid)))?;
        // VIRTUAL generated columns are NULL in storage: a CHECK reads their generation expression
        // (PG `expand_generated_columns_in_expr` in `ExecRelCheck`).
        let bound = expand_virtual_columns(se, snap, table, bind_stored_text(se, &stored.sql, table)?)?;
        out.push(CheckDesc {
            oid: c.oid,
            name: c.name.clone(),
            expr: columns_to_input_refs(bound)?,
            validated: c.validated,
        });
    }
    Ok(out)
}

/// Replace references to VIRTUAL generated columns of `table` (bind-time `Column{rt_index: 1}`) with
/// their bound generation expressions, which themselves reference only non-virtual columns.
fn expand_virtual_columns(se: &SessionEnv, snap: &CatalogSnapshot, table: Oid, expr: BoundExpr) -> Result<BoundExpr> {
    let rel = match snap.relation(table) {
        Some(r) => r,
        None => return Ok(expr),
    };
    let mut gens: Vec<(i16, BoundExpr)> = Vec::new();
    for c in rel.columns.iter() {
        if c.dropped || c.generated != Some(GeneratedKind::Virtual) {
            continue;
        }
        if let Some(d) = c.default.as_ref() {
            gens.push((c.attnum, bind_column_default_text(se, &d.expr.sql, table, &c.name, c.type_id)?));
        }
    }
    if gens.is_empty() {
        return Ok(expr);
    }
    expr.transform(&mut |node: BoundExpr| -> Result<BoundExpr> {
        if let ExprKind::Column(c) = &node.kind {
            if c.levels_up == 0 && c.rt_index == 1 {
                if let Some((_, g)) = gens.iter().find(|(a, _)| *a == c.attnum) {
                    return Ok(g.clone());
                }
            }
        }
        Ok(node)
    })
}

fn fk_desc(snap: &CatalogSnapshot, c: &Constraint) -> Result<FkDesc> {
    let fk = c
        .fk
        .as_ref()
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for constraint {}", c.oid)))?;
    let (fk_schema, fk_table_name) = qualified_rel_name(snap, c.table)?;
    let (ref_schema, ref_table_name) = qualified_rel_name(snap, fk.ref_table)?;
    Ok(FkDesc {
        oid: c.oid,
        name: c.name.clone(),
        fk_table: c.table,
        fk_schema,
        fk_table_name,
        cols: c.columns.clone(),
        ref_table: fk.ref_table,
        ref_schema,
        ref_table_name,
        ref_cols: fk.ref_columns.clone(),
        ref_index: fk.ref_index,
        on_update: fk.on_update,
        on_delete: fk.on_delete,
        match_type: fk.match_type,
        deferrable: c.deferrable,
        initially_deferred: c.initially_deferred,
        validated: c.validated,
        delete_set_columns: fk.delete_set_columns.clone(),
    })
}

/// Bind stored text and rewrite its column references to table-row inputs.
fn bound_row_expr(se: &SessionEnv, sql: &str, rel_oid: Oid) -> Result<BoundExpr> {
    columns_to_input_refs(bind_stored_text(se, sql, rel_oid)?)
}

/// Parse and bind catalog expression TEXT (`StoredExpr::sql`) in the scope of relation `rel_oid`:
/// creates a `BindContext` from `se` (env, `BindSession{database: se.services.database_name()}`,
/// `se.io`, `se.statement_ts`, no declared params) and a throwaway `Binder`
/// (`pgarbor_sql::binder::stored::bind_stored_expr`, `ExprCtx::check()`/`column_default()` as
/// appropriate). Columns bind to `Column{levels_up:0, rt_index:1, attnum}`; apply
/// [`columns_to_input_refs`] to make the result executable over a table row. Bind failure of stored
/// text is an internal error XX000 `could not bind stored expression`.
pub fn bind_stored_text(se: &SessionEnv, sql: &str, rel_oid: Oid) -> Result<BoundExpr> {
    bind_stored_text_as(se, sql, rel_oid, None)
}

/// Like [`bind_stored_text`] for a column DEFAULT: the bound text is assignment-coerced to the
/// column type (PG `build_column_default` / `cookDefault` store the cast; the deparsed text drops
/// the implicit cast, so `nextval()` (bigint) for an int4 column must be recast here or the value
/// has the wrong datum variant).
pub fn bind_column_default_text(
    se: &SessionEnv,
    sql: &str,
    rel_oid: Oid,
    col_name: &str,
    col_type: TypeId,
) -> Result<BoundExpr> {
    bind_stored_text_as(se, sql, rel_oid, Some((col_name, col_type)))
}

fn bind_stored_text_as(
    se: &SessionEnv,
    sql: &str,
    rel_oid: Oid,
    target: Option<(&str, TypeId)>,
) -> Result<BoundExpr> {
    let ctx = BindContext {
        env: &se.env,
        session: BindSession { database: se.services.database_name() },
        io_config: se.io.clone(),
        statement_timestamp: se.txn_ts,
        declared_params: Vec::new(),
    };
    let src = SourceMap::new(sql);
    let mut binder = Binder::new(&ctx, &src);
    let bound = match bind_stored_expr(&mut binder, sql, rel_oid, ExprCtx::check()) {
        Ok(expr) => expr,
        // Unimplemented features of a stored expression keep their 0A000 so they are not masked.
        Err(e) if e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED => return Err(e),
        Err(_) => return Err(PgError::internal("could not bind stored expression")),
    };
    match target {
        None => Ok(bound),
        Some((name, ty)) => coerce_assignment(&mut binder, &BoundArg::new(bound, None), name, ty),
    }
}

/// Rewrite bind-time `Column{levels_up: 0, rt_index: 1, attnum}` into `InputRef(attnum - 1)` (uses
/// `BoundExpr::transform` from `pgarbor_sql::ir::expr_walk`). Other column references (outer
/// levels, rt_index != 1, whole-row, system columns) are an error XX000 (they cannot occur in
/// stored table expressions). Sub-links are not allowed in stored expressions either.
pub fn columns_to_input_refs(expr: BoundExpr) -> Result<BoundExpr> {
    expr.transform(&mut |node: BoundExpr| -> Result<BoundExpr> {
        let BoundExpr { kind, ty } = node;
        match kind {
            ExprKind::Column(c) => {
                if c.levels_up == 0 && c.rt_index == 1 && c.attnum >= 1 {
                    Ok(BoundExpr::input_ref((c.attnum - 1) as u32, ty))
                } else {
                    Err(PgError::internal("could not bind stored expression: unexpected column reference"))
                }
            }
            ExprKind::OuterRef(_) | ExprKind::SubLink(_) | ExprKind::SubPlan(_) => {
                Err(PgError::internal("could not bind stored expression: unexpected sub-query or outer reference"))
            }
            other => Ok(BoundExpr { kind: other, ty }),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use pgarbor_catalog::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use pgarbor_catalog::model::{DropBehavior, Persistence, StoredExpr};
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_common::MemKv;
    use pgarbor_types::context::IoConfig;

    use super::*;
    use crate::context::{NoGuc, NoServices};
    use crate::memtxn::MemTxn;

    /// CREATE TABLE `name` in the creation schema of `env`; returns its OID and the next env.
    fn make_table(
        kv: &mut MemKv,
        env: &CatalogEnv,
        name: &str,
        columns: Vec<ColumnSpec>,
        constraints: Vec<ConstraintSpec>,
    ) -> Result<(Oid, CatalogEnv)> {
        let spec = CreateTableSpec {
            namespace: env.creation_namespace()?,
            name: name.to_string(),
            owner: env.current_user(),
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns,
            constraints,
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let mut oid = 0;
        let (_, next) = testing::run(kv, env, |t| {
            oid = t.create_table(&spec)?.map(|c| c.table).unwrap_or(0);
            Ok(())
        })?;
        Ok((oid, next))
    }

    fn session<'a>(env: CatalogEnv, guc: &'a NoGuc, services: &'a NoServices, rng: &'a Cell<u64>) -> SessionEnv<'a> {
        SessionEnv::new(env, IoConfig::default(), guc, services, rng, 0, 0)
    }

    fn pk(columns: &[&str]) -> ConstraintSpec {
        ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            include: Vec::new(),
        })
    }

    #[test]
    fn describes_keys_and_foreign_keys() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (parent, env) = make_table(&mut kv, &env, "p", vec![ColumnSpec::new("id", TypeId::INT4)], vec![pk(&["id"])])?;
        let fk = ConstraintSpec::new(ConstraintSpecKind::ForeignKey {
            columns: vec!["pid".into()],
            ref_table: parent,
            ref_columns: Vec::new(),
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            match_type: FkMatch::Simple,
            delete_set_columns: Vec::new(),
        });
        let (child, env) = make_table(
            &mut kv,
            &env,
            "c",
            vec![
                ColumnSpec::new("a", TypeId::INT4),
                ColumnSpec::new("b", TypeId::TEXT),
                ColumnSpec::new("pid", TypeId::INT4),
            ],
            vec![pk(&["a"]), fk],
        )?;
        let (guc, services, rng) = (NoGuc, NoServices, Cell::new(1u64));
        let se = session(env, &guc, &services, &rng);

        let td = build_table_desc(&se, child)?;
        assert_eq!(td.schema, "public");
        assert_eq!(td.name, "c");
        assert_eq!(td.ncols(), 3);
        assert_eq!(td.indexes.len(), 1);
        let pk_idx = td.primary_index().expect("primary key index");
        assert!(pk_idx.primary && pk_idx.unique);
        assert_eq!(pk_idx.keys[0].column, Some(1));
        assert_eq!(pk_idx.key_cols.len(), 1);
        assert!(pk_idx.constraint.is_some());
        assert_eq!(td.fks_out.len(), 1);
        let fk_out = &td.fks_out[0];
        assert_eq!((fk_out.fk_table_name.as_str(), fk_out.ref_table_name.as_str()), ("c", "p"));
        assert_eq!(fk_out.cols, vec![3]);
        assert_eq!(fk_out.ref_cols, vec![1]);
        assert_eq!(fk_out.ref_table, parent);
        assert!(td.fks_in.is_empty());

        let pd = build_table_desc(&se, parent)?;
        assert_eq!(pd.fks_in.len(), 1);
        assert_eq!(pd.fks_in[0].name, fk_out.name);
        assert_eq!(pd.fks_in[0].fk_table_name, "c");
        assert!(pd.fks_out.is_empty());
        assert!(pd.checks.is_empty());
        Ok(())
    }

    #[test]
    fn primary_index_is_listed_before_other_unique_indexes() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let unique = ConstraintSpec::new(ConstraintSpecKind::Unique {
            columns: vec!["b".into()],
            include: Vec::new(),
            nulls_not_distinct: false,
        });
        let (t, env) = make_table(
            &mut kv,
            &env,
            "t",
            vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::TEXT)],
            vec![unique, pk(&["a"])],
        )?;
        let (guc, services, rng) = (NoGuc, NoServices, Cell::new(1u64));
        let se = session(env, &guc, &services, &rng);
        let td = build_table_desc(&se, t)?;
        assert_eq!(td.indexes.len(), 2);
        assert!(td.indexes[0].primary);
        assert!(td.indexes[1].unique && !td.indexes[1].primary);
        assert_eq!(td.indexes[1].keys[0].column, Some(2));
        Ok(())
    }

    #[test]
    fn missing_values_and_dropped_slots() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (t, env) = make_table(
            &mut kv,
            &env,
            "t",
            vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("gone", TypeId::TEXT)],
            Vec::new(),
        )?;
        let mut added = ColumnSpec::new("b", TypeId::INT4);
        added.default = Some(StoredExpr::new("5"));
        added.fast_default = Some("5".into());
        let (_, env) = testing::run(&mut kv, &env, |x| x.add_column(t, &added, false).map(|_| ()))?;
        let (_, env) = testing::run(&mut kv, &env, |x| x.drop_column(t, "gone", DropBehavior::Restrict, false).map(|_| ()))?;
        let (guc, services, rng) = (NoGuc, NoServices, Cell::new(1u64));
        let se = session(env, &guc, &services, &rng);

        let td = build_table_desc(&se, t)?;
        assert_eq!(td.ncols(), 3);
        assert!(td.cols[1].dropped);
        assert!(td.cols[0].missing.is_none());
        assert_eq!(td.cols[2].missing, Some(Datum::Int4(5)));
        // A stored row written before ADD COLUMN has one value: the dropped slot is NULL, `b` = 5.
        assert_eq!(td.complete_row(vec![Datum::Int4(1)]), vec![Datum::Int4(1), Datum::Null, Datum::Int4(5)]);
        assert_eq!(td.complete_row(vec![Datum::Int4(1), Datum::Text("x".into()), Datum::Int4(9), Datum::Null]).len(), 3);
        Ok(())
    }

    #[test]
    fn table_desc_is_cached_per_statement() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (t, env) = make_table(&mut kv, &env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], Vec::new())?;
        let (guc, services, rng) = (NoGuc, NoServices, Cell::new(1u64));
        let se = session(env, &guc, &services, &rng);
        let mut txn = MemTxn::new();
        let cx = ExecContext::new(&mut txn, se);
        let first = table_desc(&cx, t)?;
        let second = table_desc(&cx, t)?;
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(first.name, "t");
        assert!(table_desc(&cx, t + 100_000).is_err());
        Ok(())
    }

    #[test]
    fn columns_become_input_refs() -> Result<()> {
        let e = columns_to_input_refs(BoundExpr::column(0, 1, 2, TypeId::INT4))?;
        assert!(matches!(e.kind, ExprKind::InputRef(1)));
        assert!(columns_to_input_refs(BoundExpr::column(1, 1, 1, TypeId::INT4)).is_err());
        assert!(columns_to_input_refs(BoundExpr::column(0, 2, 1, TypeId::INT4)).is_err());
        assert!(columns_to_input_refs(BoundExpr::column(0, 1, 0, TypeId::INT4)).is_err());
        Ok(())
    }

    // Needs SQL-29 (stored expression binding). Evaluating the CHECK over a row is left to EXE-31 tests.
    #[test]
    #[ignore = "needs SQL-29"]
    fn check_constraint_binds_over_row() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let check = ConstraintSpec::new(ConstraintSpecKind::Check {
            expr: StoredExpr::new("(a > 0)"),
            columns: vec!["a".into()],
            no_inherit: false,
        });
        let (t, env) = make_table(
            &mut kv,
            &env,
            "t",
            vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::TEXT)],
            vec![pk(&["a"]), check],
        )?;
        let (guc, services, rng) = (NoGuc, NoServices, Cell::new(1u64));
        let se = session(env, &guc, &services, &rng);
        let td = build_table_desc(&se, t)?;
        assert_eq!(td.ncols(), 2);
        assert_eq!(td.primary_index().and_then(|i| i.keys[0].column), Some(1));
        assert_eq!(td.checks.len(), 1);
        assert!(td.checks[0].validated);
        assert!(td.checks[0].expr.any(&mut |e: &BoundExpr| matches!(e.kind, ExprKind::InputRef(0))));
        assert!(!td.checks[0].expr.any(&mut |e: &BoundExpr| matches!(e.kind, ExprKind::Column(_))));
        Ok(())
    }
}
