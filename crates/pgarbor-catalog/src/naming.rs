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
use pgarbor_common::{Oid, PgError, Result};

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

/// PG `makeObjectName` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn make_object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "makeObjectName"))
}

/// PG `ChooseRelationName`: unique pg_class name in `namespace` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn choose_relation_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    label: &str,
    others: &[String],
) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "ChooseRelationName"))
}

/// PG `ChooseConstraintName`: unique constraint name in `namespace` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn choose_constraint_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name1: &str,
    name2: Option<&str>,
    label: &str,
    others: &[String],
) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "ChooseConstraintName"))
}

/// PG `ChooseIndexName`: `primary` → `choose_relation_name(table, None, "pkey")`; `constraint`
/// (UNIQUE/EXCLUDE constraint index) → label `key` / `excl` with name2 = the column-name addition;
/// otherwise label `idx`. The addition (PG `ChooseIndexNameAddition`) joins `column_names` with `_`,
/// stopping once it reaches 63 bytes.
// CONTRACT
#[allow(unused_variables)]
pub fn choose_index_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    table_name: &str,
    column_names: &[String],
    primary: bool,
    unique_constraint: bool,
    others: &[String],
) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "ChooseIndexName"))
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
#[allow(unused_variables)]
pub fn choose_index_column_names(keys: &[IndexKeySource]) -> Result<Vec<String>> {
    Err(PgError::unimplemented("CAT-06", "ChooseIndexColumnNames"))
}

/// PG `ChooseForeignKeyConstraintNameAddition` / the column-list addition: names joined with `_`,
/// truncated so the result stays ≤ 63 bytes (`["a","b"]` → `a_b`).
// CONTRACT
#[allow(unused_variables)]
pub fn column_list_addition(columns: &[String]) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "constraint name column addition"))
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
#[allow(unused_variables)]
pub fn choose_array_type_name(
    snapshot: &CatalogSnapshot,
    namespace: Oid,
    name: &str,
    others: &[String],
) -> Result<String> {
    Err(PgError::unimplemented("CAT-06", "makeArrayTypeName"))
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
}
