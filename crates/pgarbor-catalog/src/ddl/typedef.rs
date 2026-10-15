//! User-defined types: enums, domains, composite types. Lease CAT-18. PG sources:
//! `commands/typecmds.c` (`DefineEnum`, `DefineDomain`, `AlterDomain*`, `DefineCompositeType`,
//! `AlterEnum`, `RenameType`, `AlterTypeNamespace`, `AlterTypeOwner`), `catalog/pg_enum.c`
//! (`EnumValuesCreate`, `AddEnumLabel`, `RenameEnumLabel`, `RenumberEnumType`), `catalog/pg_type.c`
//! (`TypeCreate`, `moveArrayTypeName`), `commands/tablecmds.c` (`DefineRelation` for composites).
//!
//! Type name space: types, domains, enums, relation rowtypes and ALL array types share one
//! (namespace, typname) space -> `ddl::common::resolve_type_name_clash` before creating
//! (42710 `type "x" already exists`; an auto array type in the way is renamed, oracle-verified).
//! Every type gets an array type (`TypeObj::array_type`, name `naming::choose_array_type_name`
//! stored in `array_type_name` only when non-canonical).
//!
//! ## Enums
//! `create_enum`: label validation - each label at most 63 bytes (42602
//! `errors::invalid_enum_label`, DETAIL `Labels must be 63 bytes or less.`); EMPTY labels are accepted (oracle: `create type e as enum ('')`
//! works); duplicate labels in the list -> 23505 `duplicate key value violates unique constraint
//! "pg_enum_typid_label_index"` DETAIL `Key (enumtypid, enumlabel)=(<type oid>, a) already exists.`
//! (PG relies on the unique index; replicate exactly). `sort_order` = 1, 2, 3, ... as f32; each label
//! gets its own `new_oid`.
//! `add_enum_value(type, label, position, if_not_exists)`: type must be an enum (42809 `"t" is not
//! an enum`: `errors::not_an_enum`), label length rule, existing label -> 42710
//! `errors::enum_label_already_exists` (NOTICE `..., skipping` + `Ok(false)` with `if_not_exists`);
//! position `Append` -> `max(sort_order) + 1` (1 when the enum is empty); `Before(x)`/`After(x)`: x must
//! exist (22023 `errors::not_an_existing_enum_label`); PG `AddEnumLabel`: let `nbr` be x's index and
//! `other` the adjacent label on the requested side; no `other` below -> `nbr.sort_order - 1`; no
//! `other` above -> `nbr.sort_order + 1`; otherwise the f32 midpoint `(nbr + other) / 2` computed in
//! f32; if the midpoint equals either neighbour (precision exhausted) renumber ALL labels 1.0..n
//! (`RenumberEnumType`) and retry. Oracle: labels `''(1), a(2)`, `add value 'z' before 'a'` ->
//! sort order 1.5; `add value 'ab' after 'a'` (a last) -> 3. Labels are kept sorted by `sort_order`.
//! `rename_enum_value(type, old, new)`: old must exist (22023 `"x" is not an existing enum label`),
//! new must not (42710 `enum label "b" already exists`), new length rule.
//!
//! ## Domains
//! `create_domain`: base type must exist (binder); base cannot be a pseudo-type (42804? PG:
//! `"x" is not a valid base type for a domain` 42804) or another domain's... (domains over domains
//! are allowed). Constraint naming (oracle PG 18.6): CHECKs `d_check`, `d_check1`, ... via
//! `naming::choose_constraint_name(ns, domain, None, "check")`; NOT NULL `d_not_null`; explicit names
//! kept. Domain constraints are `Constraint{domain = type oid, table = 0}`; `TypeDef::Domain.not_null`
//! mirrors the NOT NULL constraint. `typcollation` = explicit COLLATE or the base type's (only for
//! collatable bases; else 42804 `collations are not supported by type integer`).
//! `NOT VALID` domain CHECK -> `validated = false`.
//! `alter_domain_default(domain, Option<StoredExpr>)`, `alter_domain_not_null(domain, set)` (SET NOT NULL
//! on a domain used in tables queues a validation: `FollowUp::ValidateNotNull` is table/column
//! specific, so the catalog returns the list of (table, attnum) columns of this domain type via
//! `domain_columns` and the executor validates: PG error 23502 `column "c" of table "t" contains
//! null values`), `add_domain_constraint`, `drop_domain_constraint(domain, name, missing_ok,
//! behavior)` (42704 `constraint "nosuch" of domain "d" does not exist`; skipping NOTICE
//! `constraint "nosuch" of domain "d" does not exist, skipping`),
//! `rename_domain_constraint`, `validate_domain_constraint`. Duplicate constraint name on one domain ->
//! 42710 `constraint "c3" for domain "d" already exists`. A non-domain target ->
//! 42809 `e is not a domain` (`errors::not_a_domain`, unquoted).
//!
//! ## Composite types
//! `create_composite`: a `Relation{kind: CompositeType, columns, rowtype, array_type}` (no storage,
//! no system columns). Column rules as CREATE TABLE (names unique, <= 1600, not pseudo-types);
//! no defaults/constraints.
//! `ALTER TYPE c ADD/DROP/RENAME ATTRIBUTE` and `ALTER ATTRIBUTE ... TYPE` are
//! `alter_table::{add_column, drop_column, rename_column, alter_column_type}` on the composite's
//! relation OID. For ADD/DROP/ALTER TYPE they additionally queue `FollowUp::RequireEmptyTable` for every
//! table column declared with the composite type (oracle PG 18.6: `create type c as (a int, b text);
//! create table u(c c); alter type c add attribute z int; alter type c drop attribute a; alter type c
//! rename attribute b to bb` ALL succeed while `u` is empty; PG refuses with 0A000 `cannot alter type
//! "c" because column "u.c" uses it` only when `u` has rows, which only the executor can see).
//! RENAME ATTRIBUTE needs no check. `DROP TYPE c` with `u(c c)` present -> 2BP01 `cannot drop type c
//! because other objects depend on it` DETAIL `column c of table u depends on type c`.
//!
//! ## Renaming / moving / owner
//! `rename_type`, `set_type_schema`, `alter_type_owner` apply to enum/domain `TypeObj`s; for a
//! relation rowtype OID they delegate to the relation functions (ALTER TYPE on a table rowtype is
//! 42809 `"t" is a table's row type` HINT `Use ALTER TABLE instead.`; composite types OK).
//! Renaming re-chooses the array type name (`array_type_name` reset/recomputed after moving a
//! clashing array out of the way, PG `RenameTypeInternal`). Owner change rewrites the ACL
//! (`acl::aclnewowner`) and requires CREATE on the schema for the new owner. `SET SCHEMA` checks the
//! target schema for name clashes of the type and its array. Stored expressions mentioning the
//! type are rewritten through `ExprRewriter` (`NameChange::Type`).
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

