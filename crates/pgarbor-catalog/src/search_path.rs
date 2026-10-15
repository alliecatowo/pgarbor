//! `search_path` parsing and resolution (PG `catalog/namespace.c` `recomputeNamespacePath`,
//! `utils/adt/varlena.c` `SplitIdentifierString`). Lease CAT-03.
//!
//! ## Parsing (`SearchPath::parse`)
//! The GUC value is a comma-separated identifier list (PG `SplitIdentifierString(value, ',')`):
//! - whitespace around items is ignored; unquoted items are DOWN-CASED (ASCII only, like PG's
//!   `downcase_identifier` for single-byte chars) and truncated to 63 bytes;
//! - `"Quoted"` items keep case; `""` inside quotes is a literal `"`; an empty quoted item `""`
//!   is invalid;
//! - the item `$user` (unquoted or quoted `"$user"`) means the current user's same-named schema;
//! - empty string → empty list (valid: nothing in path, CREATE fails with 3F000
//!   `no schema has been selected to create in`);
//! - syntax errors (empty item like `a,,b`, unterminated quote, junk after a quoted item) →
//!   22023 `invalid value for parameter "search_path": "<value>"` + DETAIL `List syntax is invalid.`
//!   (`errors::invalid_search_path`).
//! Examples: `"$user", public` → [CurrentUser, Name("public")]; `A, "B"` → [Name("a"), Name("B")];
//! `pg_temp, public` → [Name("pg_temp"), Name("public")].
//!
//! ## Resolution (`resolve_search_path`)
//! Walk the items in order and build `explicit`:
//! - `CurrentUser` → the schema named exactly like the current role, if it exists;
//! - `Name("pg_temp")` → the session temp schema if one exists (`temp_namespace`), else skipped
//!   (PG would create it lazily on first temp object; later work);
//! - `Name(n)` → the schema `n` if it exists; nonexistent schemas are silently skipped;
//! - duplicates are skipped (first occurrence wins).
//! (Privilege checks — USAGE on the schema — are later work; v1 treats every schema as usable.)
//! `creation_namespace` = the first element of `explicit` (PG `firstNS = linitial(oidlist)`). It CAN
//! be pg_catalog when listed first; creating there then fails with 42501
//! `permission denied to create "pg_catalog.x"`. If `pg_temp` is first, the temp schema is the creation
//! namespace. None if `explicit` is empty.
//! `effective` = `explicit` with implicit additions in front: if `pg_catalog` is not in `explicit`
//! it is prepended; if a temp schema exists and is not in `explicit`, it is prepended before that
//! (relations only: `effective_for_relations`; types/functions/operators never search the temp
//! schema implicitly, PG `activeTempCreationPending` rules).
//! `current_schemas(false)` = `explicit` names; `current_schemas(true)` = `effective` names
//! (PG 18: `SELECT current_schemas(true)` with default path → `{pg_catalog,public}`).
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};

/// One parsed search_path element.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SearchPathItem {
    /// `$user`
    CurrentUser,
    /// A schema name (already case-folded / unquoted).
    Name(String),
}

/// A parsed `search_path` setting.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SearchPath {
    pub items: Vec<SearchPathItem>,
}

impl Default for SearchPath {
    /// PG default `"$user", public`.
    fn default() -> SearchPath {
        SearchPath { items: vec![SearchPathItem::CurrentUser, SearchPathItem::Name("public".into())] }
    }
}

impl SearchPath {
    /// The PG default GUC text.
    pub const DEFAULT_TEXT: &'static str = "\"$user\", public";

    /// Parse the GUC value (see module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn parse(value: &str) -> Result<SearchPath> {
        Err(PgError::unimplemented("CAT-03", "search_path parsing"))
    }
}

/// A search path resolved against one snapshot and role.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedSearchPath {
    /// Existing schemas named by the setting, in order, deduplicated.
    pub explicit: Vec<Oid>,
    /// Search order for types, functions, operators, collations (implicit pg_catalog included).
    pub effective: Vec<Oid>,
    /// Search order for relations (temp schema first when it exists, then `effective`).
    pub effective_for_relations: Vec<Oid>,
    /// Where unqualified CREATE puts objects.
    pub creation_namespace: Option<Oid>,
    /// The session's temp schema, if any.
    pub temp_namespace: Option<Oid>,
}

impl ResolvedSearchPath {
    /// Builtin-only path used before a session exists: [pg_catalog, public].
    pub fn builtin_default() -> ResolvedSearchPath {
        ResolvedSearchPath {
            explicit: vec![crate::builtin::PUBLIC_NAMESPACE],
            effective: vec![crate::builtin::PG_CATALOG_NAMESPACE, crate::builtin::PUBLIC_NAMESPACE],
            effective_for_relations: vec![crate::builtin::PG_CATALOG_NAMESPACE, crate::builtin::PUBLIC_NAMESPACE],
            creation_namespace: Some(crate::builtin::PUBLIC_NAMESPACE),
            temp_namespace: None,
        }
    }
    /// Position of `ns` in `effective` (used as `FuncCandidate::path_pos`), None if not searched.
    pub fn position(&self, ns: Oid) -> Option<u32> {
        self.effective.iter().position(|n| *n == ns).map(|p| p as u32)
    }
}

/// Resolve `path` for `current_user_name` (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn resolve_search_path(
    snapshot: &CatalogSnapshot,
    path: &SearchPath,
    current_user_name: &str,
    temp_namespace: Option<Oid>,
) -> Result<ResolvedSearchPath> {
    Err(PgError::unimplemented("CAT-03", "search_path resolution"))
}
