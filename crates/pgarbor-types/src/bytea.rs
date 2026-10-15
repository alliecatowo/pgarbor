//! `bytea` I/O and functions (TYPE-07). Lease TYP-18.
//!
//! PG18 reference: `utils/adt/varlena.c` (`byteain`, `byteaout`, `bytearecv`, `byteasend`, bytea
//! functions), `utils/adt/encode.c` (`hex_encode/decode`, base64, escape formats).
//!
//! Text input (`byteain`):
//! - Hex format if the input starts with `\x` (or `\X`): pairs of hex digits (either case), whitespace
//!   allowed BETWEEN pairs; odd digit count → 22023 `invalid hexadecimal data: odd number of digits`;
//!   bad digit → 22023 `invalid hexadecimal digit: "Z"` (oracle: `'\xZZ'::bytea`).
//! - Otherwise escape format: bytes as-is, `\\` → backslash, `\ooo` (3 octal digits, first 0-3) → byte;
//!   any other backslash use → 22P02 `invalid input syntax for type bytea` (no value in the message).
//!   Non-ASCII characters are taken as their UTF-8 bytes (`'café'::bytea` = `\x636166c3a9`).
//! Text output: `bytea_output = hex` → `\x` + lowercase hex (`\x0102`; empty → `\x`);
//! `escape` → printable ASCII 0x20..=0x7E as-is except `\` → `\\`, others as `\ooo`.
//! Binary: raw bytes.
//!
//! Owned procs (ImplModule::Bytea): `byteaeq byteane bytealt byteale byteagt byteage byteacmp
//! bytea_larger bytea_smaller hashbytea(+extended)` (memcmp order, shorter prefix first), `byteacat` (`||`),
//! `byteaoctetlen` (length), SQL-language `bit_length(bytea)` (oid 1810), `bytea_substr(_no_len)`,
//! `byteaoverlay(_no_len)`, `byteapos`, `byteatrim bytealtrim byteartrim`, `byteaGetByte byteaSetByte
//! byteaGetBit byteaSetBit` (index out of range → 2202E `index %d out of valid range, 0..%d`),
//! `bytea_bit_count`, `bytea_reverse`, `binary_encode` (`encode(bytea, 'hex'|'base64'|'escape')`),
//! `binary_decode` (`decode(text, fmt)`; unknown format → 22023 `unrecognized encoding: "x"`;
//! base64 output wraps lines at 76 chars with `\n` exactly as PG), `int2_bytea int4_bytea int8_bytea`
//! (big-endian bytes) and `bytea_int2 bytea_int4 bytea_int8` (PG18 casts; too long → 22003
//! `smallint out of range` etc.).
use crate::catalog_data::BuiltinProc;
use crate::context::{ByteaOutput, FuncCtx, IoCtx};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result, SqlState};

/// SQLSTATE 22011 (`ERRCODE_SUBSTRING_ERROR`), not in the shared table.
const SUBSTRING_ERROR: SqlState = SqlState(*b"22011");

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Parse bytea text (hex or escape format).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_bytea(s: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    // PG's byteain matches a lowercase 'x' only.
    if b.len() >= 2 && b[0] == b'\\' && b[1] == b'x' {
        hex_decode(&b[2..])
    } else {
        escape_decode(b)
    }
}

/// Format bytea per `bytea_output`.
// CONTRACT
#[allow(unused_variables)]
pub fn format_bytea(b: &[u8], hex: bool) -> Result<String> {
    if hex {
        let mut out = String::with_capacity(2 + 2 * b.len());
        out.push('\\');
        out.push('x');
        for &c in b {
            out.push(HEX_LOWER[(c >> 4) as usize] as char);
            out.push(HEX_LOWER[(c & 0x0f) as usize] as char);
        }
        Ok(out)
    } else {
        // byteaout, escape branch: printable ASCII as-is, backslash doubled, all else octal.
        let mut out = String::with_capacity(b.len());
        for &c in b {
            if c == b'\\' {
                out.push_str("\\\\");
            } else if !(0x20..=0x7e).contains(&c) {
                out.push_str(&format!("\\{:03o}", c));
            } else {
                out.push(c as char);
            }
        }
        Ok(out)
    }
}

/// `byteain`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Ok(Datum::Bytea(parse_bytea(s)?))
}

