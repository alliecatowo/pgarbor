//! `pg_tables`, `pg_views`, `pg_matviews`, `pg_indexes`, `pg_sequences`: convenience views over
//! the model (PG defines them in `catalog/system_views.sql` as joins over pg_class). Lease CAT-36.
//! Ordered by (schemaname, name) like the usual `ORDER BY` clients add; the executor sorts anyway.
//! `*owner` columns are ROLE NAMES (`snapshot.role_name`, unknown -> `unknown (OID=n)`).
//!
//! - **pg_tables**: relkind r and p (builtin system tables too): `schemaname, tablename, tableowner,
//!   tablespace` NULL, `hasindexes` (`indexes_of` non-empty), `hasrules` false, `hastriggers` false,
//!   `rowsecurity`.
//! - **pg_views**: relkind v (user and system views): `schemaname, viewname, viewowner, definition`
//!   = `deparse::viewdef` text (for system views the stored `SYSTEM_VIEW_DEFS`).
//! - **pg_matviews**: relkind m: `schemaname, matviewname, matviewowner, tablespace` NULL,
//!   `hasindexes, ispopulated, definition`.
//! - **pg_indexes**: user indexes: `schemaname, tablename, indexname, tablespace` NULL, `indexdef` =
//!   `deparse::indexdef(env, index, None, false)`. System indexes are omitted (gap).
//! - **pg_sequences**: `schemaname, sequencename, sequenceowner, data_type` (`regtype` = `Oid(type_oid)`),
//!   `start_value, min_value, max_value, increment_by` (`Int8`), `cycle, cache_size`, `last_value` =
//!   `session.sequence_state(oid)?.last_value` (NULL if never called or no state).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid};
use crate::deparse;
use crate::model::{RelKind, Relation};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::Datum;

/// `*owner` column text: the role name, or PG's `unknown (OID=n)` (`pg_get_userbyid`).
fn owner_name(snap: &CatalogSnapshot, oid: Oid) -> String {
    match snap.role_name(oid) {
        Some(name) => name.to_string(),
        None => format!("unknown (OID={oid})"),
    }
}

/// Schema name of a namespace OID. An unknown schema is catalog corruption (a dangling reference).
fn schema_name(snap: &CatalogSnapshot, oid: Oid) -> Result<String> {
    snap.namespace(oid)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::corruption(format!("namespace {oid} referenced by a catalog object does not exist")))
}

/// Every relation: the builtin system tables and views first, then the user relations.
fn all_relations(snap: &CatalogSnapshot) -> Vec<&Relation> {
    let mut out: Vec<&Relation> = Vec::new();
    for rel in builtin::builtin_relations() {
        out.push(rel);
    }
    for rel in snap.user_relations() {
        out.push(rel);
    }
    out
}

/// A definition that the deparser must produce for an existing object (`None` is corruption).
fn required_definition(text: Option<String>, oid: Oid) -> Result<String> {
    text.ok_or_else(|| PgError::internal(format!("no definition text for object {oid}")))
}

/// Sort `(schemaname, name, row)` triples by (schemaname, name) and drop the sort keys.
fn ordered(mut rows: Vec<(String, String, Row)>) -> Vec<Row> {
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows.into_iter().map(|(_, _, row)| row).collect()
}

/// Producer for `pg_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgTables;

impl VirtualRelation for PgTables {
    fn oid(&self) -> Oid {
        relid::PG_TABLES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        // No fast path: the executor re-applies the WHERE clause.
        let _ = req;
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out: Vec<(String, String, Row)> = Vec::new();
        for rel in all_relations(snap) {
            if !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable) {
                continue;
            }
            let schemaname = schema_name(snap, rel.namespace)?;
            let has_indexes = !snap.indexes_of(rel.oid).is_empty();
            let mut b = RowBuilder::new(schema);
            // tablespace stays NULL (no tablespaces).
            b.set("schemaname", d::name(&schemaname))
                .set("tablename", d::name(&rel.name))
                .set("tableowner", d::name(&owner_name(snap, rel.owner)))
                .set("hasindexes", d::bool_(has_indexes))
                .set("hasrules", d::bool_(false))
                .set("hastriggers", d::bool_(false))
                .set("rowsecurity", d::bool_(rel.row_security));
            out.push((schemaname, rel.name.clone(), b.finish()?));
        }
        Ok(ordered(out))
    }
}

