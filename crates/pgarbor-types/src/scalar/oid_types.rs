//! OID-like types: oid, xid, cid, xid8, pg_lsn, reg* aliases, int2vector, oidvector
//! (TYPE-01, TYPE-02, TYPE-15). Lease TYP-08.
//!
//! PG18 reference: `utils/adt/oid.c`, `xid.c`, `regproc.c`, `int.c` (int2vector), `pg_lsn.c`, `numutils.c`
//! (`uint32in_subr`, `uint64in_subr`).
//!
//! Datum mapping: oid/xid/cid/reg* → `Datum::Oid(u32)`; xid8 and pg_lsn → `Datum::Int8` (u64 bits);
//! int2vector → 1-D `Array{elem: INT2, lbound 0}`; oidvector → 1-D `Array{elem: OID, lbound 0}`.
//!
//! Text I/O:
//! - `oid`: unsigned 32-bit decimal; input also accepts a NEGATIVE value >= -2147483648 and wraps it
//!   (`'-1'::oid` = 4294967295) like `strtoul`; trailing junk → 22P02 `invalid input syntax for type oid: "x"`;
//!   out of range → 22003 `value "4294967296" is out of range for type oid`. Output: unsigned decimal.
//! - `xid`, `cid`: like oid (messages say `xid`/`cid`). `xid8`: unsigned 64-bit decimal.
//! - `pg_lsn`: `XXXXXXXX/XXXXXXXX` uppercase hex halves (`0/16B6C50`); invalid → 22P02
//!   `invalid input syntax for type pg_lsn: "x"`.
//! - `regclass`, `regtype`, `regproc`, `regprocedure`, `regoper`, `regoperator`, `regnamespace`,
//!   `regrole`, `regcollation`, `regconfig`, `regdictionary`:
//!   input: all-digits string → that OID (no lookup); `-` → 0; otherwise name lookup via
//!   `ctx.env.reg_lookup(kind, text)` (for `regtype`, first try `names::parse_type_string`);
//!   not found → regclass 42P01 `relation "x" does not exist`, regtype 42704 `type "x" does not exist`,
//!   regproc 42883 `function "x" does not exist`, regnamespace 3F000 `schema "x" does not exist`,
//!   regrole 42704 `role "x" does not exist`, regoper 42883 `operator does not exist: x`.
//!   output: 0 → `-`; else `ctx.env.reg_name(kind, oid)`; for regtype fall back to
//!   `names::format_type(env, oid, -1)`; if the object does not exist print the bare number.
//! - `int2vector` / `oidvector`: space-separated decimal list (`"1 2 3"`), output same; empty → `""`.
//!
//! Binary: oid/xid/cid/reg* = u32 big-endian (4 bytes); xid8/pg_lsn = u64 big-endian;
//! int2vector/oidvector = the PG array binary format (delegate to `array::io` with lbound 0).
//!
//! Functions owned (ImplModule::OidTypes): `oideq oidne oidlt oidle oidgt oidge btoidcmp oidlarger oidsmaller
//! hashoid(+extended) oidtoi8 i8tooid` (i8tooid: out of [0, 2^32) → 22003 `OID out of range`),
//! `oidvector*` comparisons (element-wise then length), `xideq xidneq cideq xid8eq/ne/lt/gt/le/ge xid8cmp
//! xid8_larger xid8_smaller xid8toxid hashxid hashxid8 hashcid (+extended)`.
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, IoCtx, RegKind, TypeEnv};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::{FuncImpl, ScalarFn};
use crate::io::{invalid_binary, invalid_input_syntax};
use crate::names;
use crate::oids;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// The reg* kind for a reg type OID (regclass → `RegKind::Class`, ...), or None for plain oid/xid/cid.
pub fn reg_kind_of(type_oid: Oid) -> Option<RegKind> {
    match type_oid {
        oids::REGCLASS => Some(RegKind::Class),
        oids::REGTYPE => Some(RegKind::Type),
        oids::REGPROC => Some(RegKind::Proc),
        oids::REGPROCEDURE => Some(RegKind::Procedure),
        oids::REGOPER => Some(RegKind::Oper),
        oids::REGOPERATOR => Some(RegKind::Operator),
        oids::REGNAMESPACE => Some(RegKind::Namespace),
        oids::REGROLE => Some(RegKind::Role),
        oids::REGCOLLATION => Some(RegKind::Collation),
        oids::REGCONFIG => Some(RegKind::Config),
        oids::REGDICTIONARY => Some(RegKind::Dictionary),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C library emulation: strtoul/strtol (glibc, LP64) and isspace in the C locale
// ---------------------------------------------------------------------------

/// C `isspace` in the "C" locale (includes `\v` and `\f`).
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Result of a strtoul/strtol-style scan. `end == 0` means no conversion (C `endptr == s`).
struct Scan {
    value: u64,
    end: usize,
    erange: bool,
}

/// `strtoul(s, &end, 0)` on an LP64 platform: base 0 (`0x` hex, leading `0` octal, else decimal),
/// optional sign (a minus wraps the magnitude), ERANGE when the magnitude exceeds 64 bits.
fn scan_ulong_base0(b: &[u8]) -> Scan {
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut base: u32 = 10;
    if i < b.len() && b[i] == b'0' {
        let hex_prefix = matches!(b.get(i + 1), Some(b'x') | Some(b'X'))
            && b.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit());
        if hex_prefix {
            base = 16;
            i += 2;
        } else {
            base = 8;
        }
    }
    let digits_start = i;
    let mut value: u64 = 0;
    let mut erange = false;
    while i < b.len() {
        let Some(d) = (b[i] as char).to_digit(base) else { break };
        match value.checked_mul(u64::from(base)).and_then(|v| v.checked_add(u64::from(d))) {
            Some(v) => value = v,
            None => erange = true,
        }
        i += 1;
    }
    if i == digits_start {
        return Scan { value: 0, end: 0, erange: false };
    }
    if erange {
        return Scan { value: u64::MAX, end: i, erange: true };
    }
    let value = if neg { value.wrapping_neg() } else { value };
    Scan { value, end: i, erange: false }
}

/// `strtol(s, &end, 10)` on an LP64 platform. Returns (value, end, erange); `end == 0` = no conversion.
fn scan_long_base10(b: &[u8]) -> (i64, usize, bool) {
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let digits_start = i;
    let mut mag: u64 = 0;
    let mut over = false;
    while i < b.len() && b[i].is_ascii_digit() {
        let d = u64::from(b[i] - b'0');
        match mag.checked_mul(10).and_then(|v| v.checked_add(d)) {
            Some(v) => mag = v,
            None => over = true,
        }
        i += 1;
    }
    if i == digits_start {
        return (0, 0, false);
    }
    if neg {
        if over || mag > (1u64 << 63) {
            (i64::MIN, i, true)
        } else {
            ((mag as i64).wrapping_neg(), i, false)
        }
    } else if over || mag > i64::MAX as u64 {
        (i64::MAX, i, true)
    } else {
        (mag as i64, i, false)
    }
}

fn out_of_range(s: &str, type_name: &str) -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{s}\" is out of range for type {type_name}"))
}

