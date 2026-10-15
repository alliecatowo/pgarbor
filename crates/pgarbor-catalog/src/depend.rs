//! Dependency graph DERIVED from the catalog model (the equivalent of `pg_depend` + `pg_shdepend`).
//! Lease CAT-23 (sonnet). PGArbor does NOT persist dependency rows: every edge is a function of the
//! stored objects, so rows can never go stale across branches, rollbacks or schema diffs, and
//! `pg_depend` (a virtual relation) is generated from the same function.
//! PG sources: `catalog/dependency.c` (`recordDependencyOn*`, deptype semantics),
//! `catalog/heap.c` (`heap_create_with_catalog`, `StoreAttrDefault`, `StoreRelCheck`),
//! `catalog/index.c` (`index_create`, `index_constraint_create`), `catalog/pg_constraint.c`
//! (`CreateConstraintEntry`), `commands/sequence.c` (`process_owned_by`), `catalog/pg_type.c`,
//! `catalog/pg_proc.c`, `commands/view.c`.
//!
//! ## Edge table (oracle-verified against PG 18.6 `pg_depend` for the objects listed)
//! "X -> Y (t)" means dependent X references Y with deptype t. `col(R,n)` = `ObjectAddress::column`.
//! Skip any edge whose REFERENCED object `is_pinned()` (builtin types, collations, languages,
//! opclasses ...) - PG records them as pinned and never reports them.
//!
//! | dependent | referenced | type |
//! |---|---|---|
//! | Relation R (table/view/matview/composite/sequence) | its namespace | Normal |
//! | rowtype `Type(R.rowtype)` | Relation R | Internal |
//! | array type of a rowtype / of a `TypeObj` | the element type | Internal |
//! | TypeObj (enum/domain) | its namespace | Normal |
//! | domain | base type (if not pinned) | Normal; default expr `depends_on` Normal |
//! | domain Constraint | the domain Type | Auto; its check expr `depends_on` Normal |
//! | column `col(R,n)` | its declared type (non-pinned, incl. via arrays: element type) | Normal |
//! | AttrDefault `{AttrDefault, default.oid}` | `col(R,n)` | Auto (plain default); Internal (generated column) |
//! | AttrDefault | every `expr.depends_on` entry | Normal |
//! | Sequence S with `owned_by = (R,n)`, `identity = false` (serial) | `col(R,n)` | Auto |
//! | Sequence S with `identity = true` | `col(R,n)` | Internal |
//! | Index I (not constraint-owned) | `col(T,k)` for each plain key column (incl. INCLUDE) | Auto; expression/predicate `depends_on` on columns of T -> `col` Auto, other objects Normal; an index with no column reference at all -> whole table T Auto |
//! | Index I with `constraint = Some(C)` | Constraint C | Internal (and NO edge to the table) |
//! | Constraint PK/UNIQUE/NOT NULL of table T | `col(T,k)` for each `columns` entry | Auto |
//! | Constraint CHECK of T | `col(T,k)` for each `columns` entry | Auto; PLUS each `check.depends_on` entry | Normal (so a column referenced by a CHECK has BOTH an Auto and a Normal edge - oracle `t_c_check`) |
//! | Constraint FOREIGN KEY of T | `col(T,k)` referencing columns | Auto; `col(RefT,j)` referenced columns | Normal; referenced index (`fk.ref_index`) | Normal |
//! | View/matview V | every `view.query.depends_on` entry | Normal |
//! | Function F | namespace; every argument/return/default type (non-pinned); `depends_on`; default exprs' `depends_on` | Normal |
//! Subtle (replicate): a Normal edge from a view to a COLUMN of a table is reported as "view v
//! depends on column a of table t"; a view over `SELECT *` lists every column used.
//!
//! ## Rules the planner (`drop.rs`) applies (documented here because they define the types)
//! - Auto: dependent is dropped silently with the referenced object (any deptype Auto/Internal edge
//!   on a dependent makes it "silent" even if it also has a Normal edge).
//! - Internal: dependent is part of the referenced object's implementation: dropped with it
//!   silently; dropping the dependent ALONE is refused (`errors::required_by`).
//! - Normal: RESTRICT refuses; CASCADE drops the dependent and reports it.
//!
//! ## Shared (role) dependencies - PG `pg_shdepend`
//! [`role_dependencies`]: for DROP ROLE / REASSIGN OWNED / DROP OWNED: every object whose
//! `owner` is the role (kind `Owner`), and every object whose ACL (or column ACL) mentions the role
//! as grantee or grantor (kind `Acl`).
use crate::env::CatalogEnv;
use crate::model::*;
use crate::snapshot::{CatalogSnapshot, UserTypeRef};
use pgarbor_common::{Oid, Result};
use std::collections::HashMap;