/// Producer for `pg_views`.
#[derive(Clone, Copy, Debug)]
pub struct PgViews;

impl VirtualRelation for PgViews {
    fn oid(&self) -> Oid {
        relid::PG_VIEWS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out: Vec<(String, String, Row)> = Vec::new();
        for rel in all_relations(snap) {
            if rel.kind != RelKind::View {
                continue;
            }
            let schemaname = schema_name(snap, rel.namespace)?;
            let definition = required_definition(deparse::viewdef(ctx.env, rel.oid, false)?, rel.oid)?;
            let mut b = RowBuilder::new(schema);
            b.set("schemaname", d::name(&schemaname))
                .set("viewname", d::name(&rel.name))
                .set("viewowner", d::name(&owner_name(snap, rel.owner)))
                .set("definition", d::text(&definition));
            out.push((schemaname, rel.name.clone(), b.finish()?));
        }
        Ok(ordered(out))
    }
}

/// Producer for `pg_matviews`.
#[derive(Clone, Copy, Debug)]
pub struct PgMatviews;

impl VirtualRelation for PgMatviews {
    fn oid(&self) -> Oid {
        relid::PG_MATVIEWS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out: Vec<(String, String, Row)> = Vec::new();
        for rel in all_relations(snap) {
            if rel.kind != RelKind::MatView {
                continue;
            }
            let schemaname = schema_name(snap, rel.namespace)?;
            let has_indexes = !snap.indexes_of(rel.oid).is_empty();
            // `relispopulated` is recorded on the view metadata (WITH NO DATA -> false).
            let populated = rel.view.as_ref().map_or(true, |v| v.populated);
            let definition = required_definition(deparse::viewdef(ctx.env, rel.oid, false)?, rel.oid)?;
            let mut b = RowBuilder::new(schema);
            // tablespace stays NULL (no tablespaces).
            b.set("schemaname", d::name(&schemaname))
                .set("matviewname", d::name(&rel.name))
                .set("matviewowner", d::name(&owner_name(snap, rel.owner)))
                .set("hasindexes", d::bool_(has_indexes))
                .set("ispopulated", d::bool_(populated))
                .set("definition", d::text(&definition));
            out.push((schemaname, rel.name.clone(), b.finish()?));
        }
        Ok(ordered(out))
    }
}

/// Producer for `pg_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgIndexes;

impl VirtualRelation for PgIndexes {
    fn oid(&self) -> Oid {
        relid::PG_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out: Vec<(String, String, Row)> = Vec::new();
        // User indexes only (system indexes are not linked to tables in this catalog).
        for index in snap.data().indexes.values() {
            let table = snap.relation(index.table).ok_or_else(|| {
                PgError::corruption(format!("index {} refers to table {} which does not exist", index.oid, index.table))
            })?;
            let schemaname = schema_name(snap, index.namespace)?;
            let indexdef = required_definition(deparse::indexdef(ctx.env, index.oid, None, false)?, index.oid)?;
            let mut b = RowBuilder::new(schema);
            // tablespace stays NULL (no tablespaces).
            b.set("schemaname", d::name(&schemaname))
                .set("tablename", d::name(&table.name))
                .set("indexname", d::name(&index.name))
                .set("indexdef", d::text(&indexdef));
            out.push((schemaname, index.name.clone(), b.finish()?));
        }
        Ok(ordered(out))
    }
}

/// Producer for `pg_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgSequences;