/// Shared head of PG `uint32in_subr` / `uint64in_subr`: no-conversion → 22P02, overflow → 22003.
/// Returns (magnitude as converted by strtoul, end offset).
fn uint_core(s: &str, type_name: &str) -> Result<(u64, usize)> {
    let sc = scan_ulong_base0(s.as_bytes());
    if sc.end == 0 {
        return Err(invalid_input_syntax(type_name, s));
    }
    if sc.erange {
        return Err(out_of_range(s, type_name));
    }
    Ok((sc.value, sc.end))
}

/// The "allow only whitespace after the number" check used when `endloc` is NULL.
fn check_trailing(s: &str, end: usize, type_name: &str) -> Result<()> {
    if s.as_bytes()[end..].iter().all(|&c| is_c_space(c)) {
        Ok(())
    } else {
        Err(invalid_input_syntax(type_name, s))
    }
}

/// Parse an unsigned 32-bit OID-style value (PG `uint32in_subr`); `type_name` for messages.
// CONTRACT
pub fn parse_oid(s: &str, type_name: &str) -> Result<u32> {
    let (cvt, end) = uint_core(s, type_name)?;
    check_trailing(s, end, type_name)?;
    narrow_u32(s, type_name, cvt)
}

/// LP64 range rule of `uint32in_subr`: the value must survive as u32 unsigned or as a sign-extended
/// int, which accepts `-1` … `-2147483648` and rejects everything else.
fn narrow_u32(s: &str, type_name: &str, cvt: u64) -> Result<u32> {
    let result = cvt as u32;
    if cvt != u64::from(result) && cvt != ((result as i32) as i64 as u64) {
        return Err(out_of_range(s, type_name));
    }
    Ok(result)
}

/// `uint32in_subr` with `endloc`: parses one token at the start of `s`, returns (value, end offset).
fn uint32_token(s: &str, type_name: &str) -> Result<(u32, usize)> {
    let (cvt, end) = uint_core(s, type_name)?;
    Ok((narrow_u32(s, type_name, cvt)?, end))
}

/// `uint64in_subr` for a whole string.
fn parse_uint64(s: &str, type_name: &str) -> Result<u64> {
    let (cvt, end) = uint_core(s, type_name)?;
    check_trailing(s, end, type_name)?;
    Ok(cvt)
}

/// `pg_lsn_in_internal`: `HEX/HEX`, 1..=8 hex digits per half, nothing else.
fn parse_lsn(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let len1 = b.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    if len1 == 0 || len1 > 8 || b.get(len1) != Some(&b'/') {
        return None;
    }
    let rest = &b[len1 + 1..];
    let len2 = rest.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    if len2 == 0 || len2 > 8 || len2 != rest.len() {
        return None;
    }
    let hi = u32::from_str_radix(&s[..len1], 16).ok()?;
    let lo = u32::from_str_radix(&s[len1 + 1..], 16).ok()?;
    Some((u64::from(hi) << 32) | u64::from(lo))
}

