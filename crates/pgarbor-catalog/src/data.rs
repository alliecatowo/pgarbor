//! [`CatalogData`]: the raw set of persisted (non-builtin) catalog objects of one branch state,
//! keyed by OID. `persist::load` fills it from KV; `CatalogSnapshot` indexes it; `CatalogTxn`
//! mutates a working copy and writes each change through to KV.
//!
//! Fully implemented, frozen (v1).
use crate::model::*;
use pgarbor_common::Oid;
use std::collections::BTreeMap;

/// All persisted catalog objects (user objects plus the bootstrap-mutable `public` schema,
/// `postgres` role and `postgres` database).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogData {
    pub namespaces: BTreeMap<Oid, Namespace>,
    pub relations: BTreeMap<Oid, Relation>,
    pub indexes: BTreeMap<Oid, Index>,
    pub sequences: BTreeMap<Oid, Sequence>,
    pub types: BTreeMap<Oid, TypeObj>,
    pub constraints: BTreeMap<Oid, Constraint>,
    pub functions: BTreeMap<Oid, Function>,
    pub roles: BTreeMap<Oid, Role>,
    pub memberships: BTreeMap<Oid, RoleMembership>,
    pub databases: BTreeMap<Oid, Database>,
    pub descriptions: BTreeMap<DescriptionKey, String>,
    /// Extensibility objects (operators, aggregate definitions, operator families/classes, casts,
    /// conversions, collations, extensions), amendment pgarbor-catalog-09.
    pub ext: BTreeMap<Oid, ExtObject>,
}

impl CatalogData {
    /// Insert or replace an object.
    pub fn insert(&mut self, obj: CatalogObject) {
        match obj {
            CatalogObject::Namespace(o) => {
                self.namespaces.insert(o.oid, o);
            }
            CatalogObject::Relation(o) => {
                self.relations.insert(o.oid, o);
            }
            CatalogObject::Index(o) => {
                self.indexes.insert(o.oid, o);
            }
            CatalogObject::Sequence(o) => {
                self.sequences.insert(o.oid, o);
            }
            CatalogObject::Type(o) => {
                self.types.insert(o.oid, o);
            }
            CatalogObject::Constraint(o) => {
                self.constraints.insert(o.oid, o);
            }
            CatalogObject::Function(o) => {
                self.functions.insert(o.oid, o);
            }
            CatalogObject::Role(o) => {
                self.roles.insert(o.oid, o);
            }
            CatalogObject::RoleMembership(o) => {
                self.memberships.insert(o.oid, o);
            }
            CatalogObject::Database(o) => {
                self.databases.insert(o.oid, o);
            }
            CatalogObject::Ext(o) => {
                self.ext.insert(o.oid(), o);
            }
        }
    }

    /// Remove an object; returns whether it existed. `ObjKind::Description` is ignored (use
    /// `descriptions` directly).
    pub fn remove(&mut self, kind: ObjKind, oid: Oid) -> bool {
        match kind {
            ObjKind::Namespace => self.namespaces.remove(&oid).is_some(),
            ObjKind::Relation => self.relations.remove(&oid).is_some(),
            ObjKind::Index => self.indexes.remove(&oid).is_some(),
            ObjKind::Sequence => self.sequences.remove(&oid).is_some(),
            ObjKind::Type => self.types.remove(&oid).is_some(),
            ObjKind::Constraint => self.constraints.remove(&oid).is_some(),
            ObjKind::Function => self.functions.remove(&oid).is_some(),
            ObjKind::Role => self.roles.remove(&oid).is_some(),
            ObjKind::RoleMembership => self.memberships.remove(&oid).is_some(),
            ObjKind::Database => self.databases.remove(&oid).is_some(),
            ObjKind::Ext => self.ext.remove(&oid).is_some(),
            ObjKind::Description => false,
        }
    }

    /// Clone out an object by kind and OID.
    pub fn get(&self, kind: ObjKind, oid: Oid) -> Option<CatalogObject> {
        Some(match kind {
            ObjKind::Namespace => CatalogObject::Namespace(self.namespaces.get(&oid)?.clone()),
            ObjKind::Relation => CatalogObject::Relation(self.relations.get(&oid)?.clone()),
            ObjKind::Index => CatalogObject::Index(self.indexes.get(&oid)?.clone()),
            ObjKind::Sequence => CatalogObject::Sequence(self.sequences.get(&oid)?.clone()),
            ObjKind::Type => CatalogObject::Type(self.types.get(&oid)?.clone()),
            ObjKind::Constraint => CatalogObject::Constraint(self.constraints.get(&oid)?.clone()),
            ObjKind::Function => CatalogObject::Function(self.functions.get(&oid)?.clone()),
            ObjKind::Role => CatalogObject::Role(self.roles.get(&oid)?.clone()),
            ObjKind::RoleMembership => CatalogObject::RoleMembership(self.memberships.get(&oid)?.clone()),
            ObjKind::Database => CatalogObject::Database(self.databases.get(&oid)?.clone()),
            ObjKind::Ext => CatalogObject::Ext(self.ext.get(&oid)?.clone()),
            ObjKind::Description => return None,
        })
    }

