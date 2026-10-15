//! Array functions and operators (TYPE-09). Lease TYP-38.
//!
//! PG18 reference: `utils/adt/arrayfuncs.c`, `array_userfuncs.c`, `arrayutils.c`, `varlena.c`
//! (`text_to_array`, `array_to_text`, `text_to_table`).
//! Owned procs (ImplModule::ArrayFuncs): `array_eq array_ne array_lt array_gt array_le array_ge btarraycmp
//! array_larger array_smaller hash_array(_extended)` (element-wise via `compare` on the element type; NULL
//! elements sort AFTER non-NULL and equal each other for ordering; then more elements is greater; then ndims;
//! then dims/lbounds. Oracle: `array[1,2] < array[1,2,0]` is true, `array[1,null] < array[1,2]` is false,
//! `array[1,null] = array[1,null]` is TRUE (array_eq treats NULL elements as equal)); `arraycontains
//! arraycontained arrayoverlap` (`@>` `<@` `&&`, NULL elements never match; duplicates irrelevant:
//! `'{a}' @> '{a,a}'` true); `array_append array_prepend array_cat` (`||`; dimension rules of `array_cat`:
//! concatenating N-dim with (N-1)-dim etc.; element type mismatch -> 42804, dimension mismatch -> 2202E, both
//! `cannot concatenate incompatible arrays` with PG's DETAIL); `array_length array_upper array_lower array_ndims
//! array_dims` (`[2:3]`, `[1:2][1:2]`) `array_cardinality` (cardinality); `array_position(_start)
//! array_positions` (IS NOT DISTINCT FROM semantics for NULL search; multi-dim -> 0A000 `searching for elements
//! in multidimensional arrays is not supported` — a genuine PG error, not a PGArbor gap);
//! `array_remove array_replace` (multi-dim remove -> 0A000 `removing elements from multidimensional arrays is not
//! supported`); `array_fill array_fill_with_lower_bounds`; `array_to_text array_to_text_null`
//! (`array_to_string(array[1,null,3], ',', '*')` = `1,*,3`; NULLs skipped when no null string);
//! `text_to_array text_to_array_null` (`string_to_array('a,b,,c', ',')` = `{a,b,"",c}`; NULL delimiter
//! splits into characters; empty input -> `{}`); `text_to_table text_to_table_null` (SRF);
//! `array_unnest` (SRF `unnest(anyarray)`, flattens all dims); `generate_subscripts(_nodir)` (SRF);
//! SQL-language `_pg_expandarray` (oid 13294; SRF of Record(x, n)); `trim_array`, `array_reverse`,
//! `array_sample`/`array_shuffle` (random via `ctx.session`), `array_sort`/`array_sort_order`/
//! `array_sort_order_nulls_first` (PG18), `width_bucket_array`.
//! Result element type for polymorphic functions: `ctx.result_type` (array OID) / `Array.elem`.
//!
//! Hash functions: `hash_array` combines element hashes with PG's `hashint*`/`hashtext`/`hashfloat*`
//! algorithms (`access/hash/hashfunc.c`) for the element types listed in `hash_kind`. Other element types
//! (numeric, interval, enums, records, ...) are reported as 0A000 through `PgError::unimplemented`; json
//! gives 42883 `could not identify a hash function for type json`.
use crate::array::{Array, MAXDIM};
use crate::compare;
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, TypeEnv};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::{FuncImpl, SetIter};
use crate::io;
use crate::names::display_name;
use crate::oids;
use crate::registry;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;

/// PostgreSQL `MaxArraySize` = `MaxAllocSize / sizeof(Datum)`.
const MAX_ARRAY_SIZE: usize = 134_217_727;

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    if proc.prosrc.is_empty() {
        // SQL-language builtins carry no C symbol: dispatch on the OID.
        return match proc.oid {
            13294 => Some(FuncImpl::Set(pg_expandarray_fn)),
            _ => None,
        };
    }
    Some(match proc.prosrc {
        "array_eq" => FuncImpl::Scalar(array_eq_fn),
        "array_ne" => FuncImpl::Scalar(array_ne_fn),
        "array_lt" => FuncImpl::Scalar(array_lt_fn),
        "array_gt" => FuncImpl::Scalar(array_gt_fn),
        "array_le" => FuncImpl::Scalar(array_le_fn),
        "array_ge" => FuncImpl::Scalar(array_ge_fn),
        "btarraycmp" => FuncImpl::Scalar(btarraycmp_fn),
        "array_larger" => FuncImpl::Scalar(array_larger_fn),
        "array_smaller" => FuncImpl::Scalar(array_smaller_fn),
        "hash_array" => FuncImpl::Scalar(hash_array_fn),
        "hash_array_extended" => FuncImpl::Scalar(hash_array_extended_fn),
        "arrayoverlap" => FuncImpl::Scalar(arrayoverlap_fn),
        "arraycontains" => FuncImpl::Scalar(arraycontains_fn),
        "arraycontained" => FuncImpl::Scalar(arraycontained_fn),
        "array_append" => FuncImpl::Scalar(array_append_fn),
        "array_prepend" => FuncImpl::Scalar(array_prepend_fn),
        "array_cat" => FuncImpl::Scalar(array_cat_fn),
        "array_length" => FuncImpl::Scalar(array_length_fn),
        "array_upper" => FuncImpl::Scalar(array_upper_fn),
        "array_lower" => FuncImpl::Scalar(array_lower_fn),
        "array_ndims" => FuncImpl::Scalar(array_ndims_fn),
        "array_dims" => FuncImpl::Scalar(array_dims_fn),
        "array_cardinality" => FuncImpl::Scalar(array_cardinality_fn),
        "array_position" => FuncImpl::Scalar(array_position_fn),
        "array_position_start" => FuncImpl::Scalar(array_position_start_fn),
        "array_positions" => FuncImpl::Scalar(array_positions_fn),
        "array_remove" => FuncImpl::Scalar(array_remove_fn),
        "array_replace" => FuncImpl::Scalar(array_replace_fn),
        "array_fill" => FuncImpl::Scalar(array_fill_fn),
        "array_fill_with_lower_bounds" => FuncImpl::Scalar(array_fill_with_lower_bounds_fn),
        "array_to_text" | "array_to_text_null" => FuncImpl::Scalar(array_to_text_fn),
        "text_to_array" | "text_to_array_null" => FuncImpl::Scalar(text_to_array_fn),
        "text_to_table" | "text_to_table_null" => FuncImpl::Set(text_to_table_fn),
        "array_unnest" => FuncImpl::Set(array_unnest_fn),
        "generate_subscripts" | "generate_subscripts_nodir" => FuncImpl::Set(generate_subscripts_fn),
        "trim_array" => FuncImpl::Scalar(trim_array_fn),
        "array_reverse" => FuncImpl::Scalar(array_reverse_fn),
        "array_shuffle" => FuncImpl::Scalar(array_shuffle_fn),
        "array_sample" => FuncImpl::Scalar(array_sample_fn),
        "array_sort" => FuncImpl::Scalar(array_sort_fn),
        "array_sort_order" => FuncImpl::Scalar(array_sort_order_fn),
        "array_sort_order_nulls_first" => FuncImpl::Scalar(array_sort_order_nulls_first_fn),
        "width_bucket_array" => FuncImpl::Scalar(width_bucket_array_fn),
        _ => return None,
    })
}

