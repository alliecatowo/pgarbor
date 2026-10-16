//! Cross-cutting catalog model types: object addresses (pg_depend / pg_description keys),
//! dependency kinds, stored expressions, drop behavior and ACLs.
//!
//! Everything here is plain data, fully implemented, and frozen (v1). The serde shapes are part of
//! the on-disk catalog format (see `crate::persist::codec`): never rename fields or variants;
//! add new fields only with `#[serde(default)]`.
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};
use std::fmt;

/// `pg_class` OIDs of the catalogs that identify object classes (PG `*RelationId` constants).
/// These are the `classid`/`refclassid` values of pg_depend and the `classoid` of pg_description.
pub mod class_oid {
    use pgarbor_common::Oid;
    pub const PG_TYPE: Oid = 1247;
    pub const PG_ATTRIBUTE: Oid = 1249;
    pub const PG_PROC: Oid = 1255;
    pub const PG_CLASS: Oid = 1259;
    pub const PG_AUTHID: Oid = 1260;
    pub const PG_AUTH_MEMBERS: Oid = 1261;
    pub const PG_DATABASE: Oid = 1262;
    pub const PG_TABLESPACE: Oid = 1213;
    pub const PG_AM: Oid = 2601;
    pub const PG_ATTRDEF: Oid = 2604;
    pub const PG_CAST: Oid = 2605;
    pub const PG_CONSTRAINT: Oid = 2606;
    pub const PG_LANGUAGE: Oid = 2612;
    pub const PG_NAMESPACE: Oid = 2615;
    pub const PG_OPCLASS: Oid = 2616;
    pub const PG_OPERATOR: Oid = 2617;
    pub const PG_REWRITE: Oid = 2618;
    pub const PG_TRIGGER: Oid = 2620;
    pub const PG_OPFAMILY: Oid = 2753;
    pub const PG_EXTENSION: Oid = 3079;
    pub const PG_COLLATION: Oid = 3456;
    pub const PG_ENUM: Oid = 3501;
    pub const PG_SEQUENCE: Oid = 2224;
    pub const PG_AMOP: Oid = 2602;
    pub const PG_AMPROC: Oid = 2603;
    pub const PG_CONVERSION: Oid = 2607;
}

/// The class of a catalog object (which catalog its OID lives in).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjClass {
    /// pg_class: tables, views, matviews, indexes, sequences, composite-type relations.
    /// With `sub_id > 0` the address names a column (attnum).
    Relation,
    /// pg_type: enum / domain / composite rowtype / array types.
    Type,
    Namespace,
    Constraint,
    /// pg_attrdef (column default or generation expression).
    AttrDefault,
    /// pg_proc (user functions/procedures; builtins are referenced but never dropped).
    Proc,
    Role,
    Database,
    Collation,
    Operator,
    Cast,
    Language,
    AccessMethod,
    Extension,
    /// pg_rewrite (the `_RETURN` rule of a view). PGArbor has no separate rule objects: the address
    /// `{Rewrite, view_oid}` is used only for PG-compatible pg_depend rows / descriptions.
    Rewrite,
    Trigger,
    Tablespace,
    /// pg_opclass / pg_opfamily / pg_amop / pg_amproc / pg_conversion (amendment pgarbor-catalog-09).
    OpClass,
    OpFamily,
    AmOp,
    AmProc,
    Conversion,
}

impl ObjClass {
    /// The pg_class OID of the catalog holding this class (pg_depend.classid).
    pub fn classid(self) -> Oid {
        use class_oid::*;
        match self {
            ObjClass::Relation => PG_CLASS,
            ObjClass::Type => PG_TYPE,
            ObjClass::Namespace => PG_NAMESPACE,
            ObjClass::Constraint => PG_CONSTRAINT,
            ObjClass::AttrDefault => PG_ATTRDEF,
            ObjClass::Proc => PG_PROC,
            ObjClass::Role => PG_AUTHID,
            ObjClass::Database => PG_DATABASE,
            ObjClass::Collation => PG_COLLATION,
            ObjClass::Operator => PG_OPERATOR,
            ObjClass::Cast => PG_CAST,
            ObjClass::Language => PG_LANGUAGE,
            ObjClass::AccessMethod => PG_AM,
            ObjClass::Extension => PG_EXTENSION,
            ObjClass::Rewrite => PG_REWRITE,
            ObjClass::Trigger => PG_TRIGGER,
            ObjClass::Tablespace => PG_TABLESPACE,
            ObjClass::OpClass => PG_OPCLASS,
            ObjClass::OpFamily => PG_OPFAMILY,
            ObjClass::AmOp => PG_AMOP,
            ObjClass::AmProc => PG_AMPROC,
            ObjClass::Conversion => PG_CONVERSION,
        }
    }

