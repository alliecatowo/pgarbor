//! Integer operators and functions (TYPE-14). Lease TYP-05.
//!
//! PG18 reference: `src/backend/utils/adt/int.c`, `int8.c`.
//! Owned procs (ImplModule::IntOps; list them with
//! `grep 'module: ImplModule::IntOps' src/catalog_data/procs.rs`), matched by `proc.prosrc`:
//! - arithmetic for every width pair: `int{2,4,8}{pl,mi,mul,div}`, `int24pl`, `int42mi`, `int48mul`,
//!   `int84div`, `int28*`, `int82*` ..., `int{2,4,8}mod`, `int{2,4,8}um` (negate), `int{2,4,8}up`,
//!   `int{2,4,8}abs`, `int{2,4}larger/smaller`, `int8larger/smaller`, `int4inc`;
//! - comparison for every width pair: `int{2,4,8}{eq,ne,lt,le,gt,ge}`, `int24eq`, `int48lt`, ...,
//!   `btint{2,4,8}cmp`, `btint24cmp` ... (return Int4 -1/0/1);
//! - bitwise: `int{2,4,8}{and,or,xor,not,shl,shr}`;
//! - casts: `i2toi4`, `i4toi2`, `int48` (int4→int8), `int84` (int8→int4), `int28`, `int82`;
//! - `int4gcd`, `int8gcd`, `int4lcm`, `int8lcm`; `int4random`/`int8random` (random(min,max) via
//!   `ctx.session.random_f64()`, uniform inclusive; error if min > max: 22023
//!   `lower bound must be less than or equal to upper bound`);
//! - `in_range_int{2,4,8}_int{2,4,8}` (window RANGE offsets; `in_range(val, base, offset, sub, less)`,
//!   negative offset → 22013 `invalid preceding or following size in window function`);
//! - `hashint2`, `hashint4`, `hashint8` (+ `extended` with int8 seed) = PG `hash_uint32` /
//!   `hashint8` (folds high/low halves: `lohalf ^ (val >= 0 ? hihalf : ~hihalf)`) via `compare::pg_hash_*`.
//!
//! Semantics (oracle-verified):
//! - Results take the WIDER argument type (`int24pl` → int4, `int48mul` → int8, `int82div` → int8).
//! - Overflow → 22003 with the RESULT type's message: `smallint out of range`,
//!   `integer out of range`, `bigint out of range`. Includes `-(-2147483648)`, `abs(-2147483648)`,
//!   `(-2147483648) / -1`, `(-32768)::int2 / -1::int2`, and narrowing casts (`i4toi2` → `smallint out of range`,
//!   `int84` → `integer out of range`).
//! - Division by zero (`/` and `%`) → 22012 `division by zero`. Division truncates toward zero
//!   (`-7/2 = -3`); `%` has the sign of the dividend (`-7 % 3 = -1`, `7 % -3 = 1`); `INT_MIN % -1 = 0`
//!   (no error).
//! - Shifts mask the count like x86 C: int4 `<<`/`>>` use `count & 31`, int8 `count & 63`, int2 is
//!   computed in int32 with `count & 31` then truncated to i16 (`1::int4 << 33 = 2`). `>>` is arithmetic.
//! - gcd/lcm: `gcd(INT_MIN, INT_MIN)` and `gcd(INT_MIN, 0)` → 22003 `integer out of range`;
//!   lcm overflow → `integer out of range`; results are non-negative.
//!
//! Implementation: every operand is widened to `i64` (exact for int2/int4/int8), the operation runs
//! with `checked_*` on `i64`, and the exact result is narrowed to the result width (`fit`). For
//! narrower operands the i64 math can never overflow, so the narrowing check alone produces the PG
//! overflow error for the result type. Division/modulo special cases are handled before the call.
use crate::compare;
use crate::context::FuncCtx;
use crate::datum::{type_mismatch, Datum};
use crate::func::{FuncImpl, ScalarFn};
use crate::catalog_data::BuiltinProc;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// SQLSTATE 22013 `invalid_preceding_or_following_size` (not in the shared constant table).
const INVALID_PRECEDING_OR_FOLLOWING_SIZE: SqlState = SqlState(*b"22013");

