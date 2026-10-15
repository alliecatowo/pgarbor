//! information_schema, part 3 (constraints): `table_constraints`, `key_column_usage`,
//! `referential_constraints`, `constraint_column_usage`, `constraint_table_usage`,
//! `check_constraints`. Lease CAT-42. Authoritative semantics: the CREATE VIEW of each name in PG
//! 18.6 `information_schema.sql` (e.g. `check_constraints` at line ~431); visibility / conventions:
//! `is_core.rs`. Oracle facts (PG 18.6) for tables `p(id pk, b unique, x check)` and
//! `c(a not null references p(id) on delete cascade, n, constraint cc check (n<5), unique(a,n))`:
//! - **table_constraints** (`constraint_catalog/schema/name, table_catalog/schema/name,
//!   constraint_type, is_deferrable, initially_deferred, enforced, nulls_distinct`): PRIMARY KEY, UNIQUE,
//!   FOREIGN KEY, CHECK, and NOT NULL constraints reported as `CHECK` (`ConstraintKind::info_schema_type`):
//!   rows `c_a_fkey FOREIGN KEY`, `c_a_n_key UNIQUE`, `c_a_not_null CHECK`, `cc CHECK`, `p_b_key UNIQUE`,
//!   `p_id_not_null CHECK`, `p_pkey PRIMARY KEY`, `p_x_check CHECK`; `enforced` `YES`/`NO`;
//!   `nulls_distinct` `YES`/`NO` for UNIQUE (NULL otherwise).
//! - **key_column_usage**: PK/UNIQUE/FK constraints only, one row per key column in key order:
//!   `constraint_*, table_*, column_name, ordinal_position` (1-based), `position_in_unique_constraint`
//!   (FK only: position of the referenced column in the referenced key, 1-based; NULL otherwise).
//!   Oracle: `c_a_fkey | c | a | 1 | 1`, `c_a_n_key | c | a | 1 | NULL`, `c_a_n_key | c | n | 2 | NULL`.
//! - **referential_constraints**: FK constraints: `constraint_*`, `unique_constraint_catalog/schema/
//!   name` = the referenced PK/UNIQUE constraint (the constraint owning `fk.ref_index`; NULL if that
//!   index has no constraint), `match_option` (`NONE`/`FULL`/`PARTIAL`: `FkMatch::info_schema`),
//!   `update_rule`, `delete_rule` (`FkAction::sql`). Oracle: `c_a_fkey | p_pkey | NONE | NO ACTION | CASCADE`.
//! - **constraint_column_usage**: columns used by constraints, from the viewpoint of the table that
//!   OWNS the column: for CHECK/NOT NULL/PK/UNIQUE the constrained columns; for FOREIGN KEY the
//!   REFERENCED columns (oracle: `p | id | c_a_fkey`). Columns: `table_catalog/schema/name,
//!   column_name, constraint_catalog/schema/name`. Domain constraints are not listed.
//! - **constraint_table_usage**: tables that carry a constraint, from the table's viewpoint, for
//!   PK/UNIQUE/FK only (FK -> the REFERENCED table; oracle: `p | c_a_fkey`, no CHECK rows): `table_*,
//!   constraint_*`.
//! - **check_constraints**: CHECK constraints and NOT NULL constraints and DOMAIN checks:
//!   `constraint_catalog/schema/name, check_clause` = `check.sql` text (`(n < 5)`, `(VALUE > 0)`) or
//!   `<column> IS NOT NULL` / `VALUE IS NOT NULL` for NOT NULL constraints (oracle: `c_a_not_null |
//!   a IS NOT NULL`). All visible by `pg_has_role(owner of table/domain, 'USAGE')`.
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin::{self, relid, SystemRel};
use crate::env::CatalogEnv;
use crate::model::{acl_mode, Column, Constraint, ConstraintKind, ForeignKey, RelKind, Relation, StoredExpr};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use std::borrow::Cow;

/// `has_table_privilege(.., 'INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')` (ANY bit).
const TABLE_DML: u32 = acl_mode::INSERT
    | acl_mode::UPDATE
    | acl_mode::DELETE
    | acl_mode::TRUNCATE
    | acl_mode::REFERENCES
    | acl_mode::TRIGGER;