/// One dependency edge (a `pg_depend` row).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DepEntry {
    pub dependent: ObjectAddress,
    pub referenced: ObjectAddress,
    pub deptype: DepType,
}

/// All edges of one snapshot with both directions indexed.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    pub(crate) entries: Vec<DepEntry>,
    /// referenced address (exact, including `sub_id`) -> indexes into `entries`.
    pub(crate) by_referenced: HashMap<ObjectAddress, Vec<usize>>,
    /// dependent address (exact) -> indexes into `entries`.
    pub(crate) by_dependent: HashMap<ObjectAddress, Vec<usize>>,
}

impl DepGraph {
    /// Derive every edge from `snapshot` (edge table above). Deterministic order: sorted by
    /// (dependent, referenced, deptype) so `pg_depend` output is stable.
    // CONTRACT
    pub fn build(snapshot: &CatalogSnapshot) -> Result<DepGraph> {
        let mut out: Vec<DepEntry> = Vec::new();
        let data = snapshot.data();
        for r in data.relations.values() {
            derive_relation(snapshot, r, &mut out);
        }
        for s in data.sequences.values() {
            derive_sequence(s, &mut out);
        }
        for i in data.indexes.values() {
            derive_index(i, &mut out);
        }
        for c in data.constraints.values() {
            derive_constraint(c, &mut out);
        }
        for t in data.types.values() {
            derive_type(snapshot, t, &mut out);
        }
        for f in data.functions.values() {
            derive_function(snapshot, f, &mut out);
        }
        Ok(DepGraph::from_entries(out))
    }

    /// Build from an explicit edge list (tests, and `build` after deriving).
    pub fn from_entries(mut entries: Vec<DepEntry>) -> DepGraph {
        entries.sort_by(|a, b| {
            (a.dependent, a.referenced, a.deptype.as_char()).cmp(&(b.dependent, b.referenced, b.deptype.as_char()))
        });
        entries.dedup();
        let mut by_referenced: HashMap<ObjectAddress, Vec<usize>> = HashMap::new();
        let mut by_dependent: HashMap<ObjectAddress, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_referenced.entry(e.referenced).or_default().push(i);
            by_dependent.entry(e.dependent).or_default().push(i);
        }
        DepGraph { entries, by_referenced, by_dependent }
    }

    /// Every edge.
    pub fn entries(&self) -> &[DepEntry] {
        &self.entries
    }

    /// Edges whose REFERENCED side is exactly `addr` (same `sub_id`).
    pub fn dependents_of(&self, addr: &ObjectAddress) -> Vec<&DepEntry> {
        self.by_referenced.get(addr).map(|v| v.iter().map(|i| &self.entries[*i]).collect()).unwrap_or_default()
    }

    /// Edges whose DEPENDENT side is exactly `addr`.
    pub fn references_of(&self, addr: &ObjectAddress) -> Vec<&DepEntry> {
        self.by_dependent.get(addr).map(|v| v.iter().map(|i| &self.entries[*i]).collect()).unwrap_or_default()
    }

    /// Edges referencing the WHOLE object `addr.whole()` or any column of it (what dropping the
    /// object must consider): for a relation also `col(R,n)` references.
    pub fn dependents_of_object_and_columns(&self, snapshot: &CatalogSnapshot, addr: &ObjectAddress) -> Vec<&DepEntry> {
        let mut out = self.dependents_of(&addr.whole());
        if addr.class == ObjClass::Relation && addr.sub_id == 0 {
            if let Some(r) = snapshot.relation(addr.oid) {
                for c in &r.columns {
                    out.extend(self.dependents_of(&ObjectAddress::column(addr.oid, c.attnum)));
                }
            }
        }
        out
    }
}

