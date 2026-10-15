//! String functions and character-type operators (TYPE-13). Lease TYP-14.
//!
//! PG18 reference: `utils/adt/varlena.c`, `varchar.c`, `name.c`, `char.c`, `oracle_compat.c`,
//! `formatting.c` (lower/upper/initcap), `ascii.c`.
//! Owned procs (ImplModule::Text; `grep 'module: ImplModule::Text' src/catalog_data/procs.rs`), by prosrc:
//! - comparison (codepoint/bytewise order for every collation in v1): `texteq textne text_lt text_le
//!   text_gt text_ge bttextcmp text_larger text_smaller text_pattern_* bttext_pattern_cmp`,
//!   name: `nameeq namene namelt namele namegt namege btnamecmp` and cross-type `nameeqtext namelttext ...
//!   texteqname textltname ... btnametextcmp bttextnamecmp`; bpchar: `bpchareq bpcharne bpcharlt ...
//!   bpcharcmp bpchar_larger bpchar_smaller bpchar_pattern_* btbpchar_pattern_cmp` (all compare with
//!   trailing spaces REMOVED: `'ab  '::char(4) = 'ab'` is true); "char": `chareq charne charlt ... btcharcmp`
//!   (unsigned byte order); hashes `hashtext hashname hashbpchar hashchar (+extended)` (bpchar hashes the
//!   trimmed value).
//! - `textcat` (`||`), `textlen` (char_length: characters), `bpcharlen` (characters, trailing spaces
//!   excluded: `length('ab  '::char(4))` = 2), `textoctetlen`/`bpcharoctetlen` (bytes; bpchar INCLUDES padding),
//!   `bit_length` (SQL-language, oid 1811: octet_length*8), `text_substr` / `text_substr_no_len`
//!   (`substr(s, start [, count])`, 1-based, start may be <= 0: `substr('abc', 0, 2)` = `a`; negative count →
//!   22011 `negative substring length not allowed`), `textoverlay(_no_len)`, `textpos` (position, 1-based,
//!   0 if absent, empty needle → 1), `text_starts_with`, `lower`, `upper`, `initcap` (word = run of
//!   alphanumerics), `casefold`, `ltrim/rtrim/btrim` (2-arg: trim chars set) and `ltrim1/rtrim1/btrim1`
//!   (spaces), `lpad`/`rpad` (truncate to length when longer; empty fill → no padding; SQL-language
//!   2-arg versions oids 879/880 pad with spaces), `repeat` (count <= 0 → ''), `replace_text`,
//!   `split_part` (field 0 → 22023 `field position must not be zero`; negative counts from the end),
//!   `translate`, `text_reverse`, `text_left`, `text_right` (negative n = all but |n|), `ascii`
//!   (code point of first char, 0 for ''), `chr` (0 → 54000 `null character not permitted`; invalid
//!   code point → 54000 `requested character too large for encoding: N`), `unistr`, `to_ascii_*`
//!   (unsupported → keep in `lookup` returning None), `unicode_normalize_func`, `unicode_is_normalized`,
//!   `unicode_assigned`;
//! - casts/conversions: `text_name`, `name_text`, `bpchar_name`, `name_bpchar`, `text_char`, `char_text`,
//!   `char_bpchar`, `chartoi4`, `i4tochar` (outside -128..=127 → 22003 `"char" out of range`),
//!   `nameconcatoid`;
//! - SQL-language: `textanycat` (oid 2003, `text || anynonarray`) and `anytextcat` (oid 2004): convert the
//!   non-text arg with `crate::io::output` of `ctx.arg_types[i]` and concatenate.
//!
//! Oracle: `'ab'::char(4) || 'x'` = `abx` (bpchar → text cast strips padding);
//! `length('ab  '::char(4))` = 2; `'abcd'::varchar(3)` = `abc` (explicit cast truncates).
//!
//! Implementation notes (PG 18.6 behavior, verified against the reference source):
//! - Comparisons follow `varstr_cmp` under the C collation: `memcmp` on the bytes, and when the common
//!   prefix is equal the shorter string sorts first (-1/0/1). `bttextcmp` returns the first differing
//!   byte difference like glibc `memcmp`.
//! - `lower`/`upper`/`initcap` use the builtin provider's SIMPLE (one-to-one) case mapping, because the
//!   pinned C.UTF-8 locale is not `PG_UNICODE_FAST`. Multi-character full mappings (`ß` → `SS`) do not
//!   apply, so `upper('straße')` = `STRAßE`.
//! - Known gaps in this module: `casefold` (needs simple case-folding tables), `normalize` /
//!   `is_normalized` / `unicode_assigned` (need Unicode normalization and assignment tables) and
//!   `to_ascii` (locale transliteration tables) return 0A000 through `unsupported`.
use crate::catalog_data::BuiltinProc;
use crate::compare::{pg_hash_bytes, pg_hash_bytes_extended, pg_hash_uint32, pg_hash_uint32_extended};
use crate::context::FuncCtx;
use crate::datum::{type_mismatch, Datum};
use crate::func::{FuncImpl, ScalarFn};
use crate::io;
use crate::text::io::{bpchar_trimmed, truncate_name};
use pgarbor_common::{PgError, Result, SqlState};

/// `ERRCODE_SUBSTRING_ERROR` (class 22, `negative substring length not allowed`).
const SUBSTRING_ERROR: SqlState = SqlState(*b"22011");