/// `has_any_column_privilege(.., 'INSERT, UPDATE, REFERENCES')`.
const COLUMN_WRITE: u32 = acl_mode::INSERT | acl_mode::UPDATE | acl_mode::REFERENCES;
/// `has_column_privilege(.., 'SELECT, INSERT, UPDATE, REFERENCES')`.
const COLUMN_READ: u32 = acl_mode::SELECT | acl_mode::INSERT | acl_mode::UPDATE | acl_mode::REFERENCES;

/// Producer for `information_schema.table_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsTableConstraints;

impl VirtualRelation for IsTableConstraints {
    fn oid(&self) -> Oid {
        relid::IS_TABLE_CONSTRAINTS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_constraints_rows(ctx.env)
    }
}

/// Producer for `information_schema.key_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsKeyColumnUsage;

impl VirtualRelation for IsKeyColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_KEY_COLUMN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        key_column_usage_rows(ctx.env)
    }
}

/// Producer for `information_schema.referential_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsReferentialConstraints;

impl VirtualRelation for IsReferentialConstraints {
    fn oid(&self) -> Oid {
        relid::IS_REFERENTIAL_CONSTRAINTS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        referential_constraints_rows(ctx.env)
    }
}

/// Producer for `information_schema.constraint_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsConstraintColumnUsage;

impl VirtualRelation for IsConstraintColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_CONSTRAINT_COLUMN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        constraint_column_usage_rows(ctx.env)
    }
}

/// Producer for `information_schema.constraint_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsConstraintTableUsage;

impl VirtualRelation for IsConstraintTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_CONSTRAINT_TABLE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        constraint_table_usage_rows(ctx.env)
    }
}

/// Producer for `information_schema.check_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsCheckConstraints;

impl VirtualRelation for IsCheckConstraints {
    fn oid(&self) -> Oid {
        relid::IS_CHECK_CONSTRAINTS
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        check_constraints_rows(ctx.env)
    }
}

// ----------------------------------------------------------------------------- helpers

/// The current user's view of one snapshot: role graph for `pg_has_role` / privilege checks and
/// the name of the current database (`current_database()`).
struct Vis<'a> {
    snap: &'a CatalogSnapshot,
    graph: RoleGraph,
    user: Oid,
    database: &'a str,
}

impl<'a> Vis<'a> {
    fn new(env: &'a CatalogEnv) -> Result<Vis<'a>> {
        let snap = env.snapshot();
        let db_oid = env.database();
        let database = snap
            .database(db_oid)
            .map(|db| db.name.as_str())
            .ok_or_else(|| PgError::internal(format!("current database {db_oid} is not in the catalog")))?;
        Ok(Vis { snap, graph: RoleGraph::build(snap, db_oid)?, user: env.current_user(), database })
    }

    /// Schema name of a namespace OID (a missing namespace is catalog corruption).
    fn namespace(&self, oid: Oid) -> Result<&'a str> {
        self.snap
            .namespace(oid)
            .map(|n| n.name.as_str())
            .ok_or_else(|| PgError::corruption(format!("namespace {oid} does not exist")))
    }

    /// Any pg_class relation (table, view, ...).
    fn rel(&self, oid: Oid) -> Option<&'a Relation> {
        self.snap.relation(oid)
    }

    /// A relation with constraints in the SQL sense: relkind 'r' or 'p'.
    fn table(&self, oid: Oid) -> Option<&'a Relation> {
        self.snap.relation(oid).filter(|r| is_table_kind(r.kind))
    }

    /// `pg_has_role(owner, 'USAGE')`.
    fn owner_usage(&self, owner: Oid) -> Result<bool> {
        self.graph.has_privs_of_role(self.user, owner)
    }

    /// `has_table_privilege(rel, mask)`: any bit of `mask` held on the table.
    fn table_any(&self, rel: &Relation, mask: u32) -> Result<bool> {
        let eff = acl::effective_acl(AclObject::Table, rel.owner, rel.acl.as_ref())?;
        Ok(acl::privilege_mask(&self.graph, self.user, rel.owner, &eff, mask, false)? != 0)
    }

    /// Column-level grants only (no table-level inheritance).
    fn column_grant(&self, rel: &Relation, col: &Column, mask: u32) -> Result<bool> {
        let eff = acl::effective_acl(AclObject::Column, rel.owner, col.acl.as_ref())?;
        Ok(acl::privilege_mask(&self.graph, self.user, rel.owner, &eff, mask, false)? != 0)
    }

    /// `has_column_privilege(rel, col, mask)`: table-level or column-level grant (ANY bit).
    fn column_any(&self, rel: &Relation, col: &Column, mask: u32) -> Result<bool> {
        if self.table_any(rel, mask)? {
            return Ok(true);
        }
        self.column_grant(rel, col, mask)
    }

    /// `has_any_column_privilege(rel, mask)`: table-level or any live column grant.
    fn any_column(&self, rel: &Relation, mask: u32) -> Result<bool> {
        if self.table_any(rel, mask)? {
            return Ok(true);
        }
        for col in rel.columns.iter().filter(|c| !c.dropped) {
            if self.column_grant(rel, col, mask)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Visibility of table_constraints / referential_constraints rows of a table:
    /// owner USAGE, or DML/REFERENCES/TRIGGER on the table, or INSERT/UPDATE/REFERENCES on a column.
    fn table_visible(&self, rel: &Relation) -> Result<bool> {
        Ok(self.owner_usage(rel.owner)? || self.table_any(rel, TABLE_DML)? || self.any_column(rel, COLUMN_WRITE)?)
    }
}

fn is_table_kind(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::PartitionedTable)
}