/// `byteaout` (uses `ctx.config.bytea_output`).
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let b = arg_bytes(d)?;
    format_bytea(b, matches!(ctx.config.bytea_output, ByteaOutput::Hex))
}

/// `bytearecv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Ok(Datum::Bytea(buf.to_vec()))
}

/// `byteasend`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Ok(arg_bytes(d)?.to_vec())
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: crate::func::ScalarFn = match proc.prosrc {
        "byteaoctetlen" => f_octet_length,
        "byteaGetByte" => f_get_byte,
        "byteaSetByte" => f_set_byte,
        "byteaGetBit" => f_get_bit,
        "byteaSetBit" => f_set_bit,
        "byteaoverlay" => f_overlay,
        "byteaoverlay_no_len" => f_overlay_no_len,
        "bytea_substr" => f_substr,
        "bytea_substr_no_len" => f_substr_no_len,
        "binary_encode" => f_encode,
        "binary_decode" => f_decode,
        "byteaeq" => f_eq,
        "byteane" => f_ne,
        "bytealt" => f_lt,
        "byteale" => f_le,
        "byteagt" => f_gt,
        "byteage" => f_ge,
        "byteacmp" => f_cmp,
        "bytea_larger" => f_larger,
        "bytea_smaller" => f_smaller,
        "byteacat" => f_cat,
        "byteapos" => f_pos,
        "byteatrim" => f_btrim,
        "bytealtrim" => f_ltrim,
        "byteartrim" => f_rtrim,
        "bytea_bit_count" => f_bit_count,
        "bytea_reverse" => f_reverse,
        "hashbytea" => f_hash,
        "hashbyteaextended" => f_hash_extended,
        "int2_bytea" => f_int2_bytea,
        "int4_bytea" => f_int4_bytea,
        "int8_bytea" => f_int8_bytea,
        "bytea_int2" => f_bytea_int2,
        "bytea_int4" => f_bytea_int4,
        "bytea_int8" => f_bytea_int8,
        "" if proc.oid == 1810 => f_bit_length,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Bytea-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-18", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| {
        PgError::internal(format!("bytea function called with {} arguments, argument {} missing", args.len(), i))
    })
}

fn arg_bytes(d: &Datum) -> Result<&[u8]> {
    match d {
        Datum::Bytea(b) => Ok(b.as_slice()),
        other => Err(type_mismatch("Bytea", other)),
    }
}

fn arg_text(d: &Datum) -> Result<&str> {
    match d {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("Text", other)),
    }
}

fn arg_i16(d: &Datum) -> Result<i16> {
    match d {
        Datum::Int2(v) => Ok(*v),
        other => Err(type_mismatch("Int2", other)),
    }
}

fn arg_i32(d: &Datum) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("Int4", other)),
    }
}

fn arg_i64(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("Int8", other)),
    }
}

fn int_out_of_range() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

fn len_to_i32(n: usize) -> Result<i32> {
    i32::try_from(n).map_err(|_| int_out_of_range())
}

fn index_error(n: i64, last: i64) -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, format!("index {n} out of valid range, 0..{last}"))
}

// ---------------------------------------------------------------------------------------------
// Text and encoding helpers (encode.c)
// ---------------------------------------------------------------------------------------------

fn invalid_bytea_input() -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type bytea")
}

/// The character starting at byte `i`, as PG's `pg_mblen_range` would show it in an error.
fn char_at(bytes: &[u8], i: usize) -> String {
    let lead = bytes.get(i).copied().unwrap_or(0);
    let width = match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    let end = i.saturating_add(width).min(bytes.len());
    bytes.get(i..end).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `hex_decode_safe`: whitespace (space, \n, \t, \r) is skipped only between digit pairs.
fn hex_decode(src: &[u8]) -> Result<Vec<u8>> {
    let n = src.len();
    let mut out = Vec::with_capacity(n / 2);
    let mut i = 0usize;
    while i < n {
        if matches!(src[i], b' ' | b'\n' | b'\t' | b'\r') {
            i += 1;
            continue;
        }
        let hi = hex_val(src[i]).ok_or_else(|| {
            PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid hexadecimal digit: \"{}\"", char_at(src, i)),
            )
        })?;
        i += 1;
        if i >= n {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "invalid hexadecimal data: odd number of digits",
            ));
        }
        let lo = hex_val(src[i]).ok_or_else(|| {
            PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid hexadecimal digit: \"{}\"", char_at(src, i)),
            )
        })?;
        i += 1;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// True when `src[i..]` starts with a `\ooo` escape (first octal digit 0-3).