/// Where `ALTER TYPE ... ADD VALUE` puts the new label.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumPosition {
    /// After the current last label.
    Append,
    Before(String),
    After(String),
}

impl<'a> CatalogTxn<'a> {
    /// CREATE TYPE ... AS ENUM. Returns the type OID.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_enum(&mut self, spec: &CreateEnumSpec) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-18", "CREATE TYPE AS ENUM"))
    }

    /// ALTER TYPE ... ADD VALUE. `Ok(false)` when skipped by `if_not_exists`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_enum_value(&mut self, ty: Oid, label: &str, position: &EnumPosition, if_not_exists: bool) -> Result<bool> {
        Err(PgError::unimplemented("CAT-18", "ALTER TYPE ADD VALUE"))
    }

    /// ALTER TYPE ... RENAME VALUE.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_enum_value(&mut self, ty: Oid, old: &str, new: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER TYPE RENAME VALUE"))
    }

    /// CREATE DOMAIN. Returns the type OID.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_domain(&mut self, spec: &CreateDomainSpec) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-18", "CREATE DOMAIN"))
    }

    /// ALTER DOMAIN ... SET DEFAULT expr / DROP DEFAULT (`None`).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_domain_default(&mut self, domain: Oid, default: Option<StoredExpr>) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN default"))
    }

    /// ALTER DOMAIN ... SET NOT NULL / DROP NOT NULL. Returns, for SET NOT NULL, the (table, attnum)
    /// columns of this domain type that the executor must validate (module docs); empty otherwise.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_domain_not_null(&mut self, domain: Oid, set: bool) -> Result<Vec<(Oid, i16)>> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN NOT NULL"))
    }

    /// ALTER DOMAIN ... ADD CONSTRAINT. Returns the constraint OID and, unless NOT VALID, the
    /// (table, attnum) columns to validate.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_domain_constraint(&mut self, domain: Oid, spec: &DomainConstraintSpec) -> Result<(Oid, Vec<(Oid, i16)>)> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN ADD CONSTRAINT"))
    }

    /// ALTER DOMAIN ... DROP CONSTRAINT. `Ok(false)` when skipped by `missing_ok`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_domain_constraint(&mut self, domain: Oid, name: &str, missing_ok: bool, behavior: DropBehavior) -> Result<bool> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN DROP CONSTRAINT"))
    }

    /// ALTER DOMAIN ... RENAME CONSTRAINT.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_domain_constraint(&mut self, domain: Oid, old: &str, new: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN RENAME CONSTRAINT"))
    }

    /// ALTER DOMAIN ... VALIDATE CONSTRAINT. Returns the columns to validate.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn validate_domain_constraint(&mut self, domain: Oid, name: &str) -> Result<Vec<(Oid, i16)>> {
        Err(PgError::unimplemented("CAT-18", "ALTER DOMAIN VALIDATE CONSTRAINT"))
    }

    /// CREATE TYPE name AS (attrs). Returns the composite's relation OID (= its `typrelid`).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_composite(&mut self, spec: &CreateCompositeSpec) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-18", "CREATE TYPE AS composite"))
    }

    /// ALTER TYPE/DOMAIN ... RENAME TO.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_type(&mut self, ty: Oid, new_name: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER TYPE RENAME"))
    }

    /// ALTER TYPE/DOMAIN ... SET SCHEMA.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_type_schema(&mut self, ty: Oid, new_schema: Oid, rewriter: &dyn ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER TYPE SET SCHEMA"))
    }

    /// ALTER TYPE/DOMAIN ... OWNER TO.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_type_owner(&mut self, ty: Oid, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-18", "ALTER TYPE OWNER"))
    }

    /// (table, attnum) of every non-dropped column whose declared type is the domain `domain`
    /// (or an array of it) - used for validation lists.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn domain_columns(&mut self, domain: Oid) -> Result<Vec<(Oid, i16)>> {
        Err(PgError::unimplemented("CAT-18", "domain column lookup"))
    }
}