fn format_lsn(v: u64) -> String {
    format!("{:X}/{:X}", v >> 32, v & 0xFFFF_FFFF)
}

// ---------------------------------------------------------------------------
// Vectors (int2vector / oidvector) text form
// ---------------------------------------------------------------------------

fn vector_elem(oid: Oid) -> Oid {
    if oid == oids::INT2VECTOR {
        oids::INT2
    } else {
        oids::OID
    }
}

fn vector_name(oid: Oid) -> &'static str {
    if oid == oids::INT2VECTOR {
        "int2vector"
    } else {
        "oidvector"
    }
}

/// Element size on the wire and in PG's array header for the vector's element type.
fn vector_elem_size(oid: Oid) -> usize {
    if oid == oids::INT2VECTOR {
        2
    } else {
        4
    }
}

fn vector_datum(elem: Oid, items: Vec<Datum>) -> Datum {
    if items.is_empty() {
        return Datum::array(Array::empty(elem));
    }
    let n = items.len() as i32;
    Datum::array(Array { elem, dims: vec![(n, 0)], items })
}

/// `int2vectorin`: `strtol` base 10 per token; the separator after a token must be exactly one space
/// or the end of the string.
fn parse_int2vector(s: &str) -> Result<Vec<Datum>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && is_c_space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let rest = &s[i..];
        let (value, end, erange) = scan_long_base10(rest.as_bytes());
        if end == 0 {
            return Err(invalid_input_syntax("smallint", rest));
        }
        if erange || value < i64::from(i16::MIN) || value > i64::from(i16::MAX) {
            return Err(out_of_range(rest, "smallint"));
        }
        if end < rest.len() && rest.as_bytes()[end] != b' ' {
            return Err(invalid_input_syntax("smallint", rest));
        }
        out.push(Datum::Int2(value as i16));
        i += end;
    }
    Ok(out)
}

/// `oidvectorin`: `uint32in_subr` with `endloc` per token; no separator check after a token.
fn parse_oidvector(s: &str) -> Result<Vec<Datum>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && is_c_space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let rest = &s[i..];
        let (value, end) = uint32_token(rest, "oid")?;
        out.push(Datum::Oid(value));
        i += end;
    }
    Ok(out)
}

fn array_of(d: &Datum) -> Result<&Array> {
    match d {
        Datum::Array(a) => Ok(&**a),
        other => Err(type_mismatch("Array", other)),
    }
}

fn vector_output(d: &Datum) -> Result<String> {
    let arr = array_of(d)?;
    let parts: Result<Vec<String>> = arr
        .items
        .iter()
        .map(|item| match item {
            Datum::Int2(v) => Ok(v.to_string()),
            Datum::Oid(v) => Ok(v.to_string()),
            other => Err(type_mismatch("Int2 or Oid", other)),
        })
        .collect();
    Ok(parts?.join(" "))
}

// ---------------------------------------------------------------------------
// reg* names
// ---------------------------------------------------------------------------

fn is_numeric_oid_text(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// Identifier components of a possibly qualified, possibly quoted name (the lexing of
/// `stringToQualifiedNameList`, simplified): unquoted parts fold to ASCII lower case, quoted parts keep
/// their case and `""` unescapes to `"`.
fn name_components(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut part_quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
            part_quoted = true;
        } else if c == '.' {
            out.push(finish_component(std::mem::take(&mut cur), part_quoted));
            part_quoted = false;
        } else {
            cur.push(c.to_ascii_lowercase());
        }
    }
    out.push(finish_component(cur, part_quoted));
    out
}

fn finish_component(text: String, quoted: bool) -> String {
    if quoted {
        text
    } else {
        text.trim().to_string()
    }
}

/// Not-found error. Like PG, names quote the parsed identifier (`NameListToString`) for regclass,
/// regconfig, regdictionary and regcollation, the first identifier for regrole and regnamespace, and the
/// raw text for regproc, regprocedure, regoper and regoperator.
fn reg_not_found(kind: RegKind, s: &str) -> PgError {
    let parsed = name_components(s);
    let joined = parsed.join(".");
    let first = parsed.first().cloned().unwrap_or_default();
    match kind {
        RegKind::Class => PgError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{joined}\" does not exist")),
        RegKind::Type => PgError::new(SqlState::UNDEFINED_OBJECT, format!("type \"{joined}\" does not exist")),
        RegKind::Proc | RegKind::Procedure => {
            PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function \"{s}\" does not exist"))
        }
        RegKind::Oper | RegKind::Operator => {
            PgError::new(SqlState::UNDEFINED_FUNCTION, format!("operator does not exist: {s}"))
        }
        RegKind::Namespace => PgError::new(SqlState::INVALID_SCHEMA_NAME, format!("schema \"{first}\" does not exist")),
        RegKind::Role => PgError::new(SqlState::UNDEFINED_OBJECT, format!("role \"{first}\" does not exist")),
        RegKind::Collation => PgError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("collation \"{joined}\" for encoding \"UTF8\" does not exist"),
        ),
        RegKind::Config => {
            PgError::new(SqlState::UNDEFINED_OBJECT, format!("text search configuration \"{joined}\" does not exist"))
        }
        RegKind::Dictionary => {
            PgError::new(SqlState::UNDEFINED_OBJECT, format!("text search dictionary \"{joined}\" does not exist"))
        }
    }
}