/// Result width of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Width {
    W2,
    W4,
    W8,
}

/// Marker types that select the result width of a generic implementation.
trait Out {
    const W: Width;
}
struct O2;
struct O4;
struct O8;
impl Out for O2 {
    const W: Width = Width::W2;
}
impl Out for O4 {
    const W: Width = Width::W4;
}
impl Out for O8 {
    const W: Width = Width::W8;
}

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let imp: ScalarFn = match proc.prosrc {
        // comparisons (all widths widen to i64)
        "int2eq" | "int4eq" | "int8eq" | "int24eq" | "int42eq" | "int28eq" | "int82eq" | "int48eq"
        | "int84eq" => op_eq,
        "int2ne" | "int4ne" | "int8ne" | "int24ne" | "int42ne" | "int28ne" | "int82ne" | "int48ne"
        | "int84ne" => op_ne,
        "int2lt" | "int4lt" | "int8lt" | "int24lt" | "int42lt" | "int28lt" | "int82lt" | "int48lt"
        | "int84lt" => op_lt,
        "int2le" | "int4le" | "int8le" | "int24le" | "int42le" | "int28le" | "int82le" | "int48le"
        | "int84le" => op_le,
        "int2gt" | "int4gt" | "int8gt" | "int24gt" | "int42gt" | "int28gt" | "int82gt" | "int48gt"
        | "int84gt" => op_gt,
        "int2ge" | "int4ge" | "int8ge" | "int24ge" | "int42ge" | "int28ge" | "int82ge" | "int48ge"
        | "int84ge" => op_ge,
        "btint2cmp" | "btint4cmp" | "btint8cmp" | "btint24cmp" | "btint42cmp" | "btint28cmp"
        | "btint82cmp" | "btint48cmp" | "btint84cmp" => op_btcmp,

        // addition
        "int2pl" => op_add::<O2>,
        "int4pl" | "int24pl" | "int42pl" => op_add::<O4>,
        "int8pl" | "int28pl" | "int82pl" | "int48pl" | "int84pl" => op_add::<O8>,
        // subtraction
        "int2mi" => op_sub::<O2>,
        "int4mi" | "int24mi" | "int42mi" => op_sub::<O4>,
        "int8mi" | "int28mi" | "int82mi" | "int48mi" | "int84mi" => op_sub::<O8>,
        // multiplication
        "int2mul" => op_mul::<O2>,
        "int4mul" | "int24mul" | "int42mul" => op_mul::<O4>,
        "int8mul" | "int28mul" | "int82mul" | "int48mul" | "int84mul" => op_mul::<O8>,
        // division
        "int2div" => op_div::<O2>,
        "int4div" | "int24div" | "int42div" => op_div::<O4>,
        "int8div" | "int28div" | "int82div" | "int48div" | "int84div" => op_div::<O8>,
        // modulo
        "int2mod" => op_mod::<O2>,
        "int4mod" => op_mod::<O4>,
        "int8mod" => op_mod::<O8>,

        // unary
        "int2um" => op_neg::<O2>,
        "int4um" => op_neg::<O4>,
        "int8um" => op_neg::<O8>,
        "int2up" => op_plus::<O2>,
        "int4up" => op_plus::<O4>,
        "int8up" => op_plus::<O8>,
        "int2abs" => op_abs::<O2>,
        "int4abs" => op_abs::<O4>,
        "int8abs" => op_abs::<O8>,
        "int4inc" => op_inc,
        "int2larger" => op_larger::<O2>,
        "int4larger" => op_larger::<O4>,
        "int8larger" => op_larger::<O8>,
        "int2smaller" => op_smaller::<O2>,
        "int4smaller" => op_smaller::<O4>,
        "int8smaller" => op_smaller::<O8>,

        // bitwise
        "int2and" => bit_and::<O2>,
        "int4and" => bit_and::<O4>,
        "int8and" => bit_and::<O8>,
        "int2or" => bit_or::<O2>,
        "int4or" => bit_or::<O4>,
        "int8or" => bit_or::<O8>,
        "int2xor" => bit_xor::<O2>,
        "int4xor" => bit_xor::<O4>,
        "int8xor" => bit_xor::<O8>,
        "int2not" => bit_not::<O2>,
        "int4not" => bit_not::<O4>,
        "int8not" => bit_not::<O8>,
        "int2shl" => op_shl2,
        "int2shr" => op_shr2,
        "int4shl" => op_shl4,
        "int4shr" => op_shr4,
        "int8shl" => op_shl8,
        "int8shr" => op_shr8,

        // casts: the result width does the range check
        "i2toi4" => op_plus::<O4>,
        "i4toi2" => op_plus::<O2>,
        "int48" => op_plus::<O8>,
        "int84" => op_plus::<O4>,
        "int28" => op_plus::<O8>,
        "int82" => op_plus::<O2>,

        // gcd / lcm
        "int4gcd" => op_gcd::<O4>,
        "int8gcd" => op_gcd::<O8>,
        "int4lcm" => op_lcm::<O4>,
        "int8lcm" => op_lcm::<O8>,

        // random(min, max)
        "int4random" => op_random4,
        "int8random" => op_random8,

        // window RANGE offsets
        "in_range_int8_int8"
        | "in_range_int4_int8"
        | "in_range_int4_int4"
        | "in_range_int4_int2"
        | "in_range_int2_int4"
        | "in_range_int2_int2"
        | "in_range_int2_int8" => in_range_any,

        // hashing
        "hashint2" | "hashint4" => hash_u32_key,
        "hashint8" => hash_i64_key,
        "hashint2extended" | "hashint4extended" => hash_u32_key_extended,
        "hashint8extended" => hash_i64_key_extended,

        _ => return None,
    };
    Some(FuncImpl::Scalar(imp))
}