/// 0A000 for an ArrayFuncs-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-38", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument and error helpers
// ---------------------------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("array function is missing argument {i}")))
}

fn array_arg(d: &Datum) -> Result<&Array> {
    match d {
        Datum::Array(a) => Ok(&**a),
        other => Err(type_mismatch("array", other)),
    }
}

/// `Null` -> None, array -> Some.
fn opt_array(d: &Datum) -> Result<Option<&Array>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Array(a) => Ok(Some(&**a)),
        other => Err(type_mismatch("array", other)),
    }
}

fn int4_of(d: &Datum) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("int4", other)),
    }
}

fn int8_of(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("int8", other)),
    }
}

fn bool_of(d: &Datum) -> Result<bool> {
    match d {
        Datum::Bool(v) => Ok(*v),
        other => Err(type_mismatch("bool", other)),
    }
}

fn text_of(d: &Datum) -> Result<&str> {
    match d {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("text", other)),
    }
}

fn int_out_of_range() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

fn too_big() -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
    )
}

fn lower_too_large(lb: i32) -> PgError {
    PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("array lower bound is too large: {lb}"))
}

fn one_dim_err() -> PgError {
    PgError::new(SqlState::DATA_EXCEPTION, "argument must be empty or one-dimensional array")
}

fn subscript_err(msg: &str) -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, msg)
}

fn incompatible(msg: &str) -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "cannot concatenate incompatible arrays").with_detail(msg)
}

fn different_elem_types() -> PgError {
    PgError::new(SqlState::DATATYPE_MISMATCH, "cannot compare arrays of different element types")
}

/// Element count check (`MaxArraySize`).
fn check_count(n: usize) -> Result<()> {
    if n > MAX_ARRAY_SIZE {
        return Err(too_big());
    }
    Ok(())
}

/// `ArrayGetNItems` + `ArrayCheckBounds`: element count of `dims`, rejecting negative lengths,
/// lower-bound overflow and totals above `MaxArraySize`. Empty `dims` gives 0.
fn n_items(dims: &[(i32, i32)]) -> Result<usize> {
    if dims.is_empty() {
        return Ok(0);
    }
    let mut total: usize = 1;
    for &(len, lb) in dims {
        if len < 0 {
            return Err(too_big());
        }
        if lb.checked_add(len).is_none() {
            return Err(lower_too_large(lb));
        }
        total = total.checked_mul(len as usize).ok_or_else(too_big)?;
        if total > MAX_ARRAY_SIZE {
            return Err(too_big());
        }
    }
    Ok(total)
}

/// Element type of a NULL array argument (PG `fetch_array_arg_replace_nulls`).
fn null_array_elem(ctx: &FuncCtx, argno: usize) -> Result<Oid> {
    let t = ctx.arg_type(argno).oid;
    if t == 0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "could not determine input data type"));
    }
    registry::element_type_of(ctx.env, t)
        .ok_or_else(|| PgError::new(SqlState::DATATYPE_MISMATCH, "input data type is not an array"))
}

/// Iterator over already-computed rows (SRF results are owned: `SetIter` is 'static).
fn rows(v: Vec<Datum>) -> SetIter {
    Box::new(v.into_iter().map(|d| -> Result<Datum> { Ok(d) }))
}

fn int4_row(v: i64) -> Result<Datum> {
    Ok(Datum::Int4(v as i32))
}

/// Element ordering with NULLs greater than everything (PG `array_cmp` rule).
fn cmp_elem_nulls_last(env: &dyn TypeEnv, elem: TypeId, a: &Datum, b: &Datum) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => compare::cmp_datums(env, elem, a, b),
    }
}

/// Element equality for `array_eq` (NULL = NULL is true; NULL vs non-NULL is false).
fn elem_eq_nulls_equal(env: &dyn TypeEnv, elem: TypeId, a: &Datum, b: &Datum) -> Result<bool> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(true),
        (false, false) => Ok(compare::sql_eq(env, elem, a, b)? == Some(true)),
        _ => Ok(false),
    }
}

fn ordering_i32(o: Ordering) -> i32 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// Rows of the first dimension, each a slice of `row` items (row-major).
fn rows_of(a: &Array) -> Vec<&[Datum]> {
    let n = match a.dims.first() {
        Some(&(len, _)) if len > 0 => len as usize,
        _ => return Vec::new(),
    };
    if a.items.is_empty() {
        return Vec::new();
    }
    let row = a.items.len() / n;
    if row == 0 {
        return Vec::new();
    }
    a.items.chunks(row).collect()
}

/// Uniform index in `[0, span)` from the session PRNG. `span` must be at least 1.
fn random_index(ctx: &FuncCtx, span: usize) -> usize {
    let r = ctx.session.random_f64();
    let k = (r * span as f64) as usize;
    k.min(span - 1)
}

// ---------------------------------------------------------------------------------------------
// Comparison: array_eq / array_cmp
// ---------------------------------------------------------------------------------------------

/// PG `array_eq`: same dims and lower bounds, then element-wise equality.
fn array_eq_impl(env: &dyn TypeEnv, x: &Array, y: &Array) -> Result<bool> {
    if x.elem != y.elem {
        return Err(different_elem_types());
    }
    if x.dims != y.dims {
        return Ok(false);
    }
    let elem = TypeId::of(x.elem);
    for (a, b) in x.items.iter().zip(y.items.iter()) {
        if !elem_eq_nulls_equal(env, elem, a, b)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// PG `array_cmp` (also `btarraycmp`): element-wise, then item count, ndims, dims, lbounds.
fn array_cmp_impl(env: &dyn TypeEnv, x: &Array, y: &Array) -> Result<Ordering> {
    if x.elem != y.elem {
        return Err(different_elem_types());
    }
    let elem = TypeId::of(x.elem);
    if !compare::is_orderable(env, elem)? {
        return Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not identify a comparison function for type {}", display_name(env, x.elem)),
        ));
    }
    let n = x.items.len().min(y.items.len());
    for i in 0..n {
        let o = cmp_elem_nulls_last(env, elem, &x.items[i], &y.items[i])?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    if x.items.len() != y.items.len() {
        return Ok(x.items.len().cmp(&y.items.len()));
    }
    if x.dims.len() != y.dims.len() {
        return Ok(x.dims.len().cmp(&y.dims.len()));
    }
    for (dx, dy) in x.dims.iter().zip(y.dims.iter()) {
        if dx.0 != dy.0 {
            return Ok(dx.0.cmp(&dy.0));
        }
    }
    for (dx, dy) in x.dims.iter().zip(y.dims.iter()) {
        if dx.1 != dy.1 {
            return Ok(dx.1.cmp(&dy.1));
        }
    }
    Ok(Ordering::Equal)
}

fn array_eq_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(array_eq_impl(ctx.env, x, y)?))
}

