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
use crate::array::{Array, MAXDIM};
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::io as tio;
use crate::names;
use crate::registry;
use pgarbor_common::{Oid, PgError, Result, SqlState, FIRST_NORMAL_OID};

/// `MaxArraySize` = MaxAllocSize / sizeof(Datum).
const MAX_ARRAY_SIZE: i64 = 134_217_727;

/// `scanner_isspace`.
#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn malformed(orig: &str, detail: impl Into<String>) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("malformed array literal: \"{orig}\""))
        .with_detail(detail)
}

fn size_error() -> PgError {
    PgError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
    )
}

fn elem_info(ctx: &IoCtx, ty: TypeId) -> Result<Oid> {
    registry::element_type_of(ctx.env, ty.oid)
        .ok_or_else(|| PgError::internal(format!("type {} is not an array type", ty.oid)))
}

fn delim_of(ctx: &IoCtx, elem: Oid) -> Result<u8> {
    let info = registry::type_info(ctx.env, elem)?;
    Ok(if info.delim == 0 { b',' } else { info.delim })
}

// ---------------------------------------------------------------------------------------------
// Text input
// ---------------------------------------------------------------------------------------------

/// `ReadDimensionInt`: no digits (and no sign) leaves `pos` unchanged and yields 0.
fn read_dimension_int(b: &[u8], pos: &mut usize) -> Result<i32> {
    let mut p = *pos;
    match b.get(p) {
        Some(c) if c.is_ascii_digit() || *c == b'-' || *c == b'+' => {}
        _ => return Ok(0),
    }
    let neg = b[p] == b'-';
    if b[p] == b'-' || b[p] == b'+' {
        p += 1;
    }
    let ds = p;
    let mut v: i64 = 0;
    let mut over = false;
    while let Some(c) = b.get(p) {
        if !c.is_ascii_digit() {
            break;
        }
        if !over {
            v = v * 10 + i64::from(c - b'0');
            if v > i64::from(i32::MAX) + 1 {
                over = true;
            }
        }
        p += 1;
    }
    if p == ds {
        // A sign without digits: strtol consumes nothing.
        return Ok(0);
    }
    let v = if neg { -v } else { v };
    if over || v > i64::from(i32::MAX) || v < i64::from(i32::MIN) {
        return Err(PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "array bound is out of integer range"));
    }
    *pos = p;
    Ok(v as i32)
}

/// `ReadArrayDimensions`. Returns ndim; fills `dim` / `lb`.
fn read_array_dimensions(
    b: &[u8],
    pos: &mut usize,
    dim: &mut [i32; MAXDIM],
    lb: &mut [i32; MAXDIM],
    orig: &str,
) -> Result<usize> {
    let mut p = *pos;
    let mut ndim = 0usize;
    loop {
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if b.get(p) != Some(&b'[') {
            break;
        }
        p += 1;
        if ndim >= MAXDIM {
            return Err(PgError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("number of array dimensions exceeds the maximum allowed ({MAXDIM})"),
            ));
        }
        let q = p;
        let i = read_dimension_int(b, &mut p)?;
        if p == q {
            return Err(malformed(orig, "\"[\" must introduce explicitly-specified array dimensions."));
        }
        let ub;
        if b.get(p) == Some(&b':') {
            lb[ndim] = i;
            p += 1;
            let q = p;
            ub = read_dimension_int(b, &mut p)?;
            if p == q {
                return Err(malformed(orig, "Missing array dimension value."));
            }
        } else {
            lb[ndim] = 1;
            ub = i;
        }
        if b.get(p) != Some(&b']') {
            return Err(malformed(orig, "Missing \"]\" after array dimensions."));
        }
        p += 1;
        if ub < lb[ndim] {
            return Err(PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "upper bound cannot be less than lower bound"));
        }
        if ub == i32::MAX {
            return Err(PgError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("array upper bound is too large: {ub}"),
            ));
        }
        let len = i64::from(ub) - i64::from(lb[ndim]) + 1;
        if len > i64::from(i32::MAX) {
            return Err(size_error());
        }
        dim[ndim] = len as i32;
        ndim += 1;
    }
    *pos = p;
    Ok(ndim)
}