    /// Inverse of [`ObjClass::classid`].
    pub fn from_classid(classid: Oid) -> Option<ObjClass> {
        use class_oid::*;
        Some(match classid {
            PG_CLASS => ObjClass::Relation,
            PG_TYPE => ObjClass::Type,
            PG_NAMESPACE => ObjClass::Namespace,
            PG_CONSTRAINT => ObjClass::Constraint,
            PG_ATTRDEF => ObjClass::AttrDefault,
            PG_PROC => ObjClass::Proc,
            PG_AUTHID => ObjClass::Role,
            PG_DATABASE => ObjClass::Database,
            PG_COLLATION => ObjClass::Collation,
            PG_OPERATOR => ObjClass::Operator,
            PG_CAST => ObjClass::Cast,
            PG_LANGUAGE => ObjClass::Language,
            PG_AM => ObjClass::AccessMethod,
            PG_EXTENSION => ObjClass::Extension,
            PG_REWRITE => ObjClass::Rewrite,
            PG_TRIGGER => ObjClass::Trigger,
            PG_TABLESPACE => ObjClass::Tablespace,
            PG_OPCLASS => ObjClass::OpClass,
            PG_OPFAMILY => ObjClass::OpFamily,
            PG_AMOP => ObjClass::AmOp,
            PG_AMPROC => ObjClass::AmProc,
            PG_CONVERSION => ObjClass::Conversion,
            _ => return None,
        })
    }
}

/// PG `ObjectAddress`: (class, oid, sub-id). `sub_id` is the column attnum for column addresses
/// (`{Relation, table_oid, attnum}`), else 0.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectAddress {
    pub class: ObjClass,
    pub oid: Oid,
    #[serde(default)]
    pub sub_id: i32,
}

impl ObjectAddress {
    pub const fn new(class: ObjClass, oid: Oid) -> ObjectAddress {
        ObjectAddress { class, oid, sub_id: 0 }
    }
    pub const fn relation(oid: Oid) -> ObjectAddress {
        ObjectAddress::new(ObjClass::Relation, oid)
    }
    /// Address of column `attnum` of relation `rel`.
    pub const fn column(rel: Oid, attnum: i16) -> ObjectAddress {
        ObjectAddress { class: ObjClass::Relation, oid: rel, sub_id: attnum as i32 }
    }
    pub const fn type_(oid: Oid) -> ObjectAddress {
        ObjectAddress::new(ObjClass::Type, oid)
    }
    pub const fn namespace(oid: Oid) -> ObjectAddress {
        ObjectAddress::new(ObjClass::Namespace, oid)
    }
    pub const fn constraint(oid: Oid) -> ObjectAddress {
        ObjectAddress::new(ObjClass::Constraint, oid)
    }
    pub const fn proc(oid: Oid) -> ObjectAddress {
        ObjectAddress::new(ObjClass::Proc, oid)
    }
    /// The whole object (sub_id cleared): a column address → its relation.
    pub const fn whole(self) -> ObjectAddress {
        ObjectAddress { class: self.class, oid: self.oid, sub_id: 0 }
    }
    pub const fn is_column(&self) -> bool {
        matches!(self.class, ObjClass::Relation) && self.sub_id != 0
    }
    /// Pinned (system) objects can never be dropped: every OID below `FIRST_NORMAL_OID` except the
    /// bootstrap-mutable `public` namespace (2200). PG reports dropping them as 2BP01
    /// `cannot drop <desc> because it is required by the database system`.
    pub fn is_pinned(&self) -> bool {
        self.oid < pgarbor_common::FIRST_NORMAL_OID
            && !(self.class == ObjClass::Namespace && self.oid == crate::builtin::PUBLIC_NAMESPACE)
    }
}