/// 0A000 for an IntOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-05", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// PG 22003 `NUMERIC_VALUE_OUT_OF_RANGE` with the result type's message.
fn overflow(w: Width) -> PgError {
    let what = match w {
        Width::W2 => "smallint out of range",
        Width::W4 => "integer out of range",
        Width::W8 => "bigint out of range",
    };
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, what)
}

/// PG 22012 `division by zero`.
fn div_zero() -> PgError {
    PgError::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}

/// Narrow an exact i64 result to width `w`. `None` (i64 overflow) and out-of-range values are the
/// PG overflow error of the result type.
fn fit(v: Option<i64>, w: Width) -> Result<Datum> {
    let v = v.ok_or_else(|| overflow(w))?;
    match w {
        Width::W2 => i16::try_from(v).map(Datum::Int2).map_err(|_| overflow(w)),
        Width::W4 => i32::try_from(v).map(Datum::Int4).map_err(|_| overflow(w)),
        Width::W8 => Ok(Datum::Int8(v)),
    }
}

/// Integer argument of any width, widened to i64 (exact).
fn int_arg(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Int2(v)) => Ok(i64::from(*v)),
        Some(Datum::Int4(v)) => Ok(i64::from(*v)),
        Some(Datum::Int8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("integer", other)),
        None => Err(PgError::internal("missing integer function argument")),
    }
}

/// Boolean argument (`in_range` sub/less flags).
fn bool_arg(args: &[Datum], i: usize) -> Result<bool> {
    match args.get(i) {
        Some(Datum::Bool(b)) => Ok(*b),
        Some(other) => Err(type_mismatch("boolean", other)),
        None => Err(PgError::internal("missing boolean function argument")),
    }
}