/// PG `MaxAllocSize` minus one: the largest allocation whose size check passes.
const MAX_ALLOC_SIZE: usize = 0x3FFF_FFFF;

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = if proc.prosrc.is_empty() {
        // SQL-language builtins are matched by OID.
        match proc.oid {
            879 => lpad2,
            880 => rpad2,
            1811 => bit_length,
            2003 => textanycat,
            2004 => anytextcat,
            _ => return None,
        }
    } else {
        match proc.prosrc {
            // comparison
            "texteq" => texteq,
            "textne" => textne,
            "text_lt" => text_lt,
            "text_le" => text_le,
            "text_gt" => text_gt,
            "text_ge" => text_ge,
            "bttextcmp" => bttextcmp,
            "text_larger" => text_larger,
            "text_smaller" => text_smaller,
            "text_pattern_lt" => text_pattern_lt,
            "text_pattern_le" => text_pattern_le,
            "text_pattern_gt" => text_pattern_gt,
            "text_pattern_ge" => text_pattern_ge,
            "bttext_pattern_cmp" => bttext_pattern_cmp,
            "nameeq" => nameeq,
            "namene" => namene,
            "namelt" => namelt,
            "namele" => namele,
            "namegt" => namegt,
            "namege" => namege,
            "btnamecmp" => btnamecmp,
            "nameeqtext" => nameeqtext,
            "namelttext" => namelttext,
            "nameletext" => nameletext,
            "namegetext" => namegetext,
            "namegttext" => namegttext,
            "namenetext" => namenetext,
            "btnametextcmp" => btnametextcmp,
            "texteqname" => texteqname,
            "textltname" => textltname,
            "textlename" => textlename,
            "textgename" => textgename,
            "textgtname" => textgtname,
            "textnename" => textnename,
            "bttextnamecmp" => bttextnamecmp,
            "bpchareq" => bpchareq,
            "bpcharne" => bpcharne,
            "bpcharlt" => bpcharlt,
            "bpcharle" => bpcharle,
            "bpchargt" => bpchargt,
            "bpcharge" => bpcharge,
            "bpcharcmp" => bpcharcmp,
            "bpchar_larger" => bpchar_larger,
            "bpchar_smaller" => bpchar_smaller,
            "bpchar_pattern_lt" => bpchar_pattern_lt,
            "bpchar_pattern_le" => bpchar_pattern_le,
            "bpchar_pattern_ge" => bpchar_pattern_ge,
            "bpchar_pattern_gt" => bpchar_pattern_gt,
            "btbpchar_pattern_cmp" => btbpchar_pattern_cmp,
            "chareq" => chareq,
            "charne" => charne,
            "charlt" => charlt,
            "charle" => charle,
            "chargt" => chargt,
            "charge" => charge,
            "btcharcmp" => btcharcmp,
            // hashing
            "hashtext" | "hashname" => hashtext,
            "hashtextextended" | "hashnameextended" => hashtextextended,
            "hashbpchar" => hashbpchar,
            "hashbpcharextended" => hashbpcharextended,
            "hashchar" => hashchar,
            "hashcharextended" => hashcharextended,
            // conversions and casts
            "text_name" => text_name,
            "name_text" => name_text,
            "bpchar_name" => bpchar_name,
            "name_bpchar" => name_bpchar,
            "text_char" => text_char,
            "char_text" => char_text,
            "char_bpchar" => char_bpchar,
            "chartoi4" => chartoi4,
            "i4tochar" => i4tochar,
            "nameconcatoid" => nameconcatoid,
            // string functions
            "textcat" => textcat,
            "textlen" => textlen,
            "bpcharlen" => bpcharlen,
            "textoctetlen" => textoctetlen,
            "bpcharoctetlen" => bpcharoctetlen,
            "text_substr" => text_substr,
            "text_substr_no_len" => text_substr_no_len,
            "textoverlay" => textoverlay,
            "textoverlay_no_len" => textoverlay_no_len,
            "textpos" => textpos,
            "text_starts_with" => text_starts_with,
            "lower" => lower,
            "upper" => upper,
            "initcap" => initcap,
            "ltrim" => ltrim,
            "rtrim" => rtrim,
            "btrim" => btrim,
            "ltrim1" => ltrim1,
            "rtrim1" => rtrim1,
            "btrim1" => btrim1,
            "lpad" => lpad,
            "rpad" => rpad,
            "repeat" => repeat,
            "replace_text" => replace_text,
            "split_part" => split_part,
            "translate" => translate,
            "text_reverse" => text_reverse,
            "text_left" => text_left,
            "text_right" => text_right,
            "ascii" => ascii,
            "chr" => chr,
            "unistr" => unistr,
            _ => return None,
        }
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Text-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-14", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument and error helpers
// ---------------------------------------------------------------------------------------------

fn datum_arg(a: &[Datum], i: usize) -> Result<&Datum> {
    a.get(i).ok_or_else(|| PgError::internal(format!("missing function argument {i}")))
}

fn text_arg(a: &[Datum], i: usize) -> Result<&str> {
    match datum_arg(a, i)? {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("Text", other)),
    }
}

fn int4_arg(a: &[Datum], i: usize) -> Result<i32> {
    match datum_arg(a, i)? {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("Int4", other)),
    }
}