/// reg* text input (`regclassin` and friends).
fn reg_input(env: &dyn TypeEnv, kind: RegKind, s: &str) -> Result<Datum> {
    // regoper/regoperator accept only numeric OIDs (parseNumericOid), not "-".
    let dash_allowed = !matches!(kind, RegKind::Oper | RegKind::Operator);
    if dash_allowed && s == "-" {
        return Ok(Datum::Oid(0));
    }
    if is_numeric_oid_text(s) {
        return Ok(Datum::Oid(parse_oid(s, "oid")?));
    }
    if kind == RegKind::Type {
        match names::parse_type_string(env, s) {
            Ok(t) => return Ok(Datum::Oid(t.oid)),
            Err(e) if e.is_unimplemented() => {}
            Err(e) => return Err(e),
        }
    }
    match env.reg_lookup(kind, s)? {
        Some(oid) => Ok(Datum::Oid(oid)),
        None => Err(reg_not_found(kind, s)),
    }
}

/// reg* text output (`regclassout` and friends).
fn reg_output(env: &dyn TypeEnv, kind: RegKind, oid: Oid) -> Result<String> {
    if oid == 0 {
        // regoperout and regoperatorout print "0" for InvalidOid; the others print "-".
        return Ok(match kind {
            RegKind::Oper | RegKind::Operator => "0".to_string(),
            _ => "-".to_string(),
        });
    }
    if let Some(name) = env.reg_name(kind, oid) {
        return Ok(name);
    }
    if kind == RegKind::Type && env.type_info(oid).is_some() {
        return names::format_type(env, oid, -1);
    }
    Ok(oid.to_string())
}

// ---------------------------------------------------------------------------
// Datum accessors
// ---------------------------------------------------------------------------

fn oid_of(d: &Datum) -> Result<u32> {
    match d {
        Datum::Oid(v) => Ok(*v),
        other => Err(type_mismatch("Oid", other)),
    }
}

fn int8_of(d: &Datum) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("Int8", other)),
    }
}

fn missing_arg(i: usize) -> PgError {
    PgError::internal(format!("oid-type function is missing argument {i}"))
}

/// Oid-style function argument: `Datum::Oid`, or an `Int4` whose bits are the value
/// (`xideqint4` shares `xideq`'s C function and receives an int4 second argument).
fn arg_u32(args: &[Datum], i: usize) -> Result<u32> {
    match args.get(i) {
        Some(Datum::Oid(v)) => Ok(*v),
        Some(Datum::Int4(v)) => Ok(*v as u32),
        Some(other) => Err(type_mismatch("Oid", other)),
        None => Err(missing_arg(i)),
    }
}

/// xid8-style function argument (`Int8` bits, read unsigned).
fn arg_u64(args: &[Datum], i: usize) -> Result<u64> {
    match args.get(i) {
        Some(Datum::Int8(v)) => Ok(*v as u64),
        Some(other) => Err(type_mismatch("Int8", other)),
        None => Err(missing_arg(i)),
    }
}

fn arg_vec(args: &[Datum], i: usize) -> Result<&Array> {
    match args.get(i) {
        Some(d) => array_of(d),
        None => Err(missing_arg(i)),
    }
}

// ---------------------------------------------------------------------------
// Hashing: PG common/hashfn.c (Bob Jenkins lookup3 variant), ported to Rust
// ---------------------------------------------------------------------------

/// PG `mix`: reversible mixing of three 32-bit words.
fn hash_mix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(4);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(6);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(8);
    b = b.wrapping_add(a);
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(16);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(19);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(4);
    b = b.wrapping_add(a);
    (a, b, c)
}

/// PG `final`: final avalanche of (a, b, c); the result is in `c` (and `b` for the 64-bit variants).
fn hash_final(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    (a, b, c)
}

fn le32(s: &[u8]) -> u32 {
    u32::from_le_bytes([s[0], s[1], s[2], s[3]])
}