/// What kind of shared (role) dependency.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedDepKind {
    /// The role owns the object (`pg_shdepend` 'o').
    Owner,
    /// The role appears in the object's ACL as grantee or grantor ('a').
    Acl,
}

/// One shared dependency of a role.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedDep {
    pub object: ObjectAddress,
    pub kind: SharedDepKind,
}

/// Objects owned by / granting-or-granted-to `role`, sorted by (class, oid, sub_id) with owner deps
/// first. Includes schemas, relations (not their indexes: PG tracks indexes via the table? it does
/// track each), sequences, types, functions, databases. Used by DROP ROLE (2BP01 detail lines
/// `owner of table t`, `privileges for schema s`), REASSIGN OWNED, DROP OWNED.
// CONTRACT
pub fn role_dependencies(snapshot: &CatalogSnapshot, role: Oid) -> Result<Vec<SharedDep>> {
    let mut out: Vec<SharedDep> = Vec::new();
    let data = snapshot.data();

    let mut note = |addr: ObjectAddress, owner: Option<Oid>, acl: Option<&Acl>| {
        if addr.is_pinned() {
            return;
        }
        if owner == Some(role) {
            out.push(SharedDep { object: addr, kind: SharedDepKind::Owner });
        }
        // PG never records an ACL dependency for the owner itself (pg_shdepend.c).
        if owner != Some(role) {
            if let Some(acl) = acl {
                if acl.iter().any(|a| a.grantee == role || a.grantor == role) {
                    out.push(SharedDep { object: addr, kind: SharedDepKind::Acl });
                }
            }
        }
    };

    for n in snapshot.namespaces() {
        note(ObjectAddress::namespace(n.oid), Some(n.owner), n.acl.as_ref());
    }
    for r in data.relations.values() {
        note(ObjectAddress::relation(r.oid), Some(r.owner), r.acl.as_ref());
        for c in &r.columns {
            if !c.dropped {
                note(ObjectAddress::column(r.oid, c.attnum), None, c.acl.as_ref());
            }
        }
    }
    for i in data.indexes.values() {
        note(ObjectAddress::relation(i.oid), Some(i.owner), None);
    }
    for s in data.sequences.values() {
        note(ObjectAddress::relation(s.oid), Some(s.owner), s.acl.as_ref());
    }
    for t in data.types.values() {
        note(ObjectAddress::type_(t.oid), Some(t.owner), t.acl.as_ref());
    }
    for f in data.functions.values() {
        note(ObjectAddress::proc(f.oid), Some(f.owner), f.acl.as_ref());
    }
    for d in data.databases.values() {
        note(ObjectAddress::new(ObjClass::Database, d.oid), Some(d.owner), d.acl.as_ref());
    }

    out.sort_by_key(|d| (d.kind != SharedDepKind::Owner, d.object));
    out.dedup();
    Ok(out)
}

// ------------------------------------------------------------------------------------------------
// Derivation helpers
// ------------------------------------------------------------------------------------------------

/// Record `dependent -> referenced (deptype)` unless the referenced object is pinned or the edge is
/// a self-reference.
fn push(out: &mut Vec<DepEntry>, dependent: ObjectAddress, referenced: ObjectAddress, deptype: DepType) {
    if referenced.is_pinned() || dependent == referenced {
        return;
    }
    out.push(DepEntry { dependent, referenced, deptype });
}

