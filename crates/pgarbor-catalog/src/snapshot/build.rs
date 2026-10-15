//! Snapshot construction: merge builtins with persisted objects and build every secondary index of
//! `SnapshotIndexes`. Lease CAT-02.
//!
//! ## `CatalogSnapshot::build(version, data)`
//! 1. **Namespaces**: insert the builtin schemas from `builtin::NAMESPACES` EXCEPT `public` (2200)
//!    as `Namespace{oid, name, owner, acl: parse_acl_text(acl)}`; then every `data.namespaces`
//!    entry (persisted `public` included). `namespace_by_name` maps name → oid.
//! 2. **Roles**: builtin `builtin::ROLES` except `postgres` (10) as `Role` (password None,
//!    valid_until None, config empty), then `data.roles`. `role_by_name`.
//! 3. **Databases**: builtin `builtin::DATABASES` except `postgres` (5) as `Database`
//!    (acl parsed, locale from the row, config empty), then `data.databases`. `database_by_name`.
//! 4. **pg_class names** (`class_by_name`): every `builtin::SYSTEM_RELS` row
//!    ((namespace, name) → `Relation(oid)` for kinds r/v, `SystemIndex(oid)` for kind i); then user
//!    relations, indexes, sequences. A duplicate (namespace, name) among persisted objects is
//!    corruption: XX001 `duplicate catalog entry for relation "<ns oid>.<name>"`.
//! 5. **User types** (`type_by_name`, `user_types`): for each `TypeObj`: (ns, name) → oid
//!    (`UserTypeRef::Type`), and its array (ns, `array_name`) → array_type (`TypeArray(oid)`), where
//!    the array name is `obj.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&name))`
//!    (CAT-06; the writer stores a non-canonical name only after a collision).
//!    For each relation with `kind.has_rowtype()`: (ns, relname) → rowtype (`RowType(rel oid)`) and
//!    the array name (same rule) → array_type (`RowTypeArray(rel oid)`). A duplicate (ns, typname)
//!    among persisted objects → XX001 `duplicate catalog entry for type "<ns oid>.<name>"`.
//! 6. **Per-table lists**: `indexes_by_table` (index.table), `constraints_by_table`
//!    (constraint.table != 0), `constraints_by_domain` (constraint.domain != 0), `fks_referencing`
//!    (`fk.ref_table`), `sequences_by_owner` (`owned_by`), all vectors sorted ascending by OID.
//! 7. **Functions**: `functions_by_name` ((namespace, name) → oids sorted).
//! 8. **Reverse maps**: `enum_label_owner` (each `EnumLabelDef.oid` → type oid), `attrdef_owner`
//!    (each `Column.default.oid` → (rel, attnum)).
//! 9. **Validation (cheap, fail closed)**: every object's `namespace` exists, every index/constraint
//!    `table` exists, otherwise XX001 `catalog is corrupt: <kind> <oid> references missing <what> <oid>`.
//!    Unknown roles as owners are NOT errors (PG prints `unknown (OID=n)`).
//!
//! Complexity: O(n log n) in the number of objects; called once per catalog version.
//!
//! ## `parse_acl_text`
//! Parses PG `aclitem[]` text (`{postgres=UC/postgres,=U/postgres}`, also with `*` grant options and
//! double-quoted role names) into `Acl`, resolving role names through `role_oid`. `None`/empty input →
//! Ok(None); `{}` → Some(empty). Unknown role name → XX000 `unknown role "<name>" in builtin acl`.
use super::core::*;
use crate::data::CatalogData;
use crate::model::Acl;
use crate::persist::CatalogVersion;
use pgarbor_common::{Oid, PgError, Result};

impl CatalogSnapshot {
    /// Build a snapshot (see module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn build(version: CatalogVersion, data: CatalogData) -> Result<CatalogSnapshot> {
        Err(PgError::unimplemented("CAT-02", "catalog snapshot build"))
    }

    /// A snapshot with only builtin objects (no persisted rows, version ZERO): useful before bootstrap
    /// and in unit tests.
    pub fn builtin_only() -> Result<CatalogSnapshot> {
        CatalogSnapshot::build(CatalogVersion::ZERO, CatalogData::default())
    }
}

/// Parse PG aclitem[] text (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_acl_text(text: Option<&str>, role_oid: &dyn Fn(&str) -> Option<Oid>) -> Result<Option<Acl>> {
    Err(PgError::unimplemented("CAT-02", "aclitem[] text parsing"))
}