/// `hash_bytes` / `hash_bytes_extended` over arbitrary bytes. Returns the final (a, b, c).
/// The byte-wise little-endian packing equals PG's aligned and unaligned paths on LE hosts.
fn hash_bytes_state(k: &[u8], seed: u64, seeded: bool) -> (u32, u32, u32) {
    let len = k.len() as u32;
    let init = 0x9e37_79b9u32.wrapping_add(len).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seeded && seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        (a, b, c) = hash_mix(a, b, c);
    }
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(le32(&rest[0..4]));
        b = b.wrapping_add(le32(&rest[4..8]));
        c = c.wrapping_add(le32(&rest[8..12]));
        (a, b, c) = hash_mix(a, b, c);
        rest = &rest[12..];
    }
    // Last 0..=11 bytes, zero padded (adding zero bytes changes nothing). The lowest byte of c is
    // reserved for the length, so bytes 8..=10 land in bits 8..=31 of c.
    let mut t = [0u8; 12];
    t[..rest.len()].copy_from_slice(rest);
    a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
    b = b.wrapping_add(u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
    c = c.wrapping_add(u32::from_le_bytes([0, t[8], t[9], t[10]]));
    hash_final(a, b, c)
}

/// PG `hash_bytes`.
fn hash_bytes(k: &[u8]) -> u32 {
    hash_bytes_state(k, 0, false).2
}

/// PG `hash_bytes_extended`.
fn hash_bytes_ext(k: &[u8], seed: u64) -> u64 {
    let (_, b, c) = hash_bytes_state(k, seed, true);
    (u64::from(b) << 32) | u64::from(c)
}

/// PG `hash_bytes_uint32` (same as `hash_bytes` of the 4 little-endian bytes).
fn hash_uint32(k: u32) -> u32 {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let a = init.wrapping_add(k);
    hash_final(a, init, init).2
}

/// PG `hash_bytes_uint32_extended`.
fn hash_uint32_ext(k: u32, seed: u64) -> u64 {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        (a, b, c) = hash_mix(a, b, c);
    }
    a = a.wrapping_add(k);
    let (_, b, c) = hash_final(a, b, c);
    (u64::from(b) << 32) | u64::from(c)
}

/// PG `hashint8`: fold the two halves so that equal values hash like int4/int2.
fn hash_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let fold = if v >= 0 { hi } else { !hi };
    hash_uint32(lo ^ fold)
}

/// PG `hashint8extended`.
fn hash_int8_ext(v: i64, seed: u64) -> u64 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    let fold = if v >= 0 { hi } else { !hi };
    hash_uint32_ext(lo ^ fold, seed)
}

/// Bytes of an oidvector's values as they sit in memory (little-endian, 4 bytes each).
fn oidvector_bytes(a: &Array) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(a.items.len() * 4);
    for d in &a.items {
        out.extend_from_slice(&oid_of(d)?.to_le_bytes());
    }
    Ok(out)
}

/// `btoidvectorcmp`: length difference first, then the first differing element.
fn vector_cmp(a: &Array, b: &Array) -> Result<i32> {
    if a.items.len() != b.items.len() {
        return Ok((a.items.len() as i64 - b.items.len() as i64) as i32);
    }
    for (x, y) in a.items.iter().zip(b.items.iter()) {
        let (x, y) = (oid_of(x)?, oid_of(y)?);
        if x != y {
            return Ok(if x > y { 1 } else { -1 });
        }
    }
    Ok(0)
}

fn ord_i32<T: PartialOrd>(a: T, b: T) -> i32 {
    if a > b {
        1
    } else if a == b {
        0
    } else {
        -1
    }
}

// ---------------------------------------------------------------------------
// Public I/O entry points
// ---------------------------------------------------------------------------

/// Text input for every type in this family (see module docs).
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    match ty.oid {
        oids::OID => Ok(Datum::Oid(parse_oid(s, "oid")?)),
        oids::XID => Ok(Datum::Oid(parse_oid(s, "xid")?)),
        oids::CID => Ok(Datum::Oid(parse_oid(s, "cid")?)),
        oids::XID8 => Ok(Datum::Int8(parse_uint64(s, "xid8")? as i64)),
        oids::PG_LSN => match parse_lsn(s) {
            Some(v) => Ok(Datum::Int8(v as i64)),
            None => Err(invalid_input_syntax("pg_lsn", s)),
        },
        oids::INT2VECTOR => Ok(vector_datum(oids::INT2, parse_int2vector(s)?)),
        oids::OIDVECTOR => Ok(vector_datum(oids::OID, parse_oidvector(s)?)),
        other => match reg_kind_of(other) {
            Some(kind) => reg_input(ctx.env, kind, s),
            None => Err(PgError::internal(format!("type {other} is not an oid-like type"))),
        },
    }
}

/// Text output for every type in this family.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    match ty.oid {
        oids::OID | oids::XID | oids::CID => Ok(oid_of(d)?.to_string()),
        oids::XID8 => Ok((int8_of(d)? as u64).to_string()),
        oids::PG_LSN => Ok(format_lsn(int8_of(d)? as u64)),
        oids::INT2VECTOR | oids::OIDVECTOR => vector_output(d),
        other => match reg_kind_of(other) {
            Some(kind) => reg_output(ctx.env, kind, oid_of(d)?),
            None => Err(PgError::internal(format!("type {other} is not an oid-like type"))),
        },
    }
}

fn read_be_exact<const N: usize>(buf: &[u8]) -> Result<[u8; N]> {
    <[u8; N]>::try_from(buf).map_err(|_| invalid_binary(""))
}