fn array_ne_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(!array_eq_impl(ctx.env, x, y)?))
}

fn array_lt_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(array_cmp_impl(ctx.env, x, y)? == Ordering::Less))
}

fn array_gt_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(array_cmp_impl(ctx.env, x, y)? == Ordering::Greater))
}

fn array_le_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(array_cmp_impl(ctx.env, x, y)? != Ordering::Greater))
}

fn array_ge_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(array_cmp_impl(ctx.env, x, y)? != Ordering::Less))
}

fn btarraycmp_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    Ok(Datum::Int4(ordering_i32(array_cmp_impl(ctx.env, x, y)?)))
}

fn array_larger_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    if array_cmp_impl(ctx.env, x, y)? > Ordering::Equal {
        Ok(arg(args, 0)?.clone())
    } else {
        Ok(arg(args, 1)?.clone())
    }
}

fn array_smaller_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let x = array_arg(arg(args, 0)?)?;
    let y = array_arg(arg(args, 1)?)?;
    if array_cmp_impl(ctx.env, x, y)? < Ordering::Equal {
        Ok(arg(args, 0)?.clone())
    } else {
        Ok(arg(args, 1)?.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Containment: @>, <@, &&
// ---------------------------------------------------------------------------------------------

/// PG `array_contain_compare`. With `matchall` every element of `a1` must have a match in `a2`
/// (NULL elements of `a1` fail); otherwise any non-NULL element of `a1` matching a non-NULL element of
/// `a2` is enough. Matching is `=` on the element type.
fn contain_compare(env: &dyn TypeEnv, a1: &Array, a2: &Array, matchall: bool) -> Result<bool> {
    if a1.elem != a2.elem {
        return Err(different_elem_types());
    }
    let elem = TypeId::of(a1.elem);
    for e1 in &a1.items {
        if e1.is_null() {
            if matchall {
                return Ok(false);
            }
            continue;
        }
        let mut found = false;
        for e2 in &a2.items {
            if e2.is_null() {
                continue;
            }
            if compare::sql_eq(env, elem, e1, e2)? == Some(true) {
                found = true;
                break;
            }
        }
        if found {
            if !matchall {
                return Ok(true);
            }
        } else if matchall {
            return Ok(false);
        }
    }
    Ok(matchall)
}

fn arrayoverlap_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a1 = array_arg(arg(args, 0)?)?;
    let a2 = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(contain_compare(ctx.env, a1, a2, false)?))
}

/// `a1 @> a2`: every element of `a2` is in `a1`.
fn arraycontains_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a1 = array_arg(arg(args, 0)?)?;
    let a2 = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(contain_compare(ctx.env, a2, a1, true)?))
}

/// `a1 <@ a2`: every element of `a1` is in `a2`.
fn arraycontained_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a1 = array_arg(arg(args, 0)?)?;
    let a2 = array_arg(arg(args, 1)?)?;
    Ok(Datum::Bool(contain_compare(ctx.env, a1, a2, true)?))
}

// ---------------------------------------------------------------------------------------------
// Hashing (hash_array, hash_array_extended)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum HashKind {
    Bool,
    Char,
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    Text,
    Bpchar,
    Bytea,
    Uuid,
}

/// Element hash support. Errors are raised before any element is hashed, as PG does at lookup time.
fn hash_kind(env: &dyn TypeEnv, elem: Oid) -> Result<HashKind> {
    Ok(match elem {
        oids::BOOL => HashKind::Bool,
        oids::CHAR => HashKind::Char,
        oids::INT2 => HashKind::Int2,
        oids::INT4 | oids::DATE => HashKind::Int4,
        oids::INT8 | oids::TIME | oids::TIMESTAMP | oids::TIMESTAMPTZ => HashKind::Int8,
        oids::OID
        | oids::REGPROC
        | oids::REGPROCEDURE
        | oids::REGOPER
        | oids::REGOPERATOR
        | oids::REGCLASS
        | oids::REGTYPE
        | oids::REGROLE
        | oids::REGNAMESPACE
        | oids::REGCOLLATION
        | oids::REGCONFIG
        | oids::REGDICTIONARY => HashKind::Oid,
        oids::FLOAT4 => HashKind::Float4,
        oids::FLOAT8 => HashKind::Float8,
        oids::TEXT | oids::VARCHAR | oids::NAME => HashKind::Text,
        oids::BPCHAR => HashKind::Bpchar,
        oids::BYTEA => HashKind::Bytea,
        oids::UUID => HashKind::Uuid,
        oids::JSON => {
            return Err(PgError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("could not identify a hash function for type {}", display_name(env, elem)),
            ))
        }
        _ => {
            return Err(PgError::unimplemented(
                "TYP-38",
                format!("hash_array of element type {}", display_name(env, elem)),
            ))
        }
    })
}

/// `hashint8` fold of a 64-bit value to 32 bits (also used for timestamps, times and dates).
fn fold_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    lo ^ if v >= 0 { hi } else { !hi }
}

/// `hashfloat8` / `hashfloat4`: zero hashes to 0, NaN is canonicalised, then `hash_any` of the 8 bytes.
fn hash_float(v: f64) -> Result<u32> {
    if v == 0.0 {
        return Ok(0);
    }
    let key = if v.is_nan() { f64::NAN } else { v };
    compare::pg_hash_bytes(&key.to_ne_bytes())
}

/// `hashfloat8extended` / `hashfloat4extended`: zero hashes to the seed.
fn hash_float_extended(v: f64, seed: u64) -> Result<u64> {
    if v == 0.0 {
        return Ok(seed);
    }
    let key = if v.is_nan() { f64::NAN } else { v };
    compare::pg_hash_bytes_extended(&key.to_ne_bytes(), seed)
}

