//! Automatic object naming, PG-exact (PG `commands/indexcmds.c` `makeObjectName`,
//! `ChooseRelationName`, `ChooseIndexName`, `ChooseIndexNameAddition`, `ChooseIndexColumnNames`;
//! `catalog/pg_constraint.c` `ChooseConstraintName`; `commands/tablecmds.c`
//! `ChooseForeignKeyConstraintNameAddition`; `catalog/pg_type.c` `makeArrayTypeName`;
//! `catalog/heap.c` CHECK / NOT NULL naming). Lease CAT-06.
//!
//! Oracle fixtures (PG 18.6, table `t`):
//! - serial column `id` → sequence `t_id_seq`; identity column `d` → `t_d_seq`
//! - PRIMARY KEY → index and constraint `t_pkey`
//! - UNIQUE (a) → `t_a_key`; UNIQUE (a, b) → `t_a_b_key`
//! - CHECK on column e (column constraint, or table constraint whose expression's FIRST referenced
//!   column is e) → `t_e_check`; CHECK referencing no column → `t_check`
//! - NOT NULL on column a → `t_a_not_null`; domain `pos` NOT NULL → `pos_not_null`, domain CHECK →
//!   `pos_check`
//! - FOREIGN KEY (f) → `t_f_fkey`; FOREIGN KEY (a, b) → `t_a_b_fkey`
//! - CREATE INDEX ON t (lower(b)) → `t_lower_idx`; ON t ((a+1), (a+2)) → `t_expr_expr1_idx`;
//!   ON t (a) → `t_a_idx`; second identical → `t_a_idx1`
//! - index attribute names: column name; function call → function name (`lower`); any other
//!   expression → `expr`; duplicates get a numeric suffix (`expr`, `expr1`, `expr2`)
//! - array type of `mood` → `_mood`; on collision `_mood_1`, `_mood_2`, ... (makeArrayTypeName, see
//!   `choose_array_type_name`; oracle: `create type e as enum('a'); create type _e as enum('a')` leaves
//!   e's array as `__e` (moved out of the way, named from the NEW type's name) and gives `_e`'s own
//!   array the name `__e_1`)
//!
//! `make_object_name(name1, name2, label)` (PG makeObjectName): result = name1 [`_` name2] [`_` label],
//! fitting in 63 bytes by truncating name1 and/or name2 (never the label). Exactly PG's loop:
//! `overhead = (name2 ? 1 : 0) + (label ? len(label) + 1 : 0)`, `avail = 63 - overhead`; while
//! `n1 + n2 > avail { if n1 > n2 { n1 -= 1 } else { n2 -= 1 } }` (so name2 shrinks on ties), then
//! clip each part back to a UTF-8 character boundary (`pg_mbcliplen`). The string is
//! `name1[..n1]` + (`_` + `name2[..n2]` if name2) + (`_` + label if label). With `name1 = ""` and a
//! name2 (array type names) the result starts with `_`.
//!
//! Uniqueness loops: `choose_relation_name` tries `label`, then `label1`, `label2`, ... (appended to the
//! label: `t_a_idx1`) until no pg_class object in `namespace` has that name and the name is not in
//! `others` (names chosen earlier in the same statement but not yet in the snapshot).
//! `choose_constraint_name` does the same against constraint names in `namespace` (PG checks
//! pg_constraint by (conname, connamespace) — NOT per table).
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PG NAMEDATALEN - 1: maximum identifier length in bytes.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// Truncate to at most 63 bytes on a UTF-8 character boundary (PG `truncate_identifier` /
/// `pg_mbcliplen`). The NOTICE `identifier "..." will be truncated to "..."` (42622) is the
/// parser's job, not this function's.
pub fn truncate_identifier(name: &str) -> String {
    if name.len() <= MAX_IDENTIFIER_BYTES {
        return name.to_string();
    }
    let mut end = MAX_IDENTIFIER_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// Canonical array type name: `_` + name, truncated to 63 bytes (no collision handling; see
/// `choose_array_type_name`).
pub fn array_type_name(name: &str) -> String {
    truncate_identifier(&format!("_{name}"))
}

/// PG `pg_mbcliplen`: the largest byte length `<= limit` that ends on a UTF-8 character boundary.
fn clip_len(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Infallible core of [`make_object_name`] (PG `makeObjectName`).
fn object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> String {
    let mut overhead = 0usize;
    let mut n2 = 0usize;
    if let Some(name2) = name2 {
        overhead += 1; // separating underscore
        n2 = name2.len();
    }
    if let Some(label) = label {
        overhead += label.len() + 1;
    }
    let avail = MAX_IDENTIFIER_BYTES.saturating_sub(overhead);
    let mut n1 = name1.len();
    // Preferentially truncate the longer name; name2 shrinks on ties.
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let n1 = clip_len(name1, n1);
    let mut out = String::with_capacity(n1 + n2 + overhead);
    out.push_str(&name1[..n1]);
    if let Some(name2) = name2 {
        let n2 = clip_len(name2, n2);
        out.push('_');
        out.push_str(&name2[..n2]);
    }
    if let Some(label) = label {
        out.push('_');
        out.push_str(label);
    }
    out
}

/// PG `makeObjectName` (module docs).
// CONTRACT
pub fn make_object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> Result<String> {
    Ok(object_name(name1, name2, label))
}

/// PG `ChooseIndexNameAddition` / `ChooseForeignKeyConstraintNameAddition`: names joined with `_`
/// (no leading `_` before the first non-empty piece), each name cut to 63 bytes, stopping once the
/// buffer reaches 63 bytes; the result is at most 63 bytes.
fn join_names(names: &[String]) -> String {
    let mut buf = String::new();
    for name in names {
        if !buf.is_empty() {
            buf.push('_');
        }
        buf.push_str(&truncate_identifier(name));
        if buf.len() >= MAX_IDENTIFIER_BYTES {
            break;
        }
    }
    truncate_identifier(&buf)
}

/// True when a pg_constraint row named `name` exists in `namespace` (namespace-wide, not per table).
fn constraint_name_exists(snapshot: &CatalogSnapshot, namespace: Oid, name: &str) -> bool {
    snapshot.data().constraints.values().any(|c| c.namespace == namespace && c.name == name)
}

/// The shared PG loop of `ChooseRelationName` / `ChooseConstraintName`: the first candidate uses
/// `label` (pass 0) and later candidates append the pass number (`label1`, `label2`, ...). A
/// candidate is taken when it is in `others` or `taken` reports it.
fn choose_name_loop<F: Fn(&str) -> bool>(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    first_pass: u64,
    others: &[String],
    taken: F,
) -> String {
    let mut pass = first_pass;
    loop {
        let cand = if pass == 0 {
            object_name(name1, name2, Some(label))
        } else {
            object_name(name1, name2, Some(&format!("{label}{pass}")))
        };
        if !others.iter().any(|o| o == &cand) && !taken(cand.as_str()) {
            return cand;
        }
        pass += 1;
    }
}

/// `ChooseRelationName` with PG's `isconstraint` flag: when set, a constraint name in the namespace
/// also counts as taken (PG `ChooseIndexName` passes true for PRIMARY KEY / UNIQUE / EXCLUDE indexes).
fn choose_relation_name_impl(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    label: &str,
    others: &[String],
    is_constraint: bool,
) -> String {
    choose_name_loop(name1, name2, label, 0, others, |c| {
        snapshot.class_in_namespace(namespace, c).is_some()
            || (is_constraint && constraint_name_exists(snapshot, namespace, c))
    })
}

/// PG `ChooseRelationName`: unique pg_class name in `namespace` (module docs).
// CONTRACT
pub fn choose_relation_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    label: &str,
    others: &[String],
) -> Result<String> {
    Ok(choose_relation_name_impl(snapshot, namespace, name1, name2, label, others, false))
}

/// Name of an identity sequence: an explicit `SEQUENCE NAME` is used as given and must be unused in
/// `namespace` (42P07 `relation "x" already exists`); otherwise the PG-chosen name.
pub fn choose_identity_sequence_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    explicit: Option<&str>,
    others: &[String],
) -> Result<String> {
    match explicit {
        Some(n) => {
            if snapshot.class_in_namespace(namespace, n).is_some() || others.iter().any(|o| o == n) {
                return Err(crate::errors::relation_already_exists(n));
            }
            Ok(n.to_string())
        }
        None => choose_relation_name(snapshot, namespace, name1, name2, "seq", others),
    }
}

