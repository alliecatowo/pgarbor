//! User-defined types (`pg_type` + `pg_enum`): enums and domains (ranges later).
//!
//! Composite types are NOT here: `CREATE TYPE c AS (...)` creates a `Relation` of kind
//! `CompositeType` whose `rowtype`/`array_type` are the pg_type rows, exactly like a table's rowtype.
//! Array types of user types are implicit (`TypeObj::array_type`, named `_<name>`; derived rows).
//!
//! Plain data, frozen (v1).
use super::common::{Acl, StoredExpr};
use pgarbor_common::Oid;
use pgarbor_types::TypeId;
use serde::{Deserialize, Serialize};

/// One enum label (`pg_enum` row).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnumLabelDef {
    /// pg_enum OID (allocated from the branch OID counter; PG uses even OIDs when it can, which
    /// PGArbor does not emulate).
    pub oid: Oid,
    /// Max 63 bytes (42602 `invalid enum label "..."` + DETAIL `Labels must be 63 bytes or less.`).
    pub label: String,
    /// `enumsortorder`. CREATE assigns 1, 2, 3...; `ADD VALUE` appends max+1, or for
    /// `BEFORE/AFTER x` the midpoint between neighbours (PG `AddEnumLabel`, f32 arithmetic;
    /// when the midpoint is not representable PG renumbers: `RenumberEnumType`).
    pub sort_order: f32,
}

/// What kind of user type.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TypeDef {
    /// `CREATE TYPE t AS ENUM (...)`. Labels kept sorted by `sort_order`.
    Enum { labels: Vec<EnumLabelDef> },
    /// `CREATE DOMAIN d AS base [COLLATE c] [DEFAULT e] [constraints]`.
    /// Domain CHECK / NOT NULL constraints are `Constraint` objects with `domain = type oid`
    /// (PG 18 names them `<domain>_check`, `<domain>_not_null`).
    Domain {
        /// Base type + typmod (`typbasetype`, `typtypmod`).
        base: TypeId,
        /// `typnotnull` (some NOT NULL constraint exists).
        not_null: bool,
        /// `typdefault` / `typdefaultbin` (text as `pg_get_expr` prints it).
        default: Option<StoredExpr>,
        /// `typcollation` (base type's collation unless COLLATE given).
        collation: Oid,
        /// `typndims` (dimensions when the base is an array).
        ndims: i32,
    },
}

/// A user type.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeObj {
    pub oid: Oid,
    /// Max 63 bytes. Shares the pg_type name space with table rowtypes and array types
    /// (42710 `type "x" already exists`).
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    /// The array type OID (`_<name>`, truncated per PG `makeArrayTypeName`).
    pub array_type: Oid,
    /// Array typname when not canonical (see `Relation::array_type_name`). None = canonical.
    #[serde(default)]
    pub array_type_name: Option<String>,
    pub def: TypeDef,
    #[serde(default)]
    pub acl: Option<Acl>,
}

impl TypeObj {
    pub fn is_enum(&self) -> bool {
        matches!(self.def, TypeDef::Enum { .. })
    }
    pub fn is_domain(&self) -> bool {
        matches!(self.def, TypeDef::Domain { .. })
    }
    /// `typtype`: b'e' / b'd'.
    pub fn typtype(&self) -> u8 {
        match self.def {
            TypeDef::Enum { .. } => b'e',
            TypeDef::Domain { .. } => b'd',
        }
    }
}