fn int8_arg(a: &[Datum], i: usize) -> Result<i64> {
    match datum_arg(a, i)? {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("Int8", other)),
    }
}

fn char_arg(a: &[Datum], i: usize) -> Result<u8> {
    match datum_arg(a, i)? {
        Datum::Char(c) => Ok(*c),
        other => Err(type_mismatch("Char", other)),
    }
}

fn oid_arg(a: &[Datum], i: usize) -> Result<u32> {
    match datum_arg(a, i)? {
        Datum::Oid(v) => Ok(*v),
        other => Err(type_mismatch("Oid", other)),
    }
}

/// `integer out of range` (22003), used when a character or byte count exceeds int4.
fn int4_overflow() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

fn negative_substring() -> PgError {
    PgError::new(SUBSTRING_ERROR, "negative substring length not allowed")
}

fn requested_length_too_large() -> PgError {
    PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "requested length too large")
}

fn invalid_utf8_byte(c: u8) -> PgError {
    PgError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": 0x{c:02x}"),
    )
}

fn invalid_surrogate_pair() -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, "invalid Unicode surrogate pair")
}

fn char_count_i32(s: &str) -> Result<i32> {
    i32::try_from(s.chars().count()).map_err(|_| int4_overflow())
}

fn byte_count_i32(s: &str) -> Result<i32> {
    i32::try_from(s.len()).map_err(|_| int4_overflow())
}

/// Slice starting after `skip` characters and holding at most `take` characters (the whole remainder
/// when `take` is `None`). Clips at the end of the string.
fn take_chars(s: &str, skip: usize, take: Option<usize>) -> &str {
    let start = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(s.len());
    let rest = &s[start..];
    match take {
        None => rest,
        Some(n) => {
            let end = rest.char_indices().nth(n).map(|(i, _)| i).unwrap_or(rest.len());
            &rest[..end]
        }
    }
}

/// `varstr_cmp` under the C collation: `memcmp` on the common prefix (glibc-style byte difference),
/// then the shorter string first.
fn str_cmp(x: &[u8], y: &[u8]) -> i32 {
    if let Some(i) = x.iter().zip(y.iter()).position(|(p, q)| p != q) {
        return x[i] as i32 - y[i] as i32;
    }
    match x.len().cmp(&y.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Compare the first two text arguments. `bpchar` drops trailing spaces first (`bcTruelen`).
fn cmp_args(a: &[Datum], bpchar: bool) -> Result<i32> {
    let x = text_arg(a, 0)?;
    let y = text_arg(a, 1)?;
    if bpchar {
        Ok(str_cmp(bpchar_trimmed(x).as_bytes(), bpchar_trimmed(y).as_bytes()))
    } else {
        Ok(str_cmp(x.as_bytes(), y.as_bytes()))
    }
}

macro_rules! text_pred {
    ($name:ident, $bp:expr, $pred:expr) => {
        fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let c = cmp_args(a, $bp)?;
            let p: fn(i32) -> bool = $pred;
            Ok(Datum::Bool(p(c)))
        }
    };
}

macro_rules! text_cmp_int {
    ($name:ident, $bp:expr) => {
        fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            Ok(Datum::Int4(cmp_args(a, $bp)?))
        }
    };
}

macro_rules! text_minmax {
    ($name:ident, $bp:expr, $pick_first:expr) => {
        fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let c = cmp_args(a, $bp)?;
            let pick: fn(i32) -> bool = $pick_first;
            Ok(if pick(c) { datum_arg(a, 0)?.clone() } else { datum_arg(a, 1)?.clone() })
        }
    };
}