impl VirtualRelation for PgSequences {
    fn oid(&self) -> Oid {
        relid::PG_SEQUENCES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out: Vec<(String, String, Row)> = Vec::new();
        for seq in snap.data().sequences.values() {
            let schemaname = schema_name(snap, seq.namespace)?;
            // NULL until the sequence has been called (`is_called = false` in the state row).
            let last_value = ctx.session.sequence_state(seq.oid)?.and_then(|state| state.last_value);
            let mut b = RowBuilder::new(schema);
            b.set("schemaname", d::name(&schemaname))
                .set("sequencename", d::name(&seq.name))
                .set("sequenceowner", d::name(&owner_name(snap, seq.owner)))
                .set("data_type", d::oid(seq.type_oid))
                .set("start_value", d::int8(seq.start))
                .set("min_value", d::int8(seq.min))
                .set("max_value", d::int8(seq.max))
                .set("increment_by", d::int8(seq.increment))
                .set("cycle", d::bool_(seq.cycle))
                .set("cache_size", d::int8(seq.cache))
                .set("last_value", d::opt_int8(last_value));
            out.push((schemaname, seq.name.clone(), b.finish()?));
        }
        Ok(ordered(out))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgTables, &PgViews, &PgMatviews, &PgIndexes, &PgSequences];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    /// Text value of a column, or a marker for a non-text Datum.
    fn text_at(row: &Row, idx: usize) -> String {
        match &row[idx] {
            Datum::Text(s) => s.clone(),
            _ => String::from("<not text>"),
        }
    }

    fn find_row<'a>(rows: &'a [Row], schema_col: usize, name_col: usize, schema: &str, name: &str) -> Option<&'a Row> {
        rows.iter().find(|r| text_at(r, schema_col) == schema && text_at(r, name_col) == name)
    }

    #[test]
    fn pg_tables_lists_system_tables_with_role_owner() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = PgTables.scan(&ctx, &ScanRequest::all()).expect("pg_tables rows");
        // pg_class is an ordinary system table; pg_tables itself is a view and must be absent.
        let pg_class = find_row(&rows, 0, 1, "pg_catalog", "pg_class").expect("pg_class row");
        assert_eq!(text_at(pg_class, 2), "postgres");
        assert!(find_row(&rows, 0, 1, "pg_catalog", "pg_tables").is_none());
        assert!(rows.iter().all(|r| r.len() == 8));
    }

    #[test]
    fn pg_tables_rows_are_ordered_by_schema_and_name() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = PgTables.scan(&ctx, &ScanRequest::all()).expect("pg_tables rows");
        for pair in rows.windows(2) {
            let a = (text_at(&pair[0], 0), text_at(&pair[0], 1));
            let b = (text_at(&pair[1], 0), text_at(&pair[1], 1));
            assert!(a <= b, "rows out of order: {a:?} > {b:?}");
        }
    }

    #[test]
    fn pg_views_definition_is_the_view_text() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = PgViews.scan(&ctx, &ScanRequest::all()).expect("pg_views rows");
        let row = find_row(&rows, 0, 1, "pg_catalog", "pg_tables").expect("pg_tables view row");
        assert_eq!(text_at(row, 2), "postgres");
        assert!(text_at(row, 3).contains("pg_class"));
        assert!(rows.iter().all(|r| r.len() == 4));
    }

    #[test]
    fn user_objects_absent_on_fresh_catalog() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        assert!(PgMatviews.scan(&ctx, &ScanRequest::all()).expect("pg_matviews").is_empty());
        assert!(PgIndexes.scan(&ctx, &ScanRequest::all()).expect("pg_indexes").is_empty());
        assert!(PgSequences.scan(&ctx, &ScanRequest::all()).expect("pg_sequences").is_empty());
    }

    #[test]
    fn owner_name_unknown_role_uses_oid_form() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        assert_eq!(owner_name(env.snapshot(), 10), "postgres");
        assert_eq!(owner_name(env.snapshot(), 987_654), "unknown (OID=987654)");
    }

    #[test]
    fn registry_matches_declared_oids() {
        let oids: Vec<Oid> = RELATIONS.iter().map(|r| r.oid()).collect();
        assert_eq!(oids, vec![relid::PG_TABLES, relid::PG_VIEWS, relid::PG_MATVIEWS, relid::PG_INDEXES, relid::PG_SEQUENCES]);
    }
}