/// PG `ChooseConstraintName`: unique constraint name in `namespace` (module docs).
// CONTRACT
pub fn choose_constraint_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    label: &str,
    others: &[String],
) -> Result<String> {
    // PG: an empty label starts at pass 1 (`1`, `2`, ...) instead of an empty label.
    let first_pass = if label.is_empty() { 1 } else { 0 };
    Ok(choose_name_loop(name1, name2, label, first_pass, others, |c| {
        constraint_name_exists(snapshot, namespace, c)
    }))
}

/// PG `ChooseIndexName`: `primary` → `choose_relation_name(table, None, "pkey")`; `constraint`
/// (UNIQUE/EXCLUDE constraint index) → label `key` / `excl` with name2 = the column-name addition;
/// otherwise label `idx`. The addition (PG `ChooseIndexNameAddition`) joins `column_names` with `_`,
/// stopping once it reaches 63 bytes.
// CONTRACT
pub fn choose_index_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    table_name: &str,
    column_names: &[String],
    primary: bool,
    unique_constraint: bool,
    others: &[String],
) -> Result<String> {
    if primary {
        // The primary key's name does not depend on the columns.
        return Ok(choose_relation_name_impl(snapshot, namespace, table_name, None, "pkey", others, true));
    }
    let addition = join_names(column_names);
    if unique_constraint {
        return Ok(choose_relation_name_impl(snapshot, namespace, table_name, Some(&addition), "key", others, true));
    }
    Ok(choose_relation_name_impl(snapshot, namespace, table_name, Some(&addition), "idx", others, false))
}

