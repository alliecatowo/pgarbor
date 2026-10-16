//! [`CatalogObject`]: the unit of catalog persistence (one KV row in space 1).
//!
//! Fully implemented, frozen (v1).
use super::*;
use pgarbor_common::Oid;

/// Persisted object kind = the kind byte of its key (`persist::keys`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjKind {
    Namespace,
    Relation,
    Index,
    Sequence,
    Type,
    Constraint,
    Function,
    Role,
    RoleMembership,
    Database,
    /// pg_description row (key is a `DescriptionKey`, not an OID).
    Description,
    /// Extensibility objects (`ExtObject`, amendment pgarbor-catalog-09).
    Ext,
}

impl ObjKind {
    /// Key kind byte.
    pub fn byte(self) -> u8 {
        match self {
            ObjKind::Namespace => b'n',
            ObjKind::Relation => b'r',
            ObjKind::Index => b'i',
            ObjKind::Sequence => b'S',
            ObjKind::Type => b't',
            ObjKind::Constraint => b'c',
            ObjKind::Function => b'f',
            ObjKind::Role => b'a',
            ObjKind::RoleMembership => b'm',
            ObjKind::Database => b'd',
            ObjKind::Description => b'D',
            ObjKind::Ext => b'x',
        }
    }
    pub fn from_byte(b: u8) -> Option<ObjKind> {
        Some(match b {
            b'n' => ObjKind::Namespace,
            b'r' => ObjKind::Relation,
            b'i' => ObjKind::Index,
            b'S' => ObjKind::Sequence,
            b't' => ObjKind::Type,
            b'c' => ObjKind::Constraint,
            b'f' => ObjKind::Function,
            b'a' => ObjKind::Role,
            b'm' => ObjKind::RoleMembership,
            b'd' => ObjKind::Database,
            b'D' => ObjKind::Description,
            b'x' => ObjKind::Ext,
            _ => return None,
        })
    }
    /// All OID-keyed kinds (everything but Description).
    pub const OID_KINDS: [ObjKind; 11] = [
        ObjKind::Namespace,
        ObjKind::Relation,
        ObjKind::Index,
        ObjKind::Sequence,
        ObjKind::Type,
        ObjKind::Constraint,
        ObjKind::Function,
        ObjKind::Role,
        ObjKind::RoleMembership,
        ObjKind::Database,
        ObjKind::Ext,
    ];
}

/// One OID-keyed catalog object.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum CatalogObject {
    Namespace(Namespace),
    Relation(Relation),
    Index(Index),
    Sequence(Sequence),
    Type(TypeObj),
    Constraint(Constraint),
    Function(Function),
    Role(Role),
    RoleMembership(RoleMembership),
    Database(Database),
    Ext(ExtObject),
}

impl CatalogObject {
    pub fn kind(&self) -> ObjKind {
        match self {
            CatalogObject::Namespace(_) => ObjKind::Namespace,
            CatalogObject::Relation(_) => ObjKind::Relation,
            CatalogObject::Index(_) => ObjKind::Index,
            CatalogObject::Sequence(_) => ObjKind::Sequence,
            CatalogObject::Type(_) => ObjKind::Type,
            CatalogObject::Constraint(_) => ObjKind::Constraint,
            CatalogObject::Function(_) => ObjKind::Function,
            CatalogObject::Role(_) => ObjKind::Role,
            CatalogObject::RoleMembership(_) => ObjKind::RoleMembership,
            CatalogObject::Database(_) => ObjKind::Database,
            CatalogObject::Ext(_) => ObjKind::Ext,
        }
    }
    pub fn oid(&self) -> Oid {
        match self {
            CatalogObject::Namespace(o) => o.oid,
            CatalogObject::Relation(o) => o.oid,
            CatalogObject::Index(o) => o.oid,
            CatalogObject::Sequence(o) => o.oid,
            CatalogObject::Type(o) => o.oid,
            CatalogObject::Constraint(o) => o.oid,
            CatalogObject::Function(o) => o.oid,
            CatalogObject::Role(o) => o.oid,
            CatalogObject::RoleMembership(o) => o.oid,
            CatalogObject::Database(o) => o.oid,
            CatalogObject::Ext(o) => o.oid(),
        }
    }
}