fn read_i32_be(buf: &[u8], pos: &mut usize) -> Option<i32> {
    let end = pos.checked_add(4)?;
    let bytes: [u8; 4] = buf.get(*pos..end)?.try_into().ok()?;
    *pos = end;
    Some(i32::from_be_bytes(bytes))
}

/// Binary input for int2vector / oidvector: the 1-D array wire format with lower bound 0.
fn vector_recv(ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let elem = vector_elem(ty.oid);
    let esz = vector_elem_size(ty.oid);
    let bad = || PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, format!("invalid {} data", vector_name(ty.oid)));
    let mut pos = 0usize;
    let ndim = read_i32_be(buf, &mut pos).ok_or_else(bad)?;
    let has_nulls = read_i32_be(buf, &mut pos).ok_or_else(bad)?;
    let elem_oid = read_i32_be(buf, &mut pos).ok_or_else(bad)? as u32;
    if ndim != 1 || has_nulls != 0 || elem_oid != elem {
        return Err(bad());
    }
    let dim = read_i32_be(buf, &mut pos).ok_or_else(bad)?;
    let lbound = read_i32_be(buf, &mut pos).ok_or_else(bad)?;
    if lbound != 0 {
        return Err(bad());
    }
    let n = usize::try_from(dim).map_err(|_| bad())?;
    let mut items = Vec::with_capacity(n.min(buf.len() / esz.max(1)));
    for _ in 0..n {
        let len = read_i32_be(buf, &mut pos).ok_or_else(bad)?;
        if len != esz as i32 {
            return Err(bad());
        }
        let end = pos.checked_add(esz).ok_or_else(bad)?;
        let bytes = buf.get(pos..end).ok_or_else(bad)?;
        pos = end;
        items.push(if esz == 2 {
            Datum::Int2(i16::from_be_bytes([bytes[0], bytes[1]]))
        } else {
            Datum::Oid(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        });
    }
    if pos != buf.len() {
        return Err(invalid_binary(""));
    }
    Ok(vector_datum(elem, items))
}

/// Binary output for int2vector / oidvector. An empty vector sends PG's `ndim=1, dim=0` header.
fn vector_send(ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let arr = array_of(d)?;
    let esz = vector_elem_size(ty.oid);
    let mut out = Vec::with_capacity(20 + arr.items.len() * (4 + esz));
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&vector_elem(ty.oid).to_be_bytes());
    out.extend_from_slice(&(arr.items.len() as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    for item in &arr.items {
        match item {
            Datum::Int2(v) => {
                out.extend_from_slice(&2i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Datum::Oid(v) => {
                out.extend_from_slice(&4i32.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            other => return Err(type_mismatch("Int2 or Oid", other)),
        }
    }
    Ok(out)
}

/// Binary input (PG `typreceive`), applying `ty.typmod`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    match ty.oid {
        oids::XID8 | oids::PG_LSN => Ok(Datum::Int8(u64::from_be_bytes(read_be_exact::<8>(buf)?) as i64)),
        oids::INT2VECTOR | oids::OIDVECTOR => vector_recv(ty, buf),
        _ => Ok(Datum::Oid(u32::from_be_bytes(read_be_exact::<4>(buf)?))),
    }
}

/// Binary output.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match ty.oid {
        oids::XID8 | oids::PG_LSN => Ok((int8_of(d)? as u64).to_be_bytes().to_vec()),
        oids::INT2VECTOR | oids::OIDVECTOR => vector_send(ty, d),
        _ => Ok(oid_of(d)?.to_be_bytes().to_vec()),
    }
}

// ---------------------------------------------------------------------------
// Function implementations (ImplModule::OidTypes)
// ---------------------------------------------------------------------------

macro_rules! cmp_fn {
    ($name:ident, $get:ident, $op:tt) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
            Ok(Datum::Bool($get(args, 0)? $op $get(args, 1)?))
        }
    };
}

cmp_fn!(oid_eq, arg_u32, ==);
cmp_fn!(oid_ne, arg_u32, !=);
cmp_fn!(oid_lt, arg_u32, <);
cmp_fn!(oid_le, arg_u32, <=);
cmp_fn!(oid_gt, arg_u32, >);
cmp_fn!(oid_ge, arg_u32, >=);
cmp_fn!(xid8_eq, arg_u64, ==);
cmp_fn!(xid8_ne, arg_u64, !=);
cmp_fn!(xid8_lt, arg_u64, <);
cmp_fn!(xid8_le, arg_u64, <=);
cmp_fn!(xid8_gt, arg_u64, >);
cmp_fn!(xid8_ge, arg_u64, >=);

macro_rules! vec_cmp_fn {
    ($name:ident, $op:tt) => {
        fn $name(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
            let c = vector_cmp(arg_vec(args, 0)?, arg_vec(args, 1)?)?;
            Ok(Datum::Bool(c $op 0))
        }
    };
}