fn schema(oid: Oid) -> Result<&'static SystemRel> {
    builtin::system_rel(oid).ok_or_else(|| PgError::internal(format!("no system relation with OID {oid}")))
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "YES"
    } else {
        "NO"
    }
}

/// The foreign key details of an FK constraint (missing details are catalog corruption).
fn fk_of(c: &Constraint) -> Result<&ForeignKey> {
    c.fk
        .as_ref()
        .ok_or_else(|| PgError::corruption(format!("foreign key constraint \"{}\" has no reference details", c.name)))
}

/// The CHECK expression of a CHECK constraint (missing expression is catalog corruption).
fn check_of(c: &Constraint) -> Result<&StoredExpr> {
    c.check
        .as_ref()
        .ok_or_else(|| PgError::corruption(format!("check constraint \"{}\" has no expression", c.name)))
}

/// A column by attnum that is not dropped (`NOT a.attisdropped`).
fn live_column(rel: &Relation, attnum: i16) -> Option<&Column> {
    rel.columns.iter().find(|c| c.attnum == attnum && !c.dropped)
}

/// `_pg_index_position(index, attnum)`: 1-based position of `attnum` in the index's `indkey`.
fn index_position(snap: &CatalogSnapshot, index: Oid, attnum: i16) -> Option<i32> {
    let ix = snap.index(index)?;
    let keys = ix.indkey();
    keys.iter().position(|&k| k == attnum).map(|p| p as i32 + 1)
}

/// The table or domain owner used by check_constraints (`coalesce(c.relowner, t.typowner)`).
fn constraint_owner(v: &Vis, c: &Constraint) -> Option<Oid> {
    if c.table != 0 {
        v.snap.relation(c.table).map(|r| r.owner)
    } else if c.domain != 0 {
        v.snap.type_obj(c.domain).map(|t| t.owner)
    } else {
        None
    }
}

/// constraint_column_usage rows for one constraint and the given attnums of `rel`. Duplicate
/// columns collapse (the PG view uses DISTINCT); the owner filter is `pg_has_role(tblowner)`.
fn push_usage(
    v: &Vis,
    s: &'static SystemRel,
    rel: &Relation,
    attnums: &[i16],
    c: &Constraint,
    out: &mut Vec<Row>,
) -> Result<()> {
    if !v.owner_usage(rel.owner)? {
        return Ok(());
    }
    let mut seen: Vec<i16> = Vec::with_capacity(attnums.len());
    for &attnum in attnums {
        if seen.contains(&attnum) {
            continue;
        }
        seen.push(attnum);
        let Some(col) = live_column(rel, attnum) else { continue };
        let mut b = RowBuilder::new(s);
        b.set("table_catalog", d::text(v.database))
            .set("table_schema", d::text(v.namespace(rel.namespace)?))
            .set("table_name", d::text(&rel.name))
            .set("column_name", d::text(&col.name))
            .set("constraint_catalog", d::text(v.database))
            .set("constraint_schema", d::text(v.namespace(c.namespace)?))
            .set("constraint_name", d::text(&c.name));
        out.push(b.finish()?);
    }
    Ok(())
}

// ----------------------------------------------------------------------------- row producers