/// Three-way comparison of two integer arguments of any widths.
fn cmp_args(args: &[Datum]) -> Result<Ordering> {
    Ok(int_arg(args, 0)?.cmp(&int_arg(args, 1)?))
}

// ---------------------------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------------------------

fn op_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_eq()))
}

fn op_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_ne()))
}

fn op_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_lt()))
}

fn op_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_le()))
}

fn op_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_gt()))
}

fn op_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)?.is_ge()))
}

/// `btintNcmp`: -1, 0 or 1 as int4.
fn op_btcmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = match cmp_args(args)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Datum::Int4(v))
}

// ---------------------------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------------------------

fn op_add<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    fit(a.checked_add(b), O::W)
}

fn op_sub<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    fit(a.checked_sub(b), O::W)
}

fn op_mul<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    fit(a.checked_mul(b), O::W)
}

/// Truncating division. `MIN / -1` overflows the result width (checked_div returns None or the
/// narrowing fails).
fn op_div<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    if b == 0 {
        return Err(div_zero());
    }
    fit(a.checked_div(b), O::W)
}

/// Remainder with the sign of the dividend. `x % -1` is 0 (PG avoids the INT_MIN % -1 trap).
fn op_mod<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    if b == 0 {
        return Err(div_zero());
    }
    if b == -1 {
        return fit(Some(0), O::W);
    }
    fit(Some(a % b), O::W)
}

/// Unary minus.
fn op_neg<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(int_arg(args, 0)?.checked_neg(), O::W)
}

/// Unary plus. Also the range-checked width conversion used by the casts.
fn op_plus<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(Some(int_arg(args, 0)?), O::W)
}

fn op_abs<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(int_arg(args, 0)?.checked_abs(), O::W)
}

/// `int4inc`: x + 1.
fn op_inc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(int_arg(args, 0)?.checked_add(1), Width::W4)
}

fn op_larger<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    fit(Some(a.max(b)), O::W)
}

fn op_smaller<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    fit(Some(a.min(b)), O::W)
}

// ---------------------------------------------------------------------------------------------
// Bitwise and shifts
// ---------------------------------------------------------------------------------------------

/// Bitwise and/or/xor of sign-extended values stay in range of the operand width.
fn bit_and<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(Some(int_arg(args, 0)? & int_arg(args, 1)?), O::W)
}

fn bit_or<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(Some(int_arg(args, 0)? | int_arg(args, 1)?), O::W)
}

fn bit_xor<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(Some(int_arg(args, 0)? ^ int_arg(args, 1)?), O::W)
}

fn bit_not<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    fit(Some(!int_arg(args, 0)?), O::W)
}

/// Shift count as used by the C implementation: masked to the operand width (x86 behaviour).
fn shift_count(args: &[Datum], mask: i64) -> Result<u32> {
    Ok((int_arg(args, 1)? & mask) as u32)
}

/// `int2shl`: computed in int32, count & 31, truncated to int2.
fn op_shl2(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)? as i32;
    let c = shift_count(args, 31)?;
    Ok(Datum::Int2(a.wrapping_shl(c) as i16))
}

/// `int2shr`: arithmetic shift in int32, count & 31.
fn op_shr2(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)? as i32;
    let c = shift_count(args, 31)?;
    Ok(Datum::Int2(a.wrapping_shr(c) as i16))
}

fn op_shl4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)? as i32;
    let c = shift_count(args, 31)?;
    Ok(Datum::Int4(a.wrapping_shl(c)))
}

fn op_shr4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)? as i32;
    let c = shift_count(args, 31)?;
    Ok(Datum::Int4(a.wrapping_shr(c)))
}

fn op_shl8(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let c = shift_count(args, 63)?;
    Ok(Datum::Int8(a.wrapping_shl(c)))
}

fn op_shr8(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let c = shift_count(args, 63)?;
    Ok(Datum::Int8(a.wrapping_shr(c)))
}

