//! [`CatalogSnapshot`]: an immutable, indexed, cheaply clonable (`Arc`) view of one branch's
//! catalog at one catalog version. Fully implemented accessors; index construction is
//! `snapshot::build` (lease CAT-02).
//!
//! Lifetime / invalidation (CAT-12):
//! - The engine loads a snapshot per branch state and caches it by `CatalogVersion`
//!   (`crate::cache`). A session takes `Arc` clones; statements never see a snapshot change under
//!   them.
//! - A transaction that runs DDL works on a private copy (`CatalogTxn`), whose
//!   `snapshot()` reflects its own uncommitted changes; other sessions keep their snapshot until
//!   they see the committed version change.
//!
//! Builtin objects are merged in by `build`: `namespaces`/`roles`/`databases` maps contain the
//! builtin rows (converted to model structs) plus persisted ones; relations fall back to
//! `builtin::builtin_relation`; builtin TYPES/PROCS stay in `pgarbor_types::catalog_data` and are
//! reached through `CatalogEnv` (TypeEnv) and `resolve`.
use crate::builtin;
use crate::data::CatalogData;
use crate::model::*;
use crate::persist::{self, CatalogVersion};
use pgarbor_common::{KvRead, Oid, Result};
use std::collections::HashMap;
use std::sync::Arc;

/// What a pg_class OID / name refers to.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClassRef {
    /// A `Relation` (user or builtin system table/view).
    Relation(Oid),
    Index(Oid),
    Sequence(Oid),
    /// A builtin system index (only a `builtin::SystemRel`).
    SystemIndex(Oid),
}

impl ClassRef {
    pub fn oid(self) -> Oid {
        match self {
            ClassRef::Relation(o) | ClassRef::Index(o) | ClassRef::Sequence(o) | ClassRef::SystemIndex(o) => o,
        }
    }
}

/// What defines a USER type OID (builtin types are in `pgarbor_types::catalog_data::TYPES`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserTypeRef {
    /// Enum / domain `TypeObj`.
    Type(Oid),
    /// The array type of an enum/domain (`_mood`); payload = element type OID.
    TypeArray(Oid),
    /// A relation's rowtype (composite, `typrelid` = payload relation OID).
    RowType(Oid),
    /// The array of a relation's rowtype (`_t`); payload = relation OID.
    RowTypeArray(Oid),
}

/// Secondary indexes built by `snapshot::build` (CAT-02). Names are exact (already case-folded).
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct SnapshotIndexes {
    /// Builtin + persisted schemas.
    pub namespaces: HashMap<Oid, Namespace>,
    pub namespace_by_name: HashMap<String, Oid>,
    /// pg_class name space: (namespace, relname) → object; user objects AND builtin system
    /// tables/views/indexes (`ClassRef::SystemIndex`).
    pub class_by_name: HashMap<(Oid, String), ClassRef>,
    /// pg_type name space of USER types: (namespace, typname) → type OID (enums, domains, their
    /// arrays `_x`, rowtypes, rowtype arrays). Builtin type names are not in this map.
    pub type_by_name: HashMap<(Oid, String), Oid>,
    /// USER type OID → definition.
    pub user_types: HashMap<Oid, UserTypeRef>,
    /// table OID → its indexes (sorted by OID).
    pub indexes_by_table: HashMap<Oid, Vec<Oid>>,
    /// table OID → its constraints (sorted by OID).
    pub constraints_by_table: HashMap<Oid, Vec<Oid>>,
    /// domain type OID → its constraints (sorted by OID).
    pub constraints_by_domain: HashMap<Oid, Vec<Oid>>,
    /// referenced table OID → FK constraints pointing at it (sorted by OID).
    pub fks_referencing: HashMap<Oid, Vec<Oid>>,
    /// (table, attnum) → sequences OWNED BY that column (serial / identity).
    pub sequences_by_owner: HashMap<(Oid, i16), Vec<Oid>>,
    /// (namespace, name) → user functions with that name (sorted by OID).
    pub functions_by_name: HashMap<(Oid, String), Vec<Oid>>,
    /// Builtin + persisted roles.
    pub roles: HashMap<Oid, Role>,
    pub role_by_name: HashMap<String, Oid>,
    /// Builtin + persisted databases.
    pub databases: HashMap<Oid, Database>,
    pub database_by_name: HashMap<String, Oid>,
    /// pg_enum label OID → enum type OID.
    pub enum_label_owner: HashMap<Oid, Oid>,
    /// pg_attrdef OID → (relation, attnum).
    pub attrdef_owner: HashMap<Oid, (Oid, i16)>,
}

/// Snapshot internals (shared behind an `Arc`).
#[derive(Debug)]
pub struct SnapshotInner {
    pub version: CatalogVersion,
    pub data: CatalogData,
    pub idx: SnapshotIndexes,
}

/// Immutable catalog snapshot. `Clone` is an `Arc` clone.
// CONTRACT
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub(crate) inner: Arc<SnapshotInner>,
}