/// What an index key is, for `choose_index_column_names`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexKeySource {
    /// Plain column key.
    Column(String),
    /// Expression whose top node is a function call named `name` (e.g. `lower`).
    FunctionCall(String),
    /// Any other expression.
    Expression,
}

/// PG `ChooseIndexColumnNames` (module docs): `[Column("a"), FunctionCall("lower"), Expression,
/// Expression]` → `["a", "lower", "expr", "expr1"]`; duplicate column names in one index get
/// suffixes too (`a`, `a1`).
// CONTRACT
pub fn choose_index_column_names(keys: &[IndexKeySource]) -> Result<Vec<String>> {
    let mut result: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        let base: String = match key {
            IndexKeySource::Column(name) | IndexKeySource::FunctionCall(name) => truncate_identifier(name),
            IndexKeySource::Expression => "expr".to_string(),
        };
        // PG: the first free name among base, base1, base2, ... (suffix digits are appended to the
        // base name, clipped so the whole stays within 63 bytes).
        let mut cur = base.clone();
        let mut i = 1u32;
        while result.iter().any(|r| *r == cur) {
            let suffix = i.to_string();
            let keep = clip_len(&base, MAX_IDENTIFIER_BYTES - suffix.len());
            cur = format!("{}{}", &base[..keep], suffix);
            i += 1;
        }
        result.push(cur);
    }
    Ok(result)
}

/// PG `ChooseForeignKeyConstraintNameAddition` / the column-list addition: names joined with `_`,
/// truncated so the result stays ≤ 63 bytes (`["a","b"]` → `a_b`).
// CONTRACT
pub fn column_list_addition(columns: &[String]) -> Result<String> {
    Ok(join_names(columns))
}

