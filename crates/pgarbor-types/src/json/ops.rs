//! json/jsonb operators and jsonb ordering (TYPE-08). Lease TYP-34.
//!
//! PG18 reference: `utils/adt/jsonfuncs.c` (`json_object_field`, `jsonb_array_element`,
//! `jsonb_extract_path`, `jsonb_delete*`, `jsonb_concat`), `jsonb_op.c` (`jsonb_exists*`,
//! `jsonb_contains`, `jsonb_eq`, `jsonb_cmp`), `jsonb_util.c` (`compareJsonbContainers`,
//! `JsonbDeepContains`).
//! Owned procs (ImplModule::JsonOps): `json_array_element json_array_element_text json_extract_path
//! json_extract_path_text` (on `json`: walk `parse::parse_json_value`; `->>` output of a string is its
//! unescaped value, of other values their json text), `json_object_field json_object_field_text
//! jsonb_object_field jsonb_object_field_text` (`->`/`->>` with a text key), `jsonb_array_element jsonb_array_element_text
//! jsonb_extract_path jsonb_extract_path_text` (`->`, `->>` with int, `#>`, `#>>`; negative array index
//! counts from the end: `'[1,2,3]'::jsonb -> -1` = `3`; missing → NULL; `->>` of JSON null → SQL NULL),
//! `jsonb_exists jsonb_exists_any jsonb_exists_all` (`?` `?|` `?&`: top-level keys or string array
//! elements), `jsonb_contains jsonb_contained` (`@>` `<@`, PG containment semantics incl. scalar-in-array
//! and array-contains-array regardless of order/duplicates), `jsonb_concat` (`||`: object merge (right
//! wins), array concat, scalars wrapped as 1-element arrays: `'[1]' || '2'` = `[1, 2]`), `jsonb_delete`
//! (`- text`), `jsonb_delete_idx` (`- int`), `jsonb_delete_array` (`- text[]`), `jsonb_delete_path`
//! (`#-`), `jsonb_eq ne lt le gt ge jsonb_cmp jsonb_hash(_extended)`.
//!
//! jsonb ordering (`compare_jsonb`, also used by `compare`/`keycodec`), oracle-verified:
//! Object > Array > Boolean > Number > String > Null; arrays: more elements is greater
//! (`'[1,2]' > '[3]'`), then element-wise; objects: more pairs is greater, then pair-by-pair in storage
//! order comparing KEYS by text collation order (NOT length-first: `'{"b":1}' > '{"aa":1}'`) then values;
//! numbers numerically; strings by codepoint; false < true. Quirk: an EMPTY top-level array sorts below
//! everything, including null (`'[]'::jsonb < 'null'` is true).
use super::Jsonb;
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};
use std::cmp::Ordering;

/// PG jsonb btree ordering (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn compare_jsonb(a: &Jsonb, b: &Jsonb) -> Result<Ordering> {
    Err(PgError::unimplemented("TYP-34", "jsonb comparison"))
}

/// `a @> b` (PG `JsonbDeepContains`).
// CONTRACT
#[allow(unused_variables)]
pub fn jsonb_contains(a: &Jsonb, b: &Jsonb) -> Result<bool> {
    Err(PgError::unimplemented("TYP-34", "jsonb containment"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a JsonOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-34", format!("function {}", proc.name))
}