/// The pg_type row a declared type OID depends on: array types of user types and of rowtypes
/// resolve to their element type; everything else is the type itself.
fn type_addr(snapshot: &CatalogSnapshot, oid: Oid) -> ObjectAddress {
    match snapshot.user_type(oid) {
        Some(UserTypeRef::TypeArray(elem)) => ObjectAddress::type_(elem),
        Some(UserTypeRef::RowTypeArray(rel)) => match snapshot.relation(rel) {
            Some(r) if r.rowtype != 0 => ObjectAddress::type_(r.rowtype),
            _ => ObjectAddress::type_(oid),
        },
        _ => ObjectAddress::type_(oid),
    }
}

/// Every `depends_on` entry of `expr` as a Normal edge from `dependent`.
fn push_expr_normal(out: &mut Vec<DepEntry>, dependent: ObjectAddress, expr: &StoredExpr) {
    for d in &expr.depends_on {
        push(out, dependent, *d, DepType::Normal);
    }
}

fn derive_relation(snapshot: &CatalogSnapshot, r: &Relation, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::relation(r.oid);
    push(out, me, ObjectAddress::namespace(r.namespace), DepType::Normal);

    if r.rowtype != 0 && r.kind.has_rowtype() {
        let rt = ObjectAddress::type_(r.rowtype);
        push(out, rt, me, DepType::Internal);
        if r.array_type != 0 {
            push(out, ObjectAddress::type_(r.array_type), rt, DepType::Internal);
        }
    }

    for c in &r.columns {
        if c.dropped {
            continue;
        }
        let col = ObjectAddress::column(r.oid, c.attnum);
        if c.type_id.oid != 0 {
            push(out, col, type_addr(snapshot, c.type_id.oid), DepType::Normal);
        }
        if let Some(def) = &c.default {
            let ad = ObjectAddress::new(ObjClass::AttrDefault, def.oid);
            let t = if c.generated.is_some() { DepType::Internal } else { DepType::Auto };
            push(out, ad, col, t);
            push_expr_normal(out, ad, &def.expr);
        }
        if let Some(id) = &c.identity {
            push(out, ObjectAddress::relation(id.sequence), col, DepType::Internal);
        }
    }

    if let Some(v) = &r.view {
        push_expr_normal(out, me, &v.query);
    }
}

fn derive_sequence(s: &Sequence, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::relation(s.oid);
    push(out, me, ObjectAddress::namespace(s.namespace), DepType::Normal);
    if let Some((rel, attnum)) = s.owned_by {
        let t = if s.identity { DepType::Internal } else { DepType::Auto };
        push(out, me, ObjectAddress::column(rel, attnum), t);
    }
}

fn derive_index(i: &Index, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::relation(i.oid);
    if let Some(c) = i.constraint {
        push(out, me, ObjectAddress::constraint(c), DepType::Internal);
        return;
    }
    let mut has_column_ref = false;
    // Expression dependency: anything on the index's own table is Auto, the rest Normal.
    let expr_dep = |out: &mut Vec<DepEntry>, d: &ObjectAddress, has: &mut bool| {
        if d.class == ObjClass::Relation && d.oid == i.table {
            if d.sub_id != 0 {
                *has = true;
            }
            push(out, me, *d, DepType::Auto);
        } else {
            push(out, me, *d, DepType::Normal);
        }
    };
    for k in &i.keys {
        if let Some(att) = k.column {
            has_column_ref = true;
            push(out, me, ObjectAddress::column(i.table, att), DepType::Auto);
        }
        if let Some(e) = &k.expr {
            for d in &e.depends_on {
                expr_dep(out, d, &mut has_column_ref);
            }
        }
    }
    if let Some(p) = &i.predicate {
        for d in &p.depends_on {
            expr_dep(out, d, &mut has_column_ref);
        }
    }
    if !has_column_ref {
        push(out, me, ObjectAddress::relation(i.table), DepType::Auto);
    }
}