#[derive(PartialEq, Eq)]
enum Tok {
    LevelStart,
    LevelEnd,
    Delim,
    Elem,
    ElemNull,
}

/// `ReadArrayToken`.
fn read_array_token(b: &[u8], pos: &mut usize, elem: &mut Vec<u8>, delim: u8, orig: &str) -> Result<Tok> {
    let end_err = || malformed(orig, "Unexpected end of input.");
    let mut p = *pos;
    elem.clear();
    // Identify the token type, skipping leading whitespace.
    let quoted;
    loop {
        match b.get(p) {
            None => return Err(end_err()),
            Some(b'{') => {
                *pos = p + 1;
                return Ok(Tok::LevelStart);
            }
            Some(b'}') => {
                *pos = p + 1;
                return Ok(Tok::LevelEnd);
            }
            Some(b'"') => {
                p += 1;
                quoted = true;
                break;
            }
            Some(&c) => {
                if c == delim {
                    *pos = p + 1;
                    return Ok(Tok::Delim);
                }
                if is_space(c) {
                    p += 1;
                    continue;
                }
                quoted = false;
                break;
            }
        }
    }
    if quoted {
        loop {
            match b.get(p) {
                None => return Err(end_err()),
                Some(b'\\') => {
                    p += 1;
                    match b.get(p) {
                        None => return Err(end_err()),
                        Some(&c) => {
                            elem.push(c);
                            p += 1;
                        }
                    }
                }
                Some(b'"') => {
                    p += 1;
                    while p < b.len() {
                        let c = b[p];
                        if c == delim || c == b'}' || c == b'{' {
                            *pos = p;
                            return Ok(Tok::Elem);
                        }
                        if !is_space(c) {
                            return Err(malformed(orig, "Incorrectly quoted array element."));
                        }
                        p += 1;
                    }
                    return Err(end_err());
                }
                Some(&c) => {
                    elem.push(c);
                    p += 1;
                }
            }
        }
    }
    // Unquoted element; trailing whitespace is trimmed.
    let mut dstlen = 0usize;
    let mut has_escapes = false;
    loop {
        match b.get(p) {
            None => return Err(end_err()),
            Some(b'{') => return Err(malformed(orig, "Unexpected \"{\" character.")),
            Some(b'"') => return Err(malformed(orig, "Incorrectly quoted array element.")),
            Some(b'\\') => {
                p += 1;
                match b.get(p) {
                    None => return Err(end_err()),
                    Some(&c) => {
                        elem.push(c);
                        p += 1;
                        dstlen = elem.len();
                        has_escapes = true;
                    }
                }
            }
            Some(&c) => {
                if c == delim || c == b'}' {
                    elem.truncate(dstlen);
                    *pos = p;
                    if !has_escapes && elem.eq_ignore_ascii_case(b"NULL") {
                        return Ok(Tok::ElemNull);
                    }
                    return Ok(Tok::Elem);
                }
                elem.push(c);
                if !is_space(c) {
                    dstlen = elem.len();
                }
                p += 1;
            }
        }
    }
}