fn is_octal_escape(src: &[u8], i: usize) -> bool {
    i + 3 < src.len()
        && (b'0'..=b'3').contains(&src[i + 1])
        && (b'0'..=b'7').contains(&src[i + 2])
        && (b'0'..=b'7').contains(&src[i + 3])
}

/// Escape-format decoder shared by `byteain` and `decode(.., 'escape')`.
fn escape_decode(src: &[u8]) -> Result<Vec<u8>> {
    let n = src.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        if src[i] != b'\\' {
            out.push(src[i]);
            i += 1;
        } else if is_octal_escape(src, i) {
            let v = ((src[i + 1] - b'0') << 6) | ((src[i + 2] - b'0') << 3) | (src[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else if i + 1 < n && src[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else {
            return Err(invalid_bytea_input());
        }
    }
    Ok(out)
}

/// `esc_encode`: NUL and high-bit bytes as `\ooo`, backslash doubled, everything else raw.
fn escape_encode(src: &[u8]) -> String {
    let mut out = String::with_capacity(src.len());
    for &c in src {
        if c == 0 || c >= 0x80 {
            out.push_str(&format!("\\{:03o}", c));
        } else if c == b'\\' {
            out.push_str("\\\\");
        } else {
            out.push(c as char);
        }
    }
    out
}

fn base64_encode(src: &[u8]) -> String {
    // Mirrors pg_base64_encode: a newline follows every 76 output characters, emitted only after
    // a complete 3-byte group (the padded final group never triggers one).
    let mut out: Vec<u8> = Vec::with_capacity(src.len() / 3 * 4 + 4 + src.len() / 57 + 1);
    let mut line_end = 76usize;
    for chunk in src.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0) as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let buf = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((buf >> 18) & 0x3f) as usize]);
        out.push(BASE64_ALPHABET[((buf >> 12) & 0x3f) as usize]);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[((buf >> 6) & 0x3f) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(buf & 0x3f) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() == 3 && out.len() >= line_end {
            out.push(b'\n');
            line_end = out.len() + 76;
        }
    }
    // Only ASCII was written.
    String::from_utf8(out).unwrap_or_default()
}

fn base64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `pg_base64_decode`: whitespace skipped, '=' padding, error on malformed sequences.
fn base64_decode(src: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut pos: u32 = 0;
    // 0: no padding seen; 1: one '=' (two data chars); 2: two '=' (one data char).
    let mut end: u32 = 0;
    let mut i = 0usize;
    while i < src.len() {
        let c = src[i];
        i += 1;
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        let b: u32;
        if c == b'=' {
            if end == 0 {
                if pos == 2 {
                    end = 1;
                } else if pos == 3 {
                    end = 2;
                } else {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "unexpected \"=\" while decoding base64 sequence",
                    ));
                }
            }
            b = 0;
        } else {
            b = base64_val(c).ok_or_else(|| {
                PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid symbol \"{}\" found while decoding base64 sequence", char_at(src, i - 1)),
                )
            })?;
        }
        buf = (buf << 6).wrapping_add(b);
        pos += 1;
        if pos == 4 {
            out.push(((buf >> 16) & 255) as u8);
            if end == 0 || end > 1 {
                out.push(((buf >> 8) & 255) as u8);
            }
            if end == 0 || end > 2 {
                out.push((buf & 255) as u8);
            }
            buf = 0;
            pos = 0;
        }
    }
    if pos != 0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid base64 end sequence")
            .with_hint("Input data is missing padding, is truncated, or is otherwise corrupted."));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum BinFormat {
    Hex,
    Base64,
    Escape,
}