impl CatalogSnapshot {
    /// Load from storage: `read_version` + `load_data` + [`CatalogSnapshot::build`].
    pub fn load(kv: &dyn KvRead) -> Result<CatalogSnapshot> {
        let version = persist::read_version(kv)?;
        let data = persist::load_data(kv)?;
        CatalogSnapshot::build(version, data)
    }

    /// Wrap already-built parts (used by `build`).
    pub fn from_parts(version: CatalogVersion, data: CatalogData, idx: SnapshotIndexes) -> CatalogSnapshot {
        CatalogSnapshot { inner: Arc::new(SnapshotInner { version, data, idx }) }
    }

    pub fn version(&self) -> CatalogVersion {
        self.inner.version
    }
    /// Persisted objects.
    pub fn data(&self) -> &CatalogData {
        &self.inner.data
    }
    /// Secondary indexes.
    pub fn indexes(&self) -> &SnapshotIndexes {
        &self.inner.idx
    }
    /// True when both handles share the same snapshot allocation.
    pub fn ptr_eq(&self, other: &CatalogSnapshot) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    // ----------------------------------------------------------- schemas

    pub fn namespace(&self, oid: Oid) -> Option<&Namespace> {
        self.inner.idx.namespaces.get(&oid)
    }
    pub fn namespace_by_name(&self, name: &str) -> Option<&Namespace> {
        self.inner.idx.namespace_by_name.get(name).and_then(|o| self.namespace(*o))
    }
    /// All schemas sorted by OID.
    pub fn namespaces(&self) -> Vec<&Namespace> {
        let mut v: Vec<&Namespace> = self.inner.idx.namespaces.values().collect();
        v.sort_by_key(|n| n.oid);
        v
    }

    // ----------------------------------------------------------- pg_class objects

