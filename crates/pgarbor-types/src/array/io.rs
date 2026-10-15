//! Array text and binary I/O (TYPE-09, TYPE-15). Lease TYP-37 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/arrayfuncs.c` (`array_in`, `ReadArrayDimensions`,
//! `ReadArrayStr`, `ReadArrayToken`, `array_out`, `array_recv`, `array_send`).
//! `ty.oid` is the ARRAY type (e.g. 1007); element type = `registry::element_type_of(ctx.env, ty.oid)`
//! (or `elem` field of `TypeInfo`); element I/O goes through `crate::io::{input,output,recv,send}` with
//! `TypeId::new(elem, ty.typmod)` (an array typmod applies to its elements: `varchar(3)[]`).
//! Delimiter = element type's `TypeInfo::delim` (`,` for everything except `box` which uses `;`).
//!
//! Text input:
//! - Optional dimension decoration `[lb:ub][lb:ub]...=` (lb optional: `[3]` means `[1:3]`), then `{...}`.
//! - Elements: unquoted (whitespace around them is trimmed; backslash escapes the next char), or
//!   double-quoted (`\"` and `\\` escapes; whitespace preserved). Unquoted `NULL` (any case) is SQL NULL;
//!   quoted `"NULL"` is the string. Empty unquoted element (`{a,,b}`) → error.
//! - Nested braces give dimensions; all sub-arrays must have matching dimensions. `{}` → empty array.
//!   `'{{}}'` → empty array (oracle) .
//! - Errors (22P02 `malformed array literal: "<input>"` + DETAIL, oracle-verified):
//!   `Unexpected end of input.` (`{1,2`), `Multidimensional arrays must have sub-arrays with matching
//!   dimensions.` (`{1,{2}}`, `{{1},2}`), `Array value must start with "{" or dimension information.` (`x`),
//!   `Unexpected "}" character.`, `Unexpected array element.`, `Junk after closing right brace.`,
//!   `Specified array dimensions do not match array contents.`, `Missing "]" after array dimensions.`,
//!   `Missing "=" after array dimensions.`; more than 6 dims → 54000
//!   `number of array dimensions exceeds the maximum allowed (6)`.
//!   Element errors propagate from the element input (`'{1,a}'::int[]` → `invalid input syntax for type integer: "a"`).
//!
//! Text output: `{1,2}`, `{{1,2},{3,4}}`, `[2:3]={5,6}` (decoration only when some lower bound != 1),
//! `{}` for empty; NULL → `NULL`; an element is double-quoted if it is empty, equals `NULL`
//! (case-insensitive), or contains `"` `\` `{` `}` the delimiter or whitespace (isspace); inside quotes
//! `"` and `\` are backslash-escaped. Oracle: `array['a,b','{x}','\','"']` → `{"a,b","{x}","\\","\""}`;
//! `'{"a b",NULL,"null",""}'::text[]` → `{"a b",NULL,"null",""}`.
//!
//! Binary (big-endian): `ndim: i32, has_nulls: i32 (0/1), elem_oid: u32`, then per dim `len: i32, lbound: i32`,
//! then per element `len: i32` (-1 = NULL) followed by the element's binary send bytes. recv validates
//! ndim (0..=6), element OID must equal the expected element type (else 42804
//! `binary data has array element type %u (%s) instead of expected %u (%s)`), lengths non-negative,
//! total element count overflow (54000 `array size exceeds the maximum allowed (134217727)`).
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

/// `array_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-37", "array input"))
}

/// `array_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-37", "array output"))
}

/// `array_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-37", "array binary input"))
}

/// `array_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-37", "array binary output"))
}