vec_cmp_fn!(oidvector_ne, !=);
vec_cmp_fn!(oidvector_lt, <);
vec_cmp_fn!(oidvector_le, <=);
vec_cmp_fn!(oidvector_eq, ==);
vec_cmp_fn!(oidvector_ge, >=);
vec_cmp_fn!(oidvector_gt, >);

fn btoidcmp(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int4(ord_i32(arg_u32(args, 0)?, arg_u32(args, 1)?)))
}

fn xid8cmp(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int4(ord_i32(arg_u64(args, 0)?, arg_u64(args, 1)?)))
}

fn btoidvectorcmp(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int4(vector_cmp(arg_vec(args, 0)?, arg_vec(args, 1)?)?))
}

fn oidlarger(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Oid(arg_u32(args, 0)?.max(arg_u32(args, 1)?)))
}

fn oidsmaller(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Oid(arg_u32(args, 0)?.min(arg_u32(args, 1)?)))
}

fn xid8_larger(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int8(arg_u64(args, 0)?.max(arg_u64(args, 1)?) as i64))
}

fn xid8_smaller(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int8(arg_u64(args, 0)?.min(arg_u64(args, 1)?) as i64))
}

/// `i8tooid`: int8 → oid, only for values in [0, 2^32).
fn i8tooid(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    let v = arg_u64(args, 0)? as i64;
    if v < 0 || v > i64::from(u32::MAX) {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "OID out of range"));
    }
    Ok(Datum::Oid(v as u32))
}

fn oidtoi8(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int8(i64::from(arg_u32(args, 0)?)))
}

/// `xid8toxid`: the low 32 bits of the full transaction id.
fn xid8toxid(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Oid(arg_u64(args, 0)? as u32))
}

fn hash_oid(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int4(hash_uint32(arg_u32(args, 0)?) as i32))
}

fn hash_oid_ext(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int8(hash_uint32_ext(arg_u32(args, 0)?, arg_u64(args, 1)?) as i64))
}

fn hash_oidvector(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    let bytes = oidvector_bytes(arg_vec(args, 0)?)?;
    Ok(Datum::Int4(hash_bytes(&bytes) as i32))
}

fn hash_oidvector_ext(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    let bytes = oidvector_bytes(arg_vec(args, 0)?)?;
    Ok(Datum::Int8(hash_bytes_ext(&bytes, arg_u64(args, 1)?) as i64))
}

fn hash_xid8(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int4(hash_int8(arg_u64(args, 0)? as i64) as i32))
}

fn hash_xid8_ext(args: &[Datum], _ctx: &FuncCtx<'_>) -> Result<Datum> {
    Ok(Datum::Int8(hash_int8_ext(arg_u64(args, 0)? as i64, arg_u64(args, 1)?) as i64))
}