fn find_format(name: &str) -> Result<BinFormat> {
    if name.eq_ignore_ascii_case("hex") {
        Ok(BinFormat::Hex)
    } else if name.eq_ignore_ascii_case("base64") {
        Ok(BinFormat::Base64)
    } else if name.eq_ignore_ascii_case("escape") {
        Ok(BinFormat::Escape)
    } else {
        Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unrecognized encoding: \"{name}\""),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Substring, overlay, trim (varlena.c, oracle_compat.c)
// ---------------------------------------------------------------------------------------------

/// `bytea_substring`: `len` of `None` means "to the end of the string".
fn substring_bytes(data: &[u8], s: i32, len: Option<i32>) -> Result<&[u8]> {
    let n = data.len() as i64;
    let s1: i64 = (s as i64).max(1);
    let (start, count): (i64, Option<i64>) = match len {
        None => (s1 - 1, None),
        Some(l) if l < 0 => {
            return Err(PgError::new(SUBSTRING_ERROR, "negative substring length not allowed"));
        }
        Some(l) => match s.checked_add(l) {
            // S + L overflowed: the substring runs to the end of the string.
            None => (s1 - 1, None),
            Some(e) => {
                if e < 1 {
                    return Ok(&data[..0]);
                }
                (s1 - 1, Some(e as i64 - s1))
            }
        },
    };
    let start = start.min(n).max(0) as usize;
    let end = match count {
        None => n as usize,
        Some(c) => ((start as i64).saturating_add(c)).min(n).max(start as i64) as usize,
    };
    Ok(&data[start..end])
}

fn overlay_bytes(t1: &[u8], t2: &[u8], sp: i32, sl: i32) -> Result<Vec<u8>> {
    if sp <= 0 {
        return Err(PgError::new(SUBSTRING_ERROR, "negative substring length not allowed"));
    }
    let sp_pl_sl = sp.checked_add(sl).ok_or_else(int_out_of_range)?;
    let s1 = substring_bytes(t1, 1, Some(sp - 1))?;
    let s2 = substring_bytes(t1, sp_pl_sl, None)?;
    let mut out = Vec::with_capacity(s1.len() + t2.len() + s2.len());
    out.extend_from_slice(s1);
    out.extend_from_slice(t2);
    out.extend_from_slice(s2);
    Ok(out)
}

/// `dobyteatrim`: strip bytes belonging to `set` from the left and/or right end.
fn trim_bytes<'a>(s: &'a [u8], set: &[u8], left: bool, right: bool) -> &'a [u8] {
    if s.is_empty() || set.is_empty() {
        return s;
    }
    let mut start = 0usize;
    let mut end = s.len();
    if left {
        while start < end && set.contains(&s[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && set.contains(&s[end - 1]) {
            end -= 1;
        }
    }
    &s[start..end]
}

// ---------------------------------------------------------------------------------------------
// Hashing (hashfn.c: hash_bytes, hash_bytes_extended; Jenkins lookup3 mix/final)
// ---------------------------------------------------------------------------------------------

#[inline]
fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

#[inline]
fn final_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

/// Little-endian word from up to four bytes (missing bytes are zero).
#[inline]
fn le_word(bytes: &[u8]) -> u32 {
    let mut w = 0u32;
    for (i, &b) in bytes.iter().take(4).enumerate() {
        w |= (b as u32) << (8 * i);
    }
    w
}

/// Shared core of `hash_bytes` and `hash_bytes_extended` (seed 0 means unseeded).
fn hash_core(k: &[u8], seed: u64) -> (u32, u32, u32) {
    let len = k.len() as u32;
    let init = 0x9e37_79b9u32.wrapping_add(len).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix(&mut a, &mut b, &mut c);
    }
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(le_word(&rest[0..4]));
        b = b.wrapping_add(le_word(&rest[4..8]));
        c = c.wrapping_add(le_word(&rest[8..12]));
        mix(&mut a, &mut b, &mut c);
        rest = &rest[12..];
    }
    // Tail: the low byte of `c` is reserved for the length, so bytes 8..11 land shifted left by 8.
    if !rest.is_empty() {
        a = a.wrapping_add(le_word(rest));
    }
    if rest.len() > 4 {
        b = b.wrapping_add(le_word(&rest[4..]));
    }
    if rest.len() > 8 {
        c = c.wrapping_add(le_word(&rest[8..]) << 8);
    }
    final_mix(&mut a, &mut b, &mut c);
    (a, b, c)
}

/// `hash_bytes`, as used by `hashbytea`.
fn hash_bytes(k: &[u8]) -> u32 {
    hash_core(k, 0).2
}

/// `hash_bytes_extended`, as used by `hashbyteaextended`.
fn hash_bytes_extended(k: &[u8], seed: u64) -> u64 {
    let (_, b, c) = hash_core(k, seed);
    ((b as u64) << 32) | c as u64
}