macro_rules! char_pred {
    ($name:ident, $pred:expr) => {
        fn $name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
            let c = char_arg(a, 0)? as i32 - char_arg(a, 1)? as i32;
            let p: fn(i32) -> bool = $pred;
            Ok(Datum::Bool(p(c)))
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Comparison operators and btree support functions
// ---------------------------------------------------------------------------------------------

char_pred!(chareq, |c| c == 0);
char_pred!(charne, |c| c != 0);
char_pred!(charlt, |c| c < 0);
char_pred!(charle, |c| c <= 0);
char_pred!(chargt, |c| c > 0);
char_pred!(charge, |c| c >= 0);

fn btcharcmp(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(char_arg(a, 0)? as i32 - char_arg(a, 1)? as i32))
}

text_pred!(texteq, false, |c| c == 0);
text_pred!(textne, false, |c| c != 0);
text_pred!(text_lt, false, |c| c < 0);
text_pred!(text_le, false, |c| c <= 0);
text_pred!(text_gt, false, |c| c > 0);
text_pred!(text_ge, false, |c| c >= 0);
text_cmp_int!(bttextcmp, false);
text_minmax!(text_larger, false, |c| c > 0);
text_minmax!(text_smaller, false, |c| c < 0);
text_pred!(text_pattern_lt, false, |c| c < 0);
text_pred!(text_pattern_le, false, |c| c <= 0);
text_pred!(text_pattern_gt, false, |c| c > 0);
text_pred!(text_pattern_ge, false, |c| c >= 0);
text_cmp_int!(bttext_pattern_cmp, false);

text_pred!(nameeq, false, |c| c == 0);
text_pred!(namene, false, |c| c != 0);
text_pred!(namelt, false, |c| c < 0);
text_pred!(namele, false, |c| c <= 0);
text_pred!(namegt, false, |c| c > 0);
text_pred!(namege, false, |c| c >= 0);
text_cmp_int!(btnamecmp, false);
text_pred!(nameeqtext, false, |c| c == 0);
text_pred!(namelttext, false, |c| c < 0);
text_pred!(nameletext, false, |c| c <= 0);
text_pred!(namegetext, false, |c| c >= 0);
text_pred!(namegttext, false, |c| c > 0);
text_pred!(namenetext, false, |c| c != 0);
text_cmp_int!(btnametextcmp, false);
text_pred!(texteqname, false, |c| c == 0);
text_pred!(textltname, false, |c| c < 0);
text_pred!(textlename, false, |c| c <= 0);
text_pred!(textgename, false, |c| c >= 0);
text_pred!(textgtname, false, |c| c > 0);
text_pred!(textnename, false, |c| c != 0);
text_cmp_int!(bttextnamecmp, false);

text_pred!(bpchareq, true, |c| c == 0);
text_pred!(bpcharne, true, |c| c != 0);
text_pred!(bpcharlt, true, |c| c < 0);
text_pred!(bpcharle, true, |c| c <= 0);
text_pred!(bpchargt, true, |c| c > 0);
text_pred!(bpcharge, true, |c| c >= 0);
text_cmp_int!(bpcharcmp, true);
text_minmax!(bpchar_larger, true, |c| c > 0);
text_minmax!(bpchar_smaller, true, |c| c < 0);
text_pred!(bpchar_pattern_lt, true, |c| c < 0);
text_pred!(bpchar_pattern_le, true, |c| c <= 0);
text_pred!(bpchar_pattern_gt, true, |c| c > 0);
text_pred!(bpchar_pattern_ge, true, |c| c >= 0);
text_cmp_int!(btbpchar_pattern_cmp, true);

// ---------------------------------------------------------------------------------------------
// Hashing (PG hash_bytes / hash_uint32 ports live in crate::compare)
// ---------------------------------------------------------------------------------------------

fn hashtext(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(pg_hash_bytes(text_arg(a, 0)?.as_bytes())? as i32))
}

fn hashtextextended(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let seed = int8_arg(a, 1)? as u64;
    Ok(Datum::Int8(pg_hash_bytes_extended(text_arg(a, 0)?.as_bytes(), seed)? as i64))
}

fn hashbpchar(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(pg_hash_bytes(bpchar_trimmed(text_arg(a, 0)?).as_bytes())? as i32))
}

fn hashbpcharextended(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let seed = int8_arg(a, 1)? as u64;
    Ok(Datum::Int8(pg_hash_bytes_extended(bpchar_trimmed(text_arg(a, 0)?).as_bytes(), seed)? as i64))
}

/// `hashchar`: `hash_uint32((int32) PG_GETARG_CHAR(0))`, where `char` is signed in the C ABI.
fn hashchar(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = char_arg(a, 0)? as i8 as i32 as u32;
    Ok(Datum::Int4(pg_hash_uint32(v)? as i32))
}

fn hashcharextended(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = char_arg(a, 0)? as i8 as i32 as u32;
    let seed = int8_arg(a, 1)? as u64;
    Ok(Datum::Int8(pg_hash_uint32_extended(v, seed)? as i64))
}

// ---------------------------------------------------------------------------------------------
// Conversions and casts
// ---------------------------------------------------------------------------------------------

fn text_name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(truncate_name(text_arg(a, 0)?)?))
}

fn name_text(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.to_string()))
}

fn bpchar_name(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(truncate_name(bpchar_trimmed(text_arg(a, 0)?))?))
}

fn name_bpchar(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.to_string()))
}

/// `text_char`: first byte of the string, or 0 for ''.
fn text_char(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    Ok(Datum::Char(s.as_bytes().first().copied().unwrap_or(0)))
}

/// A `"char"` byte as text. Bytes >= 0x80 are not valid UTF-8 on their own, so they are rejected.
fn char_to_string(c: u8) -> Result<String> {
    if c == 0 {
        Ok(String::new())
    } else if c < 0x80 {
        Ok((c as char).to_string())
    } else {
        Err(invalid_utf8_byte(c))
    }
}

fn char_text(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(char_to_string(char_arg(a, 0)?)?))
}

fn char_bpchar(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(char_to_string(char_arg(a, 0)?)?))
}

/// `chartoi4`: `(int32) (int8) c`, so bytes >= 0x80 are negative.
fn chartoi4(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(char_arg(a, 0)? as i8 as i32))
}

fn i4tochar(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = int4_arg(a, 0)?;
    if !(-128..=127).contains(&v) {
        return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "\"char\" out of range"));
    }
    Ok(Datum::Char(v as i8 as u8))
}

fn nameconcatoid(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let name = text_arg(a, 0)?;
    let oid = oid_arg(a, 1)?;
    Ok(Datum::Text(truncate_name(&format!("{name}_{oid}"))?))
}

// ---------------------------------------------------------------------------------------------
// String functions
// ---------------------------------------------------------------------------------------------

fn textcat(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let x = text_arg(a, 0)?;
    let y = text_arg(a, 1)?;
    let mut s = String::with_capacity(x.len() + y.len());
    s.push_str(x);
    s.push_str(y);
    Ok(Datum::Text(s))
}