fn hash32(kind: HashKind, d: &Datum) -> Result<u32> {
    match (kind, d) {
        (HashKind::Bool, Datum::Bool(b)) => compare::pg_hash_uint32((*b as i32) as u32),
        (HashKind::Char, Datum::Char(c)) => compare::pg_hash_uint32((*c as i8 as i32) as u32),
        (HashKind::Int2, Datum::Int2(v)) => compare::pg_hash_uint32((*v as i32) as u32),
        (HashKind::Int4, Datum::Int4(v)) | (HashKind::Int4, Datum::Date(v)) => compare::pg_hash_uint32(*v as u32),
        (HashKind::Int8, Datum::Int8(v))
        | (HashKind::Int8, Datum::Time(v))
        | (HashKind::Int8, Datum::Timestamp(v))
        | (HashKind::Int8, Datum::TimestampTz(v)) => compare::pg_hash_uint32(fold_int8(*v)),
        (HashKind::Oid, Datum::Oid(v)) => compare::pg_hash_uint32(*v),
        (HashKind::Float4, Datum::Float4(f)) => hash_float(*f as f64),
        (HashKind::Float8, Datum::Float8(f)) => hash_float(*f),
        (HashKind::Text, Datum::Text(s)) => compare::pg_hash_bytes(s.as_bytes()),
        (HashKind::Bpchar, Datum::Text(s)) => compare::pg_hash_bytes(s.trim_end_matches(' ').as_bytes()),
        (HashKind::Bytea, Datum::Bytea(b)) => compare::pg_hash_bytes(b),
        (HashKind::Uuid, Datum::Uuid(u)) => compare::pg_hash_bytes(u),
        (_, other) => Err(type_mismatch("hashable array element", other)),
    }
}

fn hash64(kind: HashKind, d: &Datum, seed: u64) -> Result<u64> {
    match (kind, d) {
        (HashKind::Bool, Datum::Bool(b)) => compare::pg_hash_uint32_extended((*b as i32) as u32, seed),
        (HashKind::Char, Datum::Char(c)) => compare::pg_hash_uint32_extended((*c as i8 as i32) as u32, seed),
        (HashKind::Int2, Datum::Int2(v)) => compare::pg_hash_uint32_extended((*v as i32) as u32, seed),
        (HashKind::Int4, Datum::Int4(v)) | (HashKind::Int4, Datum::Date(v)) => {
            compare::pg_hash_uint32_extended(*v as u32, seed)
        }
        (HashKind::Int8, Datum::Int8(v))
        | (HashKind::Int8, Datum::Time(v))
        | (HashKind::Int8, Datum::Timestamp(v))
        | (HashKind::Int8, Datum::TimestampTz(v)) => compare::pg_hash_uint32_extended(fold_int8(*v), seed),
        (HashKind::Oid, Datum::Oid(v)) => compare::pg_hash_uint32_extended(*v, seed),
        (HashKind::Float4, Datum::Float4(f)) => hash_float_extended(*f as f64, seed),
        (HashKind::Float8, Datum::Float8(f)) => hash_float_extended(*f, seed),
        (HashKind::Text, Datum::Text(s)) => compare::pg_hash_bytes_extended(s.as_bytes(), seed),
        (HashKind::Bpchar, Datum::Text(s)) => {
            compare::pg_hash_bytes_extended(s.trim_end_matches(' ').as_bytes(), seed)
        }
        (HashKind::Bytea, Datum::Bytea(b)) => compare::pg_hash_bytes_extended(b, seed),
        (HashKind::Uuid, Datum::Uuid(u)) => compare::pg_hash_bytes_extended(u, seed),
        (_, other) => Err(type_mismatch("hashable array element", other)),
    }
}

fn hash_array_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let kind = hash_kind(ctx.env, a.elem)?;
    let mut result: u32 = 1;
    for item in &a.items {
        let h = if item.is_null() { 0 } else { hash32(kind, item)? };
        result = (result << 5).wrapping_sub(result).wrapping_add(h);
    }
    Ok(Datum::Int4(result as i32))
}

fn hash_array_extended_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let seed = int8_of(arg(args, 1)?)? as u64;
    let kind = hash_kind(ctx.env, a.elem)?;
    let mut result: u64 = 1;
    for item in &a.items {
        let h = if item.is_null() { 0 } else { hash64(kind, item, seed)? };
        result = (result << 5).wrapping_sub(result).wrapping_add(h);
    }
    Ok(Datum::Int8(result as i64))
}

// ---------------------------------------------------------------------------------------------
// Construction: array_append / array_prepend / array_cat
// ---------------------------------------------------------------------------------------------

fn array_append_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let value = arg(args, 1)?.clone();
    let arr = opt_array(arg(args, 0)?)?;
    let elem = match arr {
        Some(a) => a.elem,
        None => null_array_elem(ctx, 0)?,
    };
    let mut out = match arr {
        Some(a) => a.clone(),
        None => Array::empty(elem),
    };
    match out.dims.len() {
        0 => out.dims = vec![(1, 1)],
        1 => {
            let (len, lb) = out.dims[0];
            lb.checked_add(len).ok_or_else(int_out_of_range)?;
            let new_len = len.checked_add(1).ok_or_else(int_out_of_range)?;
            out.dims[0] = (new_len, lb);
        }
        _ => return Err(one_dim_err()),
    }
    check_count(out.items.len() + 1)?;
    out.items.push(value);
    Ok(Datum::array(out))
}

fn array_prepend_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let value = arg(args, 0)?.clone();
    let arr = opt_array(arg(args, 1)?)?;
    let elem = match arr {
        Some(a) => a.elem,
        None => null_array_elem(ctx, 1)?,
    };
    let mut out = match arr {
        Some(a) => a.clone(),
        None => Array::empty(elem),
    };
    match out.dims.len() {
        0 => out.dims = vec![(1, 1)],
        1 => {
            let (len, lb) = out.dims[0];
            // PG keeps the existing lower bound on prepend (`[2:3]` -> `[2:4]`);
            // the checked_sub mirrors PG's "integer out of range" at INT_MIN.
            lb.checked_sub(1).ok_or_else(int_out_of_range)?;
            let new_len = len.checked_add(1).ok_or_else(int_out_of_range)?;
            out.dims[0] = (new_len, lb);
        }
        _ => return Err(one_dim_err()),
    }
    check_count(out.items.len() + 1)?;
    out.items.insert(0, value);
    Ok(Datum::array(out))
}

/// PG `array_cat` dimension rules on two non-NULL arrays.
fn cat_arrays(env: &dyn TypeEnv, v1: &Array, v2: &Array) -> Result<Array> {
    if v1.elem != v2.elem {
        let detail = format!(
            "Arrays with element types {} and {} are not compatible for concatenation.",
            display_name(env, v1.elem),
            display_name(env, v2.elem)
        );
        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot concatenate incompatible arrays")
            .with_detail(detail));
    }
    let nd1 = v1.dims.len();
    let nd2 = v2.dims.len();
    if nd1 == 0 && nd2 > 0 {
        return Ok(v2.clone());
    }
    if nd2 == 0 {
        return Ok(v1.clone());
    }
    if nd1 != nd2 && nd1 != nd2 + 1 && nd1 + 1 != nd2 {
        let detail = format!("Arrays of {nd1} and {nd2} dimensions are not compatible for concatenation.");
        return Err(incompatible(&detail));
    }
    let differing = "Arrays with differing dimensions are not compatible for concatenation.";
    let dims = if nd1 == nd2 {
        for i in 1..nd1 {
            if v1.dims[i] != v2.dims[i] {
                return Err(incompatible(
                    "Arrays with differing element dimensions are not compatible for concatenation.",
                ));
            }
        }
        let len = v1.dims[0].0.checked_add(v2.dims[0].0).ok_or_else(too_big)?;
        let mut d = v1.dims.clone();
        d[0] = (len, v1.dims[0].1);
        d
    } else if nd1 + 1 == nd2 {
        // The first argument becomes one more element of the outer dimension of the second.
        let mut d = v2.dims.clone();
        d[0].0 = d[0].0.checked_add(1).ok_or_else(too_big)?;
        for i in 0..nd1 {
            if v1.dims[i] != d[i + 1] {
                return Err(incompatible(differing));
            }
        }
        d
    } else {
        // nd1 == nd2 + 1: the second argument is appended to the outer dimension of the first.
        let mut d = v1.dims.clone();
        d[0].0 = d[0].0.checked_add(1).ok_or_else(too_big)?;
        for i in 0..nd2 {
            if v2.dims[i] != d[i + 1] {
                return Err(incompatible(differing));
            }
        }
        d
    };
    let mut items = Vec::with_capacity(v1.items.len() + v2.items.len());
    items.extend(v1.items.iter().cloned());
    items.extend(v2.items.iter().cloned());
    n_items(&dims)?;
    Ok(Array { elem: v1.elem, dims, items })
}