// ---------------------------------------------------------------------------------------------
// gcd / lcm
// ---------------------------------------------------------------------------------------------

/// Absolute value as u128 (exact for every i64, including i64::MIN).
fn magnitude(v: i64) -> u128 {
    i128::from(v).unsigned_abs()
}

fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// gcd is non-negative; a result that does not fit the width (e.g. gcd(INT_MIN, 0) = 2^31) is 22003.
fn op_gcd<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = magnitude(int_arg(args, 0)?);
    let b = magnitude(int_arg(args, 1)?);
    fit(i64::try_from(gcd_u128(a, b)).ok(), O::W)
}

/// lcm(x, y) = |x / gcd(x, y) * y|; lcm with 0 is 0. The magnitude is at most 2^126, so u128 is exact.
fn op_lcm<O: Out>(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let x = int_arg(args, 0)?;
    let y = int_arg(args, 1)?;
    if x == 0 || y == 0 {
        return fit(Some(0), O::W);
    }
    let ax = magnitude(x);
    let ay = magnitude(y);
    let g = gcd_u128(ax, ay);
    let l = (ax / g) * ay;
    fit(i64::try_from(l).ok(), O::W)
}

// ---------------------------------------------------------------------------------------------
// random(min, max)
// ---------------------------------------------------------------------------------------------

/// Uniform integer in `[lo, hi]` drawn from the session PRNG. The unit draw has 53 bits of
/// precision, so for very wide ranges (int8) the distribution is approximate.
fn random_in_range(ctx: &FuncCtx, lo: i64, hi: i64) -> Result<i64> {
    if lo > hi {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "lower bound must be less than or equal to upper bound",
        ));
    }
    let span = i128::from(hi) - i128::from(lo) + 1;
    let r = ctx.session.random_f64();
    let mut off = (r * span as f64) as i128;
    if off < 0 {
        off = 0;
    }
    if off >= span {
        off = span - 1;
    }
    Ok((i128::from(lo) + off) as i64)
}

fn op_random4(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let lo = int_arg(args, 0)?;
    let hi = int_arg(args, 1)?;
    Ok(Datum::Int4(random_in_range(ctx, lo, hi)? as i32))
}

fn op_random8(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let lo = int_arg(args, 0)?;
    let hi = int_arg(args, 1)?;
    Ok(Datum::Int8(random_in_range(ctx, lo, hi)?))
}

// ---------------------------------------------------------------------------------------------
// in_range (window RANGE offsets)
// ---------------------------------------------------------------------------------------------

/// `in_range(val, base, offset, sub, less)` for all seven width combinations. All math is exact in
/// i64; when `base +/- offset` leaves i64 (only possible for int8), or lies outside the range of the
/// narrower operand types, the answer is the same as for an infinite sum: `less` for `+`, `!less`
/// for `-` (PG `in_range_int8_int8` and friends).
fn in_range_any(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let val = int_arg(args, 0)?;
    let base = int_arg(args, 1)?;
    let offset = int_arg(args, 2)?;
    let sub = bool_arg(args, 3)?;
    let less = bool_arg(args, 4)?;
    if offset < 0 {
        return Err(PgError::new(
            INVALID_PRECEDING_OR_FOLLOWING_SIZE,
            "invalid preceding or following size in window function",
        ));
    }
    // offset >= 0, so the negation cannot overflow.
    let off = if sub { -offset } else { offset };
    match base.checked_add(off) {
        Some(sum) => Ok(Datum::Bool(if less { val <= sum } else { val >= sum })),
        None => Ok(Datum::Bool(if sub { !less } else { less })),
    }
}

// ---------------------------------------------------------------------------------------------
// Hashing (PG hashfunc.c)
// ---------------------------------------------------------------------------------------------

/// `hashint2` / `hashint4`: hash_uint32 of the sign-extended value.
fn hash_u32_key(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = int_arg(args, 0)? as i32 as u32;
    Ok(Datum::Int4(compare::pg_hash_uint32(v)? as i32))
}

