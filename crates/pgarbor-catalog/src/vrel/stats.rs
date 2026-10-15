//! Per-relation statistics views: `pg_stat_{all,user,sys}_{tables,indexes}`,
//! `pg_statio_{all,user,sys}_{tables,indexes,sequences}`, `pg_stat_xact_{all,user,sys}_tables`.
//! Lease CAT-39. PGArbor does not collect these counters (documented gap): every counter column is
//! reported as zero / NULL exactly like a freshly created PG table that was never touched, so tools
//! that list tables with statistics (pgAdmin, DBeaver, Grafana dashboards) work. Row SETS are real:
//! - `*_tables`: one row per relation of relkind r, m, t, p: `relid, schemaname, relname` + the
//!   counters; `n_live_tup`/`n_dead_tup` etc. `Int8(0)`; timestamp columns NULL; `total_*_time`
//!   `Float8(0.0)`. `user` = relations outside pg_catalog/information_schema/pg_toast; `sys` = the rest;
//!   `all` = both.
//! - `*_indexes`: one row per index (`indexrelid, relid` = table, `schemaname, relname, indexrelname`),
//!   counters 0 / NULL. System indexes are omitted (not linked to tables, gap).
//! - `pg_statio_*_sequences`: `relid, schemaname, relname, blks_read 0, blks_hit 0`.
//! - `pg_stat_xact_*_tables`: same shape as `pg_stat_*_tables` minus the vacuum columns (use the
//!   column list of `builtin::system_rel`), zero counters.
//! Column names/types are authoritative in `builtin::SYSTEM_RELS`; build rows with `RowBuilder`
//! (unset = NULL) and set every non-nullable counter to 0.
//! A future epoch may feed real counters through `SessionInfo`; until then these relations must
//! not pretend to measure anything: this paragraph is the compatibility statement.
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::model::{Index, RelKind, Relation};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::{oids, Datum};

/// Which schemas a view lists (`all` = both `user` and `sys`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    All,
    User,
    Sys,
}

impl Scope {
    fn lists(self, schema: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::User => !is_system_schema(schema),
            Scope::Sys => is_system_schema(schema),
        }
    }
}

/// PG's `sys` filter: `schemaname IN ('pg_catalog', 'information_schema') OR schemaname ~ '^pg_toast'`.
fn is_system_schema(schema: &str) -> bool {
    matches!(schema, "pg_catalog" | "information_schema") || schema.starts_with("pg_toast")
}

/// relkinds listed by `pg_stat_{all,user,sys}_tables` and `pg_stat_xact_*_tables`: r, t, m, p.
fn stat_table_kind(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::Toast | RelKind::MatView | RelKind::PartitionedTable)
}

/// relkinds listed by `pg_statio_*_tables` and by the `*_indexes` views: r, t, m (no partitioned).
fn heap_kind(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::Toast | RelKind::MatView)
}

/// Counter columns that PG reports as NULL when the relation has no index: PG sums them over a
/// LEFT JOIN of `pg_index`, and `sum()` over zero rows is NULL.
const NULL_WITHOUT_INDEX: &[&str] = &["idx_scan", "idx_tup_fetch", "idx_blks_read", "idx_blks_hit"];

/// Columns that are NULL for every table: PGArbor creates no TOAST tables (`reltoastrelid` 0), and
/// PG's `pg_statio_*_tables` joins them through `reltoastrelid`, which yields NULL.
const ALWAYS_NULL: &[&str] = &["toast_blks_read", "toast_blks_hit", "tidx_blks_read", "tidx_blks_hit"];

/// A row of `rel` with every `int8` column 0, every `float8` column 0.0 and every other column NULL.
fn zeroed(rel: &'static SystemRel) -> RowBuilder {
    let mut b = RowBuilder::new(rel);
    for c in rel.columns {
        match c.type_oid {
            oids::INT8 => {
                b.set(c.name, Datum::Int8(0));
            }
            oids::FLOAT8 => {
                b.set(c.name, Datum::Float8(0.0));
            }
            _ => {}
        }
    }
    b
}

/// Sets `column` only when `rel` has it: the nine views differ in their column lists, and the
/// lists in `builtin::SYSTEM_RELS` are authoritative.
fn put(b: &mut RowBuilder, rel: &SystemRel, column: &str, value: Datum) {
    if rel.columns.iter().any(|c| c.name == column) {
        b.set(column, value);
    }
}

/// Schema name of a namespace OID; a relation pointing at a missing schema is catalog corruption.
fn schema_name(snap: &CatalogSnapshot, oid: Oid) -> Result<&str> {
    match snap.namespace(oid) {
        Some(n) => Ok(n.name.as_str()),
        None => Err(PgError::corruption(format!("relation refers to missing schema {oid}"))),
    }
}