fn derive_constraint(c: &Constraint, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::constraint(c.oid);
    if c.is_domain_constraint() {
        push(out, me, ObjectAddress::type_(c.domain), DepType::Auto);
        if let Some(e) = &c.check {
            push_expr_normal(out, me, e);
        }
        return;
    }
    if c.table == 0 {
        return;
    }
    if c.columns.is_empty() {
        // CreateConstraintEntry: no conkey -> the constraint hangs off the whole table.
        push(out, me, ObjectAddress::relation(c.table), DepType::Auto);
    }
    for k in &c.columns {
        push(out, me, ObjectAddress::column(c.table, *k), DepType::Auto);
    }
    match c.kind {
        ConstraintKind::Check => {
            if let Some(e) = &c.check {
                push_expr_normal(out, me, e);
            }
        }
        ConstraintKind::ForeignKey => {
            if let Some(fk) = &c.fk {
                for k in &fk.ref_columns {
                    push(out, me, ObjectAddress::column(fk.ref_table, *k), DepType::Normal);
                }
                if fk.ref_columns.is_empty() {
                    push(out, me, ObjectAddress::relation(fk.ref_table), DepType::Normal);
                }
                if fk.ref_index != 0 {
                    push(out, me, ObjectAddress::relation(fk.ref_index), DepType::Normal);
                }
            }
        }
        _ => {}
    }
}

fn derive_type(snapshot: &CatalogSnapshot, t: &TypeObj, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::type_(t.oid);
    push(out, me, ObjectAddress::namespace(t.namespace), DepType::Normal);
    if t.array_type != 0 {
        push(out, ObjectAddress::type_(t.array_type), me, DepType::Internal);
    }
    if let TypeDef::Domain { base, default, .. } = &t.def {
        push(out, me, type_addr(snapshot, base.oid), DepType::Normal);
        if let Some(d) = default {
            push_expr_normal(out, me, d);
        }
    }
}

fn derive_function(snapshot: &CatalogSnapshot, f: &Function, out: &mut Vec<DepEntry>) {
    let me = ObjectAddress::proc(f.oid);
    push(out, me, ObjectAddress::namespace(f.namespace), DepType::Normal);
    for a in &f.args {
        push(out, me, type_addr(snapshot, a.type_oid), DepType::Normal);
        if let Some(d) = &a.default {
            push_expr_normal(out, me, d);
        }
    }
    if f.ret != 0 {
        push(out, me, type_addr(snapshot, f.ret), DepType::Normal);
    }
    for d in &f.depends_on {
        push(out, me, *d, DepType::Normal);
    }
    if let Some(b) = &f.sql_body {
        push_expr_normal(out, me, b);
    }
}

/// `pg_depend` virtual rows are produced from `DepGraph::build`; this helper maps a deptype to the
/// letter PG prints.
pub fn deptype_char(t: DepType) -> char {
    t.as_char() as char
}