fn array_cat_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = opt_array(arg(args, 0)?)?;
    let b = opt_array(arg(args, 1)?)?;
    match (a, b) {
        (None, None) => Ok(Datum::Null),
        (None, Some(b)) => Ok(Datum::array(b.clone())),
        (Some(a), None) => Ok(Datum::array(a.clone())),
        (Some(a), Some(b)) => Ok(Datum::array(cat_arrays(ctx.env, a, b)?)),
    }
}

// ---------------------------------------------------------------------------------------------
// Dimension introspection
// ---------------------------------------------------------------------------------------------

fn array_length_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let req = int4_of(arg(args, 1)?)?;
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM || req <= 0 || req as usize > nd {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int4(a.dims[req as usize - 1].0))
}

fn array_upper_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let req = int4_of(arg(args, 1)?)?;
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM || req <= 0 || req as usize > nd {
        return Ok(Datum::Null);
    }
    let (len, lb) = a.dims[req as usize - 1];
    Ok(Datum::Int4((lb as i64 + len as i64 - 1) as i32))
}

fn array_lower_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let req = int4_of(arg(args, 1)?)?;
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM || req <= 0 || req as usize > nd {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int4(a.dims[req as usize - 1].1))
}

fn array_ndims_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int4(nd as i32))
}

fn array_dims_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM {
        return Ok(Datum::Null);
    }
    let mut s = String::new();
    for &(len, lb) in &a.dims {
        s.push_str(&format!("[{}:{}]", lb, lb as i64 + len as i64 - 1));
    }
    Ok(Datum::Text(s))
}

fn array_cardinality_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    Ok(Datum::Int4(a.items.len() as i32))
}

// ---------------------------------------------------------------------------------------------
// Searching: array_position / array_positions
// ---------------------------------------------------------------------------------------------

/// Shared logic of `array_position(_start)` and `array_positions`: returns the matching positions
/// (1-based subscripts, starting at `min`). `search` NULL matches NULL elements only.
fn matching_positions(ctx: &FuncCtx, a: &Array, search: &Datum, min: i64, stop_at_first: bool) -> Result<Vec<i32>> {
    let elem = TypeId::of(a.elem);
    let search_null = search.is_null();
    let mut position = a.dims[0].1 as i64 - 1;
    let mut out = Vec::new();
    for item in &a.items {
        position += 1;
        if position < min {
            continue;
        }
        let hit = if search_null {
            item.is_null()
        } else if item.is_null() {
            false
        } else {
            compare::sql_eq(ctx.env, elem, search, item)? == Some(true)
        };
        if hit {
            out.push(position as i32);
            if stop_at_first {
                break;
            }
        }
    }
    Ok(out)
}

/// PG `array_position_common`: NULL array -> NULL; multi-dim -> 0A000; empty array -> NULL;
/// NULL search without NULL elements -> NULL; NULL start -> 22004.
fn array_position_common(args: &[Datum], ctx: &FuncCtx, with_start: bool) -> Result<Datum> {
    let a = match opt_array(arg(args, 0)?)? {
        Some(a) => a,
        None => return Ok(Datum::Null),
    };
    if a.dims.len() > 1 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    if a.dims.is_empty() {
        return Ok(Datum::Null);
    }
    let search = arg(args, 1)?;
    if search.is_null() && !a.items.iter().any(|d| d.is_null()) {
        return Ok(Datum::Null);
    }
    let min = if with_start {
        match args.get(2) {
            Some(Datum::Null) | None => {
                return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "initial position must not be null"))
            }
            Some(d) => int4_of(d)? as i64,
        }
    } else {
        a.dims[0].1 as i64
    };
    let found = matching_positions(ctx, a, search, min, true)?;
    Ok(match found.first() {
        Some(p) => Datum::Int4(*p),
        None => Datum::Null,
    })
}

fn array_position_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    array_position_common(args, ctx, false)
}

fn array_position_start_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    array_position_common(args, ctx, true)
}

fn array_positions_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = match opt_array(arg(args, 0)?)? {
        Some(a) => a,
        None => return Ok(Datum::Null),
    };
    if a.dims.len() > 1 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "searching for elements in multidimensional arrays is not supported",
        ));
    }
    if a.dims.is_empty() {
        return Ok(Datum::array(Array::empty(oids::INT4)));
    }
    let search = arg(args, 1)?;
    if search.is_null() && !a.items.iter().any(|d| d.is_null()) {
        return Ok(Datum::array(Array::empty(oids::INT4)));
    }
    let min = a.dims[0].1 as i64;
    let found = matching_positions(ctx, a, search, min, false)?;
    let items: Vec<Datum> = found.into_iter().map(Datum::Int4).collect();
    Ok(Datum::array(Array::from_vec(oids::INT4, items)))
}

// ---------------------------------------------------------------------------------------------
// array_remove / array_replace
// ---------------------------------------------------------------------------------------------

/// PG `array_replace_internal`. `remove` drops matching elements (1-D only); otherwise matching
/// elements become `replace`. A NULL `search` matches NULL elements.
fn replace_internal(env: &dyn TypeEnv, a: &Array, search: &Datum, replace: &Datum, remove: bool) -> Result<Array> {
    if a.items.is_empty() {
        return Ok(a.clone());
    }
    if remove && a.dims.len() > 1 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "removing elements from multidimensional arrays is not supported",
        ));
    }
    let elem = TypeId::of(a.elem);
    let search_null = search.is_null();
    let mut out: Vec<Datum> = Vec::with_capacity(a.items.len());
    let mut changed = false;
    for item in &a.items {
        if item.is_null() {
            if search_null {
                if remove {
                    changed = true;
                    continue;
                }
                if !replace.is_null() {
                    out.push(replace.clone());
                    changed = true;
                    continue;
                }
            }
            out.push(Datum::Null);
        } else if search_null {
            out.push(item.clone());
        } else if compare::sql_eq(env, elem, item, search)? == Some(true) {
            changed = true;
            if !remove {
                out.push(replace.clone());
            }
        } else {
            out.push(item.clone());
        }
    }
    if !changed {
        return Ok(a.clone());
    }
    if out.is_empty() {
        return Ok(Array::empty(a.elem));
    }
    let mut dims = a.dims.clone();
    if remove {
        dims[0].0 = out.len() as i32;
    }
    Ok(Array { elem: a.elem, dims, items: out })
}