fn table_constraints_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_TABLE_CONSTRAINTS)?;
    let mut out = Vec::new();
    for c in v.snap.data().constraints.values() {
        // NOT NULL is reported as CHECK; trigger and exclusion constraints are not shown.
        let Some(kind_text) = c.kind.info_schema_type() else { continue };
        if c.is_domain_constraint() {
            continue;
        }
        let Some(rel) = v.table(c.table) else { continue };
        if !v.table_visible(rel)? {
            continue;
        }
        // nulls_distinct: YES when the unique index treats NULLs as distinct, NO otherwise
        // (CASE WHEN NOT indnullsnotdistinct THEN 'YES' ELSE 'NO' END, so a missing index gives NO).
        let nulls_distinct = match c.kind {
            ConstraintKind::Unique => {
                Some(yes_no(v.snap.index(c.index).is_some_and(|ix| !ix.nulls_not_distinct)))
            }
            _ => None,
        };
        let mut b = RowBuilder::new(s);
        b.set("constraint_catalog", d::text(v.database))
            .set("constraint_schema", d::text(v.namespace(c.namespace)?))
            .set("constraint_name", d::text(&c.name))
            .set("table_catalog", d::text(v.database))
            .set("table_schema", d::text(v.namespace(rel.namespace)?))
            .set("table_name", d::text(&rel.name))
            .set("constraint_type", d::text(kind_text))
            .set("is_deferrable", d::text(yes_no(c.deferrable)))
            .set("initially_deferred", d::text(yes_no(c.initially_deferred)))
            .set("enforced", d::text(yes_no(c.enforced)))
            .set("nulls_distinct", d::opt_text(nulls_distinct));
        out.push(b.finish()?);
    }
    Ok(out)
}

fn key_column_usage_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_KEY_COLUMN_USAGE)?;
    let mut out = Vec::new();
    for c in v.snap.data().constraints.values() {
        if !matches!(c.kind, ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::ForeignKey) {
            continue;
        }
        let Some(rel) = v.table(c.table) else { continue };
        let fk: Option<&ForeignKey> = if c.kind == ConstraintKind::ForeignKey { Some(fk_of(c)?) } else { None };
        for (i, &attnum) in c.columns.iter().enumerate() {
            let Some(col) = live_column(rel, attnum) else { continue };
            if !(v.owner_usage(rel.owner)? || v.column_any(rel, col, COLUMN_READ)?) {
                continue;
            }
            // FK only: position of the referenced column in the referenced unique/PK index.
            let position = fk.and_then(|f| {
                f.ref_columns.get(i).and_then(|&rc| index_position(v.snap, f.ref_index, rc))
            });
            let mut b = RowBuilder::new(s);
            b.set("constraint_catalog", d::text(v.database))
                .set("constraint_schema", d::text(v.namespace(c.namespace)?))
                .set("constraint_name", d::text(&c.name))
                .set("table_catalog", d::text(v.database))
                .set("table_schema", d::text(v.namespace(rel.namespace)?))
                .set("table_name", d::text(&rel.name))
                .set("column_name", d::text(&col.name))
                .set("ordinal_position", d::int4((i + 1) as i32))
                .set("position_in_unique_constraint", d::opt_int4(position));
            out.push(b.finish()?);
        }
    }
    Ok(out)
}

fn referential_constraints_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_REFERENTIAL_CONSTRAINTS)?;
    let mut out = Vec::new();
    for c in v.snap.data().constraints.values() {
        if c.kind != ConstraintKind::ForeignKey {
            continue;
        }
        let Some(rel) = v.rel(c.table) else { continue };
        if !v.table_visible(rel)? {
            continue;
        }
        let fk = fk_of(c)?;
        // The PRIMARY KEY / UNIQUE constraint that owns the referenced index.
        let unique = v
            .snap
            .constraints_of(fk.ref_table)
            .into_iter()
            .find(|p| matches!(p.kind, ConstraintKind::PrimaryKey | ConstraintKind::Unique) && p.index == fk.ref_index);
        let (uc_catalog, uc_schema, uc_name) = match unique {
            Some(p) => (Some(v.database), Some(v.namespace(p.namespace)?), Some(p.name.as_str())),
            None => (None, None, None),
        };
        let mut b = RowBuilder::new(s);
        b.set("constraint_catalog", d::text(v.database))
            .set("constraint_schema", d::text(v.namespace(c.namespace)?))
            .set("constraint_name", d::text(&c.name))
            .set("unique_constraint_catalog", d::opt_text(uc_catalog))
            .set("unique_constraint_schema", d::opt_text(uc_schema))
            .set("unique_constraint_name", d::opt_text(uc_name))
            .set("match_option", d::text(fk.match_type.info_schema()))
            .set("update_rule", d::text(fk.on_update.sql()))
            .set("delete_rule", d::text(fk.on_delete.sql()));
        out.push(b.finish()?);
    }
    Ok(out)
}