fn textlen(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(char_count_i32(text_arg(a, 0)?)?))
}

fn bpcharlen(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(char_count_i32(bpchar_trimmed(text_arg(a, 0)?))?))
}

fn textoctetlen(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(byte_count_i32(text_arg(a, 0)?)?))
}

fn bpcharoctetlen(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(byte_count_i32(text_arg(a, 0)?)?))
}

/// SQL-language `bit_length(text)` (oid 1811): `octet_length(s) * 8`.
fn bit_length(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let bits = text_arg(a, 0)?.len().checked_mul(8).ok_or_else(int4_overflow)?;
    Ok(Datum::Int4(i32::try_from(bits).map_err(|_| int4_overflow())?))
}

/// PG `text_substring` (varlena.c), character based. `length == None` means "to the end".
fn substring_impl(s: &str, start: i64, length: Option<i64>) -> Result<String> {
    let s1 = start.max(1);
    let skip = (s1 - 1) as usize;
    match length {
        None => Ok(take_chars(s, skip, None).to_string()),
        Some(l) if l < 0 => Err(negative_substring()),
        Some(l) => {
            let end = start + l;
            if end < 1 {
                return Ok(String::new());
            }
            Ok(take_chars(s, skip, Some((end - s1) as usize)).to_string())
        }
    }
}

fn text_substr(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let start = int4_arg(a, 1)? as i64;
    let len = int4_arg(a, 2)? as i64;
    Ok(Datum::Text(substring_impl(s, start, Some(len))?))
}

fn text_substr_no_len(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let start = int4_arg(a, 1)? as i64;
    Ok(Datum::Text(substring_impl(s, start, None)?))
}

/// PG `text_overlay`: `substring(s, 1, sp - 1) || placing || substring(s, sp + sl)`.
fn overlay_impl(s: &str, placing: &str, sp: i32, sl: i32) -> Result<Datum> {
    if sp <= 0 {
        return Err(negative_substring());
    }
    let end = sp as i64 + sl as i64;
    if end > i32::MAX as i64 || end < i32::MIN as i64 {
        return Err(int4_overflow());
    }
    let head = substring_impl(s, 1, Some(sp as i64 - 1))?;
    let tail = substring_impl(s, end, None)?;
    let mut out = String::with_capacity(head.len() + placing.len() + tail.len());
    out.push_str(&head);
    out.push_str(placing);
    out.push_str(&tail);
    Ok(Datum::Text(out))
}

fn textoverlay(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let placing = text_arg(a, 1)?;
    let sp = int4_arg(a, 2)?;
    let sl = int4_arg(a, 3)?;
    overlay_impl(s, placing, sp, sl)
}

fn textoverlay_no_len(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let placing = text_arg(a, 1)?;
    let sp = int4_arg(a, 2)?;
    let sl = char_count_i32(placing)?;
    overlay_impl(s, placing, sp, sl)
}

/// 1-based character position of `needle` in `s`, 0 if absent, 1 for an empty needle.
fn text_position(s: &str, needle: &str) -> Result<i32> {
    if needle.is_empty() {
        return Ok(1);
    }
    match s.find(needle) {
        Some(b) => {
            let n = s[..b].chars().count() + 1;
            i32::try_from(n).map_err(|_| int4_overflow())
        }
        None => Ok(0),
    }
}

fn textpos(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(text_position(text_arg(a, 0)?, text_arg(a, 1)?)?))
}

fn text_starts_with(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(text_arg(a, 0)?.starts_with(text_arg(a, 1)?)))
}

/// Simple (one-to-one) lowercase mapping, as PG's builtin provider uses for C.UTF-8.
fn simple_lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Simple uppercase mapping. A character whose full mapping is multi-character keeps itself
/// (`ß` stays `ß`), which is the simple mapping for it.
fn simple_upper(c: char) -> char {
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// Simple titlecase mapping: the Latin digraphs differ from uppercase, the rest use uppercase.
fn simple_title(c: char) -> char {
    match c as u32 {
        0x1C4..=0x1C6 => '\u{1C5}',
        0x1C7..=0x1C9 => '\u{1C8}',
        0x1CA..=0x1CC => '\u{1CB}',
        0x1F1..=0x1F3 => '\u{1F2}',
        _ => simple_upper(c),
    }
}

fn lower(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.chars().map(simple_lower).collect()))
}

fn upper(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.chars().map(simple_upper).collect()))
}

/// Words are maximal runs of alphanumerics: the first character of each word is titlecased, the
/// rest are lowercased, and everything else is copied unchanged.
fn initcap(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let mut out = String::with_capacity(s.len());
    let mut prev_alnum = false;
    for c in s.chars() {
        let alnum = c.is_alphanumeric();
        if !alnum {
            out.push(c);
        } else if prev_alnum {
            out.push(simple_lower(c));
        } else {
            out.push(simple_title(c));
        }
        prev_alnum = alnum;
    }
    Ok(Datum::Text(out))
}

fn ltrim(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let set = text_arg(a, 1)?;
    Ok(Datum::Text(s.trim_start_matches(|c: char| set.contains(c)).to_string()))
}

fn rtrim(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let set = text_arg(a, 1)?;
    Ok(Datum::Text(s.trim_end_matches(|c: char| set.contains(c)).to_string()))
}