fn array_remove_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = match opt_array(arg(args, 0)?)? {
        Some(a) => a,
        None => return Ok(Datum::Null),
    };
    Ok(Datum::array(replace_internal(ctx.env, a, arg(args, 1)?, &Datum::Null, true)?))
}

fn array_replace_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = match opt_array(arg(args, 0)?)? {
        Some(a) => a,
        None => return Ok(Datum::Null),
    };
    Ok(Datum::array(replace_internal(ctx.env, a, arg(args, 1)?, arg(args, 2)?, false)?))
}

// ---------------------------------------------------------------------------------------------
// array_fill
// ---------------------------------------------------------------------------------------------

/// Shared body of `array_fill` and `array_fill_with_lower_bounds` (PG `array_fill_internal`).
fn array_fill_common(ctx: &FuncCtx, value: &Datum, dims_d: &Datum, lbs_d: Option<&Datum>) -> Result<Datum> {
    let elmtype = ctx.arg_type(0).oid;
    if dims_d.is_null() || lbs_d.map_or(false, |d| d.is_null()) {
        return Err(PgError::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "dimension array or low bound array cannot be null",
        ));
    }
    let dims = array_arg(dims_d)?;
    if dims.dims.len() > 1 {
        return Err(subscript_err("wrong number of array subscripts").with_detail("Dimension array must be one dimensional."));
    }
    if dims.items.iter().any(|d| d.is_null()) {
        return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "dimension values cannot be null"));
    }
    let ndims = dims.items.len();
    if ndims > MAXDIM {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("number of array dimensions ({ndims}) exceeds the maximum allowed ({MAXDIM})"),
        ));
    }
    let mut dimv = Vec::with_capacity(ndims);
    for d in &dims.items {
        dimv.push(int4_of(d)?);
    }
    let lbsv: Vec<i32> = match lbs_d {
        None => vec![1; ndims],
        Some(d) => {
            let l = array_arg(d)?;
            if l.dims.len() > 1 {
                return Err(subscript_err("wrong number of array subscripts")
                    .with_detail("Dimension array must be one dimensional."));
            }
            if l.items.iter().any(|d| d.is_null()) {
                return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "dimension values cannot be null"));
            }
            if ndims != l.items.len() {
                return Err(subscript_err("wrong number of array subscripts")
                    .with_detail("Low bound array has different size than dimensions array."));
            }
            let mut v = Vec::with_capacity(ndims);
            for d in &l.items {
                v.push(int4_of(d)?);
            }
            v
        }
    };
    let pairs: Vec<(i32, i32)> = dimv.into_iter().zip(lbsv).collect();
    let nitems = n_items(&pairs)?;
    if nitems == 0 {
        return Ok(Datum::array(Array::empty(elmtype)));
    }
    Ok(Datum::array(Array { elem: elmtype, dims: pairs, items: vec![value.clone(); nitems] }))
}

fn array_fill_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    array_fill_common(ctx, arg(args, 0)?, arg(args, 1)?, None)
}

fn array_fill_with_lower_bounds_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    array_fill_common(ctx, arg(args, 0)?, arg(args, 1)?, Some(arg(args, 2)?))
}

// ---------------------------------------------------------------------------------------------
// Text conversion: array_to_text / text_to_array / text_to_table
// ---------------------------------------------------------------------------------------------

/// PG `array_to_text_internal`: element output joined by `sep`; NULLs become `null_str` when given,
/// otherwise they are skipped.
fn array_to_text_internal(ctx: &FuncCtx, a: &Array, sep: &str, null_str: Option<&str>) -> Result<String> {
    if a.items.is_empty() {
        return Ok(String::new());
    }
    let io_ctx = ctx.io_ctx();
    let ty = TypeId::of(a.elem);
    let mut out = String::new();
    let mut printed = false;
    for item in &a.items {
        if item.is_null() {
            if let Some(ns) = null_str {
                if printed {
                    out.push_str(sep);
                }
                out.push_str(ns);
                printed = true;
            }
        } else {
            let s = io::output(&io_ctx, ty, item)?;
            if printed {
                out.push_str(sep);
            }
            out.push_str(&s);
            printed = true;
        }
    }
    Ok(out)
}

fn array_to_text_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = match opt_array(arg(args, 0)?)? {
        Some(a) => a,
        None => return Ok(Datum::Null),
    };
    let sep = match arg(args, 1)? {
        Datum::Null => return Ok(Datum::Null),
        d => text_of(d)?,
    };
    let null_str = match args.get(2) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(text_of(d)?),
    };
    Ok(Datum::Text(array_to_text_internal(ctx, a, sep, null_str)?))
}

fn field_datum(s: &str, null_str: Option<&str>) -> Datum {
    if Some(s) == null_str {
        Datum::Null
    } else {
        Datum::Text(s.to_string())
    }
}

/// PG `split_text`: fields of `input` separated by `delim` (NULL = one field per character, empty =
/// the whole input). Fields equal to `null_str` become NULL. Empty input gives no fields.
fn split_fields(input: &str, delim: Option<&str>, null_str: Option<&str>) -> Vec<Datum> {
    let mut out = Vec::new();
    if input.is_empty() {
        return out;
    }
    match delim {
        Some(d) if !d.is_empty() => {
            for part in input.split(d) {
                out.push(field_datum(part, null_str));
            }
        }
        Some(_) => out.push(field_datum(input, null_str)),
        None => {
            for c in input.chars() {
                let s = c.to_string();
                out.push(field_datum(&s, null_str));
            }
        }
    }
    out
}

fn text_to_array_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let input = match arg(args, 0)? {
        Datum::Null => return Ok(Datum::Null),
        d => text_of(d)?,
    };
    let delim = match args.get(1) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(text_of(d)?),
    };
    let null_str = match args.get(2) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(text_of(d)?),
    };
    let fields = split_fields(input, delim, null_str);
    Ok(Datum::array(Array::from_vec(oids::TEXT, fields)))
}

fn text_to_table_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let input = match arg(args, 0)? {
        Datum::Null => return Ok(Box::new(std::iter::empty::<Result<Datum>>())),
        d => text_of(d)?,
    };
    let delim = match args.get(1) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(text_of(d)?),
    };
    let null_str = match args.get(2) {
        None | Some(Datum::Null) => None,
        Some(d) => Some(text_of(d)?),
    };
    Ok(rows(split_fields(input, delim, null_str)))
}

