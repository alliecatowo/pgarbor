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
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for an IntOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-05", format!("function {}", proc.name))
}