/// `array_in`.
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let elem = elem_info(ctx, ty)?;
    let delim = delim_of(ctx, elem)?;
    let elem_ty = TypeId::new(elem, ty.typmod);
    let b = s.as_bytes();

    let mut dim = [-1i32; MAXDIM];
    let mut lb = [1i32; MAXDIM];
    let mut p = 0usize;
    let mut ndim = read_array_dimensions(b, &mut p, &mut dim, &mut lb, s)?;

    if ndim == 0 {
        if b.get(p) != Some(&b'{') {
            return Err(malformed(s, "Array value must start with \"{\" or dimension information."));
        }
    } else {
        if b.get(p) != Some(&b'=') {
            return Err(malformed(s, "Missing \"=\" after array dimensions."));
        }
        p += 1;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if b.get(p) != Some(&b'{') {
            return Err(malformed(s, "Array contents must start with \"{\"."));
        }
    }

    // ReadArrayStr
    let dimensions_specified = ndim != 0;
    let dim_err = || {
        if dimensions_specified {
            malformed(s, "Specified array dimensions do not match array contents.")
        } else {
            malformed(s, "Multidimensional arrays must have sub-arrays with matching dimensions.")
        }
    };
    let mut items: Vec<Datum> = Vec::new();
    let mut elembuf: Vec<u8> = Vec::new();
    let mut nelems = [0i32; MAXDIM];
    let mut nest_level = 0usize;
    let mut ndim_frozen = dimensions_specified;
    let mut expect_delim = false;
    loop {
        let tok = read_array_token(b, &mut p, &mut elembuf, delim, s)?;
        match tok {
            Tok::LevelStart => {
                if expect_delim {
                    return Err(malformed(s, "Unexpected \"{\" character."));
                }
                if nest_level >= MAXDIM {
                    return Err(PgError::new(
                        SqlState::PROGRAM_LIMIT_EXCEEDED,
                        format!("number of array dimensions exceeds the maximum allowed ({MAXDIM})"),
                    ));
                }
                nelems[nest_level] = 0;
                nest_level += 1;
                if nest_level > ndim {
                    if ndim_frozen {
                        return Err(dim_err());
                    }
                    ndim = nest_level;
                }
            }
            Tok::LevelEnd => {
                if nelems[nest_level - 1] > 0 && !expect_delim {
                    return Err(malformed(s, "Unexpected \"}\" character."));
                }
                nest_level -= 1;
                if nest_level > 0 {
                    nelems[nest_level - 1] += 1;
                }
                if dim[nest_level] < 0 {
                    dim[nest_level] = nelems[nest_level];
                } else if nelems[nest_level] != dim[nest_level] {
                    return Err(dim_err());
                }
                expect_delim = true;
            }
            Tok::Delim => {
                if !expect_delim {
                    return Err(malformed(s, format!("Unexpected \"{}\" character.", delim as char)));
                }
                expect_delim = false;
            }
            Tok::Elem | Tok::ElemNull => {
                if expect_delim {
                    return Err(malformed(s, "Unexpected array element."));
                }
                if items.len() as i64 >= MAX_ARRAY_SIZE {
                    return Err(size_error());
                }
                if tok == Tok::ElemNull {
                    items.push(Datum::Null);
                } else {
                    let text = std::str::from_utf8(&elembuf)
                        .map_err(|_| malformed(s, "Invalid byte sequence in array element."))?;
                    items.push(tio::input(ctx, elem_ty, text)?);
                }
                ndim_frozen = true;
                if nest_level != ndim {
                    return Err(dim_err());
                }
                nelems[nest_level - 1] += 1;
                expect_delim = true;
            }
        }
        if nest_level == 0 {
            break;
        }
    }

    // Only whitespace may follow the closing brace.
    while p < b.len() {
        let c = b[p];
        p += 1;
        if !is_space(c) {
            return Err(malformed(s, "Junk after closing right brace."));
        }
    }

    if items.is_empty() {
        return Ok(Datum::Array(Box::new(Array::empty(elem))));
    }
    let dims: Vec<(i32, i32)> = (0..ndim).map(|i| (dim[i], lb[i])).collect();
    Ok(Datum::Array(Box::new(Array { elem, dims, items })))
}

// ---------------------------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------------------------

fn as_array<'a>(d: &'a Datum, what: &str) -> Result<&'a Array> {
    match d {
        Datum::Array(a) => Ok(&**a),
        _ => Err(PgError::internal(format!("{what} called with a non-array datum"))),
    }
}

