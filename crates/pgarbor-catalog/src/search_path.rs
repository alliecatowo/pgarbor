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
use crate::builtin::PG_CATALOG_NAMESPACE;
use crate::errors;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};

/// PG `NAMEDATALEN - 1`: identifiers in `search_path` are truncated to this many bytes.
const MAX_IDENT_BYTES: usize = 63;
/// The special `$user` item.
const CURRENT_USER_ITEM: &str = "$user";
/// The special `pg_temp` item (the session's temp schema).
const TEMP_ITEM: &str = "pg_temp";

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
    pub fn parse(value: &str) -> Result<SearchPath> {
        match split_identifiers(value) {
            Some(items) => Ok(SearchPath { items }),
            None => Err(errors::invalid_search_path(value)),
        }
    }
}

/// Whitespace as PG's `scanner_isspace`: space, tab, newline, carriage return, vertical tab, form feed.
fn is_list_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Index of the first non-whitespace byte at or after `i` (`bytes.len()` if none).
fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && is_list_space(bytes[i]) {
        i += 1;
    }
    i
}

/// PG `SplitIdentifierString(value, ',')` plus `$user` recognition. Returns `None` on any syntax
/// error (the caller reports 22023). An empty or all-whitespace value is an empty list.
fn split_identifiers(value: &str) -> Option<Vec<SearchPathItem>> {
    let bytes = value.as_bytes();
    // A NUL byte cannot appear in a GUC value (PG C strings end there); reject it as bad syntax.
    if bytes.contains(&0) {
        return None;
    }
    let n = bytes.len();
    let mut items = Vec::new();
    let mut i = skip_space(bytes, 0);
    if i == n {
        return Some(items);
    }
    loop {
        // An item is required here: the value starts with one, or a comma was just consumed.
        if i >= n {
            return None;
        }
        let name = if bytes[i] == b'"' {
            let (name, next) = quoted_identifier(value, i)?;
            i = next;
            name
        } else {
            // Unquoted: runs to a separator or whitespace; folded to ASCII lower case.
            let start = i;
            while i < n && bytes[i] != b',' && !is_list_space(bytes[i]) {
                i += 1;
            }
            if i == start {
                return None;
            }
            value[start..i].to_ascii_lowercase()
        };
        items.push(classify(truncate_ident(name)));
        i = skip_space(bytes, i);
        if i == n {
            return Some(items);
        }
        if bytes[i] != b',' {
            return None;
        }
        i = skip_space(bytes, i + 1);
    }
}

/// Parse a double-quoted identifier whose opening quote is at byte `start`. Returns the name (case
/// kept, `""` collapsed to `"`) and the byte index just past the closing quote. An empty name is
/// invalid, and so is an unterminated quote.
fn quoted_identifier(value: &str, start: usize) -> Option<(String, usize)> {
    let bytes = value.as_bytes();
    let n = bytes.len();
    let mut name = String::new();
    // `seg` is the start of the not-yet-copied run; all indices sit next to ASCII quotes, so they
    // are char boundaries and the slices below cannot split a UTF-8 sequence.
    let mut seg = start + 1;
    let mut j = start + 1;
    loop {
        if j >= n {
            return None;
        }
        if bytes[j] == b'"' {
            name.push_str(&value[seg..j]);
            if j + 1 < n && bytes[j + 1] == b'"' {
                name.push('"');
                j += 2;
                seg = j;
            } else {
                break;
            }
        } else {
            j += 1;
        }
    }
    if name.is_empty() {
        return None;
    }
    Some((name, j + 1))
}

/// Truncate to `MAX_IDENT_BYTES` bytes on a character boundary (PG `pg_mbcliplen`).
fn truncate_ident(mut name: String) -> String {
    if name.len() > MAX_IDENT_BYTES {
        let mut cut = MAX_IDENT_BYTES;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
    }
    name
}