/// `pg_depend.deptype`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DepType {
    /// 'n': normal: dropping the referenced object needs CASCADE (and drops the dependent).
    Normal,
    /// 'a': auto: the dependent is silently dropped with the referenced object (even under RESTRICT).
    Auto,
    /// 'i': internal: the dependent is part of the referenced object's implementation; dropping the
    /// dependent alone is refused (`cannot drop index t_pkey because constraint t_pkey on table t
    /// requires it`), dropping the owner drops it silently.
    Internal,
    /// 'P' / 'S': partition dependencies (later).
    PartitionPri,
    PartitionSec,
    /// 'e': extension membership.
    Extension,
    /// 'x': auto-extension.
    AutoExtension,
}

impl DepType {
    /// The pg_depend.deptype character.
    pub fn as_char(self) -> u8 {
        match self {
            DepType::Normal => b'n',
            DepType::Auto => b'a',
            DepType::Internal => b'i',
            DepType::PartitionPri => b'P',
            DepType::PartitionSec => b'S',
            DepType::Extension => b'e',
            DepType::AutoExtension => b'x',
        }
    }
}

/// DROP behavior.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DropBehavior {
    #[default]
    Restrict,
    Cascade,
}

/// An expression stored in the catalog (column default, generation expression, CHECK, index
/// expression/predicate, view query, function default argument).
///
/// pgarbor-catalog cannot depend on pgarbor-sql, so an expression is stored as:
/// - `sql`: the CANONICAL deparsed text, exactly what PG's `pg_get_expr(adbin, adrelid)` /
///   `pg_get_viewdef` print (e.g. `nextval('t_id_seq'::regclass)`, `'x'::text`, `(a * 2)`,
///   `(e > 0)` for a CHECK body; a view query in `pg_get_viewdef(oid)` format: `" SELECT id\n   FROM t;"`).
///   The binder produces this text (pgarbor-sql owns the deparser); the catalog only stores it.
/// - `bound`: optional opaque payload owned by pgarbor-sql (e.g. a serialized bound expression).
///   The catalog never interprets it. `None` means "re-bind `sql` when needed".
/// - `depends_on`: every object the expression references (columns as column addresses, sequences,
///   functions, types, other relations). These become pg_depend rows (`DepType::Normal`, except where
///   documented otherwise) and drive DROP RESTRICT/CASCADE.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredExpr {
    pub sql: String,
    #[serde(default)]
    pub bound: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<ObjectAddress>,
}

impl StoredExpr {
    /// An expression without dependencies (constants).
    pub fn new(sql: impl Into<String>) -> StoredExpr {
        StoredExpr { sql: sql.into(), bound: None, depends_on: Vec::new() }
    }
    /// Same expression with a dependency list.
    pub fn with_deps(mut self, deps: Vec<ObjectAddress>) -> StoredExpr {
        self.depends_on = deps;
        self
    }
}