/// User relations and builtin system relations whose relkind passes `want`, sorted by OID.
fn listed_tables<'a>(snap: &'a CatalogSnapshot, want: fn(RelKind) -> bool) -> Vec<&'a Relation> {
    let mut out: Vec<&'a Relation> = Vec::new();
    for r in snap.user_relations() {
        if want(r.kind) {
            out.push(r);
        }
    }
    for r in builtin::builtin_relations() {
        if want(r.kind) {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.oid);
    out
}

/// One row per listed table (`pg_stat_*_tables`, `pg_statio_*_tables`, `pg_stat_xact_*_tables`),
/// ordered by relid.
fn table_rows(ctx: &ScanContext<'_>, rel: &'static SystemRel, scope: Scope, want: fn(RelKind) -> bool) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let mut rows = Vec::new();
    for table in listed_tables(snap, want) {
        let schema = schema_name(snap, table.namespace)?;
        if !scope.lists(schema) {
            continue;
        }
        let mut b = zeroed(rel);
        b.set("relid", d::oid(table.oid)).set("schemaname", d::name(schema)).set("relname", d::name(&table.name));
        if snap.indexes_of(table.oid).is_empty() {
            for &col in NULL_WITHOUT_INDEX {
                put(&mut b, rel, col, Datum::Null);
            }
        }
        for &col in ALWAYS_NULL {
            put(&mut b, rel, col, Datum::Null);
        }
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// One row per index of a listed table (`pg_stat_*_indexes`, `pg_statio_*_indexes`), ordered by
/// (relid, indexrelid). The schema is the table's, as in PG's `C.relnamespace`.
fn index_rows(ctx: &ScanContext<'_>, rel: &'static SystemRel, scope: Scope) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let mut entries: Vec<(&Relation, &Index)> = Vec::new();
    for index in snap.data().indexes.values() {
        let table = snap.relation(index.table).ok_or_else(|| {
            PgError::corruption(format!("index {} refers to missing table {}", index.oid, index.table))
        })?;
        if heap_kind(table.kind) {
            entries.push((table, index));
        }
    }
    entries.sort_by_key(|(t, i)| (t.oid, i.oid));
    let mut rows = Vec::with_capacity(entries.len());
    for (table, index) in entries {
        let schema = schema_name(snap, table.namespace)?;
        if !scope.lists(schema) {
            continue;
        }
        let mut b = zeroed(rel);
        b.set("relid", d::oid(table.oid))
            .set("indexrelid", d::oid(index.oid))
            .set("schemaname", d::name(schema))
            .set("relname", d::name(&table.name))
            .set("indexrelname", d::name(&index.name));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// One row per user sequence (`pg_statio_*_sequences`), ordered by OID.
fn sequence_rows(ctx: &ScanContext<'_>, rel: &'static SystemRel, scope: Scope) -> Result<Vec<Row>> {
    let snap = ctx.env.snapshot();
    let mut rows = Vec::new();
    for seq in snap.data().sequences.values() {
        let schema = schema_name(snap, seq.namespace)?;
        if !scope.lists(schema) {
            continue;
        }
        let mut b = zeroed(rel);
        b.set("relid", d::oid(seq.oid)).set("schemaname", d::name(schema)).set("relname", d::name(&seq.name));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// Producer for `pg_stat_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatAllTables;

impl VirtualRelation for PgStatAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ALL_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::All, stat_table_kind)
    }
}

/// Producer for `pg_stat_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserTables;

impl VirtualRelation for PgStatUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::User, stat_table_kind)
    }
}

/// Producer for `pg_stat_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysTables;

impl VirtualRelation for PgStatSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::Sys, stat_table_kind)
    }
}

/// Producer for `pg_stat_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatAllIndexes;

impl VirtualRelation for PgStatAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ALL_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::All)
    }
}

/// Producer for `pg_stat_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserIndexes;

impl VirtualRelation for PgStatUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::User)
    }
}

/// Producer for `pg_stat_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysIndexes;

impl VirtualRelation for PgStatSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::Sys)
    }
}

/// Producer for `pg_statio_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllTables;

impl VirtualRelation for PgStatioAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::All, heap_kind)
    }
}

/// Producer for `pg_statio_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserTables;

impl VirtualRelation for PgStatioUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::User, heap_kind)
    }
}

/// Producer for `pg_statio_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysTables;

impl VirtualRelation for PgStatioSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::Sys, heap_kind)
    }
}

/// Producer for `pg_statio_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllIndexes;

impl VirtualRelation for PgStatioAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::All)
    }
}

/// Producer for `pg_statio_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserIndexes;

impl VirtualRelation for PgStatioUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::User)
    }
}

/// Producer for `pg_statio_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysIndexes;

impl VirtualRelation for PgStatioSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_INDEXES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        index_rows(ctx, self.schema()?, Scope::Sys)
    }
}