// ---------------------------------------------------------------------------------------------
// Set-returning: unnest, generate_subscripts, _pg_expandarray
// ---------------------------------------------------------------------------------------------

fn array_unnest_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let a = array_arg(arg(args, 0)?)?;
    Ok(rows(a.items.clone()))
}

fn generate_subscripts_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let a = array_arg(arg(args, 0)?)?;
    let reqdim = int4_of(arg(args, 1)?)?;
    let reverse = match args.get(2) {
        Some(d) => bool_of(d)?,
        None => false,
    };
    let nd = a.dims.len();
    if nd == 0 || nd > MAXDIM || reqdim <= 0 || reqdim as usize > nd {
        return Ok(Box::new(std::iter::empty::<Result<Datum>>()));
    }
    let (len, lb) = a.dims[reqdim as usize - 1];
    let lower = lb as i64;
    let upper = lb as i64 + len as i64 - 1;
    if reverse {
        Ok(Box::new((lower..=upper).rev().map(int4_row)))
    } else {
        Ok(Box::new((lower..=upper).map(int4_row)))
    }
}

/// `_pg_expandarray(anyarray)`: `SELECT ($1)[s], s FROM generate_series(array_lower($1,1), array_upper($1,1))`.
fn pg_expandarray_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let a = array_arg(arg(args, 0)?)?;
    let nd = a.dims.len();
    if nd == 0 {
        return Ok(Box::new(std::iter::empty::<Result<Datum>>()));
    }
    let (len, lb) = a.dims[0];
    let mut out = Vec::with_capacity(len.max(0) as usize);
    for k in 0..len.max(0) as usize {
        // `($1)[s]` with one subscript on a multi-dimensional array is NULL (fewer subscripts than dims).
        let x = if nd == 1 { a.items.get(k).cloned().unwrap_or(Datum::Null) } else { Datum::Null };
        let s = lb as i64 + k as i64;
        out.push(Datum::Record(vec![x, Datum::Int4(s as i32)]));
    }
    Ok(rows(out))
}

// ---------------------------------------------------------------------------------------------
// trim_array, array_reverse, array_shuffle, array_sample, array_sort, width_bucket_array
// ---------------------------------------------------------------------------------------------

fn trim_array_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let n = int4_of(arg(args, 1)?)?;
    let len = if a.dims.is_empty() { 0 } else { a.dims[0].0 };
    if n < 0 || n > len {
        return Err(PgError::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            format!("number of elements to trim must be between 0 and {len}"),
        ));
    }
    let keep = len - n;
    if a.dims.is_empty() || keep == 0 {
        return Ok(Datum::array(Array::empty(a.elem)));
    }
    let row = a.items.len() / len as usize;
    let mut dims = vec![(keep, 1)];
    for &(l, _) in &a.dims[1..] {
        dims.push((l, 1));
    }
    let items = a.items[..keep as usize * row].to_vec();
    Ok(Datum::array(Array { elem: a.elem, dims, items }))
}

fn array_reverse_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    if a.dims.is_empty() || a.dims[0].0 < 2 {
        return Ok(arg(args, 0)?.clone());
    }
    let mut rows_v = rows_of(a);
    rows_v.reverse();
    let items: Vec<Datum> = rows_v.concat();
    Ok(Datum::array(Array { elem: a.elem, dims: a.dims.clone(), items }))
}

/// Partial Fisher-Yates over the rows: the first `take` rows of a random permutation.
fn shuffle_rows(ctx: &FuncCtx, a: &Array, take: usize) -> Vec<Vec<Datum>> {
    let mut rows_v: Vec<Vec<Datum>> = rows_of(a).into_iter().map(|r| r.to_vec()).collect();
    let n = rows_v.len();
    for i in 0..take.min(n) {
        let j = i + random_index(ctx, n - i);
        rows_v.swap(i, j);
    }
    rows_v.truncate(take);
    rows_v
}

fn array_shuffle_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    if a.dims.is_empty() || a.dims[0].0 < 2 {
        return Ok(arg(args, 0)?.clone());
    }
    let n = a.dims[0].0 as usize;
    let rows_v = shuffle_rows(ctx, a, n);
    let items: Vec<Datum> = rows_v.into_iter().flatten().collect();
    Ok(Datum::array(Array { elem: a.elem, dims: a.dims.clone(), items }))
}

fn array_sample_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let n = int4_of(arg(args, 1)?)?;
    let nitem = if a.dims.is_empty() { 0 } else { a.dims[0].0 };
    if n < 0 || n > nitem {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("sample size must be between 0 and {nitem}"),
        ));
    }
    if n < 1 {
        return Ok(Datum::array(Array::empty(a.elem)));
    }
    let mut dims = a.dims.clone();
    dims[0] = (n, 1);
    let rows_v = shuffle_rows(ctx, a, n as usize);
    let items: Vec<Datum> = rows_v.into_iter().flatten().collect();
    Ok(Datum::array(Array { elem: a.elem, dims, items }))
}

fn cmp_rows(env: &dyn TypeEnv, elem: TypeId, x: &[Datum], y: &[Datum]) -> Result<Ordering> {
    for (p, q) in x.iter().zip(y.iter()) {
        let o = cmp_elem_nulls_last(env, elem, p, q)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// PG `array_sort_internal`: 1-D sorts elements (NULL placement by `nulls_first`); N-D sorts the
/// first-dimension sub-arrays as arrays. The first lower bound is kept.
fn sort_array(ctx: &FuncCtx, a: &Array, desc: bool, nulls_first: bool) -> Result<Datum> {
    if a.dims.is_empty() || a.dims[0].0 < 2 {
        return Ok(Datum::array(a.clone()));
    }
    let env = ctx.env;
    let elem = TypeId::of(a.elem);
    if !compare::is_orderable(env, elem)? {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("could not identify a comparison function for type {}", display_name(env, a.elem)),
        ));
    }
    let mut err: Option<PgError> = None;
    let items: Vec<Datum> = if a.dims.len() == 1 {
        let mut v = a.items.clone();
        v.sort_by(|x, y| match compare::cmp_sort(env, elem, x, y, desc, nulls_first) {
            Ok(o) => o,
            Err(e) => {
                if err.is_none() {
                    err = Some(e);
                }
                Ordering::Equal
            }
        });
        v
    } else {
        let mut rows_v = rows_of(a);
        rows_v.sort_by(|x, y| match cmp_rows(env, elem, x, y) {
            Ok(o) => {
                if desc {
                    o.reverse()
                } else {
                    o
                }
            }
            Err(e) => {
                if err.is_none() {
                    err = Some(e);
                }
                Ordering::Equal
            }
        });
        rows_v.concat()
    };
    if let Some(e) = err {
        return Err(e);
    }
    Ok(Datum::array(Array { elem: a.elem, dims: a.dims.clone(), items }))
}