// ---------------------------------------------------------------------------------------------
// Scalar implementations
// ---------------------------------------------------------------------------------------------

fn f_octet_length(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let n = arg_bytes(arg(args, 0)?)?.len();
    Ok(Datum::Int4(len_to_i32(n)?))
}

/// SQL-language `bit_length(bytea)` = `octet_length($1) * 8` (int4 arithmetic).
fn f_bit_length(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let n = arg_bytes(arg(args, 0)?)?.len();
    let bytes = len_to_i32(n)?;
    let bits = bytes.checked_mul(8).ok_or_else(int_out_of_range)?;
    Ok(Datum::Int4(bits))
}

fn f_get_byte(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let n = arg_i32(arg(args, 1)?)?;
    let len = data.len() as i64;
    if n < 0 || n as i64 >= len {
        return Err(index_error(n as i64, len - 1));
    }
    Ok(Datum::Int4(data[n as usize] as i32))
}

fn f_set_byte(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let n = arg_i32(arg(args, 1)?)?;
    let new_byte = arg_i32(arg(args, 2)?)?;
    let len = data.len() as i64;
    if n < 0 || n as i64 >= len {
        return Err(index_error(n as i64, len - 1));
    }
    let mut out = data.to_vec();
    // PG stores the int into an unsigned char, so the value is truncated mod 256.
    out[n as usize] = new_byte as u8;
    Ok(Datum::Bytea(out))
}

fn f_get_bit(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let n = arg_i64(arg(args, 1)?)?;
    let total = (data.len() as i64).saturating_mul(8);
    if n < 0 || n >= total {
        return Err(index_error(n, total - 1));
    }
    let byte = data[(n / 8) as usize];
    let bit = (byte >> (n % 8)) & 1;
    Ok(Datum::Int4(bit as i32))
}

fn f_set_bit(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let n = arg_i64(arg(args, 1)?)?;
    let new_bit = arg_i32(arg(args, 2)?)?;
    let total = (data.len() as i64).saturating_mul(8);
    if n < 0 || n >= total {
        return Err(index_error(n, total - 1));
    }
    if new_bit != 0 && new_bit != 1 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "new bit must be 0 or 1"));
    }
    let mut out = data.to_vec();
    let byte_no = (n / 8) as usize;
    let mask = 1u8 << (n % 8);
    if new_bit == 0 {
        out[byte_no] &= !mask;
    } else {
        out[byte_no] |= mask;
    }
    Ok(Datum::Bytea(out))
}

fn f_overlay(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t1 = arg_bytes(arg(args, 0)?)?;
    let t2 = arg_bytes(arg(args, 1)?)?;
    let sp = arg_i32(arg(args, 2)?)?;
    let sl = arg_i32(arg(args, 3)?)?;
    Ok(Datum::Bytea(overlay_bytes(t1, t2, sp, sl)?))
}

fn f_overlay_no_len(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let t1 = arg_bytes(arg(args, 0)?)?;
    let t2 = arg_bytes(arg(args, 1)?)?;
    let sp = arg_i32(arg(args, 2)?)?;
    let sl = len_to_i32(t2.len())?;
    Ok(Datum::Bytea(overlay_bytes(t1, t2, sp, sl)?))
}

fn f_substr(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let s = arg_i32(arg(args, 1)?)?;
    let l = arg_i32(arg(args, 2)?)?;
    Ok(Datum::Bytea(substring_bytes(data, s, Some(l))?.to_vec()))
}

fn f_substr_no_len(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let s = arg_i32(arg(args, 1)?)?;
    Ok(Datum::Bytea(substring_bytes(data, s, None)?.to_vec()))
}

/// `binary_encode(bytea, text)`.
fn f_encode(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let name = arg_text(arg(args, 1)?)?;
    let out = match find_format(name)? {
        BinFormat::Hex => format_bytea(data, true)?[2..].to_string(),
        BinFormat::Base64 => base64_encode(data),
        BinFormat::Escape => escape_encode(data),
    };
    Ok(Datum::Text(out))
}

/// `binary_decode(text, text)`.
fn f_decode(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_text(arg(args, 0)?)?.as_bytes();
    let name = arg_text(arg(args, 1)?)?;
    let out = match find_format(name)? {
        BinFormat::Hex => hex_decode(data)?,
        BinFormat::Base64 => base64_decode(data)?,
        BinFormat::Escape => escape_decode(data)?,
    };
    Ok(Datum::Bytea(out))
}

