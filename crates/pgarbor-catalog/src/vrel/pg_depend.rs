//! `pg_depend`, `pg_description`, `pg_shdescription`, `pg_shdepend`. Lease CAT-32.
//!
//! ## pg_depend
//! One row per `DepEntry` of `DepGraph::build(snapshot)` (`depend.rs` edge table): `classid` =
//! `ObjClass::classid()` of the dependent, `objid`, `objsubid` (= `sub_id`), `refclassid`,
//! `refobjid`, `refobjsubid`, `deptype` = `Char(DepType::as_char)`. Ordered by (classid, objid,
//! objsubid, refclassid, refobjid, refobjsubid). Builtin (pinned) referenced objects never appear
//! (PG shows `deptype` 'p' rows with refobjid 0; PGArbor omits them - documented). Fast paths:
//! eq on `objid` (col 1) and `refobjid` (col 4) (psql `\d` and ORMs filter by them).
//! `ObjClass::AttrDefault` rows use classid 2604 (pg_attrdef), `Rewrite` 2618.
//!
//! ## pg_description
//! One row per description: persisted `snapshot.data().descriptions` UNION builtin descriptions of
//! `builtin::BUILTIN_DESCRIPTIONS` (objsubid 0) minus builtin entries overridden by a persisted
//! description of the same key, EXCLUDING classoid 1260 (pg_authid) and 1262 (pg_database), which
//! belong to `pg_shdescription`. Columns `objoid, classoid, objsubid, description`. Ordered by
//! (objoid, classoid, objsubid). Fast path: eq on `objoid` (col 0).
//!
//! ## pg_shdescription
//! The classoid 1260/1262 descriptions (roles and databases; `COMMENT ON DATABASE postgres` is shown
//! with the bootstrap text `default administrative connection database`): columns `objoid, classoid,
//! description`.
//!
//! ## pg_shdepend
//! `depend::role_dependencies` for every role with at least one dependency: `dbid` = the database
//! OID (5), `classid` = class of the dependent object, `objid`, `objsubid` (column ACLs: attnum, else
//! 0), `refclassid` 1260 (pg_authid), `refobjid` = role OID, `deptype` 'o' (owner) or 'a' (ACL).
//! Owner rows for pinned/builtin roles of builtin objects are omitted. Ordered by
//! (classid, objid, objsubid, refobjid).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel, POSTGRES_DATABASE};
use crate::depend::{self, DepGraph, SharedDepKind};
use crate::model::{class_oid, DescriptionKey};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result, FIRST_NORMAL_OID};
use std::collections::BTreeSet;

/// Producer for `pg_depend`.
#[derive(Clone, Copy, Debug)]
pub struct PgDepend;

impl VirtualRelation for PgDepend {
    fn oid(&self) -> Oid {
        relid::PG_DEPEND
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        depend_rows(ctx.env.snapshot(), req, self.schema()?)
    }
}

/// Producer for `pg_description`.
#[derive(Clone, Copy, Debug)]
pub struct PgDescription;

impl VirtualRelation for PgDescription {
    fn oid(&self) -> Oid {
        relid::PG_DESCRIPTION
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        description_rows(ctx.env.snapshot(), req, self.schema()?)
    }
}

/// Producer for `pg_shdescription`.
#[derive(Clone, Copy, Debug)]
pub struct PgShdescription;

impl VirtualRelation for PgShdescription {
    fn oid(&self) -> Oid {
        relid::PG_SHDESCRIPTION
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        shared_description_rows(ctx.env.snapshot(), req, self.schema()?)
    }
}

/// Producer for `pg_shdepend`.
#[derive(Clone, Copy, Debug)]
pub struct PgShdepend;

impl VirtualRelation for PgShdepend {
    fn oid(&self) -> Oid {
        relid::PG_SHDEPEND
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        shdepend_rows(ctx.env.snapshot(), self.schema()?)
    }
}

/// Equality filter check for an OID column. A filter that is not an OID is ignored (the executor
/// re-applies the WHERE clause, so ignoring a filter is always correct).
fn keep(req: &ScanRequest, idx: usize, value: Oid) -> bool {
    req.oid_eq_on(idx).map_or(true, |want| want == value)
}

/// `pg_shdescription` classes: roles and databases.
fn is_shared_class(classoid: Oid) -> bool {
    classoid == class_oid::PG_AUTHID || classoid == class_oid::PG_DATABASE
}