fn array_sort_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    sort_array(ctx, a, false, false)
}

fn array_sort_order_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let desc = bool_of(arg(args, 1)?)?;
    sort_array(ctx, a, desc, desc)
}

fn array_sort_order_nulls_first_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = array_arg(arg(args, 0)?)?;
    let desc = bool_of(arg(args, 1)?)?;
    let nulls_first = bool_of(arg(args, 2)?)?;
    sort_array(ctx, a, desc, nulls_first)
}

/// PG `width_bucket_array`: number of thresholds `<= operand` (binary search).
fn width_bucket_array_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let operand = arg(args, 0)?;
    let th = array_arg(arg(args, 1)?)?;
    if th.dims.len() > 1 {
        return Err(subscript_err("thresholds must be one-dimensional array"));
    }
    if th.items.iter().any(|d| d.is_null()) {
        return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "thresholds array must not contain NULLs"));
    }
    let n = th.items.len();
    if th.elem == oids::FLOAT8 {
        let op = match operand {
            Datum::Float8(f) => *f,
            other => return Err(type_mismatch("float8", other)),
        };
        if op.is_nan() {
            return Ok(Datum::Int4(n as i32));
        }
        let mut left = 0usize;
        let mut right = n;
        while left < right {
            let mid = (left + right) / 2;
            let t = match &th.items[mid] {
                Datum::Float8(f) => *f,
                other => return Err(type_mismatch("float8", other)),
            };
            if t.is_nan() || op < t {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        return Ok(Datum::Int4(left as i32));
    }
    let elem = TypeId::of(th.elem);
    if !compare::is_orderable(ctx.env, elem)? {
        return Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not identify a comparison function for type {}", display_name(ctx.env, th.elem)),
        ));
    }
    let mut left = 0usize;
    let mut right = n;
    while left < right {
        let mid = (left + right) / 2;
        if compare::cmp_datums(ctx.env, elem, operand, &th.items[mid])? == Ordering::Less {
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    Ok(Datum::Int4(left as i32))
}

// ---------------------------------------------------------------------------------------------
// Tests (executed centrally)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry::builtin_env;

    fn ints(v: &[i32]) -> Datum {
        Datum::array(Array::from_vec(oids::INT4, v.iter().map(|x| Datum::Int4(*x)).collect()))
    }

    fn texts(v: &[Option<&str>]) -> Datum {
        let items = v
            .iter()
            .map(|x| match x {
                Some(s) => Datum::Text((*s).to_string()),
                None => Datum::Null,
            })
            .collect();
        Datum::array(Array::from_vec(oids::TEXT, items))
    }

    fn run<R>(arg_types: &[TypeId], f: impl FnOnce(&FuncCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types,
            result_type: TypeId::of(oids::TEXT),
            collation: 0,
        };
        f(&ctx)
    }

    #[test]
    fn append_and_prepend() {
        let r = run(&[TypeId::of(oids::INT4_ARRAY), TypeId::INT4], |c| {
            array_append_fn(&[ints(&[1]), Datum::Int4(2)], c)
        });
        assert_eq!(r.unwrap(), ints(&[1, 2]));
        let r = run(&[TypeId::INT4, TypeId::of(oids::INT4_ARRAY)], |c| {
            array_prepend_fn(&[Datum::Int4(0), ints(&[1])], c)
        });
        assert_eq!(r.unwrap(), ints(&[0, 1]));
    }

    #[test]
    fn dims_and_lengths() {
        let arr = Datum::array(Array { elem: oids::INT4, dims: vec![(2, 2)], items: vec![Datum::Int4(5), Datum::Int4(6)] });
        let r = run(&[], |c| array_dims_fn(std::slice::from_ref(&arr), c));
        assert_eq!(r.unwrap(), Datum::Text("[2:3]".to_string()));
        let r = run(&[], |c| array_cardinality_fn(std::slice::from_ref(&arr), c));
        assert_eq!(r.unwrap(), Datum::Int4(2));
    }

    #[test]
    fn position_and_remove() {
        let r = run(&[], |c| array_position_fn(&[texts(&[Some("a"), Some("b")]), Datum::Text("b".into())], c));
        assert_eq!(r.unwrap(), Datum::Int4(2));
        let r = run(&[], |c| array_remove_fn(&[ints(&[1, 2, 1]), Datum::Int4(1)], c));
        assert_eq!(r.unwrap(), ints(&[2]));
    }

    #[test]
    fn null_equality_and_ordering() {
        let with_null = Datum::array(Array {
            elem: oids::INT4,
            dims: vec![(2, 1)],
            items: vec![Datum::Int4(1), Datum::Null],
        });
        let r = run(&[], |c| array_eq_fn(&[with_null.clone(), with_null.clone()], c));
        assert_eq!(r.unwrap(), Datum::Bool(true));
        let r = run(&[], |c| array_lt_fn(&[ints(&[1, 2]), ints(&[1, 2, 0])], c));
        assert_eq!(r.unwrap(), Datum::Bool(true));
    }

    #[test]
    fn containment() {
        let r = run(&[], |c| arraycontains_fn(&[texts(&[Some("a")]), texts(&[Some("a"), Some("a")])], c));
        assert_eq!(r.unwrap(), Datum::Bool(true));
    }

    #[test]
    fn text_conversions() {
        let r = run(&[], |c| text_to_array_fn(&[Datum::Text("a,b,,c".into()), Datum::Text(",".into())], c));
        assert_eq!(r.unwrap(), texts(&[Some("a"), Some("b"), Some(""), Some("c")]));
        let nulls = Datum::array(Array {
            elem: oids::INT4,
            dims: vec![(3, 1)],
            items: vec![Datum::Int4(1), Datum::Null, Datum::Int4(3)],
        });
        let r = run(&[], |c| {
            array_to_text_fn(&[nulls.clone(), Datum::Text(",".into()), Datum::Text("*".into())], c)
        });
        assert_eq!(r.unwrap(), Datum::Text("1,*,3".to_string()));
    }

    #[test]
    fn unnest_and_trim() {
        let two_d = Datum::array(Array {
            elem: oids::INT4,
            dims: vec![(2, 1), (2, 1)],
            items: vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3), Datum::Int4(4)],
        });
        let n = run(&[], |c| array_unnest_fn(std::slice::from_ref(&two_d), c).map(|it| it.count()));
        assert_eq!(n.unwrap(), 4);
        let r = run(&[], |c| trim_array_fn(&[ints(&[1, 2, 3, 4, 5, 6]), Datum::Int4(2)], c));
        assert_eq!(r.unwrap(), ints(&[1, 2, 3, 4]));
    }

    #[test]
    fn width_bucket_counts_thresholds() {
        let r = run(&[], |c| width_bucket_array_fn(&[Datum::Int4(3), ints(&[1, 2, 4])], c));
        assert_eq!(r.unwrap(), Datum::Int4(2));
    }
}
