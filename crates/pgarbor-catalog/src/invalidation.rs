//! Catalog change detection for prepared plans and cached session state (CAT-12). Fully
//! implemented, frozen (v1).
//!
//! `diff(old, new)` compares two snapshots object by object (structural equality of the model
//! structs; O(objects) with early exit on `ptr_eq`) and returns the set of changed objects. A
//! prepared statement records its [`PlanDeps`] at plan time (every relation/type/function the
//! plan mentions, plus whether it used the search_path) and asks `plan_is_stale(deps, &inv)` after
//! each snapshot swap. Stale -> re-parse/re-bind/re-plan (PG: `cached plan must not change result
//! type` 0A000 if the result descriptor changed for a protocol-level prepared statement; that
//! check is the session layer's).
use crate::model::{ObjClass, ObjectAddress};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::Oid;
use std::collections::BTreeSet;

/// What changed between two snapshots.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invalidation {
    /// Changed objects (added, removed or modified), whole-object addresses. Relations, indexes and
    /// sequences are all `ObjClass::Relation`; constraints `ObjClass::Constraint` ...
    pub changed: BTreeSet<ObjectAddress>,
    /// Some schema was added/removed/renamed/re-owned: cached search_path resolutions are stale.
    pub schemas: bool,
    /// Some role or role membership changed (privilege caches are stale).
    pub roles: bool,
}

impl Invalidation {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && !self.schemas && !self.roles
    }
    pub fn touches(&self, addr: &ObjectAddress) -> bool {
        self.changed.contains(&addr.whole())
    }
}

/// Objects a prepared plan depends on.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanDeps {
    pub relations: Vec<Oid>,
    pub types: Vec<Oid>,
    pub functions: Vec<Oid>,
    /// The statement resolved unqualified names through search_path.
    pub uses_search_path: bool,
}

fn diff_map<K: Ord + Copy, V: PartialEq>(
    old: &std::collections::BTreeMap<K, V>,
    new: &std::collections::BTreeMap<K, V>,
    mut mark: impl FnMut(K),
) {
    for (k, v) in old {
        match new.get(k) {
            Some(n) if n == v => {}
            _ => mark(*k),
        }
    }
    for k in new.keys() {
        if !old.contains_key(k) {
            mark(*k);
        }
    }
}

/// Compare two snapshots (module docs).
pub fn diff(old: &CatalogSnapshot, new: &CatalogSnapshot) -> Invalidation {
    let mut inv = Invalidation::default();
    if old.ptr_eq(new) || old.version() == new.version() {
        return inv;
    }
    let (o, n) = (old.data(), new.data());
    diff_map(&o.relations, &n.relations, |k| {
        inv.changed.insert(ObjectAddress::relation(k));
    });
    diff_map(&o.indexes, &n.indexes, |k| {
        inv.changed.insert(ObjectAddress::relation(k));
    });
    diff_map(&o.sequences, &n.sequences, |k| {
        inv.changed.insert(ObjectAddress::relation(k));
    });
    diff_map(&o.types, &n.types, |k| {
        inv.changed.insert(ObjectAddress::type_(k));
    });
    diff_map(&o.constraints, &n.constraints, |k| {
        inv.changed.insert(ObjectAddress::constraint(k));
    });
    diff_map(&o.functions, &n.functions, |k| {
        inv.changed.insert(ObjectAddress::proc(k));
    });
    diff_map(&o.databases, &n.databases, |k| {
        inv.changed.insert(ObjectAddress::new(ObjClass::Database, k));
    });
    let mut schemas = false;
    diff_map(&o.namespaces, &n.namespaces, |k| {
        schemas = true;
        inv.changed.insert(ObjectAddress::namespace(k));
    });
    inv.schemas = schemas;
    let mut roles = false;
    diff_map(&o.roles, &n.roles, |k| {
        roles = true;
        inv.changed.insert(ObjectAddress::new(ObjClass::Role, k));
    });
    diff_map(&o.memberships, &n.memberships, |_| roles = true);
    inv.roles = roles;
    inv
}

/// True when a plan with `deps` must be rebuilt after `inv`. A relation counts as changed when the
/// relation itself, one of its indexes/constraints/sequences, or its rowtype changed; since indexes
/// and sequences are themselves `Relation`-class addresses, callers include them in
/// `PlanDeps::relations` only if the plan uses them directly - table-level changes (new index,
/// new constraint, column changes) always touch the table's own `Relation` row or add an `Index`
/// whose `table` field the planner re-reads, so `plan_is_stale` also treats any changed
/// index/constraint of a dependent table as stale via [`table_children_changed`].
pub fn plan_is_stale(deps: &PlanDeps, inv: &Invalidation, old: &CatalogSnapshot, new: &CatalogSnapshot) -> bool {
    if inv.is_empty() {
        return false;
    }
    if deps.uses_search_path && inv.schemas {
        return true;
    }
    if deps.relations.iter().any(|r| inv.touches(&ObjectAddress::relation(*r)) || table_children_changed(*r, inv, old, new)) {
        return true;
    }
    deps.types.iter().any(|t| inv.touches(&ObjectAddress::type_(*t)))
        || deps.functions.iter().any(|f| inv.touches(&ObjectAddress::proc(*f)))
}

/// True when an index or constraint belonging to `table` is among the changed objects (a plan over
/// `table` may have chosen or relied on it).
pub fn table_children_changed(table: Oid, inv: &Invalidation, old: &CatalogSnapshot, new: &CatalogSnapshot) -> bool {
    inv.changed.iter().any(|a| match a.class {
        ObjClass::Relation => {
            new.index(a.oid).is_some_and(|i| i.table == table) || old.index(a.oid).is_some_and(|i| i.table == table)
        }
        ObjClass::Constraint => {
            new.constraint(a.oid).is_some_and(|c| c.table == table)
                || old.constraint(a.oid).is_some_and(|c| c.table == table)
        }
        _ => false,
    })
}