/// Function registry hook (see `crate::func`). Every OidTypes proc is matched by its C symbol.
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "xideq" | "cideq" | "oideq" => oid_eq,
        "xidneq" | "oidne" => oid_ne,
        "oidlt" => oid_lt,
        "oidle" => oid_le,
        "oidgt" => oid_gt,
        "oidge" => oid_ge,
        "btoidcmp" => btoidcmp,
        "btoidvectorcmp" => btoidvectorcmp,
        "oidvectorne" => oidvector_ne,
        "oidvectorlt" => oidvector_lt,
        "oidvectorle" => oidvector_le,
        "oidvectoreq" => oidvector_eq,
        "oidvectorge" => oidvector_ge,
        "oidvectorgt" => oidvector_gt,
        "hashoid" | "hashxid" | "hashcid" => hash_oid,
        "hashoidextended" | "hashxidextended" | "hashcidextended" => hash_oid_ext,
        "hashoidvector" => hash_oidvector,
        "hashoidvectorextended" => hash_oidvector_ext,
        "i8tooid" => i8tooid,
        "oidtoi8" => oidtoi8,
        "oidlarger" => oidlarger,
        "oidsmaller" => oidsmaller,
        "xid8toxid" => xid8toxid,
        "xid8eq" => xid8_eq,
        "xid8ne" => xid8_ne,
        "xid8lt" => xid8_lt,
        "xid8le" => xid8_le,
        "xid8gt" => xid8_gt,
        "xid8ge" => xid8_ge,
        "xid8cmp" => xid8cmp,
        "xid8_larger" => xid8_larger,
        "xid8_smaller" => xid8_smaller,
        "hashxid8" => hash_xid8,
        "hashxid8extended" => hash_xid8_ext,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for an OidTypes-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-08", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_data::{ImplModule, PROCS};
    use crate::context::IoConfig;
    use crate::registry;

    fn code_and_message<T: std::fmt::Debug>(r: Result<T>) -> (String, String) {
        let e = r.expect_err("expected an error");
        (e.sqlstate.code().to_string(), e.message)
    }

    #[test]
    fn oid_parse_base0_and_sign_wrap() {
        assert_eq!(parse_oid("-1", "oid").expect("wrap"), 4_294_967_295);
        assert_eq!(parse_oid("-2147483648", "oid").expect("min"), 2_147_483_648);
        assert_eq!(parse_oid("010", "oid").expect("octal"), 8);
        assert_eq!(parse_oid("0x1F", "oid").expect("hex"), 31);
        assert_eq!(parse_oid("  42  ", "oid").expect("whitespace"), 42);
        assert!(parse_oid("08", "oid").is_err());
        assert!(parse_oid("-2147483649", "oid").is_err());
    }

    #[test]
    fn oid_parse_errors_match_pg() {
        let (code, msg) = code_and_message(parse_oid("4294967296", "oid"));
        assert_eq!(code, "22003");
        assert_eq!(msg, "value \"4294967296\" is out of range for type oid");
        let (code, msg) = code_and_message(parse_oid("x", "oid"));
        assert_eq!(code, "22P02");
        assert_eq!(msg, "invalid input syntax for type oid: \"x\"");
    }

    #[test]
    fn xid8_is_unsigned_64_bit() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let ty = TypeId::of(oids::XID8);
        let d = input(&ctx, ty, "-1").expect("xid8");
        assert_eq!(d, Datum::Int8(-1));
        assert_eq!(output(&ctx, ty, &d).expect("out"), "18446744073709551615");
    }

    #[test]
    fn int2vector_and_oidvector_text() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let v = input(&ctx, TypeId::of(oids::INT2VECTOR), "1 2").expect("int2vector");
        assert_eq!(output(&ctx, TypeId::of(oids::INT2VECTOR), &v).expect("out"), "1 2");
        let (code, _) = code_and_message(input(&ctx, TypeId::of(oids::INT2VECTOR), "1\t2"));
        assert_eq!(code, "22P02");
        let (code, msg) = code_and_message(input(&ctx, TypeId::of(oids::INT2VECTOR), "99999"));
        assert_eq!(code, "22003");
        assert_eq!(msg, "value \"99999\" is out of range for type smallint");
        let o = input(&ctx, TypeId::of(oids::OIDVECTOR), "").expect("empty oidvector");
        assert_eq!(output(&ctx, TypeId::of(oids::OIDVECTOR), &o).expect("out"), "");
    }

    #[test]
    fn pg_lsn_roundtrip() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let ty = TypeId::of(oids::PG_LSN);
        let d = input(&ctx, ty, "0/16B6C50").expect("lsn");
        assert_eq!(output(&ctx, ty, &d).expect("out"), "0/16B6C50");
        let (code, msg) = code_and_message(input(&ctx, ty, "x"));
        assert_eq!(code, "22P02");
        assert_eq!(msg, "invalid input syntax for type pg_lsn: \"x\"");
    }

    #[test]
    fn reg_input_and_output_without_catalog() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let ty = TypeId::of(oids::REGCLASS);
        assert_eq!(output(&ctx, ty, &Datum::Oid(0)).expect("dash"), "-");
        let (code, msg) = code_and_message(input(&ctx, ty, "nosuch"));
        assert_eq!(code, "42P01");
        assert_eq!(msg, "relation \"nosuch\" does not exist");
        let (code, _) = code_and_message(input(&ctx, TypeId::of(oids::REGNAMESPACE), "nosuch"));
        assert_eq!(code, "3F000");
    }

    #[test]
    #[ignore = "needs TYP-01 (format_type / parse_type_string)"]
    fn regtype_output_uses_format_type() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        assert_eq!(output(&ctx, TypeId::of(oids::REGTYPE), &Datum::Oid(23)).expect("regtype"), "integer");
    }

    #[test]
    fn extended_hashes_agree_with_plain_hashes_under_zero_seed() {
        for k in [0u32, 1, 12_345, u32::MAX] {
            assert_eq!(hash_uint32_ext(k, 0) as u32, hash_uint32(k));
            assert_eq!(hash_bytes(&k.to_le_bytes()), hash_uint32(k));
        }
        let bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        assert_eq!(hash_bytes_ext(&bytes, 0) as u32, hash_bytes(&bytes));
    }

    #[test]
    fn oidvector_compare_is_length_first() {
        let a = Array { elem: oids::OID, dims: vec![(2, 0)], items: vec![Datum::Oid(1), Datum::Oid(2)] };
        let b = Array { elem: oids::OID, dims: vec![(1, 0)], items: vec![Datum::Oid(9)] };
        assert_eq!(vector_cmp(&a, &b).expect("len"), 1);
        assert_eq!(vector_cmp(&b, &a).expect("len"), -1);
        let c = Array { elem: oids::OID, dims: vec![(2, 0)], items: vec![Datum::Oid(1), Datum::Oid(3)] };
        assert_eq!(vector_cmp(&a, &c).expect("elem"), -1);
    }

    #[test]
    fn every_oidtypes_proc_has_an_implementation() {
        for p in PROCS.iter().filter(|p| p.module == ImplModule::OidTypes) {
            assert!(lookup(p).is_some(), "no implementation for {} ({})", p.name, p.prosrc);
        }
    }
}
