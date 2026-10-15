//! Array functions and operators (TYPE-09). Lease TYP-38.
//!
//! PG18 reference: `utils/adt/arrayfuncs.c`, `array_userfuncs.c`, `arrayutils.c`, `varlena.c`
//! (`text_to_array`, `array_to_text`), `array_selfuncs.c` (no), `rangetypes` (no).
//! Owned procs (ImplModule::ArrayFuncs): `array_eq array_ne array_lt array_gt array_le array_ge btarraycmp
//! array_larger array_smaller hash_array(_extended)` (element-wise via `compare::cmp_datums` on the
//! element type; NULL elements sort AFTER non-NULL and equal each other for ordering; then more elements
//! is greater; then ndims; then dims/lbounds. Oracle: `array[1,2] < array[1,2,0]` is true,
//! `array[1,null] < array[1,2]` is false, `array[1,null] = array[1,null]` is TRUE (array_eq treats NULL
//! elements as equal)); `arraycontains arraycontained arrayoverlap` (`@>` `<@` `&&`, NULL elements never
//! match; duplicates irrelevant: `'{a}' @> '{a,a}'` true); `array_append array_prepend array_cat` (`||`;
//! dimension rules of `array_cat`: concatenating N-dim with (N-1)-dim etc.; element type mismatch →
//! 42804, dimension mismatch → 2202E, both `cannot concatenate incompatible arrays` with PG's DETAIL); `array_length array_upper array_lower array_ndims
//! array_dims` (`[2:3]`, `[1:2][1:2]`) `array_cardinality` (cardinality); `array_position(_start)
//! array_positions` (IS NOT DISTINCT FROM semantics; multi-dim → 0A000 `searching for elements in
//! multidimensional arrays is not supported` — a genuine PG error, not a PGArbor gap);
//! `array_remove array_replace` (multi-dim → 0A000 `removing elements from multidimensional arrays is not
//! supported`); `array_fill array_fill_with_lower_bounds`; `array_to_text array_to_text_null`
//! (`array_to_string(array[1,null,3], ',', '*')` = `1,*,3`; NULLs skipped when no null string);
//! `text_to_array text_to_array_null` (`string_to_array('a,b,,c', ',')` = `{a,b,"",c}`; NULL delimiter
//! splits into characters; empty input → `{}`); `text_to_table text_to_table_null` (SRF);
//! `array_unnest` (SRF `unnest(anyarray)`, flattens all dims); `generate_subscripts(_nodir)` (SRF);
//! SQL-language `_pg_expandarray` (oid 13294; SRF of Record(x, n)); `trim_array`, `array_reverse`,
//! `array_sample`/`array_shuffle` (random via `ctx.session`), `array_sort`/`array_sort_order`/
//! `array_sort_order_nulls_first` (PG18), `width_bucket_array`.
//! Result element type for polymorphic functions: `ctx.result_type` (array OID) / `Array.elem`.
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for an ArrayFuncs-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-38", format!("function {}", proc.name))
}