fn btrim(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let set = text_arg(a, 1)?;
    Ok(Datum::Text(s.trim_matches(|c: char| set.contains(c)).to_string()))
}

fn ltrim1(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.trim_start_matches(' ').to_string()))
}

fn rtrim1(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.trim_end_matches(' ').to_string()))
}

fn btrim1(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.trim_matches(' ').to_string()))
}

/// PG `lpad`/`rpad` (oracle_compat.c): truncate to `len` characters, then pad with the repeated
/// `fill` up to `len`. An empty fill never pads.
fn pad_impl(s: &str, len: i32, fill: &str, left: bool) -> Result<String> {
    let target: usize = if len < 0 { 0 } else { len as usize };
    let base = take_chars(s, 0, Some(target));
    if fill.is_empty() {
        return Ok(base.to_string());
    }
    if target > MAX_ALLOC_SIZE / 4 {
        return Err(requested_length_too_large());
    }
    let m = target - base.chars().count();
    let fill_chars: Vec<char> = fill.chars().collect();
    let mut pad = String::with_capacity(m);
    for i in 0..m {
        pad.push(fill_chars[i % fill_chars.len()]);
    }
    let mut out = String::with_capacity(base.len() + pad.len());
    if left {
        out.push_str(&pad);
        out.push_str(base);
    } else {
        out.push_str(base);
        out.push_str(&pad);
    }
    Ok(out)
}

fn lpad(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let len = int4_arg(a, 1)?;
    let fill = text_arg(a, 2)?;
    Ok(Datum::Text(pad_impl(s, len, fill, true)?))
}

fn rpad(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let len = int4_arg(a, 1)?;
    let fill = text_arg(a, 2)?;
    Ok(Datum::Text(pad_impl(s, len, fill, false)?))
}

/// SQL-language `lpad(text, int4)` (oid 879): pads with spaces.
fn lpad2(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let len = int4_arg(a, 1)?;
    Ok(Datum::Text(pad_impl(s, len, " ", true)?))
}

/// SQL-language `rpad(text, int4)` (oid 880): pads with spaces.
fn rpad2(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let len = int4_arg(a, 1)?;
    Ok(Datum::Text(pad_impl(s, len, " ", false)?))
}

fn repeat(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let n = int4_arg(a, 1)?;
    if n <= 0 || s.is_empty() {
        return Ok(Datum::Text(String::new()));
    }
    let count = n as usize;
    let total = s.len().checked_mul(count).ok_or_else(requested_length_too_large)?;
    if total > MAX_ALLOC_SIZE {
        return Err(requested_length_too_large());
    }
    Ok(Datum::Text(s.repeat(count)))
}

fn replace_text(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let from = text_arg(a, 1)?;
    let to = text_arg(a, 2)?;
    if from.is_empty() {
        return Ok(Datum::Text(s.to_string()));
    }
    Ok(Datum::Text(s.replace(from, to)))
}

/// PG `split_part`: field numbers are 1-based, negative counts from the end.
fn split_part(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let sep = text_arg(a, 1)?;
    let n = int4_arg(a, 2)?;
    if n == 0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "field position must not be zero"));
    }
    if s.is_empty() {
        return Ok(Datum::Text(String::new()));
    }
    if sep.is_empty() {
        return Ok(Datum::Text(if n == 1 || n == -1 { s.to_string() } else { String::new() }));
    }
    let fields: Vec<&str> = s.split(sep).collect();
    let count = fields.len() as i64;
    let idx: i64 = if n > 0 { n as i64 - 1 } else { count + n as i64 };
    if idx < 0 || idx >= count {
        return Ok(Datum::Text(String::new()));
    }
    Ok(Datum::Text(fields[idx as usize].to_string()))
}

/// PG `translate`: the first occurrence of each `from` character decides; a character with no
/// counterpart in `to` is deleted.
fn translate(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let from: Vec<char> = text_arg(a, 1)?.chars().collect();
    let to: Vec<char> = text_arg(a, 2)?.chars().collect();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match from.iter().position(|&f| f == c) {
            Some(idx) => {
                if idx < to.len() {
                    out.push(to[idx]);
                }
            }
            None => out.push(c),
        }
    }
    Ok(Datum::Text(out))
}

fn text_reverse(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(text_arg(a, 0)?.chars().rev().collect()))
}

/// `left(s, n)`: first n characters; a negative n keeps all but the last |n|.
fn text_left(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let n = int4_arg(a, 1)? as i64;
    let keep: usize = if n < 0 {
        let c = s.chars().count() as i64 + n;
        if c < 0 {
            0
        } else {
            c as usize
        }
    } else {
        n as usize
    };
    Ok(Datum::Text(take_chars(s, 0, Some(keep)).to_string()))
}

/// `right(s, n)`: last n characters; a negative n drops the first |n|.
fn text_right(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = text_arg(a, 0)?;
    let n = int4_arg(a, 1)? as i64;
    let skip: usize = if n < 0 {
        (-n) as usize
    } else {
        let len = s.chars().count() as i64;
        if n >= len {
            0
        } else {
            (len - n) as usize
        }
    };
    Ok(Datum::Text(take_chars(s, skip, None).to_string()))
}

fn ascii(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(text_arg(a, 0)?.chars().next().map(|c| c as i32).unwrap_or(0)))
}