fn check_shape(a: &Array) -> Result<()> {
    if a.items.len() != a.expected_len() {
        return Err(PgError::internal("array item count does not match its dimensions"));
    }
    Ok(())
}

/// `array_out`.
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let a = as_array(d, "array_out")?;
    check_shape(a)?;
    if a.items.is_empty() {
        return Ok("{}".to_string());
    }
    let delim_b = delim_of(ctx, a.elem)?;
    let delim = delim_b as char;
    let elem_ty = TypeId::new(a.elem, ty.typmod);
    let ndim = a.dims.len();

    let mut out = String::new();
    if !a.default_bounds() {
        for (len, lb) in &a.dims {
            let ub = i64::from(*lb) + i64::from(*len) - 1;
            out.push_str(&format!("[{lb}:{ub}]"));
        }
        out.push('=');
    }
    out.push('{');
    let mut indx = [0i32; MAXDIM];
    let mut j: usize = 0;
    let mut k = 0usize;
    loop {
        for _ in j..ndim.saturating_sub(1) {
            out.push('{');
        }
        match &a.items[k] {
            Datum::Null => out.push_str("NULL"),
            item => {
                let v = tio::output(ctx, elem_ty, item)?;
                let need = v.is_empty()
                    || v.eq_ignore_ascii_case("NULL")
                    || v.bytes().any(|c| {
                        c == b'"' || c == b'\\' || c == b'{' || c == b'}' || c == delim_b || is_space(c)
                    });
                if need {
                    out.push('"');
                    for ch in v.chars() {
                        if ch == '"' || ch == '\\' {
                            out.push('\\');
                        }
                        out.push(ch);
                    }
                    out.push('"');
                } else {
                    out.push_str(&v);
                }
            }
        }
        k += 1;
        // Advance the odometer, closing finished levels.
        let mut i = ndim;
        let mut more = false;
        while i > 0 {
            i -= 1;
            indx[i] += 1;
            if indx[i] < a.dims[i].0 {
                out.push(delim);
                more = true;
                break;
            }
            indx[i] = 0;
            out.push('}');
        }
        if !more {
            break;
        }
        j = i;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Binary I/O
// ---------------------------------------------------------------------------------------------

fn short_msg() -> PgError {
    PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message")
}

fn get_i32(buf: &[u8], pos: &mut usize) -> Result<i32> {
    let end = pos.checked_add(4).ok_or_else(short_msg)?;
    let bytes = buf.get(*pos..end).ok_or_else(short_msg)?;
    *pos = end;
    Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// `array_recv`.
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let expected = elem_info(ctx, ty)?;
    let mut pos = 0usize;
    let ndim = get_i32(buf, &mut pos)?;
    if ndim < 0 {
        return Err(PgError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            format!("invalid number of dimensions: {ndim}"),
        ));
    }
    if ndim as usize > MAXDIM {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("number of array dimensions ({ndim}) exceeds the maximum allowed ({MAXDIM})"),
        ));
    }
    let flags = get_i32(buf, &mut pos)?;
    if flags != 0 && flags != 1 {
        return Err(PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, "invalid array flags"));
    }
    let wire_elem = get_i32(buf, &mut pos)? as u32;
    if wire_elem != expected && wire_elem < FIRST_NORMAL_OID && expected < FIRST_NORMAL_OID {
        return Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "binary data has array element type {} ({}) instead of expected {} ({})",
                wire_elem,
                names::display_name(ctx.env, wire_elem),
                expected,
                names::display_name(ctx.env, expected)
            ),
        ));
    }
    let elem = expected;
    let ndim = ndim as usize;
    let mut dims: Vec<(i32, i32)> = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        let len = get_i32(buf, &mut pos)?;
        let lb = get_i32(buf, &mut pos)?;
        dims.push((len, lb));
    }
    // ArrayGetNItems
    let mut nitems: i64 = if ndim == 0 { 0 } else { 1 };
    for (len, _) in &dims {
        if *len < 0 {
            return Err(size_error());
        }
        nitems *= i64::from(*len);
        if nitems > i64::from(i32::MAX) {
            return Err(size_error());
        }
    }
    if nitems > MAX_ARRAY_SIZE {
        return Err(size_error());
    }
    // ArrayCheckBounds
    for (len, lb) in &dims {
        if len.checked_add(*lb).is_none() {
            return Err(PgError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("array lower bound is too large: {lb}"),
            ));
        }
    }
    if nitems == 0 {
        if pos != buf.len() {
            return Err(tio::invalid_binary(""));
        }
        return Ok(Datum::Array(Box::new(Array::empty(elem))));
    }
    let elem_ty = TypeId::new(elem, ty.typmod);
    let cap = (nitems as usize).min((buf.len() - pos) / 4 + 1);
    let mut items: Vec<Datum> = Vec::with_capacity(cap);
    for _ in 0..nitems {
        let itemlen = get_i32(buf, &mut pos)?;
        if itemlen == -1 {
            items.push(Datum::Null);
            continue;
        }
        if itemlen < -1 || itemlen as usize > buf.len() - pos {
            return Err(PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, "insufficient data left in message"));
        }
        let end = pos + itemlen as usize;
        items.push(tio::recv(ctx, elem_ty, &buf[pos..end])?);
        pos = end;
    }
    if pos != buf.len() {
        return Err(tio::invalid_binary(""));
    }
    Ok(Datum::Array(Box::new(Array { elem, dims, items })))
}