/// Producer for `pg_statio_all_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllSequences;

impl VirtualRelation for PgStatioAllSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_SEQUENCES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        sequence_rows(ctx, self.schema()?, Scope::All)
    }
}

/// Producer for `pg_statio_user_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserSequences;

impl VirtualRelation for PgStatioUserSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_SEQUENCES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        sequence_rows(ctx, self.schema()?, Scope::User)
    }
}

/// Producer for `pg_statio_sys_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysSequences;

impl VirtualRelation for PgStatioSysSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_SEQUENCES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        sequence_rows(ctx, self.schema()?, Scope::Sys)
    }
}

/// Producer for `pg_stat_xact_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactAllTables;

impl VirtualRelation for PgStatXactAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_ALL_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::All, stat_table_kind)
    }
}

/// Producer for `pg_stat_xact_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactUserTables;

impl VirtualRelation for PgStatXactUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_USER_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::User, stat_table_kind)
    }
}

/// Producer for `pg_stat_xact_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactSysTables;

impl VirtualRelation for PgStatXactSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_SYS_TABLES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        table_rows(ctx, self.schema()?, Scope::Sys, stat_table_kind)
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &PgStatAllTables,
    &PgStatUserTables,
    &PgStatSysTables,
    &PgStatAllIndexes,
    &PgStatUserIndexes,
    &PgStatSysIndexes,
    &PgStatioAllTables,
    &PgStatioUserTables,
    &PgStatioSysTables,
    &PgStatioAllIndexes,
    &PgStatioUserIndexes,
    &PgStatioSysIndexes,
    &PgStatioAllSequences,
    &PgStatioUserSequences,
    &PgStatioSysSequences,
    &PgStatXactAllTables,
    &PgStatXactUserTables,
    &PgStatXactSysTables,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use crate::env::CatalogEnv;
    use crate::model::Persistence;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::{IoConfig, TypeId};

    /// `CREATE TABLE <name> (id int4 [, PRIMARY KEY (id)])` in `public`.
    fn table_spec(name: &str, primary_key: bool) -> CreateTableSpec {
        let constraints = if primary_key {
            vec![ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
                columns: vec!["id".to_string()],
                include: Vec::new(),
            })]
        } else {
            Vec::new()
        };
        CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("id", TypeId { oid: 23, typmod: -1 })],
            constraints,
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    fn scan_rows(env: &CatalogEnv, oid: Oid) -> Result<Vec<Row>> {
        let io = IoConfig::default();
        let ctx = ScanContext { env, session: &NullSessionInfo, io_config: &io };
        crate::vrel::scan(&ctx, oid, &ScanRequest::all())
    }

    fn col(oid: Oid, name: &str) -> usize {
        builtin::system_rel(oid)
            .and_then(|r| r.columns.iter().position(|c| c.name == name))
            .expect("column of a statistics view")
    }

    fn text_at(row: &Row, idx: usize) -> &str {
        match &row[idx] {
            Datum::Text(s) => s.as_str(),
            _ => "",
        }
    }

    fn oid_of(env: &CatalogEnv, name: &str) -> Oid {
        env.snapshot().class_in_namespace(PUBLIC_NAMESPACE, name).map(|c| c.oid()).unwrap_or(0)
    }

    fn find_by_oid(rows: &[Row], idx: usize, oid: Oid) -> Option<&Row> {
        rows.iter().find(|r| matches!(r[idx], Datum::Oid(o) if o == oid))
    }

    #[test]
    fn every_view_scans_with_schema_width() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        for r in RELATIONS {
            // `vrel::scan` verifies every row's width against the schema.
            scan_rows(&env, r.oid())?;
        }
        Ok(())
    }

    #[test]
    fn user_and_sys_table_lists_split_by_schema() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        let users = scan_rows(&env, relid::PG_STAT_USER_TABLES)?;
        assert_eq!(users.len(), 1);
        let schema_idx = col(relid::PG_STAT_USER_TABLES, "schemaname");
        let name_idx = col(relid::PG_STAT_USER_TABLES, "relname");
        assert_eq!(text_at(&users[0], schema_idx), "public");
        assert_eq!(text_at(&users[0], name_idx), "t1");

        let sys = scan_rows(&env, relid::PG_STAT_SYS_TABLES)?;
        assert!(!sys.is_empty());
        for row in &sys {
            let s = text_at(row, col(relid::PG_STAT_SYS_TABLES, "schemaname"));
            assert!(s == "pg_catalog" || s == "information_schema", "unexpected sys schema {s}");
        }
        let all = scan_rows(&env, relid::PG_STAT_ALL_TABLES)?;
        assert_eq!(all.len(), sys.len() + users.len());
        Ok(())
    }

    #[test]
    fn user_table_counters_follow_fresh_table_rules() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t2", false)).map(|_| ()))?;
        let rows = scan_rows(&env, relid::PG_STAT_USER_TABLES)?;
        let relid_idx = col(relid::PG_STAT_USER_TABLES, "relid");
        let t1 = oid_of(&env, "t1");
        let t2 = oid_of(&env, "t2");
        assert_ne!(t1, 0);
        assert_ne!(t2, 0);

        let with_pk = find_by_oid(&rows, relid_idx, t1).expect("t1 listed");
        let without_pk = find_by_oid(&rows, relid_idx, t2).expect("t2 listed");
        for row in [with_pk, without_pk] {
            assert!(matches!(row[col(relid::PG_STAT_USER_TABLES, "seq_scan")], Datum::Int8(0)));
            assert!(matches!(row[col(relid::PG_STAT_USER_TABLES, "last_seq_scan")], Datum::Null));
            assert!(matches!(row[col(relid::PG_STAT_USER_TABLES, "n_live_tup")], Datum::Int8(0)));
            assert!(matches!(row[col(relid::PG_STAT_USER_TABLES, "last_vacuum")], Datum::Null));
            assert!(matches!(row[col(relid::PG_STAT_USER_TABLES, "total_vacuum_time")], Datum::Float8(v) if v == 0.0));
        }
        // idx_scan / idx_tup_fetch: 0 when the table has an index, NULL otherwise (PG's sum() over no rows).
        assert!(matches!(with_pk[col(relid::PG_STAT_USER_TABLES, "idx_scan")], Datum::Int8(0)));
        assert!(matches!(with_pk[col(relid::PG_STAT_USER_TABLES, "idx_tup_fetch")], Datum::Int8(0)));
        assert!(matches!(without_pk[col(relid::PG_STAT_USER_TABLES, "idx_scan")], Datum::Null));
        assert!(matches!(without_pk[col(relid::PG_STAT_USER_TABLES, "idx_tup_fetch")], Datum::Null));
        Ok(())
    }

    #[test]
    fn user_indexes_list_each_index_once_with_table_identity() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        let t1 = oid_of(&env, "t1");
        let indexes = env.snapshot().indexes_of(t1);
        assert_eq!(indexes.len(), 1);
        let index_oid = indexes[0].oid;

        let rows = scan_rows(&env, relid::PG_STAT_USER_INDEXES)?;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(matches!(row[col(relid::PG_STAT_USER_INDEXES, "relid")], Datum::Oid(o) if o == t1));
        assert!(matches!(row[col(relid::PG_STAT_USER_INDEXES, "indexrelid")], Datum::Oid(o) if o == index_oid));
        assert_eq!(text_at(row, col(relid::PG_STAT_USER_INDEXES, "schemaname")), "public");
        assert_eq!(text_at(row, col(relid::PG_STAT_USER_INDEXES, "relname")), "t1");
        assert!(matches!(row[col(relid::PG_STAT_USER_INDEXES, "idx_scan")], Datum::Int8(0)));
        assert!(matches!(row[col(relid::PG_STAT_USER_INDEXES, "last_idx_scan")], Datum::Null));

        let io_rows = scan_rows(&env, relid::PG_STATIO_USER_INDEXES)?;
        assert_eq!(io_rows.len(), 1);
        assert!(matches!(io_rows[0][col(relid::PG_STATIO_USER_INDEXES, "idx_blks_read")], Datum::Int8(0)));
        assert!(matches!(io_rows[0][col(relid::PG_STATIO_USER_INDEXES, "idx_blks_hit")], Datum::Int8(0)));
        Ok(())
    }

    #[test]
    fn statio_tables_null_toast_and_system_indexes_are_gap() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        let t1 = oid_of(&env, "t1");
        let rows = scan_rows(&env, relid::PG_STATIO_USER_TABLES)?;
        let row = find_by_oid(&rows, col(relid::PG_STATIO_USER_TABLES, "relid"), t1).expect("t1 listed");
        assert!(matches!(row[col(relid::PG_STATIO_USER_TABLES, "heap_blks_read")], Datum::Int8(0)));
        assert!(matches!(row[col(relid::PG_STATIO_USER_TABLES, "idx_blks_read")], Datum::Int8(0)));
        assert!(matches!(row[col(relid::PG_STATIO_USER_TABLES, "toast_blks_read")], Datum::Null));
        assert!(matches!(row[col(relid::PG_STATIO_USER_TABLES, "tidx_blks_hit")], Datum::Null));
        // System indexes are not linked to tables (documented gap): no sys index rows.
        assert!(scan_rows(&env, relid::PG_STAT_SYS_INDEXES)?.is_empty());
        // No user sequences were created.
        assert!(scan_rows(&env, relid::PG_STATIO_USER_SEQUENCES)?.is_empty());
        Ok(())
    }
}
