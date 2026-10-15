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
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
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
    #[allow(unused_variables)]
    pub fn build(snapshot: &CatalogSnapshot) -> Result<DepGraph> {
        Err(PgError::unimplemented("CAT-23", "dependency graph derivation"))
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
#[allow(unused_variables)]
pub fn role_dependencies(snapshot: &CatalogSnapshot, role: Oid) -> Result<Vec<SharedDep>> {
    Err(PgError::unimplemented("CAT-23", "role dependencies"))
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