    /// The kind of the persisted object holding `oid`, if any (OIDs are unique across kinds within
    /// a branch because they come from one counter).
    pub fn kind_of(&self, oid: Oid) -> Option<ObjKind> {
        if self.relations.contains_key(&oid) {
            Some(ObjKind::Relation)
        } else if self.indexes.contains_key(&oid) {
            Some(ObjKind::Index)
        } else if self.sequences.contains_key(&oid) {
            Some(ObjKind::Sequence)
        } else if self.types.contains_key(&oid) {
            Some(ObjKind::Type)
        } else if self.constraints.contains_key(&oid) {
            Some(ObjKind::Constraint)
        } else if self.namespaces.contains_key(&oid) {
            Some(ObjKind::Namespace)
        } else if self.functions.contains_key(&oid) {
            Some(ObjKind::Function)
        } else if self.roles.contains_key(&oid) {
            Some(ObjKind::Role)
        } else if self.memberships.contains_key(&oid) {
            Some(ObjKind::RoleMembership)
        } else if self.databases.contains_key(&oid) {
            Some(ObjKind::Database)
        } else if self.ext.contains_key(&oid) {
            Some(ObjKind::Ext)
        } else {
            None
        }
    }

    /// The largest OID used ANYWHERE in the data: object keys AND OIDs embedded in values
    /// (`Relation::rowtype/array_type`, `Column.default.oid`, `TypeObj::array_type`,
    /// `EnumLabelDef::oid`, `Index::constraint`, ...). Returns 0 when empty. Used by
    /// `persist::oid_seed`.
    pub fn max_oid(&self) -> Oid {
        let mut m: Oid = 0;
        let mut see = |o: Oid| {
            if o > m {
                m = o;
            }
        };
        self.namespaces.keys().for_each(|o| see(*o));
        self.indexes.keys().for_each(|o| see(*o));
        self.sequences.keys().for_each(|o| see(*o));
        self.constraints.keys().for_each(|o| see(*o));
        self.functions.keys().for_each(|o| see(*o));
        self.roles.keys().for_each(|o| see(*o));
        self.memberships.keys().for_each(|o| see(*o));
        self.databases.keys().for_each(|o| see(*o));
        self.ext.values().for_each(|e| e.allocated_oids().into_iter().for_each(&mut see));
        for r in self.relations.values() {
            see(r.oid);
            see(r.rowtype);
            see(r.array_type);
            for c in &r.columns {
                if let Some(d) = &c.default {
                    see(d.oid);
                }
            }
            r.policies.iter().for_each(|p| see(p.oid));
            r.rules.iter().for_each(|p| see(p.oid));
        }
        for t in self.types.values() {
            see(t.oid);
            see(t.array_type);
            if let TypeDef::Enum { labels } = &t.def {
                labels.iter().for_each(|l| see(l.oid));
            }
        }
        m
    }

    /// True if `oid` is used by any object, including OIDs embedded in values (rowtype, array type,
    /// attrdef, enum label).
    pub fn oid_in_use(&self, oid: Oid) -> bool {
        if self.kind_of(oid).is_some() {
            return true;
        }
        if self.ext.values().any(|e| e.allocated_oids().contains(&oid)) {
            return true;
        }
        self.relations.values().any(|r| {
            r.rowtype == oid
                || r.array_type == oid
                || r.columns.iter().any(|c| c.default.as_ref().is_some_and(|d| d.oid == oid))
                || r.policies.iter().any(|p| p.oid == oid)
                || r.rules.iter().any(|p| p.oid == oid)
        }) || self.types.values().any(|t| {
            t.array_type == oid
                || matches!(&t.def, TypeDef::Enum { labels } if labels.iter().any(|l| l.oid == oid))
        })
    }

    /// Total number of OID-keyed objects.
    pub fn len(&self) -> usize {
        self.namespaces.len()
            + self.relations.len()
            + self.indexes.len()
            + self.sequences.len()
            + self.types.len()
            + self.constraints.len()
            + self.functions.len()
            + self.roles.len()
            + self.memberships.len()
            + self.databases.len()
            + self.ext.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