/// PG `hashint8` fold: low half xor (high half, or its complement for negative values).
fn fold_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = ((v as u64) >> 32) as u32;
    let mask = if v >= 0 { hi } else { !hi };
    lo ^ mask
}

/// `hashint8`.
fn hash_i64_key(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = int_arg(args, 0)?;
    Ok(Datum::Int4(compare::pg_hash_uint32(fold_int8(v))? as i32))
}

/// `hashint2extended` / `hashint4extended`: hash_uint32_extended(value, seed).
fn hash_u32_key_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = int_arg(args, 0)? as i32 as u32;
    let seed = int_arg(args, 1)? as u64;
    Ok(Datum::Int8(compare::pg_hash_uint32_extended(v, seed)? as i64))
}

/// `hashint8extended`.
fn hash_i64_key_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = int_arg(args, 0)?;
    let seed = int_arg(args, 1)? as u64;
    Ok(Datum::Int8(compare::pg_hash_uint32_extended(fold_int8(v), seed)? as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::registry::builtin_env;

    /// Run a scalar implementation with a deterministic session.
    fn run(f: ScalarFn, args: &[Datum]) -> Result<Datum> {
        let env = builtin_env();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &io,
            session: &session,
            arg_types: &[],
            result_type: TypeId::INT4,
            collation: 0,
        };
        f(args, &ctx)
    }

    /// (SQLSTATE code, message) of an expected error.
    fn err(r: Result<Datum>) -> (String, String) {
        let e = r.expect_err("expected an error");
        (e.sqlstate.code().to_string(), e.message)
    }

    #[test]
    fn int4_add_overflow_message() {
        assert_eq!(
            err(run(op_add::<O4>, &[Datum::Int4(i32::MAX), Datum::Int4(1)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
    }

    #[test]
    fn int2_add_overflow_message() {
        assert_eq!(
            err(run(op_add::<O2>, &[Datum::Int2(i16::MAX), Datum::Int2(1)])),
            ("22003".to_string(), "smallint out of range".to_string())
        );
    }

    #[test]
    fn int8_mul_overflow_message() {
        assert_eq!(
            err(run(op_mul::<O8>, &[Datum::Int8(i64::MAX), Datum::Int8(2)])),
            ("22003".to_string(), "bigint out of range".to_string())
        );
    }

    #[test]
    fn mixed_width_result_is_wider_type() {
        assert_eq!(run(op_mul::<O8>, &[Datum::Int4(2), Datum::Int8(3)]).unwrap(), Datum::Int8(6));
        assert_eq!(run(op_add::<O4>, &[Datum::Int2(2), Datum::Int4(3)]).unwrap(), Datum::Int4(5));
    }

    #[test]
    fn division_by_zero_and_min_over_minus_one() {
        assert_eq!(
            err(run(op_div::<O4>, &[Datum::Int4(1), Datum::Int4(0)])),
            ("22012".to_string(), "division by zero".to_string())
        );
        assert_eq!(
            err(run(op_div::<O4>, &[Datum::Int4(i32::MIN), Datum::Int4(-1)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(
            err(run(op_div::<O2>, &[Datum::Int2(i16::MIN), Datum::Int2(-1)])),
            ("22003".to_string(), "smallint out of range".to_string())
        );
        assert_eq!(run(op_div::<O4>, &[Datum::Int4(-7), Datum::Int4(2)]).unwrap(), Datum::Int4(-3));
    }

    #[test]
    fn modulo_signs_and_min_over_minus_one() {
        assert_eq!(run(op_mod::<O4>, &[Datum::Int4(i32::MIN), Datum::Int4(-1)]).unwrap(), Datum::Int4(0));
        assert_eq!(run(op_mod::<O4>, &[Datum::Int4(-7), Datum::Int4(3)]).unwrap(), Datum::Int4(-1));
        assert_eq!(run(op_mod::<O4>, &[Datum::Int4(7), Datum::Int4(-3)]).unwrap(), Datum::Int4(1));
        assert_eq!(
            err(run(op_mod::<O8>, &[Datum::Int8(5), Datum::Int8(0)])),
            ("22012".to_string(), "division by zero".to_string())
        );
    }

    #[test]
    fn negate_and_abs_overflow() {
        assert_eq!(
            err(run(op_neg::<O4>, &[Datum::Int4(i32::MIN)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(
            err(run(op_abs::<O4>, &[Datum::Int4(i32::MIN)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(run(op_abs::<O8>, &[Datum::Int8(-5)]).unwrap(), Datum::Int8(5));
    }

    #[test]
    fn casts_narrow_with_range_check() {
        assert_eq!(
            err(run(op_plus::<O2>, &[Datum::Int4(40000)])),
            ("22003".to_string(), "smallint out of range".to_string())
        );
        assert_eq!(run(op_plus::<O2>, &[Datum::Int4(-32768)]).unwrap(), Datum::Int2(-32768));
        assert_eq!(
            err(run(op_plus::<O4>, &[Datum::Int8(1 << 40)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(run(op_plus::<O8>, &[Datum::Int4(7)]).unwrap(), Datum::Int8(7));
    }

    #[test]
    fn shifts_mask_the_count() {
        assert_eq!(run(op_shl4, &[Datum::Int4(1), Datum::Int4(33)]).unwrap(), Datum::Int4(2));
        assert_eq!(run(op_shl8, &[Datum::Int8(1), Datum::Int4(65)]).unwrap(), Datum::Int8(2));
        assert_eq!(run(op_shr4, &[Datum::Int4(-8), Datum::Int4(1)]).unwrap(), Datum::Int4(-4));
        // 1 << 17 in int32 is 131072, truncated to int2 = 0
        assert_eq!(run(op_shl2, &[Datum::Int2(1), Datum::Int4(17)]).unwrap(), Datum::Int2(0));
    }

    #[test]
    fn bitwise_ops() {
        assert_eq!(run(bit_not::<O2>, &[Datum::Int2(0)]).unwrap(), Datum::Int2(-1));
        assert_eq!(run(bit_xor::<O8>, &[Datum::Int8(6), Datum::Int8(3)]).unwrap(), Datum::Int8(5));
    }

    #[test]
    fn gcd_and_lcm() {
        assert_eq!(run(op_gcd::<O4>, &[Datum::Int4(12), Datum::Int4(18)]).unwrap(), Datum::Int4(6));
        assert_eq!(run(op_gcd::<O4>, &[Datum::Int4(i32::MIN), Datum::Int4(-1)]).unwrap(), Datum::Int4(1));
        assert_eq!(
            err(run(op_gcd::<O4>, &[Datum::Int4(i32::MIN), Datum::Int4(0)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(
            err(run(op_gcd::<O4>, &[Datum::Int4(i32::MIN), Datum::Int4(i32::MIN)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(run(op_lcm::<O4>, &[Datum::Int4(4), Datum::Int4(-6)]).unwrap(), Datum::Int4(12));
        assert_eq!(run(op_lcm::<O4>, &[Datum::Int4(0), Datum::Int4(9)]).unwrap(), Datum::Int4(0));
        assert_eq!(
            err(run(op_lcm::<O4>, &[Datum::Int4(i32::MAX), Datum::Int4(i32::MAX - 1)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
        assert_eq!(run(op_gcd::<O8>, &[Datum::Int8(i64::MIN), Datum::Int8(-1)]).unwrap(), Datum::Int8(1));
    }

    #[test]
    fn comparisons_and_btcmp() {
        assert_eq!(run(op_lt, &[Datum::Int2(1), Datum::Int8(2)]).unwrap(), Datum::Bool(true));
        assert_eq!(run(op_ge, &[Datum::Int4(1), Datum::Int4(2)]).unwrap(), Datum::Bool(false));
        assert_eq!(run(op_btcmp, &[Datum::Int4(1), Datum::Int4(2)]).unwrap(), Datum::Int4(-1));
        assert_eq!(run(op_btcmp, &[Datum::Int8(2), Datum::Int8(2)]).unwrap(), Datum::Int4(0));
        assert_eq!(run(op_btcmp, &[Datum::Int2(3), Datum::Int4(2)]).unwrap(), Datum::Int4(1));
    }

    #[test]
    fn larger_smaller_and_inc() {
        assert_eq!(run(op_larger::<O4>, &[Datum::Int4(3), Datum::Int4(9)]).unwrap(), Datum::Int4(9));
        assert_eq!(run(op_smaller::<O2>, &[Datum::Int2(3), Datum::Int2(9)]).unwrap(), Datum::Int2(3));
        assert_eq!(
            err(run(op_inc, &[Datum::Int4(i32::MAX)])),
            ("22003".to_string(), "integer out of range".to_string())
        );
    }

    #[test]
    fn in_range_semantics() {
        // less = true: 5 <= base + offset = 3 + 2
        assert_eq!(
            run(in_range_any, &[Datum::Int4(5), Datum::Int4(3), Datum::Int4(2), Datum::Bool(false), Datum::Bool(true)])
                .unwrap(),
            Datum::Bool(true)
        );
        // base - offset = 5, val >= 5
        assert_eq!(
            run(in_range_any, &[Datum::Int4(5), Datum::Int4(10), Datum::Int4(5), Datum::Bool(true), Datum::Bool(false)])
                .unwrap(),
            Datum::Bool(true)
        );
        // int8 sum overflow: base + offset overflows, so the sum is "infinite" and val <= sum holds
        assert_eq!(
            run(in_range_any, &[Datum::Int8(7), Datum::Int8(i64::MAX), Datum::Int8(1), Datum::Bool(false), Datum::Bool(true)])
                .unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            err(run(in_range_any, &[Datum::Int4(1), Datum::Int4(1), Datum::Int4(-1), Datum::Bool(false), Datum::Bool(true)])),
            ("22013".to_string(), "invalid preceding or following size in window function".to_string())
        );
    }

    #[test]
    fn random_bounds_error() {
        assert_eq!(
            err(run(op_random4, &[Datum::Int4(5), Datum::Int4(1)])),
            ("22023".to_string(), "lower bound must be less than or equal to upper bound".to_string())
        );
        match run(op_random4, &[Datum::Int4(1), Datum::Int4(6)]).unwrap() {
            Datum::Int4(v) => assert!((1..=6).contains(&v)),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(run(op_random8, &[Datum::Int8(4), Datum::Int8(4)]).unwrap(), Datum::Int8(4));
    }

    #[test]
    fn hash_fixtures() {
        // oracle fixtures from compare.rs: hashint4(1) = hashint8(1) = -1905060026
        assert_eq!(run(hash_u32_key, &[Datum::Int4(1)]).unwrap(), Datum::Int4(-1905060026));
        assert_eq!(run(hash_i64_key, &[Datum::Int8(1)]).unwrap(), Datum::Int4(-1905060026));
        // hashint4extended(1, 0) = -3670598878359251130
        assert_eq!(
            run(hash_u32_key_extended, &[Datum::Int4(1), Datum::Int8(0)]).unwrap(),
            Datum::Int8(-3670598878359251130)
        );
    }

    #[test]
    fn fold_matches_pg_for_negative_values() {
        // lohalf ^ ~hihalf for -1: lo = 0xffffffff, hi = 0xffffffff -> ~hi = 0 -> 0xffffffff
        assert_eq!(fold_int8(-1), 0xffff_ffff);
        assert_eq!(fold_int8(1), 1);
    }
}
