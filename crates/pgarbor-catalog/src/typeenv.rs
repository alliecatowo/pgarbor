//! `pgarbor_types::TypeEnv` for [`CatalogEnv`]: builtin types from the compiled-in registry plus
//! user types (enums, domains, table/view/composite rowtypes, and their array types) from the
//! snapshot; reg* name lookups. Lease CAT-05.
//!
//! The frozen `TypeEnv` trait returns `Option`, so the trait methods (implemented below, keep them)
//! delegate to the `Result`-returning inherent methods this lease implements and map `Err` to
//! `None` / `Err` as the trait allows. Builtin OIDs never reach the leased code
//! (`registry::lookup_type` answers them first).
//!
//! ## `user_type_info(oid)` — synthesize a `TypeInfo` exactly like PG's pg_type row
//! (oracle-verified shapes; see `contracts/reference/pg18/pg_type.csv` for builtin analogues such as
//! `pg_class` (83) / `_pg_class` (273)):
//! - **enum** (`TypeObj` Enum): name, namespace, len 4, byval true, typtype b'e', category b'E',
//!   preferred false, delim b',', relid 0, elem 0, array = obj.array_type, input `enum_in`, output
//!   `enum_out`, receive `enum_recv`, send `enum_send`, align b'i', storage b'p', basetype 0,
//!   typmod -1, ndims 0, collation 0.
//! - **domain**: typtype b'd'; category, len, byval, align, storage, delim, output and send copied
//!   from the BASE type's info (recursively resolved through `self.type_info(base)`); input
//!   `domain_in`, receive `domain_recv`; basetype = base.oid, typmod = base.typmod, ndims, collation =
//!   the domain's collation; array = obj.array_type; preferred false.
//!   (Oracle: `information_schema.sql_identifier` = len 64, byval f, category S, output `nameout`,
//!   send `namesend`, align c, storage p, basetype 19, collation 950.)
//! - **rowtype** of a relation: name = relname, namespace = rel namespace, len -1, byval false,
//!   typtype b'c', category b'C', relid = rel oid, elem 0, array = rel.array_type, input `record_in`,
//!   output `record_out`, receive `record_recv`, send `record_send`, align b'd', storage b'x',
//!   collation 0.
//! - **array** of any of the above: name = stored/canonical array name, len -1, byval false,
//!   typtype b'b', category b'A', delim b',', elem = element OID, array 0, input `array_in`,
//!   output `array_out`, receive `array_recv`, send `array_send`, align = b'd' if the element's align
//!   is b'd' else b'i', storage b'x', collation = element's collation.
//!   The `&'static str` routine names are string literals (`"enum_in"`, ...).
//! - Unknown OID → `Ok(None)`.
//!
//! ## `type_oid_by_name(schema, typname)` → `self.lookup_type(schema, typname)` (CAT-04).
//!
//! ## `enum_labels_of(oid)` → labels sorted by `sort_order` as `pgarbor_types::context::EnumLabel`;
//! `Ok(None)` if not an enum. `composite_fields_of(type_oid)`: the rowtype's relation columns in
//! attnum order as `CompositeField{name, type_id, dropped}` (builtin catalog rowtypes too: e.g.
//! type 83 `pg_class` → the 34 pg_class columns); `Ok(None)` if not a composite type.
//!
//! ## reg* (`reg_lookup_impl`, `reg_name_impl`), PG `utils/adt/regproc.c`
//! Input names are raw text as typed in the literal: `'public.t'::regclass`, `'"Mixed"'::regclass`.
//! Split with PG `stringToQualifiedNameList` rules (dots outside quotes separate, unquoted parts are
//! down-cased, quotes stripped; > 2 parts for objects in this database → 42601 / `improper
//! qualified name (too many dotted names): a.b.c.d` (42601)). Then:
//! - `Class` → `lookup_class` (any pg_class kind); `Type` → `pgarbor_types::names::parse_type_string(self, s)`
//!   (handles `integer`, `character varying(10)[]`, `public.mood`); `Proc` → unique function by name
//!   (more than one → 42725 `more than one function named "<name>"`); `Procedure` → `name(type, ...)`
//!   parsed with type names through `parse_type_string`, exact IN-arg match; `Oper` → unique builtin
//!   operator by name (42725 `more than one operator named <name>`); `Operator` → `op(left,right)`
//!   with `NONE` for a missing side; `Namespace` → schema; `Role` → role; `Collation` → collation;
//!   `Config`, `Dictionary` → `Err(PgError::unimplemented("CAT-05", "text search configuration lookup"))`
//!   (text search is not supported in v1).
//! - `Ok(None)` when not found (the types crate raises the PG error).
//! Output (`reg_name_impl`): `Class` → `quote_identifier(relname)`, schema-qualified when not
//! visible (`qualified_class_name`); `Type` → `pgarbor_types::names::format_type(self, oid, -1)`;
//! `Proc` → proname, qualified when not visible; `Procedure` → `name(argtype,argtype)` with
//! `format_type` names joined by `,` (no space, PG `format_procedure`); `Oper` → oprname;
//! `Operator` → `+(integer,integer)`; `Namespace`/`Role`/`Collation` → quoted name (collation
//! `"default"` is printed quoted as PG does). `Ok(None)` when the OID does not exist.
use crate::env::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::context::{CompositeField, EnumLabel, RegKind, TypeEnv};
use pgarbor_types::registry::{self, TypeInfo};
use std::sync::Arc;

impl CatalogEnv {
    /// `TypeInfo` of a USER type OID (module docs). Builtins are not passed here.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn user_type_info(&self, oid: Oid) -> Result<Option<TypeInfo>> {
        Err(PgError::unimplemented("CAT-05", "user type metadata"))
    }

    /// Enum labels in sort order, or None if `oid` is not a (user) enum.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn enum_labels_of(&self, oid: Oid) -> Result<Option<Arc<[EnumLabel]>>> {
        Err(PgError::unimplemented("CAT-05", "enum labels"))
    }

    /// Fields of a composite type (user or builtin rowtype), or None.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn composite_fields_of(&self, type_oid: Oid) -> Result<Option<Arc<[CompositeField]>>> {
        Err(PgError::unimplemented("CAT-05", "composite type fields"))
    }

    /// reg* input lookup (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn reg_lookup_impl(&self, kind: RegKind, name: &str) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-05", "reg* name lookup"))
    }

    /// reg* output name (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn reg_name_impl(&self, kind: RegKind, oid: Oid) -> Result<Option<String>> {
        Err(PgError::unimplemented("CAT-05", "reg* output"))
    }
}

impl TypeEnv for CatalogEnv {
    fn type_info(&self, oid: Oid) -> Option<TypeInfo> {
        if let Some(t) = registry::lookup_type(oid) {
            return Some(TypeInfo::from(t));
        }
        self.user_type_info(oid).ok().flatten()
    }
    fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid> {
        self.lookup_type(schema, typname).ok().flatten()
    }
    fn enum_labels(&self, oid: Oid) -> Option<Arc<[EnumLabel]>> {
        self.enum_labels_of(oid).ok().flatten()
    }
    fn composite_fields(&self, type_oid: Oid) -> Option<Arc<[CompositeField]>> {
        self.composite_fields_of(type_oid).ok().flatten()
    }
    fn reg_lookup(&self, kind: RegKind, name: &str) -> Result<Option<Oid>> {
        self.reg_lookup_impl(kind, name)
    }
    fn reg_name(&self, kind: RegKind, oid: Oid) -> Option<String> {
        self.reg_name_impl(kind, oid).ok().flatten()
    }
}