/// Convenience: build the graph for the env's snapshot.
pub fn graph_for(env: &CatalogEnv) -> Result<DepGraph> {
    DepGraph::build(env.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_entries_indexes_both_directions() {
        let t = ObjectAddress::relation(16384);
        let c = ObjectAddress::column(16384, 1);
        let idx = ObjectAddress::relation(16385);
        let g = DepGraph::from_entries(vec![
            DepEntry { dependent: idx, referenced: c, deptype: DepType::Auto },
            DepEntry { dependent: c, referenced: t, deptype: DepType::Auto },
            DepEntry { dependent: idx, referenced: c, deptype: DepType::Auto },
        ]);
        assert_eq!(g.entries().len(), 2);
        assert_eq!(g.dependents_of(&c).len(), 1);
        assert_eq!(g.references_of(&idx).len(), 1);
        assert_eq!(deptype_char(DepType::Internal), 'i');
    }
}

#[cfg(test)]
mod derive_tests {
    use super::*;
    use crate::data::CatalogData;
    use crate::persist::CatalogVersion;
    use pgarbor_types::TypeId;

    const NS: Oid = 2200;
    const T: Oid = 16400;
    const ROWTYPE: Oid = 16401;
    const SEQ: Oid = 16402;
    const IDX: Oid = 16403;
    const CON: Oid = 16404;
    const ATTRDEF: Oid = 16405;
    const CHK: Oid = 16406;

    fn rel(oid: Oid, name: &str, cols: Vec<Column>) -> Relation {
        Relation {
            oid,
            name: name.into(),
            namespace: NS,
            kind: RelKind::Table,
            owner: 16384,
            persistence: Persistence::Permanent,
            columns: cols,
            rowtype: oid + 1,
            array_type: 0,
            array_type_name: None,
            options: vec![],
            acl: None,
            replica_identity: ReplicaIdentity::Default,
            row_security: false,
            force_row_security: false,
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: vec![],
        }
    }

    fn snapshot_of(data: CatalogData) -> CatalogSnapshot {
        CatalogSnapshot::build(CatalogVersion::ZERO, data).expect("snapshot")
    }

    fn has(g: &DepGraph, d: ObjectAddress, r: ObjectAddress, t: DepType) -> bool {
        g.entries().iter().any(|e| e.dependent == d && e.referenced == r && e.deptype == t)
    }

    fn constraint(oid: Oid, name: &str, kind: ConstraintKind, columns: Vec<i16>) -> Constraint {
        Constraint {
            oid,
            name: name.into(),
            namespace: NS,
            kind,
            table: T,
            domain: 0,
            columns,
            index: 0,
            deferrable: false,
            initially_deferred: false,
            validated: true,
            enforced: true,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: None,
            check: None,
            parent: 0,
        }
    }

    /// Bootstrap-equivalent base: `public` (2200) is persisted by `persist::bootstrap` (owner
    /// `pg_database_owner` 6171, as in `persist/meta.rs`), not synthesized by `CatalogSnapshot::build`,
    /// so every fixture that references namespace 2200 must carry it or validation fails with XX001.
    fn base_data() -> CatalogData {
        let mut data = CatalogData::default();
        data.namespaces.insert(NS, Namespace { oid: NS, name: "public".into(), owner: 6171, acl: None });
        data
    }

    fn serial_table() -> CatalogData {
        let mut data = base_data();
        let mut id = Column::new(1, "id", TypeId::INT4, 0);
        id.not_null = true;
        id.default = Some(ColumnDefault {
            oid: ATTRDEF,
            expr: StoredExpr::new("nextval('t_id_seq'::regclass)").with_deps(vec![ObjectAddress::relation(SEQ)]),
        });
        let mut r = rel(T, "t", vec![id, Column::new(2, "e", TypeId::INT4, 0)]);
        r.rowtype = ROWTYPE;
        data.relations.insert(T, r);
        data.sequences.insert(
            SEQ,
            Sequence {
                oid: SEQ,
                name: "t_id_seq".into(),
                namespace: NS,
                owner: 16384,
                persistence: Persistence::Permanent,
                type_oid: 23,
                start: 1,
                increment: 1,
                min: 1,
                max: i32::MAX as i64,
                cache: 1,
                cycle: false,
                owned_by: Some((T, 1)),
                identity: false,
                acl: None,
            },
        );
        let mut pk = constraint(CON, "t_pkey", ConstraintKind::PrimaryKey, vec![1]);
        pk.index = IDX;
        data.constraints.insert(CON, pk);
        let mut chk = constraint(CHK, "t_e_check", ConstraintKind::Check, vec![2]);
        chk.check = Some(StoredExpr::new("(e > 0)").with_deps(vec![ObjectAddress::column(T, 2)]));
        data.constraints.insert(CHK, chk);
        data.indexes.insert(
            IDX,
            Index {
                oid: IDX,
                name: "t_pkey".into(),
                namespace: NS,
                table: T,
                owner: 16384,
                method: IndexMethod::Btree,
                keys: vec![IndexKey {
                    column: Some(1),
                    expr: None,
                    name: "id".into(),
                    type_id: TypeId::INT4,
                    collation: 0,
                    opclass: 1978,
                    descending: false,
                    nulls_first: false,
                }],
                n_key_columns: 1,
                unique: true,
                nulls_not_distinct: false,
                primary: true,
                exclusion: false,
                immediate: true,
                clustered: false,
                valid: true,
                replident: false,
                predicate: None,
                constraint: Some(CON),
                options: vec![],
                persistence: Persistence::Permanent,
            },
        );
        data
    }

    #[test]
    fn serial_primary_key_edges() {
        let g = DepGraph::build(&snapshot_of(serial_table())).expect("graph");
        let t = ObjectAddress::relation(T);
        let id = ObjectAddress::column(T, 1);
        let e = ObjectAddress::column(T, 2);
        assert!(has(&g, t, ObjectAddress::namespace(NS), DepType::Normal));
        assert!(has(&g, ObjectAddress::type_(ROWTYPE), t, DepType::Internal));
        // builtin type is pinned: no column -> type edge
        assert!(g.references_of(&id).iter().all(|d| d.referenced.class != ObjClass::Type));
        let ad = ObjectAddress::new(ObjClass::AttrDefault, ATTRDEF);
        assert!(has(&g, ad, id, DepType::Auto));
        assert!(has(&g, ad, ObjectAddress::relation(SEQ), DepType::Normal));
        assert!(has(&g, ObjectAddress::relation(SEQ), id, DepType::Auto));
        assert!(has(&g, ObjectAddress::relation(IDX), ObjectAddress::constraint(CON), DepType::Internal));
        assert!(!has(&g, ObjectAddress::relation(IDX), t, DepType::Auto));
        assert!(has(&g, ObjectAddress::constraint(CON), id, DepType::Auto));
        // CHECK: both Auto and Normal edge to the referenced column
        assert!(has(&g, ObjectAddress::constraint(CHK), e, DepType::Auto));
        assert!(has(&g, ObjectAddress::constraint(CHK), e, DepType::Normal));
    }

    #[test]
    fn identity_sequence_is_internal() {
        let mut data = serial_table();
        if let Some(s) = data.sequences.get_mut(&SEQ) {
            s.identity = true;
        }
        let g = DepGraph::build(&snapshot_of(data)).expect("graph");
        assert!(has(&g, ObjectAddress::relation(SEQ), ObjectAddress::column(T, 1), DepType::Internal));
    }

    #[test]
    fn view_depends_normal_on_each_dependency() {
        let mut data = serial_table();
        let mut v = rel(16410, "v", vec![Column::new(1, "e", TypeId::INT4, 0)]);
        v.kind = RelKind::View;
        v.view = Some(ViewDef {
            query: StoredExpr::new(" SELECT e\n   FROM t;")
                .with_deps(vec![ObjectAddress::column(T, 2), ObjectAddress::relation(T)]),
            check_option: None,
            populated: true,
        });
        data.relations.insert(16410, v);
        let g = DepGraph::build(&snapshot_of(data)).expect("graph");
        let va = ObjectAddress::relation(16410);
        assert!(has(&g, va, ObjectAddress::column(T, 2), DepType::Normal));
        assert!(has(&g, va, ObjectAddress::relation(T), DepType::Normal));
    }

    #[test]
    fn role_deps_owner_and_acl() {
        let mut data = serial_table();
        if let Some(r) = data.relations.get_mut(&T) {
            r.acl = Some(vec![AclItem { grantee: 16500, grantor: 16384, privileges: acl_mode::SELECT, grant_options: 0 }]);
        }
        let s = snapshot_of(data);
        let owner = role_dependencies(&s, 16384).expect("owner deps");
        assert!(owner.contains(&SharedDep { object: ObjectAddress::relation(T), kind: SharedDepKind::Owner }));
        let grantee = role_dependencies(&s, 16500).expect("acl deps");
        assert_eq!(grantee, vec![SharedDep { object: ObjectAddress::relation(T), kind: SharedDepKind::Acl }]);
    }
}