fn chr(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let n = int4_arg(a, 0)?;
    if n < 0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "character number must be positive"));
    }
    if n == 0 {
        return Err(PgError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "null character not permitted"));
    }
    let cv = n as u32;
    if cv > 0x10_FFFF {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {cv}"),
        ));
    }
    if (0xD800..=0xDFFF).contains(&cv) {
        return Err(PgError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character not valid for encoding: {cv}"),
        ));
    }
    match char::from_u32(cv) {
        Some(c) => Ok(Datum::Text(c.to_string())),
        None => Err(PgError::internal("chr: invalid code point after range check")),
    }
}

fn hex_digit(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some((b - b'0') as u32),
        b'a'..=b'f' => Some((b - b'a' + 10) as u32),
        b'A'..=b'F' => Some((b - b'A' + 10) as u32),
        _ => None,
    }
}

/// Value of a run of hex digits; `None` when any byte is not a hex digit.
fn hex_run(b: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    for &x in b {
        v = v * 16 + hex_digit(x)?;
    }
    Some(v)
}

/// Escape at the start of `rest` (which begins with a backslash): (code point, byte width).
fn unicode_escape(rest: &[u8]) -> Option<(u32, usize)> {
    if rest.len() >= 5 {
        if let Some(v) = hex_run(&rest[1..5]) {
            return Some((v, 5));
        }
    }
    if rest.len() >= 6 && rest[1] == b'u' {
        if let Some(v) = hex_run(&rest[2..6]) {
            return Some((v, 6));
        }
    }
    if rest.len() >= 8 && rest[1] == b'+' {
        if let Some(v) = hex_run(&rest[2..8]) {
            return Some((v, 8));
        }
    }
    if rest.len() >= 10 && rest[1] == b'U' {
        if let Some(v) = hex_run(&rest[2..10]) {
            return Some((v, 10));
        }
    }
    None
}

fn is_utf16_first(c: u32) -> bool {
    (0xD800..=0xDBFF).contains(&c)
}

fn is_utf16_second(c: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&c)
}

fn push_code_point(out: &mut String, cp: u32) -> Result<()> {
    if cp == 0 {
        // Text values never hold U+0000 (TYP-13 rule), so the NUL escape is rejected.
        return Err(invalid_utf8_byte(0));
    }
    match char::from_u32(cp) {
        Some(c) => {
            out.push(c);
            Ok(())
        }
        None => Err(invalid_surrogate_pair()),
    }
}

/// PG `unistr` (varlena.c): `\XXXX`, `\+XXXXXX`, `\uXXXX`, `\UXXXXXXXX` and `\\`, with UTF-16
/// surrogate pairs combined.
fn unistr_impl(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut pair_first: u32 = 0;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' {
            let rest = &b[i..];
            if rest.len() >= 2 && rest[1] == b'\\' {
                if pair_first != 0 {
                    return Err(invalid_surrogate_pair());
                }
                out.push('\\');
                i += 2;
                continue;
            }
            let Some((mut cp, width)) = unicode_escape(rest) else {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "invalid Unicode escape").with_hint(
                    "Unicode escapes must be \\XXXX, \\+XXXXXX, \\uXXXX, or \\UXXXXXXXX.",
                ));
            };
            if cp > 0x10_FFFF {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid Unicode code point: {cp:04X}"),
                ));
            }
            if pair_first != 0 {
                if !is_utf16_second(cp) {
                    return Err(invalid_surrogate_pair());
                }
                cp = 0x10000 + ((pair_first - 0xD800) << 10) + (cp - 0xDC00);
                pair_first = 0;
            } else if is_utf16_second(cp) {
                return Err(invalid_surrogate_pair());
            }
            if is_utf16_first(cp) {
                pair_first = cp;
            } else {
                push_code_point(&mut out, cp)?;
            }
            i += width;
        } else {
            if pair_first != 0 {
                return Err(invalid_surrogate_pair());
            }
            let c = s
                .get(i..)
                .and_then(|t| t.chars().next())
                .ok_or_else(|| PgError::internal("unistr: byte offset is not a character boundary"))?;
            out.push(c);
            i += c.len_utf8();
        }
    }
    if pair_first != 0 {
        return Err(invalid_surrogate_pair());
    }
    Ok(out)
}

fn unistr(a: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(unistr_impl(text_arg(a, 0)?)?))
}

/// SQL-language `textanycat` (oid 2003): `text || anynonarray`, the right side through its output
/// function.
fn textanycat(a: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let head = text_arg(a, 0)?;
    let tail = io::output(&ctx.io_ctx(), ctx.arg_type(1), datum_arg(a, 1)?)?;
    Ok(Datum::Text(format!("{head}{tail}")))
}

