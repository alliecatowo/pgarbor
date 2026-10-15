//! Character type I/O: text, varchar(n), bpchar(n), name, "char", unknown, cstring, pg_node_tree
//! (TYPE-01, TYPE-15). Lease TYP-13.
//!
//! PG18 reference: `utils/adt/varlena.c` (textin/out/recv/send), `varchar.c` (bpcharin, varcharin,
//! `bpchar_input`, `varchar_input`), `name.c` (namein), `char.c` (charin/charout).
//!
//! All string Datums are `Datum::Text(String)` (valid UTF-8). Rules:
//! - Any input containing U+0000 → 22021 `invalid byte sequence for encoding "UTF8": 0x00`.
//!   Binary input that is not valid UTF-8 → 22021 `invalid byte sequence for encoding "UTF8": 0xNN 0xNN`
//!   (PG prints the offending bytes, up to the remaining length of that sequence, as `0x%02x` joined by spaces).
//! - text, unknown, cstring, pg_node_tree: stored verbatim.
//! - varchar(n) (typmod n+4): length counted in CHARACTERS. If longer than n: when every excess
//!   character is a space, silently truncate to n; otherwise 22001
//!   `value too long for type character varying(n)`. No typmod → no limit.
//! - bpchar(n): same length check (message `value too long for type character(n)`), then right-pad
//!   with spaces to n characters. The STORED value keeps its padding; output prints it padded.
//!   bpchar without typmod (`'ab  '::bpchar`) is stored as given (no padding, no trimming).
//!   SQL `char` with no length is `bpchar(1)` — that is the binder's job (`names`), not this file's.
//! - name: truncated (never an error) to at most 63 BYTES on a UTF-8 character boundary.
//! - "char" (`Datum::Char(u8)`): input = first BYTE of the string (multi-byte UTF-8 → its first byte),
//!   empty string → 0, except the 4-char form `\ooo` (backslash + 3 octal digits) which gives that byte.
//!   Output: 0 → ``, byte >= 0x80 → `\ooo` (3-digit octal), otherwise the ASCII char.
//!   Binary: exactly 1 byte (recv of 0 bytes → value 0 is NOT allowed: 22P03).
//! - Binary I/O for text/varchar/bpchar/name/unknown: the raw UTF-8 bytes (recv applies the same
//!   typmod rules as input).
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

/// Apply varchar/bpchar length rules for typmod `typmod` (no-op when < 0).
/// `is_bpchar` selects padding + the `character(n)` message. `explicit` = explicit cast semantics
/// (`'abcd'::varchar(3)` silently truncates to `abc` instead of erroring; oracle-verified).
// CONTRACT
#[allow(unused_variables)]
pub fn apply_char_typmod(s: String, typmod: i32, is_bpchar: bool, explicit: bool) -> Result<String> {
    Err(PgError::unimplemented("TYP-13", "character length coercion"))
}

/// Truncate to a valid `name` (≤ 63 bytes on a char boundary).
// CONTRACT
#[allow(unused_variables)]
pub fn truncate_name(s: &str) -> Result<String> {
    Err(PgError::unimplemented("TYP-13", "name truncation"))
}

/// bpchar semantic value: the string without trailing spaces (used by comparison, length, casts to text).
pub fn bpchar_trimmed(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// Text input for this family (see module docs).
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-13", "character type input"))
}

/// Text output for this family.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-13", "character type output"))
}

/// Binary input for this family.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-13", "character type binary input"))
}

/// Binary output for this family.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-13", "character type binary output"))
}