fn f_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? == arg_bytes(arg(args, 1)?)?))
}

fn f_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? != arg_bytes(arg(args, 1)?)?))
}

fn f_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? < arg_bytes(arg(args, 1)?)?))
}

fn f_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? <= arg_bytes(arg(args, 1)?)?))
}

fn f_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? > arg_bytes(arg(args, 1)?)?))
}

fn f_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_bytes(arg(args, 0)?)? >= arg_bytes(arg(args, 1)?)?))
}

/// `byteacmp`: memcmp over the common prefix (its byte difference, as glibc returns it), then length.
fn f_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_bytes(arg(args, 0)?)?;
    let b = arg_bytes(arg(args, 1)?)?;
    let common = a.len().min(b.len());
    for i in 0..common {
        if a[i] != b[i] {
            return Ok(Datum::Int4(a[i] as i32 - b[i] as i32));
        }
    }
    let r = match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Datum::Int4(r))
}

/// `bytea_larger`: first argument when it is greater, otherwise the second.
fn f_larger(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_bytes(arg(args, 0)?)?;
    let b = arg_bytes(arg(args, 1)?)?;
    let pick_first = a > b;
    Ok(Datum::Bytea(if pick_first { a.to_vec() } else { b.to_vec() }))
}

/// `bytea_smaller`: first argument when it is less, otherwise the second.
fn f_smaller(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_bytes(arg(args, 0)?)?;
    let b = arg_bytes(arg(args, 1)?)?;
    let pick_first = a < b;
    Ok(Datum::Bytea(if pick_first { a.to_vec() } else { b.to_vec() }))
}

fn f_cat(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_bytes(arg(args, 0)?)?;
    let b = arg_bytes(arg(args, 1)?)?;
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    Ok(Datum::Bytea(out))
}

/// `byteapos(string, substring)`: 1-based position of the first occurrence, 0 when absent,
/// 1 for an empty substring.
fn f_pos(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let hay = arg_bytes(arg(args, 0)?)?;
    let needle = arg_bytes(arg(args, 1)?)?;
    if needle.is_empty() {
        return Ok(Datum::Int4(1));
    }
    let pos = hay
        .windows(needle.len())
        .position(|w| w == needle)
        .map_or(0, |p| p as i32 + 1);
    Ok(Datum::Int4(pos))
}

fn f_btrim(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = arg_bytes(arg(args, 0)?)?;
    let set = arg_bytes(arg(args, 1)?)?;
    Ok(Datum::Bytea(trim_bytes(s, set, true, true).to_vec()))
}

fn f_ltrim(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = arg_bytes(arg(args, 0)?)?;
    let set = arg_bytes(arg(args, 1)?)?;
    Ok(Datum::Bytea(trim_bytes(s, set, true, false).to_vec()))
}

fn f_rtrim(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = arg_bytes(arg(args, 0)?)?;
    let set = arg_bytes(arg(args, 1)?)?;
    Ok(Datum::Bytea(trim_bytes(s, set, false, true).to_vec()))
}

fn f_bit_count(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let n: u64 = data.iter().map(|b| b.count_ones() as u64).sum();
    Ok(Datum::Int8(n as i64))
}

fn f_reverse(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let mut out = data.to_vec();
    out.reverse();
    Ok(Datum::Bytea(out))
}

fn f_hash(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    Ok(Datum::Int4(hash_bytes(data) as i32))
}

fn f_hash_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    let seed = arg_i64(arg(args, 1)?)?;
    Ok(Datum::Int8(hash_bytes_extended(data, seed as u64) as i64))
}

fn f_int2_bytea(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(arg_i16(arg(args, 0)?)?.to_be_bytes().to_vec()))
}

fn f_int4_bytea(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(arg_i32(arg(args, 0)?)?.to_be_bytes().to_vec()))
}

fn f_int8_bytea(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(arg_i64(arg(args, 0)?)?.to_be_bytes().to_vec()))
}

/// Big-endian unsigned accumulation over at most `width` bytes (PG's `bytea_int*` casts).
fn be_accumulate(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64)
}

fn f_bytea_int2(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    if data.len() > 2 {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "smallint out of range"));
    }
    Ok(Datum::Int2(be_accumulate(data) as u16 as i16))
}