/// Persisted descriptions plus the builtin ones not overridden by a persisted key, sorted by
/// (objoid, classoid, objsubid).
fn merged_descriptions(snapshot: &CatalogSnapshot) -> Vec<(DescriptionKey, &str)> {
    let persisted = &snapshot.data().descriptions;
    let mut out: Vec<(DescriptionKey, &str)> = Vec::with_capacity(persisted.len() + builtin::BUILTIN_DESCRIPTIONS.len());
    out.extend(persisted.iter().map(|(k, v)| (*k, v.as_str())));
    for &(classoid, objoid, text) in builtin::BUILTIN_DESCRIPTIONS {
        let key = DescriptionKey { objoid, classoid, objsubid: 0 };
        if !persisted.contains_key(&key) {
            out.push((key, text));
        }
    }
    out.sort_by_key(|(k, _)| (k.objoid, k.classoid, k.objsubid));
    out
}

fn depend_rows(snapshot: &CatalogSnapshot, req: &ScanRequest, schema: &'static SystemRel) -> Result<Vec<Row>> {
    let graph = DepGraph::build(snapshot)?;
    let mut edges: Vec<_> = graph
        .entries()
        .iter()
        .filter(|e| !e.referenced.is_pinned())
        .filter(|e| keep(req, 1, e.dependent.oid) && keep(req, 4, e.referenced.oid))
        .collect();
    edges.sort_by_key(|e| {
        (
            e.dependent.class.classid(),
            e.dependent.oid,
            e.dependent.sub_id,
            e.referenced.class.classid(),
            e.referenced.oid,
            e.referenced.sub_id,
            e.deptype.as_char(),
        )
    });
    let mut rows = Vec::with_capacity(edges.len());
    for e in edges {
        let mut b = RowBuilder::new(schema);
        b.set("classid", d::oid(e.dependent.class.classid()))
            .set("objid", d::oid(e.dependent.oid))
            .set("objsubid", d::int4(e.dependent.sub_id))
            .set("refclassid", d::oid(e.referenced.class.classid()))
            .set("refobjid", d::oid(e.referenced.oid))
            .set("refobjsubid", d::int4(e.referenced.sub_id))
            .set("deptype", d::char_(e.deptype.as_char()));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

fn description_rows(snapshot: &CatalogSnapshot, req: &ScanRequest, schema: &'static SystemRel) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for (key, text) in merged_descriptions(snapshot) {
        if is_shared_class(key.classoid) || !keep(req, 0, key.objoid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("objoid", d::oid(key.objoid))
            .set("classoid", d::oid(key.classoid))
            .set("objsubid", d::int4(key.objsubid))
            .set("description", d::text(text));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

fn shared_description_rows(snapshot: &CatalogSnapshot, req: &ScanRequest, schema: &'static SystemRel) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for (key, text) in merged_descriptions(snapshot) {
        if !is_shared_class(key.classoid) || !keep(req, 0, key.objoid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("objoid", d::oid(key.objoid))
            .set("classoid", d::oid(key.classoid))
            .set("description", d::text(text));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

fn shdepend_rows(snapshot: &CatalogSnapshot, schema: &'static SystemRel) -> Result<Vec<Row>> {
    // Persisted roles plus every builtin role (a builtin role can be a grantee in a user ACL).
    let mut roles: BTreeSet<Oid> = snapshot.data().roles.keys().copied().collect();
    roles.extend(builtin::ROLES.iter().map(|r| r.oid));

    // (classid, objid, objsubid, refobjid, deptype)
    let mut found: Vec<(Oid, Oid, i32, Oid, u8)> = Vec::new();
    for role in roles {
        for dep in depend::role_dependencies(snapshot, role)? {
            if dep.object.is_pinned() {
                continue;
            }
            let (objsubid, deptype) = match dep.kind {
                SharedDepKind::Owner => {
                    // Pinned (bootstrap / predefined) owners are never recorded.
                    if role < FIRST_NORMAL_OID {
                        continue;
                    }
                    (0, b'o')
                }
                SharedDepKind::Acl => (dep.object.sub_id, b'a'),
            };
            found.push((dep.object.class.classid(), dep.object.oid, objsubid, role, deptype));
        }
    }
    found.sort();

    let mut rows = Vec::with_capacity(found.len());
    for (classid, objid, objsubid, refobjid, deptype) in found {
        let mut b = RowBuilder::new(schema);
        b.set("dbid", d::oid(POSTGRES_DATABASE))
            .set("classid", d::oid(classid))
            .set("objid", d::oid(objid))
            .set("objsubid", d::int4(objsubid))
            .set("refclassid", d::oid(class_oid::PG_AUTHID))
            .set("refobjid", d::oid(refobjid))
            .set("deptype", d::char_(deptype));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgDepend, &PgDescription, &PgShdescription, &PgShdepend];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::PUBLIC_NAMESPACE;
    use crate::data::CatalogData;
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec, SerialKind};
    use crate::model::{Persistence, RelKind};
    use crate::persist::CatalogVersion;
    use crate::testing;
    use pgarbor_types::datum::{Datum, TypeId};

    fn oid_at(row: &Row, i: usize) -> Oid {
        match &row[i] {
            Datum::Oid(o) => *o,
            _ => panic!("expected an oid column"),
        }
    }

    fn int4_at(row: &Row, i: usize) -> i32 {
        match &row[i] {
            Datum::Int4(v) => *v,
            _ => panic!("expected an int4 column"),
        }
    }

    fn char_at(row: &Row, i: usize) -> u8 {
        match &row[i] {
            Datum::Char(c) => *c,
            _ => panic!("expected a char column"),
        }
    }

    fn text_at(row: &Row, i: usize) -> String {
        match &row[i] {
            Datum::Text(s) => s.clone(),
            _ => panic!("expected a text column"),
        }
    }

    #[test]
    fn serial_table_pg_depend_shape() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: "t".to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec { serial: Some(SerialKind::Int), ..ColumnSpec::new("id", TypeId::INT4) }],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        };
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        let snap = env.snapshot();
        let table = snap.class_in_namespace(PUBLIC_NAMESPACE, "t").expect("table t").oid();
        let seq = *snap.data().sequences.keys().next().expect("serial sequence");

        let rows = depend_rows(snap, &ScanRequest::all(), PgDepend.schema()?)?;

        // Rows are ordered by (classid, objid, objsubid, refclassid, refobjid, refobjsubid).
        let keys: Vec<_> = rows
            .iter()
            .map(|r| (oid_at(r, 0), oid_at(r, 1), int4_at(r, 2), oid_at(r, 3), oid_at(r, 4), int4_at(r, 5)))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]));

        // The serial sequence depends on column 1 of the table (Auto).
        let seq_row = rows
            .iter()
            .find(|r| oid_at(r, 0) == class_oid::PG_CLASS && oid_at(r, 1) == seq)
            .expect("sequence depend row");
        assert_eq!(oid_at(seq_row, 3), class_oid::PG_CLASS);
        assert_eq!(oid_at(seq_row, 4), table);
        assert_eq!(int4_at(seq_row, 5), 1);
        assert_eq!(char_at(seq_row, 6), b'a');

        // The table depends on its namespace (Normal).
        let ns_row = rows
            .iter()
            .find(|r| oid_at(r, 1) == table && oid_at(r, 3) == class_oid::PG_NAMESPACE)
            .expect("namespace depend row");
        assert_eq!(oid_at(ns_row, 4), PUBLIC_NAMESPACE);
        assert_eq!(char_at(ns_row, 6), b'n');

        // refobjid filter keeps only matching rows.
        let filtered = depend_rows(snap, &ScanRequest { eq: vec![(4, Datum::Oid(table))] }, PgDepend.schema()?)?;
        assert!(filtered.iter().all(|r| oid_at(r, 4) == table));
        assert!(filtered.len() < rows.len());
        Ok(())
    }

    #[test]
    fn description_persisted_overrides_builtin_and_shared_split() -> Result<()> {
        let (c1, o1, _s1) = builtin::BUILTIN_DESCRIPTIONS[0];
        let (c2, o2, s2) = builtin::BUILTIN_DESCRIPTIONS[1];
        let mut data = CatalogData::default();
        data.descriptions.insert(DescriptionKey { objoid: o1, classoid: c1, objsubid: 0 }, "custom".to_string());
        data.descriptions.insert(
            DescriptionKey { objoid: POSTGRES_DATABASE, classoid: class_oid::PG_DATABASE, objsubid: 0 },
            "my db".to_string(),
        );
        let snap = CatalogSnapshot::build(CatalogVersion::ZERO, data)?;

        let rows = description_rows(&snap, &ScanRequest::all(), PgDescription.schema()?)?;
        // The persisted text overrides the builtin one.
        let hits1: Vec<&Row> = rows.iter().filter(|r| oid_at(r, 0) == o1 && oid_at(r, 1) == c1).collect();
        assert_eq!(hits1.len(), 1);
        assert_eq!(text_at(hits1[0], 3), "custom");
        // Untouched builtin rows are still served.
        let hits2: Vec<&Row> = rows.iter().filter(|r| oid_at(r, 0) == o2 && oid_at(r, 1) == c2).collect();
        assert_eq!(hits2.len(), 1);
        assert_eq!(text_at(hits2[0], 3), s2);
        // Role and database comments belong to pg_shdescription only.
        assert!(rows.iter().all(|r| !is_shared_class(oid_at(r, 1))));

        let shared = shared_description_rows(&snap, &ScanRequest::all(), PgShdescription.schema()?)?;
        assert_eq!(shared.len(), 1);
        assert_eq!(oid_at(&shared[0], 0), POSTGRES_DATABASE);
        assert_eq!(oid_at(&shared[0], 1), class_oid::PG_DATABASE);
        assert_eq!(text_at(&shared[0], 2), "my db");
        Ok(())
    }
}
