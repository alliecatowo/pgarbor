//! JSON constructors, SQL-value → JSON conversion, jsonb mutation and inspection (TYPE-08).
//! Lease TYP-35.
//!
//! PG18 reference: `utils/adt/json.c` (`datum_to_json_internal`, `json_build_object`, `to_json`,
//! `row_to_json`, `array_to_json`, `json_object`), `jsonb.c` (`datum_to_jsonb_internal`, `to_jsonb`,
//! `jsonb_build_*`, jsonb casts to scalar types), `jsonfuncs.c` (`jsonb_set`, `jsonb_set_lax`,
//! `jsonb_insert`, `jsonb_strip_nulls`, `json_strip_nulls`, `jsonb_pretty`, `json(b)_typeof`,
//! `json(b)_array_length`).
//! Owned procs (ImplModule::JsonBuild): `to_json to_jsonb json_build_object json_build_array
//! json_build_object_noargs json_build_array_noargs jsonb_build_object jsonb_build_array
//! jsonb_build_object_noargs jsonb_build_array_noargs json_object json_object_two_arg jsonb_object
//! jsonb_object_two_arg row_to_json row_to_json_pretty array_to_json array_to_json_pretty
//! json_strip_nulls jsonb_strip_nulls jsonb_set jsonb_set_lax jsonb_insert jsonb_pretty json_typeof
//! jsonb_typeof json_array_length jsonb_array_length jsonb_bool jsonb_int2 jsonb_int4 jsonb_int8
//! jsonb_float4 jsonb_float8 jsonb_numeric`.
//!
//! Value conversion (`datum_to_jsonb` / `datum_to_json_text`) by SQL type (`ctx.arg_types`):
//! NULL → null; bool → true/false; int/float/numeric → number, except NaN/±Infinity → STRING
//! (`to_jsonb('NaN'::float8)` = `"NaN"`, `"Infinity"`); date/timestamp/timestamptz/time → ISO 8601
//! strings with `T` and offsets as `+00:00` (`to_jsonb(timestamptz '2024-01-01 00:00+00')` =
//! `"2024-01-01T00:00:00+00:00"`, PG `JsonEncodeDateTime`); json/jsonb → embedded as-is; arrays →
//! JSON arrays (multi-dim nested); records/composites → objects with field names (anonymous ROW()
//! fields are `f1`, `f2`, ...); everything else → its text output as a JSON string.
//! Oracle: `to_json(row(1,'a'))` = `{"f1":1,"f2":"a"}` (json text: NO spaces); `json_build_object('a',1,'b',null)`
//! = `{"a" : 1, "b" : null}` (json_build_* use `" : "` and `", "`); `jsonb_build_array(1,'a',null,true)`
//! = `[1, "a", null, true]`; `json_object('{a,1,b,2}')` = `{"a" : "1", "b" : "2"}`.
//! Errors: `json_build_object` odd arg count → 22023 `argument list must have even number of elements`
//! with HINT `The arguments of json_build_object() must consist of alternating keys and values.`; null key →
//! 22023 `null value not allowed for object key` (oracle-verified); `json_array_length('{}')` → 22023
//! `cannot get array length of a non-array`.
//! jsonb → scalar casts: `('{"a":1}'::jsonb -> 'a')::int` = 1, `'1.5'::jsonb::int` = 2 (numeric rounding);
//! wrong kind → 22023 `cannot cast jsonb string to type integer` (kinds: string/numeric/boolean/object/array/null).
use super::Jsonb;
use crate::catalog_data::BuiltinProc;
use crate::context::FuncCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// Convert a SQL value of type `ty` to jsonb (`to_jsonb`).
// CONTRACT
#[allow(unused_variables)]
pub fn datum_to_jsonb(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<Jsonb> {
    Err(PgError::unimplemented("TYP-35", "to_jsonb"))
}

/// Convert a SQL value of type `ty` to json text (`to_json`), compact form.
// CONTRACT
#[allow(unused_variables)]
pub fn datum_to_json_text(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    Err(PgError::unimplemented("TYP-35", "to_json"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a JsonBuild-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-35", format!("function {}", proc.name))
}