fn f_bytea_int4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    if data.len() > 4 {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range"));
    }
    Ok(Datum::Int4(be_accumulate(data) as u32 as i32))
}

fn f_bytea_int8(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let data = arg_bytes(arg(args, 0)?)?;
    if data.len() > 8 {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"));
    }
    Ok(Datum::Int8(be_accumulate(data) as i64))
}

// ---------------------------------------------------------------------------------------------
// Tests (executed centrally)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, IoCtx, TestSession};
    use crate::func;
    use crate::registry;

    fn code(e: &PgError) -> &str {
        e.sqlstate.code()
    }

    fn fctx<'a>(cfg: &'a IoConfig, sess: &'a TestSession) -> FuncCtx<'a> {
        FuncCtx {
            env: registry::builtin_env(),
            io: cfg,
            session: sess,
            arg_types: &[],
            result_type: TypeId::BYTEA,
            collation: 0,
        }
    }

    #[test]
    fn hex_input() {
        assert_eq!(parse_bytea("\\x0102").unwrap(), vec![1u8, 2]);
        assert_eq!(parse_bytea("\\x0A ff\n").unwrap(), vec![0x0au8, 0xff]);
        assert_eq!(parse_bytea("\\x").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_input_errors() {
        let e = parse_bytea("\\xZZ").unwrap_err();
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "invalid hexadecimal digit: \"Z\"");
        let e = parse_bytea("\\x0").unwrap_err();
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "invalid hexadecimal data: odd number of digits");
    }

    #[test]
    fn escape_input() {
        assert_eq!(parse_bytea("caf\u{e9}").unwrap(), b"caf\xc3\xa9".to_vec());
        assert_eq!(parse_bytea("a\\\\b").unwrap(), b"a\\b".to_vec());
        assert_eq!(parse_bytea("\\000\\377").unwrap(), vec![0u8, 255]);
        // Backslash followed by 'b' is not a valid escape.
        let e = parse_bytea("a\\b").unwrap_err();
        assert_eq!(code(&e), "22P02");
        assert_eq!(e.message, "invalid input syntax for type bytea");
    }

    #[test]
    fn output_formats() {
        assert_eq!(format_bytea(&[1, 2], true).unwrap(), "\\x0102");
        assert_eq!(format_bytea(&[], true).unwrap(), "\\x");
        assert_eq!(format_bytea(&[0, 92, 65], false).unwrap(), "\\000\\\\A");
    }

    #[test]
    fn encode_decode_formats() {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = fctx(&cfg, &sess);
        let r = f_encode(&[Datum::Bytea(vec![0x00, 0xff]), Datum::text("base64")], &ctx).unwrap();
        assert_eq!(r, Datum::text("AP8="));
        let r = f_encode(&[Datum::Bytea(vec![0, 92, 65]), Datum::text("escape")], &ctx).unwrap();
        assert_eq!(r, Datum::text("\\000\\\\A"));
        let r = f_decode(&[Datum::text("AP8="), Datum::text("BASE64")], &ctx).unwrap();
        assert_eq!(r, Datum::Bytea(vec![0x00, 0xff]));
        let e = f_decode(&[Datum::text("x"), Datum::text("rot13")], &ctx).unwrap_err();
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "unrecognized encoding: \"rot13\"");
    }

    #[test]
    fn base64_wraps_at_76() {
        // 57 bytes give exactly 76 characters, followed by the newline PG emits.
        let s = base64_encode(&[0u8; 57]);
        assert_eq!(s.len(), 77);
        assert!(s.ends_with('\n'));
        // 58 bytes: one wrap, then the 4 trailing characters.
        let s = base64_encode(&[0u8; 58]);
        assert_eq!(s.len(), 81);
        assert_eq!(s.matches('\n').count(), 1);
    }

    #[test]
    fn get_byte_out_of_range() {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = fctx(&cfg, &sess);
        let h = func::implementation(721).unwrap();
        assert_eq!(h.call(&[Datum::Bytea(vec![1, 2]), Datum::Int4(1)], &ctx).unwrap(), Datum::Int4(2));
        let e = h.call(&[Datum::Bytea(vec![1, 2]), Datum::Int4(5)], &ctx).unwrap_err();
        assert_eq!(code(&e), "2202E");
        assert_eq!(e.message, "index 5 out of valid range, 0..1");
    }

    #[test]
    fn substring_and_overlay() {
        assert_eq!(substring_bytes(b"abcdef", 2, Some(3)).unwrap(), b"bcd");
        assert_eq!(substring_bytes(b"abcdef", 0, Some(2)).unwrap(), b"a");
        assert_eq!(substring_bytes(b"abcdef", 4, None).unwrap(), b"def");
        assert_eq!(substring_bytes(b"abcdef", 9, Some(2)).unwrap(), b"");
        assert_eq!(code(&substring_bytes(b"abc", 1, Some(-1)).unwrap_err()), "22011");
        assert_eq!(overlay_bytes(b"abcdef", b"XY", 2, 3).unwrap(), b"aXYef");
        assert_eq!(code(&overlay_bytes(b"abc", b"x", 0, 1).unwrap_err()), "22011");
    }

    #[test]
    fn trims_and_position() {
        assert_eq!(trim_bytes(b"xxabxx", b"x", true, true), b"ab");
        assert_eq!(trim_bytes(b"xxabxx", b"x", true, false), b"abxx");
        assert_eq!(trim_bytes(b"xxabxx", b"x", false, true), b"xxab");
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = fctx(&cfg, &sess);
        let r = f_pos(&[Datum::Bytea(b"hello".to_vec()), Datum::Bytea(b"ll".to_vec())], &ctx).unwrap();
        assert_eq!(r, Datum::Int4(3));
        let r = f_pos(&[Datum::Bytea(b"hello".to_vec()), Datum::Bytea(b"".to_vec())], &ctx).unwrap();
        assert_eq!(r, Datum::Int4(1));
    }

    #[test]
    fn int_casts() {
        assert_eq!(be_accumulate(&[1, 2]), 258);
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = fctx(&cfg, &sess);
        let r = f_bytea_int2(&[Datum::Bytea(vec![0xff, 0xff])], &ctx).unwrap();
        assert_eq!(r, Datum::Int2(-1));
        let e = f_bytea_int2(&[Datum::Bytea(vec![1, 2, 3])], &ctx).unwrap_err();
        assert_eq!(e.message, "smallint out of range");
        let r = f_int4_bytea(&[Datum::Int4(258)], &ctx).unwrap();
        assert_eq!(r, Datum::Bytea(vec![0, 0, 1, 2]));
    }

    #[test]
    fn bit_ops() {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = fctx(&cfg, &sess);
        let r = f_get_bit(&[Datum::Bytea(vec![0b0000_0100]), Datum::Int8(2)], &ctx).unwrap();
        assert_eq!(r, Datum::Int4(1));
        let r = f_set_bit(&[Datum::Bytea(vec![0]), Datum::Int8(0), Datum::Int4(1)], &ctx).unwrap();
        assert_eq!(r, Datum::Bytea(vec![1]));
        let e = f_set_bit(&[Datum::Bytea(vec![0]), Datum::Int8(0), Datum::Int4(2)], &ctx).unwrap_err();
        assert_eq!(e.message, "new bit must be 0 or 1");
        let r = f_bit_count(&[Datum::Bytea(vec![0xff, 0x01])], &ctx).unwrap();
        assert_eq!(r, Datum::Int8(9));
    }

    #[test]
    fn io_roundtrip_through_module_api() {
        let cfg = IoConfig::default();
        let env = registry::builtin_env();
        let ctx = IoCtx::new(env, &cfg);
        let d = input(&ctx, TypeId::BYTEA, "\\x636166c3a9").unwrap();
        assert_eq!(d, Datum::Bytea(b"caf\xc3\xa9".to_vec()));
        assert_eq!(output(&ctx, TypeId::BYTEA, &d).unwrap(), "\\x636166c3a9");
        assert_eq!(send(&ctx, TypeId::BYTEA, &d).unwrap(), b"caf\xc3\xa9".to_vec());
        assert_eq!(recv(&ctx, TypeId::BYTEA, b"ab").unwrap(), Datum::Bytea(b"ab".to_vec()));
    }

    #[test]
    fn lookup_covers_module_procs() {
        for oid in [720u32, 721, 749, 752, 1810, 1946, 1947, 1948, 2014, 6382, 6413, 6414, 6372] {
            let p = crate::catalog_data::builtin_proc(oid).unwrap();
            assert!(lookup(p).is_some(), "no implementation for proc {oid} ({})", p.name);
        }
    }
}