/// `array_send`.
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let a = as_array(d, "array_send")?;
    check_shape(a)?;
    let elem_ty = TypeId::new(a.elem, ty.typmod);
    let mut out: Vec<u8> = Vec::with_capacity(12 + a.dims.len() * 8 + a.items.len() * 8);
    out.extend_from_slice(&(a.dims.len() as i32).to_be_bytes());
    out.extend_from_slice(&i32::from(a.has_nulls()).to_be_bytes());
    out.extend_from_slice(&a.elem.to_be_bytes());
    for (len, lb) in &a.dims {
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&lb.to_be_bytes());
    }
    for item in &a.items {
        if item.is_null() {
            out.extend_from_slice(&(-1i32).to_be_bytes());
        } else {
            let bytes = tio::send(ctx, elem_ty, item)?;
            let len = i32::try_from(bytes.len()).map_err(|_| size_error())?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;

    const INT4_ARR: TypeId = TypeId::of(1007);
    const TEXT_ARR: TypeId = TypeId::of(1009);

    fn with_ctx<R>(f: impl FnOnce(&IoCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        f(&ctx)
    }

    fn rt(ty: TypeId, s: &str) -> String {
        with_ctx(|ctx| {
            let d = input(ctx, ty, s).unwrap();
            output(ctx, ty, &d).unwrap()
        })
    }

    fn err(ty: TypeId, s: &str) -> PgError {
        with_ctx(|ctx| input(ctx, ty, s).unwrap_err())
    }

    fn detail(ty: TypeId, s: &str) -> String {
        err(ty, s).detail.unwrap_or_default()
    }

    #[test]
    fn simple_and_bounds() {
        assert_eq!(rt(INT4_ARR, "{1,2}"), "{1,2}");
        assert_eq!(rt(INT4_ARR, " { 1 , 2 } "), "{1,2}");
        assert_eq!(rt(INT4_ARR, "[2:3]={5,6}"), "[2:3]={5,6}");
        assert_eq!(rt(INT4_ARR, "[3]={5,6,7}"), "{5,6,7}");
        assert_eq!(rt(INT4_ARR, "{{1,2},{3,4}}"), "{{1,2},{3,4}}");
        assert_eq!(rt(INT4_ARR, "{}"), "{}");
        assert_eq!(rt(INT4_ARR, "{{}}"), "{}");
        assert_eq!(rt(INT4_ARR, "{1,NULL,null}"), "{1,NULL,NULL}");
    }

    #[test]
    fn text_quoting() {
        assert_eq!(rt(TEXT_ARR, r#"{"a b",NULL,"null",""}"#), r#"{"a b",NULL,"null",""}"#);
        assert_eq!(rt(TEXT_ARR, r#"{"a,b","{x}","\\","\""}"#), r#"{"a,b","{x}","\\","\""}"#);
        assert_eq!(rt(TEXT_ARR, r"{a\ b, c  d }"), r#"{"a b","c  d"}"#);
    }

    #[test]
    fn errors() {
        let e = err(INT4_ARR, "{1,{2}}");
        assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.message, "malformed array literal: \"{1,{2}}\"");
        let multi = "Multidimensional arrays must have sub-arrays with matching dimensions.";
        assert_eq!(e.detail.as_deref(), Some(multi));
        assert_eq!(detail(INT4_ARR, "{{1},2}"), multi);
        assert_eq!(detail(INT4_ARR, "{1,2"), "Unexpected end of input.");
        assert_eq!(detail(INT4_ARR, "x"), "Array value must start with \"{\" or dimension information.");
        assert_eq!(detail(INT4_ARR, "{1,,2}"), "Unexpected \",\" character.");
        assert_eq!(detail(INT4_ARR, "{1}x"), "Junk after closing right brace.");
        assert_eq!(detail(INT4_ARR, "[1:2]={1}"), "Specified array dimensions do not match array contents.");
        assert_eq!(detail(INT4_ARR, "[1:2{1,2}"), "Missing \"]\" after array dimensions.");
        assert_eq!(detail(INT4_ARR, "[1:2]{1,2}"), "Missing \"=\" after array dimensions.");
        let e = err(INT4_ARR, "{1,a}");
        assert_eq!(e.message, "invalid input syntax for type integer: \"a\"");
        let e = err(INT4_ARR, "{{{{{{{1}}}}}}}");
        assert_eq!(e.sqlstate, SqlState::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(e.message, "number of array dimensions exceeds the maximum allowed (6)");
    }

    #[test]
    fn binary_roundtrip() {
        with_ctx(|ctx| {
            let d = input(ctx, INT4_ARR, "[2:4]={1,NULL,3}").unwrap();
            let b = send(ctx, INT4_ARR, &d).unwrap();
            assert_eq!(&b[..12], &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 23]);
            let d2 = recv(ctx, INT4_ARR, &b).unwrap();
            assert_eq!(d, d2);
            let d = input(ctx, TEXT_ARR, "{{a,b},{NULL,\"\"}}").unwrap();
            let b = send(ctx, TEXT_ARR, &d).unwrap();
            assert_eq!(recv(ctx, TEXT_ARR, &b).unwrap(), d);
            let d = input(ctx, INT4_ARR, "{}").unwrap();
            let b = send(ctx, INT4_ARR, &d).unwrap();
            assert_eq!(recv(ctx, INT4_ARR, &b).unwrap(), d);
        });
    }

    #[test]
    fn binary_errors() {
        with_ctx(|ctx| {
            // wrong element type (text instead of int4)
            let mut b = vec![0, 0, 0, 1, 0, 0, 0, 0];
            b.extend_from_slice(&25u32.to_be_bytes());
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
            let e = recv(ctx, INT4_ARR, &b).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::DATATYPE_MISMATCH);
            assert!(e.message.starts_with("binary data has array element type 25 ("));
            // too many dimensions
            let e = recv(ctx, INT4_ARR, &[0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 23]).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::PROGRAM_LIMIT_EXCEEDED);
            // negative ndim
            let e = recv(ctx, INT4_ARR, &[255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 23]).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        });
    }
}