fn constraint_column_usage_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_CONSTRAINT_COLUMN_USAGE)?;
    let mut out = Vec::new();
    // Same order as the PG view's UNION ALL: CHECK, then NOT NULL, then PRIMARY KEY / UNIQUE /
    // FOREIGN KEY (for a FK, the REFERENCED table's columns).
    for c in v.snap.data().constraints.values() {
        if c.kind != ConstraintKind::Check || c.table == 0 {
            continue;
        }
        if let Some(rel) = v.table(c.table) {
            push_usage(&v, s, rel, &c.columns, c, &mut out)?;
        }
    }
    for c in v.snap.data().constraints.values() {
        if c.kind != ConstraintKind::NotNull || c.table == 0 {
            continue;
        }
        if let Some(rel) = v.table(c.table) {
            let attnums = &c.columns[..c.columns.len().min(1)];
            push_usage(&v, s, rel, attnums, c, &mut out)?;
        }
    }
    for c in v.snap.data().constraints.values() {
        match c.kind {
            ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                if let Some(rel) = v.table(c.table) {
                    push_usage(&v, s, rel, &c.columns, c, &mut out)?;
                }
            }
            ConstraintKind::ForeignKey => {
                let fk = fk_of(c)?;
                if let Some(rel) = v.table(fk.ref_table) {
                    push_usage(&v, s, rel, &fk.ref_columns, c, &mut out)?;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn constraint_table_usage_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_CONSTRAINT_TABLE_USAGE)?;
    let mut out = Vec::new();
    for c in v.snap.data().constraints.values() {
        // PRIMARY KEY / UNIQUE: the constrained table; FOREIGN KEY: the REFERENCED table.
        let table_oid = match c.kind {
            ConstraintKind::PrimaryKey | ConstraintKind::Unique => c.table,
            ConstraintKind::ForeignKey => fk_of(c)?.ref_table,
            _ => continue,
        };
        let Some(rel) = v.table(table_oid) else { continue };
        if !v.owner_usage(rel.owner)? {
            continue;
        }
        let mut b = RowBuilder::new(s);
        b.set("table_catalog", d::text(v.database))
            .set("table_schema", d::text(v.namespace(rel.namespace)?))
            .set("table_name", d::text(&rel.name))
            .set("constraint_catalog", d::text(v.database))
            .set("constraint_schema", d::text(v.namespace(c.namespace)?))
            .set("constraint_name", d::text(&c.name));
        out.push(b.finish()?);
    }
    Ok(out)
}

fn check_constraints_rows(env: &CatalogEnv) -> Result<Vec<Row>> {
    let v = Vis::new(env)?;
    let s = schema(relid::IS_CHECK_CONSTRAINTS)?;
    let mut out = Vec::new();
    // PG: the CHECK branch of the UNION ALL first, then the NOT NULL branch.
    for pass in [ConstraintKind::Check, ConstraintKind::NotNull] {
        for c in v.snap.data().constraints.values() {
            if c.kind != pass {
                continue;
            }
            let Some(owner) = constraint_owner(&v, c) else { continue };
            if !v.owner_usage(owner)? {
                continue;
            }
            let clause: Cow<str> = match c.kind {
                ConstraintKind::Check => Cow::Borrowed(check_of(c)?.sql.as_str()),
                _ => {
                    // `<column> IS NOT NULL`, or `VALUE IS NOT NULL` for domains (no column).
                    let attname = if c.table != 0 {
                        c.columns
                            .first()
                            .and_then(|&a| v.snap.relation(c.table).and_then(|r| r.columns.iter().find(|col| col.attnum == a)))
                            .map(|col| col.name.as_str())
                            .unwrap_or("VALUE")
                    } else {
                        "VALUE"
                    };
                    Cow::Owned(format!("{attname} IS NOT NULL"))
                }
            };
            let mut b = RowBuilder::new(s);
            b.set("constraint_catalog", d::text(v.database))
                .set("constraint_schema", d::text(v.namespace(c.namespace)?))
                .set("constraint_name", d::text(&c.name))
                .set("check_clause", d::text(&clause));
            out.push(b.finish()?);
        }
    }
    Ok(out)
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsTableConstraints,
    &IsKeyColumnUsage,
    &IsReferentialConstraints,
    &IsConstraintColumnUsage,
    &IsConstraintTableUsage,
    &IsCheckConstraints,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use crate::model::{FkAction, FkMatch, Persistence};
    use crate::testing;
    use pgarbor_common::MemKv;
    use pgarbor_types::{Datum, TypeId};

    const PUBLIC: Oid = 2200;
    const POSTGRES: Oid = 10;

    fn col(name: &str, not_null: bool) -> ColumnSpec {
        let mut c = ColumnSpec::new(name, TypeId::INT4);
        if not_null {
            c.not_null = Some(None);
        }
        c
    }

    fn named(name: &str, kind: ConstraintSpecKind) -> ConstraintSpec {
        let mut c = ConstraintSpec::new(kind);
        c.name = Some(name.to_string());
        c
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn text_at(row: &Row, idx: usize) -> Option<&str> {
        match row.get(idx)? {
            Datum::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn int_at(row: &Row, idx: usize) -> Option<i32> {
        match row.get(idx)? {
            Datum::Int4(v) => Some(*v),
            _ => None,
        }
    }

    fn col_idx(oid: Oid, name: &str) -> usize {
        builtin::system_rel(oid)
            .and_then(|r| r.columns.iter().position(|c| c.name == name))
            .expect("column exists in the system relation")
    }

    /// The single row whose `key` column equals `value`.
    fn find<'r>(rows: &'r [Row], oid: Oid, key: &str, value: &str) -> &'r Row {
        let i = col_idx(oid, key);
        rows.iter().find(|r| text_at(r, i) == Some(value)).expect("row present")
    }

    /// p(id pk, b unique, x check (x > 0)); c(a not null references p(id) on delete cascade,
    /// n, constraint cc check (n < 5), unique(a, n)).
    fn fixture() -> Result<(MemKv, CatalogEnv)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let p = CreateTableSpec {
            namespace: PUBLIC,
            name: "p".into(),
            owner: POSTGRES,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![col("id", false), col("b", false), col("x", false)],
            constraints: vec![
                ConstraintSpec::new(ConstraintSpecKind::PrimaryKey { columns: strs(&["id"]), include: vec![] }),
                ConstraintSpec::new(ConstraintSpecKind::Unique {
                    columns: strs(&["b"]),
                    include: vec![],
                    nulls_not_distinct: false,
                }),
                ConstraintSpec::new(ConstraintSpecKind::Check {
                    expr: StoredExpr::new("(x > 0)"),
                    columns: strs(&["x"]),
                    no_inherit: false,
                }),
            ],
            options: vec![],
            if_not_exists: false,
            inherits: vec![],
        };
        let mut p_oid: Oid = 0;
        let (_, env) = testing::run(&mut kv, &env, |t| {
            if let Some(created) = t.create_table(&p)? {
                p_oid = created.table;
            }
            Ok(())
        })?;
        let c = CreateTableSpec {
            namespace: PUBLIC,
            name: "c".into(),
            owner: POSTGRES,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![col("a", true), col("n", false)],
            constraints: vec![
                ConstraintSpec::new(ConstraintSpecKind::ForeignKey {
                    columns: strs(&["a"]),
                    ref_table: p_oid,
                    ref_columns: vec![],
                    on_update: FkAction::NoAction,
                    on_delete: FkAction::Cascade,
                    match_type: FkMatch::Simple,
                    delete_set_columns: vec![],
                }),
                named("cc", ConstraintSpecKind::Check {
                    expr: StoredExpr::new("(n < 5)"),
                    columns: strs(&["n"]),
                    no_inherit: false,
                }),
                ConstraintSpec::new(ConstraintSpecKind::Unique {
                    columns: strs(&["a", "n"]),
                    include: vec![],
                    nulls_not_distinct: false,
                }),
            ],
            options: vec![],
            if_not_exists: false,
            inherits: vec![],
        };
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&c).map(|_| ()))?;
        Ok((kv, env))
    }

    #[test]
    fn table_constraints_match_oracle_rows() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = table_constraints_rows(&env)?;
        assert_eq!(rows.len(), 8);
        let oid = relid::IS_TABLE_CONSTRAINTS;
        let expected = [
            ("c_a_fkey", "FOREIGN KEY"),
            ("c_a_n_key", "UNIQUE"),
            ("c_a_not_null", "CHECK"),
            ("cc", "CHECK"),
            ("p_b_key", "UNIQUE"),
            ("p_id_not_null", "CHECK"),
            ("p_pkey", "PRIMARY KEY"),
            ("p_x_check", "CHECK"),
        ];
        let kind = col_idx(oid, "constraint_type");
        for (name, ty) in expected {
            let row = find(&rows, oid, "constraint_name", name);
            assert_eq!(text_at(row, kind), Some(ty), "constraint_type of {name}");
        }
        let nulls = col_idx(oid, "nulls_distinct");
        assert_eq!(text_at(find(&rows, oid, "constraint_name", "p_b_key"), nulls), Some("YES"));
        assert_eq!(text_at(find(&rows, oid, "constraint_name", "p_pkey"), nulls), None);
        Ok(())
    }

    #[test]
    fn key_column_usage_positions() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = key_column_usage_rows(&env)?;
        let oid = relid::IS_KEY_COLUMN_USAGE;
        let fk = find(&rows, oid, "constraint_name", "c_a_fkey");
        assert_eq!(text_at(fk, col_idx(oid, "table_name")), Some("c"));
        assert_eq!(text_at(fk, col_idx(oid, "column_name")), Some("a"));
        assert_eq!(int_at(fk, col_idx(oid, "ordinal_position")), Some(1));
        assert_eq!(int_at(fk, col_idx(oid, "position_in_unique_constraint")), Some(1));
        let uniq: Vec<&Row> = rows.iter().filter(|r| text_at(r, col_idx(oid, "constraint_name")) == Some("c_a_n_key")).collect();
        assert_eq!(uniq.len(), 2);
        assert_eq!(int_at(uniq[1], col_idx(oid, "ordinal_position")), Some(2));
        assert_eq!(int_at(uniq[1], col_idx(oid, "position_in_unique_constraint")), None);
        Ok(())
    }

    #[test]
    fn referential_constraints_rules() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = referential_constraints_rows(&env)?;
        let oid = relid::IS_REFERENTIAL_CONSTRAINTS;
        let r = find(&rows, oid, "constraint_name", "c_a_fkey");
        assert_eq!(text_at(r, col_idx(oid, "unique_constraint_name")), Some("p_pkey"));
        assert_eq!(text_at(r, col_idx(oid, "match_option")), Some("NONE"));
        assert_eq!(text_at(r, col_idx(oid, "update_rule")), Some("NO ACTION"));
        assert_eq!(text_at(r, col_idx(oid, "delete_rule")), Some("CASCADE"));
        Ok(())
    }

    #[test]
    fn constraint_column_usage_lists_referenced_columns() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = constraint_column_usage_rows(&env)?;
        let oid = relid::IS_CONSTRAINT_COLUMN_USAGE;
        let t = col_idx(oid, "table_name");
        let cname = col_idx(oid, "constraint_name");
        let fk_rows: Vec<&Row> = rows.iter().filter(|r| text_at(r, cname) == Some("c_a_fkey")).collect();
        assert_eq!(fk_rows.len(), 1);
        assert_eq!(text_at(fk_rows[0], t), Some("p"));
        assert_eq!(text_at(fk_rows[0], col_idx(oid, "column_name")), Some("id"));
        Ok(())
    }

    #[test]
    fn constraint_table_usage_points_fk_at_referenced_table() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = constraint_table_usage_rows(&env)?;
        let oid = relid::IS_CONSTRAINT_TABLE_USAGE;
        let r = find(&rows, oid, "constraint_name", "c_a_fkey");
        assert_eq!(text_at(r, col_idx(oid, "table_name")), Some("p"));
        Ok(())
    }

    #[test]
    fn check_constraints_clauses() -> Result<()> {
        let (_kv, env) = fixture()?;
        let rows = check_constraints_rows(&env)?;
        let oid = relid::IS_CHECK_CONSTRAINTS;
        let clause = col_idx(oid, "check_clause");
        assert_eq!(text_at(find(&rows, oid, "constraint_name", "c_a_not_null"), clause), Some("a IS NOT NULL"));
        assert_eq!(text_at(find(&rows, oid, "constraint_name", "cc"), clause), Some("(n < 5)"));
        Ok(())
    }
}