    /// User relation or builtin system table/view.
    pub fn relation(&self, oid: Oid) -> Option<&Relation> {
        self.inner.data.relations.get(&oid).or_else(|| builtin::builtin_relation(oid))
    }
    pub fn index(&self, oid: Oid) -> Option<&Index> {
        self.inner.data.indexes.get(&oid)
    }
    pub fn sequence(&self, oid: Oid) -> Option<&Sequence> {
        self.inner.data.sequences.get(&oid)
    }
    /// What a pg_class OID is.
    pub fn class(&self, oid: Oid) -> Option<ClassRef> {
        if self.relation(oid).is_some() {
            Some(ClassRef::Relation(oid))
        } else if self.inner.data.indexes.contains_key(&oid) {
            Some(ClassRef::Index(oid))
        } else if self.inner.data.sequences.contains_key(&oid) {
            Some(ClassRef::Sequence(oid))
        } else if builtin::system_rel(oid).is_some_and(|r| r.kind == b'i') {
            Some(ClassRef::SystemIndex(oid))
        } else {
            None
        }
    }
    /// (namespace, relname, relkind) of any pg_class OID.
    pub fn class_identity(&self, oid: Oid) -> Option<(Oid, &str, RelKind)> {
        match self.class(oid)? {
            ClassRef::Relation(o) => self.relation(o).map(|r| (r.namespace, r.name.as_str(), r.kind)),
            ClassRef::Index(o) => self.index(o).map(|i| (i.namespace, i.name.as_str(), RelKind::Index)),
            ClassRef::Sequence(o) => self.sequence(o).map(|s| (s.namespace, s.name.as_str(), RelKind::Sequence)),
            ClassRef::SystemIndex(o) => builtin::system_rel(o).map(|r| (r.namespace, r.name, RelKind::Index)),
        }
    }
    /// pg_class lookup by exact (namespace, name).
    pub fn class_in_namespace(&self, namespace: Oid, name: &str) -> Option<ClassRef> {
        self.inner.idx.class_by_name.get(&(namespace, name.to_string())).copied()
    }
    /// Indexes of a table (sorted by OID).
    pub fn indexes_of(&self, table: Oid) -> Vec<&Index> {
        self.inner
            .idx
            .indexes_by_table
            .get(&table)
            .map(|v| v.iter().filter_map(|o| self.index(*o)).collect())
            .unwrap_or_default()
    }
    /// Sequences OWNED BY (table, attnum).
    pub fn sequences_owned_by(&self, table: Oid, attnum: i16) -> Vec<&Sequence> {
        self.inner
            .idx
            .sequences_by_owner
            .get(&(table, attnum))
            .map(|v| v.iter().filter_map(|o| self.sequence(*o)).collect())
            .unwrap_or_default()
    }
    /// All user relations (tables, views, ...) sorted by OID (builtins excluded).
    pub fn user_relations(&self) -> impl Iterator<Item = &Relation> {
        self.inner.data.relations.values()
    }
    /// Direct inheritance children (including partitions) of `parent`, sorted by OID
    /// (amendment pgarbor-catalog-09).
    pub fn inheritance_children(&self, parent: Oid) -> Vec<Oid> {
        self.inner.data.relations.values().filter(|r| r.inherits.contains(&parent)).map(|r| r.oid).collect()
    }
    /// `parent` followed by all its descendants, each once, in breadth-first order (PG
    /// `find_all_inheritors`). Partitions of a partitioned table are listed in bound order.
    pub fn inheritance_tree(&self, parent: Oid) -> Vec<Oid> {
        let mut out = vec![parent];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            for c in self.ordered_children(p) {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            i += 1;
        }
        out
    }
    /// Direct children in scan order: partitions by bound (list/range), others by OID.
    pub fn ordered_children(&self, parent: Oid) -> Vec<Oid> {
        let children = self.inheritance_children(parent);
        match self.relation(parent) {
            Some(r) if r.partition_key.is_some() => crate::partition::order_partitions(self, r, children),
            _ => children,
        }
    }
    /// Ancestors of `rel` (parent first), following the first parent of each level.
    pub fn partition_ancestors(&self, rel: Oid) -> Vec<Oid> {
        let mut out = Vec::new();
        let mut cur = rel;
        while let Some(p) = self.relation(cur).and_then(|r| r.partition_of.as_ref().map(|(p, _)| *p)) {
            if out.contains(&p) {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    // ----------------------------------------------------------- constraints

    pub fn constraint(&self, oid: Oid) -> Option<&Constraint> {
        self.inner.data.constraints.get(&oid)
    }
    /// Constraints of a table (sorted by OID).
    pub fn constraints_of(&self, table: Oid) -> Vec<&Constraint> {
        self.inner
            .idx
            .constraints_by_table
            .get(&table)
            .map(|v| v.iter().filter_map(|o| self.constraint(*o)).collect())
            .unwrap_or_default()
    }
    /// Constraints of a domain (sorted by OID).
    pub fn domain_constraints(&self, domain: Oid) -> Vec<&Constraint> {
        self.inner
            .idx
            .constraints_by_domain
            .get(&domain)
            .map(|v| v.iter().filter_map(|o| self.constraint(*o)).collect())
            .unwrap_or_default()
    }
    /// FK constraints referencing `table`.
    pub fn foreign_keys_referencing(&self, table: Oid) -> Vec<&Constraint> {
        self.inner
            .idx
            .fks_referencing
            .get(&table)
            .map(|v| v.iter().filter_map(|o| self.constraint(*o)).collect())
            .unwrap_or_default()
    }
    /// The table's primary key constraint.
    pub fn primary_key(&self, table: Oid) -> Option<&Constraint> {
        self.constraints_of(table).into_iter().find(|c| c.kind == ConstraintKind::PrimaryKey)
    }

    // ----------------------------------------------------------- types

    pub fn type_obj(&self, oid: Oid) -> Option<&TypeObj> {
        self.inner.data.types.get(&oid)
    }
    pub fn user_type(&self, oid: Oid) -> Option<UserTypeRef> {
        self.inner.idx.user_types.get(&oid).copied()
    }
    /// User type by exact (namespace, typname).
    pub fn user_type_in_namespace(&self, namespace: Oid, typname: &str) -> Option<Oid> {
        self.inner.idx.type_by_name.get(&(namespace, typname.to_string())).copied()
    }

    // ----------------------------------------------------------- functions

    pub fn function(&self, oid: Oid) -> Option<&Function> {
        self.inner.data.functions.get(&oid)
    }
    /// User functions named `name` in `namespace`.
    pub fn functions_in_namespace(&self, namespace: Oid, name: &str) -> Vec<&Function> {
        self.inner
            .idx
            .functions_by_name
            .get(&(namespace, name.to_string()))
            .map(|v| v.iter().filter_map(|o| self.function(*o)).collect())
            .unwrap_or_default()
    }

    // ----------------------------------------------------------- roles / databases

    pub fn role(&self, oid: Oid) -> Option<&Role> {
        self.inner.idx.roles.get(&oid)
    }
    pub fn role_by_name(&self, name: &str) -> Option<&Role> {
        self.inner.idx.role_by_name.get(name).and_then(|o| self.role(*o))
    }
    /// Role name or None (PG `pg_get_userbyid` prints `unknown (OID=n)` for missing roles).
    pub fn role_name(&self, oid: Oid) -> Option<&str> {
        self.role(oid).map(|r| r.name.as_str())
    }
    pub fn database(&self, oid: Oid) -> Option<&Database> {
        self.inner.idx.databases.get(&oid)
    }
    pub fn database_by_name(&self, name: &str) -> Option<&Database> {
        self.inner.idx.database_by_name.get(name).and_then(|o| self.database(*o))
    }

    // ----------------------------------------------------------- descriptions

    /// Persisted description, falling back to the builtin description for objsubid 0.
    pub fn description(&self, key: &DescriptionKey) -> Option<&str> {
        self.inner
            .data
            .descriptions
            .get(key)
            .map(|s| s.as_str())
            .or_else(|| if key.objsubid == 0 { builtin::builtin_description(key.classoid, key.objoid) } else { None })
    }
}
