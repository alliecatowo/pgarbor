//! `pg_index` and `pg_constraint`. Lease CAT-31. PG sources: `catalog/pg_index.h`,
//! `pg_constraint.h`. Oracle-checked values.
//!
//! ## pg_index (user indexes only; builtin system indexes are not described by `pg_index` - documented gap)
//! One row per `Index`, ordered by `indexrelid`. Fast paths: eq on `indexrelid` (col 0) and
//! `indrelid` (col 1).
//! | column | value |
//! |---|---|
//! | indexrelid, indrelid | `oid`, `table` |
//! | indnatts / indnkeyatts | `keys.len()` / `n_key_columns` |
//! | indisunique, indnullsnotdistinct, indisprimary, indisexclusion, indimmediate, indisclustered, indisvalid | model flags |
//! | indcheckxmin | false |
//! | indisready / indislive | `valid` / true |
//! | indisreplident | `replident` |
//! | indkey | `d::int2vector(&index.indkey())` (0 for expression keys) |
//! | indcollation | `d::oidvector` of `IndexKey::collation` (0 for non-collatable; 100 for text) |
//! | indclass | `d::oidvector` of `IndexKey::opclass` (int4 1978, text 3126, ...) |
//! | indoption | `d::int2vector` of `IndexKey::indoption()` (DESC = 1, NULLS FIRST = 2) |
//! | indexprs | the expression texts (`StoredExpr::sql`) of expression keys joined with `, `, NULL if none (PG: a node-tree list; `pg_get_expr` returns the text) |
//! | indpred | predicate text or NULL |
//!
//! ## pg_constraint
//! One row per `Constraint` (table and domain constraints), ordered by `oid`. Fast paths: eq on
//! `oid` (0), `conrelid` (8), `contypid` (9), `conname` (1).
//! | column | value |
//! |---|---|
//! | oid, conname, connamespace | fields |
//! | contype | `ConstraintKind::as_char` |
//! | condeferrable, condeferred | fields |
//! | conenforced, convalidated | fields |
//! | conrelid, contypid, conindid, conparentid | `table`, `domain`, `index`, `parent` |
//! | confrelid | `fk.ref_table` or 0 |
//! | confupdtype, confdeltype, confmatchtype | FK: `FkAction::as_char` x2 and `FkMatch::as_char`; every OTHER constraint: `Char(b' ')` (a space - oracle) |
//! | conislocal, coninhcount | fields |
//! | connoinherit | `no_inherit` OR kind in {PrimaryKey, Unique, ForeignKey, Exclusion} (oracle: PK/UNIQUE/FK rows show `t`, CHECK and NOT NULL `f`) |
//! | conperiod | field |
//! | conkey | `d::int2_array(columns)`; NULL when `columns` is empty (domain constraints, constraints without columns) |
//! | confkey | FK `ref_columns` else NULL |
//! | conpfeqop, conppeqop, conffeqop | FK `pf_eq_ops`, `pp_eq_ops`, `ff_eq_ops` as `oid[]`, NULL when empty or not FK |
//! | confdelsetcols | `delete_set_columns` as `smallint[]` or NULL |
//! | conexclop | NULL |
//! | conbin | CHECK expression text (`check.sql`) or NULL |
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{relid, SystemRel};
use crate::model::{Constraint, ConstraintKind, Index};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};
use pgarbor_types::Datum;

/// Producer for `pg_index`.
#[derive(Clone, Copy, Debug)]
pub struct PgIndex;

impl VirtualRelation for PgIndex {
    fn oid(&self) -> Oid {
        relid::PG_INDEX
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let rel = self.schema()?;
        index_rows(ctx.env.snapshot(), rel, req)
    }
}

/// Producer for `pg_constraint`.
#[derive(Clone, Copy, Debug)]
pub struct PgConstraint;

