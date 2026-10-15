//! JSON set-returning and record functions (TYPE-08). Lease TYP-36.
//!
//! PG18 reference: `utils/adt/jsonfuncs.c` (`each_worker`, `elements_worker`, `json_object_keys`,
//! `populate_record_worker`, `populate_recordset_worker`, `to_record`).
//! Owned procs (ImplModule::JsonSrf), all `FuncImpl::Set` except the populate_record/to_record scalars:
//! - `json_each jsonb_each` → rows `Datum::Record([key Text, value Json|Jsonb])`; `json_each_text
//!   jsonb_each_text` → `[key, value Text]` (strings unescaped; JSON null → SQL NULL).
//! - `json_array_elements(_text) jsonb_array_elements(_text)` → one value per element.
//! - `json_object_keys jsonb_object_keys` → key Text per member (json: input order incl. duplicates;
//!   jsonb: storage order).
//! - Wrong container kind, all 22023 (oracle-verified): `cannot call jsonb_each on a non-object`;
//!   json_each on array `cannot deconstruct an array as an object`, on scalar `cannot deconstruct a scalar`;
//!   jsonb_array_elements on object `cannot extract elements from an object`, on scalar
//!   `cannot extract elements from a scalar`; json_array_elements: `cannot call json_array_elements on a
//!   non-array` / `... on a scalar`; `cannot call jsonb_object_keys on an array`, `cannot call
//!   json_object_keys on a scalar`.
//! - `json(b)_populate_record(base anyelement, from_json)`, `json(b)_populate_recordset`,
//!   `json(b)_to_record`, `json(b)_to_recordset`: the target row type is `ctx.result_type` (composite:
//!   fields via `ctx.env.composite_fields`; for `to_record` the binder registers the column definition
//!   list as an anonymous record type and passes it the same way). Without one the BINDER raises 42601
//!   `a column definition list is required for functions returning "record"`. Field values are produced
//!   with the column type's input function applied to the JSON text (strings unescaped).
//! Oracle: `select * from jsonb_each('{"a":1,"b":[2]}')` → (a, 1), (b, [2]).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a JsonSrf-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-36", format!("function {}", proc.name))
}