/// `$user` (unquoted or quoted) is the current-user item; everything else is a name.
fn classify(name: String) -> SearchPathItem {
    if name == CURRENT_USER_ITEM {
        SearchPathItem::CurrentUser
    } else {
        SearchPathItem::Name(name)
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
pub fn resolve_search_path(
    snapshot: &CatalogSnapshot,
    path: &SearchPath,
    current_user_name: &str,
    temp_namespace: Option<Oid>,
) -> Result<ResolvedSearchPath> {
    // explicit: items in order; missing schemas skipped; duplicates dropped (first wins).
    let mut explicit: Vec<Oid> = Vec::with_capacity(path.items.len());
    for item in &path.items {
        let found: Option<Oid> = match item {
            SearchPathItem::CurrentUser => snapshot.namespace_by_name(current_user_name).map(|ns| ns.oid),
            SearchPathItem::Name(name) if name == TEMP_ITEM => temp_namespace,
            SearchPathItem::Name(name) => snapshot.namespace_by_name(name).map(|ns| ns.oid),
        };
        if let Some(oid) = found {
            if !explicit.contains(&oid) {
                explicit.push(oid);
            }
        }
    }

    // effective: pg_catalog is implicitly searched first unless listed explicitly.
    let mut effective: Vec<Oid> = Vec::with_capacity(explicit.len() + 1);
    if !explicit.contains(&PG_CATALOG_NAMESPACE) {
        effective.push(PG_CATALOG_NAMESPACE);
    }
    effective.extend_from_slice(&explicit);

    // effective_for_relations: the temp schema is searched first for relations when it exists and
    // was not listed explicitly (PG `activeTempCreationPending` rules); otherwise identical.
    let mut effective_for_relations: Vec<Oid> = Vec::with_capacity(effective.len() + 1);
    if let Some(temp) = temp_namespace {
        if !explicit.contains(&temp) {
            effective_for_relations.push(temp);
        }
    }
    effective_for_relations.extend_from_slice(&effective);

    // PG `firstNS = linitial(oidlist)`: the first explicit schema, which may be pg_catalog.
    let creation_namespace = explicit.first().copied();

    Ok(ResolvedSearchPath { explicit, effective, effective_for_relations, creation_namespace, temp_namespace })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn items(value: &str) -> Vec<SearchPathItem> {
        SearchPath::parse(value).expect("valid search_path").items
    }

    fn name(s: &str) -> SearchPathItem {
        SearchPathItem::Name(s.to_string())
    }

    #[test]
    fn default_text_parses_to_default() {
        assert_eq!(SearchPath::parse(SearchPath::DEFAULT_TEXT).expect("default"), SearchPath::default());
        assert_eq!(SearchPath::parse("\"$user\", public").expect("default"), SearchPath::default());
    }

    #[test]
    fn unquoted_names_fold_and_quoted_keep_case() {
        assert_eq!(items("A, \"B\""), vec![name("a"), name("B")]);
        assert_eq!(items("  Public  ,\tMixed_Case  "), vec![name("public"), name("mixed_case")]);
        assert_eq!(items(r#""a""b""#), vec![name("a\"b")]);
        assert_eq!(items(r#""x y", z"#), vec![name("x y"), name("z")]);
    }

    #[test]
    fn current_user_item_forms() {
        assert_eq!(items("$user"), vec![SearchPathItem::CurrentUser]);
        assert_eq!(items("\"$user\""), vec![SearchPathItem::CurrentUser]);
        // Quoted names do not fold case, so this is a plain schema name.
        assert_eq!(items("\"$USER\""), vec![name("$USER")]);
    }

    #[test]
    fn empty_and_blank_values_are_empty_lists() {
        assert!(items("").is_empty());
        assert!(items("   \t ").is_empty());
    }

    #[test]
    fn syntax_errors_are_rejected() {
        for bad in ["a,,b", "a,", ",a", "\"abc", "\"\"", "\"a\"b", "a b", "a\0b", "\"a\"\"", ","] {
            assert!(SearchPath::parse(bad).is_err(), "expected syntax error for {bad:?}");
        }
    }

    #[test]
    fn identifiers_truncate_to_63_bytes() {
        let long = "a".repeat(70);
        assert_eq!(items(&long), vec![name(&"a".repeat(63))]);
        // 40 two-byte chars = 80 bytes; cut lands on a boundary at 62 bytes (31 chars).
        let multi = "é".repeat(40);
        assert_eq!(items(&multi), vec![name(&"é".repeat(31))]);
    }

    #[test]
    fn resolve_default_path_on_bootstrapped_snapshot() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let r = resolve_search_path(env.snapshot(), &SearchPath::default(), "postgres", None).expect("resolve");
        // `$user` names no schema for role postgres, so only public remains.
        assert_eq!(r.explicit, vec![2200]);
        assert_eq!(r.effective, vec![PG_CATALOG_NAMESPACE, 2200]);
        assert_eq!(r.effective_for_relations, vec![PG_CATALOG_NAMESPACE, 2200]);
        assert_eq!(r.creation_namespace, Some(2200));
        assert_eq!(r.temp_namespace, None);
    }

    #[test]
    fn current_user_maps_to_same_named_schema() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let r = resolve_search_path(env.snapshot(), &SearchPath::parse("$user").expect("p"), "public", None)
            .expect("resolve");
        assert_eq!(r.explicit, vec![2200]);
    }

    #[test]
    fn empty_path_has_no_creation_namespace() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let r = resolve_search_path(env.snapshot(), &SearchPath::parse("").expect("p"), "postgres", None)
            .expect("resolve");
        assert!(r.explicit.is_empty());
        assert_eq!(r.effective, vec![PG_CATALOG_NAMESPACE]);
        assert_eq!(r.creation_namespace, None);
    }

    #[test]
    fn missing_and_duplicate_schemas_are_dropped() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let p = SearchPath::parse("nope, public, public, pg_catalog, missing").expect("p");
        let r = resolve_search_path(env.snapshot(), &p, "postgres", None).expect("resolve");
        assert_eq!(r.explicit, vec![2200, PG_CATALOG_NAMESPACE]);
        // pg_catalog is listed, so it is not prepended again.
        assert_eq!(r.effective, vec![2200, PG_CATALOG_NAMESPACE]);
        assert_eq!(r.creation_namespace, Some(2200));
    }

    #[test]
    fn pg_catalog_first_is_creation_namespace() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let p = SearchPath::parse("pg_catalog, public").expect("p");
        let r = resolve_search_path(env.snapshot(), &p, "postgres", None).expect("resolve");
        assert_eq!(r.creation_namespace, Some(PG_CATALOG_NAMESPACE));
        assert_eq!(r.effective, vec![PG_CATALOG_NAMESPACE, 2200]);
    }

    #[test]
    fn pg_temp_uses_temp_namespace_when_present() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let temp: Oid = 9999;

        // Listed first: temp is the creation namespace and is not added again for relations.
        let p = SearchPath::parse("pg_temp, public").expect("p");
        let r = resolve_search_path(env.snapshot(), &p, "postgres", Some(temp)).expect("resolve");
        assert_eq!(r.explicit, vec![temp, 2200]);
        assert_eq!(r.creation_namespace, Some(temp));
        assert_eq!(r.effective, vec![PG_CATALOG_NAMESPACE, temp, 2200]);
        assert_eq!(r.effective_for_relations, vec![PG_CATALOG_NAMESPACE, temp, 2200]);
        assert_eq!(r.temp_namespace, Some(temp));

        // Not listed: temp is searched first for relations only.
        let p = SearchPath::parse("public").expect("p");
        let r = resolve_search_path(env.snapshot(), &p, "postgres", Some(temp)).expect("resolve");
        assert_eq!(r.explicit, vec![2200]);
        assert_eq!(r.effective, vec![PG_CATALOG_NAMESPACE, 2200]);
        assert_eq!(r.effective_for_relations, vec![temp, PG_CATALOG_NAMESPACE, 2200]);
        assert_eq!(r.creation_namespace, Some(2200));
    }

    #[test]
    fn pg_temp_skipped_when_no_temp_schema() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let p = SearchPath::parse("pg_temp, public").expect("p");
        let r = resolve_search_path(env.snapshot(), &p, "postgres", None).expect("resolve");
        assert_eq!(r.explicit, vec![2200]);
        assert_eq!(r.creation_namespace, Some(2200));
        assert_eq!(r.temp_namespace, None);
    }
}