/// PG `makeArrayTypeName` (pg_type.c): the candidate is `make_object_name("", name, None)` = `_name`
/// (truncated to 63 bytes, the base name shortened as needed); while a pg_type name (builtin type of
/// the namespace, user type, rowtype or array type, in the snapshot) or one in `others` already uses
/// the candidate in `namespace`, the next candidate is `make_object_name("", name, Some(pass))` for
/// pass = 1, 2, ... = `_name_1`, `_name_2`, ... (NOT extra leading underscores). PG has no try
/// limit; give up after 10000 passes with 54000 `could not form array type name for type "<name>"`.
/// `moveArrayTypeName` (a new type whose name equals an existing auto array type renames THAT array
/// to `choose_array_type_name(new type name)` first) is `ddl::common::resolve_type_name_clash`.
// CONTRACT
pub fn choose_array_type_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name: &str,
    others: &[String],
) -> Result<String> {
    /// Passes after the unsuffixed candidate before giving up (PG has no limit; this bounds ours).
    const MAX_PASSES: u64 = 10_000;
    let taken = |cand: &str| -> bool {
        others.iter().any(|o| o == cand)
            || snapshot.user_type_in_namespace(namespace, cand).is_some()
            || pgarbor_types::catalog_data::TYPES.iter().any(|t| t.namespace == namespace && t.name == cand)
    };
    let mut cand = object_name("", Some(name), None);
    let mut pass = 0u64;
    while taken(cand.as_str()) {
        pass += 1;
        if pass > MAX_PASSES {
            return Err(PgError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("could not form array type name for type \"{name}\""),
            ));
        }
        cand = object_name("", Some(name), Some(&pass.to_string()));
    }
    Ok(cand)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncation() {
        let long = "a".repeat(70);
        assert_eq!(truncate_identifier(&long).len(), 63);
        let multi = "é".repeat(40); // 80 bytes
        let t = truncate_identifier(&multi);
        assert!(t.len() <= 63 && t.is_char_boundary(t.len()));
        assert_eq!(array_type_name("mood"), "_mood");
    }

    fn empty_snapshot() -> CatalogSnapshot {
        CatalogSnapshot::build(crate::persist::CatalogVersion::ZERO, crate::data::CatalogData::default())
            .expect("builtin-only snapshot")
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn object_name_basic_and_truncation() {
        assert_eq!(make_object_name("t", Some("id"), Some("seq")).unwrap(), "t_id_seq");
        assert_eq!(make_object_name("", Some("mood"), None).unwrap(), "_mood");
        let long = "a".repeat(70);
        let n = make_object_name(&long, None, Some("idx")).unwrap();
        assert_eq!(n.len(), 63);
        assert_eq!(n, format!("{}_idx", "a".repeat(59)));
        // Multi-byte names are clipped on a character boundary.
        let multi = "é".repeat(40);
        let n = make_object_name(&multi, Some("x"), Some("pkey")).unwrap();
        assert!(n.len() <= 63 && n.ends_with("_x_pkey"));
    }

    #[test]
    fn relation_name_loops_on_others() {
        let snap = empty_snapshot();
        let ns = crate::builtin::PUBLIC_NAMESPACE;
        let r = choose_relation_name(&snap, ns, "t", Some("a"), "idx", &[]).unwrap();
        assert_eq!(r, "t_a_idx");
        let r = choose_relation_name(&snap, ns, "t", Some("a"), "idx", &strs(&["t_a_idx"])).unwrap();
        assert_eq!(r, "t_a_idx1");
        let r = choose_relation_name(&snap, ns, "t", Some("a"), "idx", &strs(&["t_a_idx", "t_a_idx1"])).unwrap();
        assert_eq!(r, "t_a_idx2");
    }

    #[test]
    fn index_column_names_are_deduplicated() {
        let keys = [
            IndexKeySource::Column("a".into()),
            IndexKeySource::FunctionCall("lower".into()),
            IndexKeySource::Expression,
            IndexKeySource::Expression,
        ];
        assert_eq!(choose_index_column_names(&keys).unwrap(), strs(&["a", "lower", "expr", "expr1"]));
        let dup = [IndexKeySource::Column("a".into()), IndexKeySource::Column("a".into())];
        assert_eq!(choose_index_column_names(&dup).unwrap(), strs(&["a", "a1"]));
    }

    #[test]
    fn index_names_follow_pg_labels() {
        let snap = empty_snapshot();
        let ns = crate::builtin::PUBLIC_NAMESPACE;
        let ab = strs(&["a", "b"]);
        assert_eq!(choose_index_name(&snap, ns, "t", &strs(&["a"]), true, false, &[]).unwrap(), "t_pkey");
        assert_eq!(choose_index_name(&snap, ns, "t", &ab, false, true, &[]).unwrap(), "t_a_b_key");
        assert_eq!(choose_index_name(&snap, ns, "t", &strs(&["a"]), false, false, &[]).unwrap(), "t_a_idx");
        assert_eq!(
            choose_index_name(&snap, ns, "t", &strs(&["a"]), false, false, &strs(&["t_a_idx"])).unwrap(),
            "t_a_idx1"
        );
        assert_eq!(column_list_addition(&ab).unwrap(), "a_b");
    }

    #[test]
    fn array_type_names_avoid_builtin_collisions() {
        let snap = empty_snapshot();
        assert_eq!(choose_array_type_name(&snap, crate::builtin::PG_CATALOG_NAMESPACE, "mood", &[]).unwrap(), "_mood");
        assert_eq!(
            choose_array_type_name(&snap, crate::builtin::PG_CATALOG_NAMESPACE, "mood", &strs(&["_mood"])).unwrap(),
            "_mood_1"
        );
        // `_int4` is the builtin array type of int4 in pg_catalog.
        assert_eq!(
            choose_array_type_name(&snap, crate::builtin::PG_CATALOG_NAMESPACE, "int4", &[]).unwrap(),
            "_int4_1"
        );
    }
}