/// Privilege bits (`AclMode`, PG `nodes/parsenodes.h`): bit i ↔ character i of [`ACL_ALL_RIGHTS_STR`].
pub mod acl_mode {
    pub const INSERT: u32 = 1 << 0; // a
    pub const SELECT: u32 = 1 << 1; // r
    pub const UPDATE: u32 = 1 << 2; // w
    pub const DELETE: u32 = 1 << 3; // d
    pub const TRUNCATE: u32 = 1 << 4; // D
    pub const REFERENCES: u32 = 1 << 5; // x
    pub const TRIGGER: u32 = 1 << 6; // t
    pub const EXECUTE: u32 = 1 << 7; // X
    pub const USAGE: u32 = 1 << 8; // U
    pub const CREATE: u32 = 1 << 9; // C
    pub const CREATE_TEMP: u32 = 1 << 10; // T
    pub const CONNECT: u32 = 1 << 11; // c
    pub const SET: u32 = 1 << 12; // s
    pub const ALTER_SYSTEM: u32 = 1 << 13; // A
    pub const MAINTAIN: u32 = 1 << 14; // m
    /// `ACL_ALL_RIGHTS_RELATION` (arwdDxtm).
    pub const ALL_RELATION: u32 = INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN;
    /// `ACL_ALL_RIGHTS_SEQUENCE` (rwU).
    pub const ALL_SEQUENCE: u32 = USAGE | SELECT | UPDATE;
    /// `ACL_ALL_RIGHTS_COLUMN` (arwx).
    pub const ALL_COLUMN: u32 = INSERT | SELECT | UPDATE | REFERENCES;
    pub const ALL_DATABASE: u32 = CREATE | CREATE_TEMP | CONNECT;
    pub const ALL_FUNCTION: u32 = EXECUTE;
    pub const ALL_LANGUAGE: u32 = USAGE;
    pub const ALL_SCHEMA: u32 = USAGE | CREATE;
    pub const ALL_TYPE: u32 = USAGE;
}

/// `ACL_ALL_RIGHTS_STR`: privilege letters in bit order.
pub const ACL_ALL_RIGHTS_STR: &str = "arwdDxtXUCTcsAm";

/// One `aclitem`: `grantee=privs/grantor` where a `*` after a letter marks WITH GRANT OPTION.
/// `grantee == 0` is PUBLIC (printed as an empty name: `=r/postgres`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AclItem {
    pub grantee: Oid,
    pub grantor: Oid,
    /// `acl_mode` bits.
    pub privileges: u32,
    /// Bits (subset of `privileges`) held WITH GRANT OPTION.
    pub grant_options: u32,
}

impl AclItem {
    /// PG `aclitemout`: `role_name(oid)` must return the role name already quoted as PG's
    /// `putid` does (double quotes when the name is not all `[A-Za-z0-9_]`). Example:
    /// `{grantee: 0, grantor: 10, privileges: SELECT}` → `=r/postgres`;
    /// `{grantee: 10, grantor: 10, privileges: USAGE|CREATE, grant_options: 0}` → `postgres=UC/postgres`.
    pub fn to_text(&self, role_name: &dyn Fn(Oid) -> String) -> String {
        let mut s = String::new();
        if self.grantee != 0 {
            s.push_str(&role_name(self.grantee));
        }
        s.push('=');
        for (i, ch) in ACL_ALL_RIGHTS_STR.chars().enumerate() {
            let bit = 1u32 << i;
            if self.privileges & bit != 0 {
                s.push(ch);
                if self.grant_options & bit != 0 {
                    s.push('*');
                }
            }
        }
        s.push('/');
        s.push_str(&role_name(self.grantor));
        s
    }
}

/// An ACL. `None` in an object means "default privileges" (PG's NULL acl column), which
/// `acldefault(objtype, owner)` expands when checking.
pub type Acl = Vec<AclItem>;

/// Key of a pg_description row (also the persisted description key, see `persist::keys`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DescriptionKey {
    pub objoid: Oid,
    pub classoid: Oid,
    pub objsubid: i32,
}

impl fmt::Display for ObjectAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}({}", self.class, self.oid)?;
        if self.sub_id != 0 {
            write!(f, ".{}", self.sub_id)?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acl_text() {
        let name = |o: Oid| if o == 10 { "postgres".to_string() } else { o.to_string() };
        let a = AclItem { grantee: 0, grantor: 10, privileges: acl_mode::SELECT, grant_options: 0 };
        assert_eq!(a.to_text(&name), "=r/postgres");
        let b = AclItem { grantee: 10, grantor: 10, privileges: acl_mode::USAGE | acl_mode::CREATE, grant_options: acl_mode::USAGE };
        assert_eq!(b.to_text(&name), "postgres=U*C/postgres");
        assert_eq!(ObjClass::from_classid(ObjClass::Constraint.classid()), Some(ObjClass::Constraint));
    }
}