/// SQL-language `anytextcat` (oid 2004): `anynonarray || text`.
fn anytextcat(a: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let head = io::output(&ctx.io_ctx(), ctx.arg_type(0), datum_arg(a, 0)?)?;
    let tail = text_arg(a, 1)?;
    Ok(Datum::Text(format!("{head}{tail}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;

    fn call(oid: u32, args: &[Datum], types: &[TypeId]) -> Result<Datum> {
        let env = crate::registry::builtin_env();
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &cfg,
            session: &sess,
            arg_types: types,
            result_type: TypeId::TEXT,
            collation: 0,
        };
        crate::func::implementation(oid)?.call(args, &ctx)
    }

    fn t(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    fn i(v: i32) -> Datum {
        Datum::Int4(v)
    }

    #[test]
    fn substr_zero_start_and_negative_length() {
        assert_eq!(call(877, &[t("abc"), i(0), i(2)], &[]).unwrap(), t("a"));
        assert_eq!(call(877, &[t("abc"), i(1), i(-1)], &[]).unwrap_err().sqlstate, SqlState(*b"22011"));
        assert_eq!(call(883, &[t("abc"), i(2)], &[]).unwrap(), t("bc"));
    }

    #[test]
    fn split_part_rules() {
        assert_eq!(call(2088, &[t("a,b,c"), t(","), i(-1)], &[]).unwrap(), t("c"));
        assert_eq!(call(2088, &[t("a,b,c"), t(","), i(2)], &[]).unwrap(), t("b"));
        assert_eq!(
            call(2088, &[t("a,b,c"), t(","), i(0)], &[]).unwrap_err().sqlstate,
            SqlState::INVALID_PARAMETER_VALUE
        );
    }

    #[test]
    fn left_right_and_negative_counts() {
        assert_eq!(call(3060, &[t("abcd"), i(-1)], &[]).unwrap(), t("abc"));
        assert_eq!(call(3061, &[t("abcd"), i(2)], &[]).unwrap(), t("cd"));
        assert_eq!(call(3061, &[t("abcd"), i(-1)], &[]).unwrap(), t("bcd"));
        assert_eq!(call(3061, &[t("abc"), i(5)], &[]).unwrap(), t("abc"));
    }

    #[test]
    fn pads() {
        assert_eq!(call(873, &[t("hi"), i(5), t("xy")], &[]).unwrap(), t("xyxhi"));
        assert_eq!(call(874, &[t("hi"), i(5), t("xy")], &[]).unwrap(), t("hixyx"));
        assert_eq!(call(879, &[t("hello"), i(2)], &[]).unwrap(), t("he"));
        assert_eq!(call(873, &[t("hi"), i(5), t("")], &[]).unwrap(), t("hi"));
    }

    #[test]
    fn case_mapping() {
        assert_eq!(
            call(872, &[t("hELLO wORLD-foo bar_baz")], &[]).unwrap(),
            t("Hello World-Foo Bar_Baz")
        );
        assert_eq!(call(871, &[t("stra\u{df}e")], &[]).unwrap(), t("STRA\u{df}E"));
        assert_eq!(call(870, &[t("ABC")], &[]).unwrap(), t("abc"));
    }

    #[test]
    fn chr_and_ascii() {
        assert_eq!(call(1621, &[i(0)], &[]).unwrap_err().sqlstate, SqlState::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(call(1621, &[i(233)], &[]).unwrap(), t("\u{e9}"));
        assert_eq!(call(1620, &[t("")], &[]).unwrap(), i(0));
    }

    #[test]
    fn bpchar_semantics() {
        assert_eq!(call(1048, &[t("ab  "), t("ab")], &[]).unwrap(), Datum::Bool(true));
        assert_eq!(call(1318, &[t("ab  ")], &[]).unwrap(), i(2));
        assert_eq!(call(1375, &[t("ab  ")], &[]).unwrap(), i(4));
    }

    #[test]
    fn char_conversions() {
        assert_eq!(
            call(78, &[i(200)], &[]).unwrap_err().sqlstate,
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE
        );
        assert_eq!(call(77, &[Datum::Char(200)], &[]).unwrap(), i(-56));
        assert_eq!(call(77, &[Datum::Char(0x41)], &[]).unwrap(), i(65));
    }

    #[test]
    fn unistr_escapes() {
        assert_eq!(call(6198, &[t("\\0041\\+000042")], &[]).unwrap(), t("AB"));
        assert_eq!(call(6198, &[t("\\\\x")], &[]).unwrap(), t("\\x"));
        assert_eq!(call(6198, &[t("\\D83D\\DE00")], &[]).unwrap(), t("\u{1F600}"));
        assert_eq!(call(6198, &[t("\\zz")], &[]).unwrap_err().sqlstate, SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn translate_replace_and_position() {
        assert_eq!(call(878, &[t("12345"), t("143"), t("ax")], &[]).unwrap(), t("a2x5"));
        assert_eq!(call(2087, &[t("aXbX"), t("X"), t("-")], &[]).unwrap(), t("a-b-"));
        assert_eq!(call(849, &[t("hello"), t("ll")], &[]).unwrap(), i(3));
        assert_eq!(call(849, &[t("hello"), t("")], &[]).unwrap(), i(1));
    }

    #[test]
    fn overlay_and_concat() {
        assert_eq!(call(1404, &[t("abcdef"), t("XY"), i(3), i(2)], &[]).unwrap(), t("abXYef"));
        assert_eq!(
            call(2003, &[t("x"), Datum::Int4(5)], &[TypeId::TEXT, TypeId::INT4]).unwrap(),
            t("x5")
        );
        assert_eq!(call(1811, &[t("abc")], &[]).unwrap(), i(24));
    }

    #[test]
    fn hash_fixture() {
        assert_eq!(call(400, &[t("abc")], &[]).unwrap(), i(-785388649));
    }
}