impl VirtualRelation for PgConstraint {
    fn oid(&self) -> Oid {
        relid::PG_CONSTRAINT
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let rel = self.schema()?;
        constraint_rows(ctx.env.snapshot(), rel, req)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgIndex, &PgConstraint];

/// `NULL` for an empty list, otherwise the array built by `f`.
fn nonempty<T>(v: &[T], f: impl FnOnce(&[T]) -> Datum) -> Datum {
    if v.is_empty() {
        Datum::Null
    } else {
        f(v)
    }
}

/// All `pg_index` rows, ordered by `indexrelid`.
fn index_rows(snap: &CatalogSnapshot, rel: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let want_id = req.oid_eq_on(0);
    let want_table = req.oid_eq_on(1);
    let mut candidates: Vec<&Index> = match want_table {
        Some(t) => snap.indexes_of(t),
        None => snap.data().indexes.values().collect(),
    };
    candidates.sort_by_key(|i| i.oid);
    let mut rows = Vec::with_capacity(candidates.len());
    for idx in candidates {
        if want_id.is_some_and(|o| o != idx.oid) || want_table.is_some_and(|t| t != idx.table) {
            continue;
        }
        rows.push(index_row(rel, idx)?);
    }
    Ok(rows)
}

fn index_row(rel: &'static SystemRel, idx: &Index) -> Result<Row> {
    let keys = &idx.keys;
    let exprs: Vec<&str> = keys.iter().filter_map(|k| k.expr.as_ref().map(|e| e.sql.as_str())).collect();
    let exprs_text = exprs.join(", ");
    let collations: Vec<Oid> = keys.iter().map(|k| k.collation).collect();
    let classes: Vec<Oid> = keys.iter().map(|k| k.opclass).collect();
    let options: Vec<i16> = keys.iter().map(|k| k.indoption()).collect();

    let mut b = RowBuilder::new(rel);
    b.set("indexrelid", d::oid(idx.oid))
        .set("indrelid", d::oid(idx.table))
        .set("indnatts", d::int2(keys.len() as i16))
        .set("indnkeyatts", d::int2(idx.n_key_columns as i16))
        .set("indisunique", d::bool_(idx.unique))
        .set("indnullsnotdistinct", d::bool_(idx.nulls_not_distinct))
        .set("indisprimary", d::bool_(idx.primary))
        .set("indisexclusion", d::bool_(idx.exclusion))
        .set("indimmediate", d::bool_(idx.immediate))
        .set("indisclustered", d::bool_(idx.clustered))
        .set("indisvalid", d::bool_(idx.valid))
        .set("indcheckxmin", d::bool_(false))
        .set("indisready", d::bool_(idx.valid))
        .set("indislive", d::bool_(true))
        .set("indisreplident", d::bool_(idx.replident))
        .set("indkey", d::int2vector(&idx.indkey()))
        .set("indcollation", d::oidvector(&collations))
        .set("indclass", d::oidvector(&classes))
        .set("indoption", d::int2vector(&options))
        .set("indexprs", d::opt_text(if exprs.is_empty() { None } else { Some(exprs_text.as_str()) }))
        .set("indpred", d::opt_text(idx.predicate.as_ref().map(|e| e.sql.as_str())));
    b.finish()
}

/// All `pg_constraint` rows, ordered by `oid`.
fn constraint_rows(snap: &CatalogSnapshot, rel: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let want_oid = req.oid_eq_on(0);
    let want_rel = req.oid_eq_on(8);
    let want_type = req.oid_eq_on(9);
    let want_name: Option<&str> = match req.eq_on(1) {
        Some(Datum::Text(s)) => Some(s.as_str()),
        _ => None,
    };
    let mut candidates: Vec<&Constraint> = if let Some(o) = want_oid {
        snap.constraint(o).into_iter().collect()
    } else if let Some(t) = want_rel {
        snap.constraints_of(t)
    } else if let Some(t) = want_type {
        snap.domain_constraints(t)
    } else {
        snap.data().constraints.values().collect()
    };
    candidates.sort_by_key(|c| c.oid);
    let mut rows = Vec::with_capacity(candidates.len());
    for c in candidates {
        if want_oid.is_some_and(|o| o != c.oid)
            || want_rel.is_some_and(|t| t != c.table)
            || want_type.is_some_and(|t| t != c.domain)
            || want_name.is_some_and(|n| n != c.name.as_str())
        {
            continue;
        }
        rows.push(constraint_row(rel, c)?);
    }
    Ok(rows)
}

fn constraint_row(rel: &'static SystemRel, c: &Constraint) -> Result<Row> {
    let fk = c.fk.as_ref();
    let (upd, del, mat) = match fk {
        Some(f) => (f.on_update.as_char(), f.on_delete.as_char(), f.match_type.as_char()),
        None => (b' ', b' ', b' '),
    };
    let no_inherit = c.no_inherit
        || matches!(
            c.kind,
            ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::ForeignKey | ConstraintKind::Exclusion
        );
    let confkey = fk.map_or(Datum::Null, |f| nonempty(f.ref_columns.as_slice(), d::int2_array));
    let conpfeqop = fk.map_or(Datum::Null, |f| nonempty(f.pf_eq_ops.as_slice(), d::oid_array));
    let conppeqop = fk.map_or(Datum::Null, |f| nonempty(f.pp_eq_ops.as_slice(), d::oid_array));
    let conffeqop = fk.map_or(Datum::Null, |f| nonempty(f.ff_eq_ops.as_slice(), d::oid_array));
    let confdelsetcols = fk.map_or(Datum::Null, |f| nonempty(f.delete_set_columns.as_slice(), d::int2_array));

    let mut b = RowBuilder::new(rel);
    b.set("oid", d::oid(c.oid))
        .set("conname", d::name(&c.name))
        .set("connamespace", d::oid(c.namespace))
        .set("contype", d::char_(c.kind.as_char()))
        .set("condeferrable", d::bool_(c.deferrable))
        .set("condeferred", d::bool_(c.initially_deferred))
        .set("conenforced", d::bool_(c.enforced))
        .set("convalidated", d::bool_(c.validated))
        .set("conrelid", d::oid(c.table))
        .set("contypid", d::oid(c.domain))
        .set("conindid", d::oid(c.index))
        .set("conparentid", d::oid(c.parent))
        .set("confrelid", d::oid(fk.map_or(0, |f| f.ref_table)))
        .set("confupdtype", d::char_(upd))
        .set("confdeltype", d::char_(del))
        .set("confmatchtype", d::char_(mat))
        .set("conislocal", d::bool_(c.is_local))
        .set("coninhcount", d::int2(c.inh_count))
        .set("connoinherit", d::bool_(no_inherit))
        .set("conperiod", d::bool_(c.period))
        .set("conkey", nonempty(c.columns.as_slice(), d::int2_array))
        .set("confkey", confkey)
        .set("conpfeqop", conpfeqop)
        .set("conppeqop", conppeqop)
        .set("conffeqop", conffeqop)
        .set("confdelsetcols", confdelsetcols)
        .set("conexclop", Datum::Null)
        .set("conbin", d::opt_text(c.check.as_ref().map(|e| e.sql.as_str())));
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use crate::model::{Persistence, RelKind};
    use crate::snapshot::ClassRef;
    use crate::testing;
    use pgarbor_types::TypeId;

    fn col(rel: &SystemRel, row: &Row, name: &str) -> Datum {
        let i = rel.columns.iter().position(|c| c.name == name).expect("column exists");
        row[i].clone()
    }

    /// `create table t (id int4 primary key)` in `public`; returns the snapshot after the statement
    /// and the table OID.
    fn create_pk_table() -> Result<(CatalogSnapshot, Oid)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ns = env.snapshot().namespace_by_name("public").expect("public schema").oid;
        let spec = CreateTableSpec {
            namespace: ns,
            name: "t".to_string(),
            owner: env.current_user(),
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("id", TypeId::INT4)],
            constraints: vec![ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
                columns: vec!["id".to_string()],
                include: vec![],
            })],
            options: vec![],
            if_not_exists: false,
            inherits: vec![],
        };
        let (res, _next) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        let table = match res.snapshot.class_in_namespace(ns, "t") {
            Some(ClassRef::Relation(o)) => o,
            _ => panic!("table t was not created"),
        };
        Ok((res.snapshot, table))
    }

    #[test]
    fn nonempty_is_null_for_empty() {
        assert_eq!(nonempty(&[] as &[i16], d::int2_array), Datum::Null);
        assert_eq!(nonempty(&[1i16], d::int2_array), d::int2_array(&[1]));
    }

    #[test]
    fn pk_index_row_shape() -> Result<()> {
        let (snap, table) = create_pk_table()?;
        let rel = PgIndex.schema()?;
        let rows = index_rows(&snap, rel, &ScanRequest { eq: vec![(1, Datum::Oid(table))] })?;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(col(rel, row, "indnatts"), d::int2(1));
        assert_eq!(col(rel, row, "indnkeyatts"), d::int2(1));
        assert_eq!(col(rel, row, "indisunique"), d::bool_(true));
        assert_eq!(col(rel, row, "indisprimary"), d::bool_(true));
        assert_eq!(col(rel, row, "indkey"), d::int2vector(&[1]));
        assert_eq!(col(rel, row, "indclass"), d::oidvector(&[1978]));
        assert_eq!(col(rel, row, "indcollation"), d::oidvector(&[0]));
        assert_eq!(col(rel, row, "indoption"), d::int2vector(&[0]));
        assert_eq!(col(rel, row, "indexprs"), Datum::Null);
        assert_eq!(col(rel, row, "indpred"), Datum::Null);
        Ok(())
    }

    #[test]
    fn pk_constraint_and_not_null_rows() -> Result<()> {
        let (snap, table) = create_pk_table()?;
        let rel = PgConstraint.schema()?;
        let rows = constraint_rows(&snap, rel, &ScanRequest { eq: vec![(8, Datum::Oid(table))] })?;
        let pk_index = snap.primary_key(table).expect("primary key").index;

        let pk = rows.iter().find(|r| col(rel, r, "contype") == Datum::Char(b'p')).expect("pk row");
        assert_eq!(col(rel, pk, "conkey"), d::int2_array(&[1]));
        assert_eq!(col(rel, pk, "connoinherit"), d::bool_(true));
        assert_eq!(col(rel, pk, "conindid"), d::oid(pk_index));
        assert_eq!(col(rel, pk, "confupdtype"), Datum::Char(b' '));
        assert_eq!(col(rel, pk, "confkey"), Datum::Null);
        assert_eq!(col(rel, pk, "conbin"), Datum::Null);

        let nn = rows.iter().find(|r| col(rel, r, "contype") == Datum::Char(b'n')).expect("not null row");
        assert_eq!(col(rel, nn, "conkey"), d::int2_array(&[1]));
        assert_eq!(col(rel, nn, "connoinherit"), d::bool_(false));
        assert_eq!(col(rel, nn, "confmatchtype"), Datum::Char(b' '));
        Ok(())
    }

    #[test]
    fn constraint_oid_filter_returns_only_that_row() -> Result<()> {
        let (snap, table) = create_pk_table()?;
        let rel = PgConstraint.schema()?;
        let all = constraint_rows(&snap, rel, &ScanRequest::all())?;
        let first_oid = match col(rel, &all[0], "oid") {
            Datum::Oid(o) => o,
            other => panic!("unexpected oid datum {other:?}"),
        };
        let one = constraint_rows(&snap, rel, &ScanRequest { eq: vec![(0, Datum::Oid(first_oid))] })?;
        assert_eq!(one.len(), 1);
        assert!(!all.is_empty());
        let _ = table;
        Ok(())
    }
}
