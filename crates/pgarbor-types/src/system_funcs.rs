//! Type-system / information functions that need no catalog beyond `TypeEnv` (TYPE-12). Lease TYP-43.
//!
//! PG18 reference: `utils/adt/misc.c` (`pg_typeof`, `pg_num_nulls`, `pg_column_size`?), `format_type.c`,
//! `version.c` (`pgsql_version`), `mbutils.c` (encodings), `dbsize.c` (`pg_size_pretty`, `pg_size_bytes`),
//! `regproc.c` (`to_regtype`, `to_regtypemod`), `src/backend/catalog/information_schema.sql`
//! (`_pg_char_max_length` etc.).
//! Owned procs (ImplModule::System):
//! - `pg_typeof(any)` → regtype `Datum::Oid(ctx.arg_types[0].oid)`; `format_type(oid, int4)` →
//!   `names::format_type` (NULL oid → NULL; unknown oid → `???`); `oidvectortypes`; `pg_basetype`.
//! - `pgsql_version` (`version()`): exactly `PostgreSQL 18.6 (PGArbor <CARGO_PKG_VERSION>) on
//!   <std::env::consts::ARCH>-<std::env::consts::OS>, compiled by rustc, 64-bit`. Tools only parse the
//!   leading `PostgreSQL 18.6`; this is a documented, intentional difference from the oracle
//!   (`PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on x86_64-pc-linux-gnu, compiled by gcc ...`).
//!   The `server_version` GUC is owned by the pgarbor crate.
//! - `pg_num_nulls`, `pg_num_nonnulls` (VARIADIC "any"); `pg_column_size(any)` (size of the
//!   `tuple::encode_value` encoding — documented divergence from PG's varlena sizes);
//!   `pg_column_compression` → NULL.
//! - Encodings: `getdatabaseencoding`, `pg_client_encoding` → `UTF8`; `PG_encoding_to_char(6)` → `UTF8`
//!   (encoding ids per PG `pg_enc`: 0 SQL_ASCII, 6 UTF8, 8 LATIN1 ...); `PG_char_to_encoding`;
//!   `pg_encoding_max_length_sql`.
//! - `pg_size_pretty(int8|numeric)` (`10 kB`, `1024 bytes`, rounding half-up per PG18 `pg_size_pretty`),
//!   `pg_size_bytes(text)` (errors 22023 `invalid size: "x"`).
//! - `to_regtype(text)`, `to_regtypemod(text)` (NULL when not found; uses `names::parse_type_string`).
//! - `timeofday()` → text like `Thu Feb 29 13:01:02.500000 2024 UTC` from `ctx.session.clock_timestamp()`.
//! - `parse_ident(text, strict bool)`, `unicode_version` (`16.0`), `icu_unicode_version` (NULL),
//!   `pg_collation_for(any)` (`"default"` for collatable types, NULL otherwise), `pg_settings_get_flags` (NULL).
//! - SQL-language information_schema helpers, matched by OID (pure functions of (typid, typmod)):
//!   `_pg_char_max_length` (13298), `_pg_char_octet_length` (13299), `_pg_numeric_precision` (13301),
//!   `_pg_numeric_precision_radix` (13302), `_pg_numeric_scale` (13303), `_pg_datetime_precision` (13304),
//!   `_pg_interval_type` (13305) — port their SQL bodies from `information_schema.sql` (they follow
//!   domains via `_pg_truetypid` — callers pass the true type already).
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, TimeZoneSetting};
use crate::datum::{type_mismatch, Datum};
use crate::func::FuncImpl;
use crate::names;
use crate::numeric::arith;
use crate::numeric::io as numeric_io;
use crate::numeric::Numeric;
use crate::oids;
use crate::tuple;
use bigdecimal::{RoundingMode, ToPrimitive};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;

const LEASE: &str = "TYP-43";

/// Database encoding id (PG `PG_UTF8`). `getdatabaseencoding()` is always UTF8 in PGArbor.
const DB_ENCODING_ID: usize = 6;

// Encoding tables generated from PG 18.6 `common/encnames.c`, `common/wchar.c` and `include/mb/pg_wchar.h`.
/// `pg_enc2name_tbl` (id = enum index in pg_wchar.h): (canonical name, maxmblen).
const ENCODINGS: [(&str, i32); 42] = [
    ("SQL_ASCII", 1),
    ("EUC_JP", 3),
    ("EUC_CN", 3),
    ("EUC_KR", 3),
    ("EUC_TW", 4),
    ("EUC_JIS_2004", 3),
    ("UTF8", 4),
    ("MULE_INTERNAL", 4),
    ("LATIN1", 1),
    ("LATIN2", 1),
    ("LATIN3", 1),
    ("LATIN4", 1),
    ("LATIN5", 1),
    ("LATIN6", 1),
    ("LATIN7", 1),
    ("LATIN8", 1),
    ("LATIN9", 1),
    ("LATIN10", 1),
    ("WIN1256", 1),
    ("WIN1258", 1),
    ("WIN866", 1),
    ("WIN874", 1),
    ("KOI8R", 1),
    ("WIN1251", 1),
    ("WIN1252", 1),
    ("ISO_8859_5", 1),
    ("ISO_8859_6", 1),
    ("ISO_8859_7", 1),
    ("ISO_8859_8", 1),
    ("WIN1250", 1),
    ("WIN1253", 1),
    ("WIN1254", 1),
    ("WIN1255", 1),
    ("WIN1257", 1),
    ("KOI8U", 1),
    ("SJIS", 2),
    ("BIG5", 2),
    ("GBK", 2),
    ("UHC", 2),
    ("GB18030", 4),
    ("JOHAB", 3),
    ("SHIFT_JIS_2004", 2),
];

/// `pg_encname_tbl`: alias names (already lower-cased, alphanumerics only) to encoding ids.
const ENCODING_ALIASES: [(&str, i32); 81] = [
    ("abc", 19),
    ("alt", 20),
    ("big5", 36),
    ("euccn", 2),
    ("eucjis2004", 5),
    ("eucjp", 1),
    ("euckr", 3),
    ("euctw", 4),
    ("gb18030", 39),
    ("gbk", 37),
    ("iso88591", 8),
    ("iso885910", 13),
    ("iso885913", 14),
    ("iso885914", 15),
    ("iso885915", 16),
    ("iso885916", 17),
    ("iso88592", 9),
    ("iso88593", 10),
    ("iso88594", 11),
    ("iso88595", 25),
    ("iso88596", 26),
    ("iso88597", 27),
    ("iso88598", 28),
    ("iso88599", 12),
    ("johab", 40),
    ("koi8", 22),
    ("koi8r", 22),
    ("koi8u", 34),
    ("latin1", 8),
    ("latin10", 17),
    ("latin2", 9),
    ("latin3", 10),
    ("latin4", 11),
    ("latin5", 12),
    ("latin6", 13),
    ("latin7", 14),
    ("latin8", 15),
    ("latin9", 16),
    ("mskanji", 35),
    ("muleinternal", 7),
    ("shiftjis", 35),
    ("shiftjis2004", 41),
    ("sjis", 35),
    ("sqlascii", 0),
    ("tcvn", 19),
    ("tcvn5712", 19),
    ("uhc", 38),
    ("unicode", 6),
    ("utf8", 6),
    ("vscii", 19),
    ("win", 23),
    ("win1250", 29),
    ("win1251", 23),
    ("win1252", 24),
    ("win1253", 30),
    ("win1254", 31),
    ("win1255", 32),
    ("win1256", 18),
    ("win1257", 33),
    ("win1258", 19),
    ("win866", 20),
    ("win874", 21),
    ("win932", 35),
    ("win936", 37),
    ("win949", 38),
    ("win950", 36),
    ("windows1250", 29),
    ("windows1251", 23),
    ("windows1252", 24),
    ("windows1253", 30),
    ("windows1254", 31),
    ("windows1255", 32),
    ("windows1256", 18),
    ("windows1257", 33),
    ("windows1258", 19),
    ("windows866", 20),
    ("windows874", 21),
    ("windows932", 35),
    ("windows936", 37),
    ("windows949", 38),
    ("windows950", 36),
];

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    if proc.lang == 14 {
        // SQL-language builtins have an empty prosrc: match by OID.
        return match proc.oid {
            13298 => Some(FuncImpl::Scalar(char_max_length_sql)),
            13299 => Some(FuncImpl::Scalar(char_octet_length_sql)),
            13301 => Some(FuncImpl::Scalar(numeric_precision_sql)),
            13302 => Some(FuncImpl::Scalar(numeric_precision_radix_sql)),
            13303 => Some(FuncImpl::Scalar(numeric_scale_sql)),
            13304 => Some(FuncImpl::Scalar(datetime_precision_sql)),
            13305 => Some(FuncImpl::Scalar(interval_type_sql)),
            _ => None,
        };
    }
    match proc.prosrc {
        "pgsql_version" => Some(FuncImpl::Scalar(version_sql)),
        "timeofday" => Some(FuncImpl::Scalar(timeofday_sql)),
        "pg_num_nulls" => Some(FuncImpl::Scalar(num_nulls_sql)),
        "pg_num_nonnulls" => Some(FuncImpl::Scalar(num_nonnulls_sql)),
        "pg_client_encoding" => Some(FuncImpl::Scalar(client_encoding_sql)),
        "getdatabaseencoding" => Some(FuncImpl::Scalar(database_encoding_sql)),
        "format_type" => Some(FuncImpl::Scalar(format_type_sql)),
        "PG_char_to_encoding" => Some(FuncImpl::Scalar(char_to_encoding_sql)),
        "parse_ident" => Some(FuncImpl::Scalar(parse_ident_sql)),
        "pg_column_size" => Some(FuncImpl::Scalar(column_size_sql)),
        "oidvectortypes" => Some(FuncImpl::Scalar(oidvectortypes_sql)),
        "PG_encoding_to_char" => Some(FuncImpl::Scalar(encoding_to_char_sql)),
        "pg_typeof" => Some(FuncImpl::Scalar(pg_typeof_sql)),
        "pg_column_compression" => Some(FuncImpl::Scalar(column_compression_sql)),
        "pg_size_pretty" => Some(FuncImpl::Scalar(size_pretty_int8_sql)),
        "pg_size_pretty_numeric" => Some(FuncImpl::Scalar(size_pretty_numeric_sql)),
        "pg_encoding_max_length_sql" => Some(FuncImpl::Scalar(encoding_max_length_sql)),
        "pg_collation_for" => Some(FuncImpl::Scalar(collation_for_sql)),
        "pg_size_bytes" => Some(FuncImpl::Scalar(size_bytes_sql)),
        "to_regtype" => Some(FuncImpl::Scalar(to_regtype_sql)),
        "to_regtypemod" => Some(FuncImpl::Scalar(to_regtypemod_sql)),
        "unicode_version" => Some(FuncImpl::Scalar(unicode_version_sql)),
        "icu_unicode_version" => Some(FuncImpl::Scalar(icu_unicode_version_sql)),
        "pg_settings_get_flags" => Some(FuncImpl::Scalar(settings_get_flags_sql)),
        "pg_basetype" => Some(FuncImpl::Scalar(basetype_sql)),
        _ => None,
    }
}

/// 0A000 for a System-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented(LEASE, format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument helpers. Strict procs receive non-NULL arguments; the non-strict ones check for NULL.
// ---------------------------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("missing function argument {}", i + 1)))
}

fn oid_at(args: &[Datum], i: usize) -> Result<Oid> {
    match arg(args, i)? {
        Datum::Oid(v) => Ok(*v),
        other => Err(type_mismatch("oid", other)),
    }
}

fn int4_at(args: &[Datum], i: usize) -> Result<i32> {
    match arg(args, i)? {
        Datum::Int4(v) => Ok(*v),
        other => Err(type_mismatch("int4", other)),
    }
}

fn int8_at(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("int8", other)),
    }
}

fn bool_at(args: &[Datum], i: usize) -> Result<bool> {
    match arg(args, i)? {
        Datum::Bool(v) => Ok(*v),
        other => Err(type_mismatch("bool", other)),
    }
}

fn text_at(args: &[Datum], i: usize) -> Result<&str> {
    match arg(args, i)? {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("text", other)),
    }
}

fn numeric_at(args: &[Datum], i: usize) -> Result<&Numeric> {
    match arg(args, i)? {
        Datum::Numeric(n) => Ok(n),
        other => Err(type_mismatch("numeric", other)),
    }
}

fn int4_or_null(v: Option<i32>) -> Datum {
    match v {
        Some(x) => Datum::Int4(x),
        None => Datum::Null,
    }
}

// ---------------------------------------------------------------------------------------------
// version / pg_typeof / format_type / oidvectortypes / pg_basetype
// ---------------------------------------------------------------------------------------------

/// `version()`.
fn version_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!(
        "PostgreSQL 18.6 (PGArbor {}) on {}-{}, compiled by rustc, 64-bit",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        std::env::consts::OS
    )))
}

/// `pg_typeof(any)`: the declared-at-call type of the argument, as regtype.
fn pg_typeof_sql(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Oid(ctx.arg_type(0).oid))
}

/// `format_type(oid, int4)` — not strict: a NULL OID gives NULL, a NULL typmod means no typmod.
fn format_type_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let oid = match arg(args, 0)? {
        Datum::Null => return Ok(Datum::Null),
        Datum::Oid(v) => *v,
        other => return Err(type_mismatch("oid", other)),
    };
    let typmod = match args.get(1) {
        None | Some(Datum::Null) => -1,
        Some(Datum::Int4(t)) => *t,
        Some(other) => return Err(type_mismatch("int4", other)),
    };
    Ok(Datum::Text(names::format_type(ctx.env, oid, typmod)?))
}

/// `oidvectortypes(oidvector)`: comma-separated `format_type` names with no typmod.
fn oidvectortypes_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let items = match arg(args, 0)? {
        Datum::Array(a) => &a.items,
        other => return Err(type_mismatch("oidvector", other)),
    };
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let oid = match item {
            Datum::Oid(v) => *v,
            other => return Err(type_mismatch("oid", other)),
        };
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&names::format_type(ctx.env, oid, -1)?);
    }
    Ok(Datum::Text(out))
}

/// `pg_basetype(regtype)`: follow domains to the bottom base type; NULL for an unknown OID.
fn basetype_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    // A domain cycle cannot exist in a consistent catalog; bound the walk anyway.
    const MAX_DOMAIN_DEPTH: usize = 1024;
    let mut oid = oid_at(args, 0)?;
    for _ in 0..MAX_DOMAIN_DEPTH {
        let Some(info) = ctx.env.type_info(oid) else {
            return Ok(Datum::Null);
        };
        if !info.is_domain() {
            return Ok(Datum::Oid(oid));
        }
        oid = info.basetype;
    }
    Err(PgError::corruption(format!("domain chain of type {oid} is too deep")))
}

// ---------------------------------------------------------------------------------------------
// num_nulls / num_nonnulls / column_size / column_compression / collation_for
// ---------------------------------------------------------------------------------------------

/// `num_nulls(VARIADIC "any")`: the arguments arrive individually (see module docs).
fn num_nulls_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let nulls = args.iter().filter(|a| a.is_null()).count();
    Ok(Datum::Int4(nulls as i32))
}

/// `num_nonnulls(VARIADIC "any")`.
fn num_nonnulls_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let nonnulls = args.iter().filter(|a| !a.is_null()).count();
    Ok(Datum::Int4(nonnulls as i32))
}

/// `pg_column_size(any)`: byte length of the row-tuple encoding of the value (documented divergence).
fn column_size_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let mut buf = Vec::new();
    tuple::encode_value(arg(args, 0)?, &mut buf)?;
    Ok(Datum::Int4(buf.len() as i32))
}

/// `pg_column_compression(any)`: PGArbor never compresses values out of line, so NULL.
fn column_compression_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Null)
}

/// `pg_collation_for(any)`: `"default"` when the argument type is collatable, NULL otherwise.
fn collation_for_sql(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let collatable = ctx.env.type_info(ctx.arg_type(0).oid).is_some_and(|t| t.is_collatable());
    Ok(if collatable { Datum::Text("\"default\"".to_string()) } else { Datum::Null })
}

// ---------------------------------------------------------------------------------------------
// Encodings (PG `mbutils.c`, `encnames.c`, `wchar.c`)
// ---------------------------------------------------------------------------------------------

fn client_encoding_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(ENCODINGS[DB_ENCODING_ID].0.to_string()))
}

fn database_encoding_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(ENCODINGS[DB_ENCODING_ID].0.to_string()))
}

/// `PG_encoding_to_char(int4)`: the canonical name, or "" for an invalid id.
fn encoding_to_char_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let id = int4_at(args, 0)?;
    let name = usize::try_from(id).ok().and_then(|i| ENCODINGS.get(i)).map_or("", |e| e.0);
    Ok(Datum::Text(name.to_string()))
}

/// `pg_encoding_max_length_sql(int4)`: maxmblen for a valid id, NULL otherwise.
fn encoding_max_length_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let id = int4_at(args, 0)?;
    let len = usize::try_from(id).ok().and_then(|i| ENCODINGS.get(i)).map(|e| e.1);
    Ok(int4_or_null(len))
}

/// `PG_char_to_encoding(name)`: id for a known (normalized) encoding name, -1 otherwise.
/// Normalization follows PG `clean_encoding_name`: ASCII alphanumerics only, lower-cased.
fn char_to_encoding_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let name = text_at(args, 0)?;
    if name.is_empty() || name.len() >= 64 {
        return Ok(Datum::Int4(-1));
    }
    let key: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    let id = ENCODING_ALIASES.iter().find(|(alias, _)| *alias == key).map_or(-1, |(_, id)| *id);
    Ok(Datum::Int4(id))
}

// ---------------------------------------------------------------------------------------------
// pg_size_pretty / pg_size_bytes (PG `dbsize.c`)
// ---------------------------------------------------------------------------------------------

struct SizeUnit {
    name: &'static str,
    limit: i64,
    round: bool,
    unitbits: u32,
}

/// `size_pretty_units`: all units are powers of 2; the last unit (PB) is always used when reached.
const SIZE_UNITS: [SizeUnit; 6] = [
    SizeUnit { name: "bytes", limit: 10 * 1024, round: false, unitbits: 0 },
    SizeUnit { name: "kB", limit: 20 * 1024 - 1, round: true, unitbits: 10 },
    SizeUnit { name: "MB", limit: 20 * 1024 - 1, round: true, unitbits: 20 },
    SizeUnit { name: "GB", limit: 20 * 1024 - 1, round: true, unitbits: 30 },
    SizeUnit { name: "TB", limit: 20 * 1024 - 1, round: true, unitbits: 40 },
    SizeUnit { name: "PB", limit: 20 * 1024 - 1, round: true, unitbits: 50 },
];

/// Shift (in bits) between two adjacent units, as PG computes it: one bit less when the larger unit
/// rounds, one bit more when the smaller unit rounds.
fn shift_between(cur: &SizeUnit, next: &SizeUnit) -> u32 {
    let bits = next.unitbits as i32 - cur.unitbits as i32 - next.round as i32 + cur.round as i32;
    bits.max(0) as u32
}

/// PG `half_rounded(x)`: `(x + sign(x)) / 2` with C truncating division.
fn half_rounded_i64(x: i64) -> i64 {
    let adj = if x < 0 { x.wrapping_sub(1) } else { x.wrapping_add(1) };
    adj / 2
}

fn size_pretty_int8_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let mut size = int8_at(args, 0)?;
    let last = SIZE_UNITS.len() - 1;
    for i in 0..last {
        let unit = &SIZE_UNITS[i];
        if size.unsigned_abs() < unit.limit as u64 {
            return Ok(Datum::Text(format_int8_size(size, unit)));
        }
        size /= 1i64 << shift_between(unit, &SIZE_UNITS[i + 1]);
    }
    Ok(Datum::Text(format_int8_size(size, &SIZE_UNITS[last])))
}

fn format_int8_size(size: i64, unit: &SizeUnit) -> String {
    let size = if unit.round { half_rounded_i64(size) } else { size };
    format!("{} {}", size, unit.name)
}

fn half_rounded_numeric(n: &Numeric) -> Result<Numeric> {
    let one = Numeric::from_i64(1);
    let adjusted = if n.pg_cmp(&Numeric::from_i64(0)) != Ordering::Less {
        arith::add(n, &one)?
    } else {
        arith::sub(n, &one)?
    };
    arith::div_trunc(&adjusted, &Numeric::from_i64(2))
}

/// `pg_size_pretty(numeric)`: the same unit ladder, using numeric arithmetic and numeric output.
fn size_pretty_numeric_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let mut size = numeric_at(args, 0)?.clone();
    let last = SIZE_UNITS.len() - 1;
    for i in 0..last {
        let unit = &SIZE_UNITS[i];
        let abs = arith::abs(&size)?;
        if abs.pg_cmp(&Numeric::from_i64(unit.limit)) == Ordering::Less {
            return Ok(Datum::Text(format_numeric_size(size, unit)?));
        }
        let divisor = Numeric::from_i64(1i64 << shift_between(unit, &SIZE_UNITS[i + 1]));
        size = arith::div_trunc(&size, &divisor)?;
    }
    Ok(Datum::Text(format_numeric_size(size, &SIZE_UNITS[last])?))
}

fn format_numeric_size(size: Numeric, unit: &SizeUnit) -> Result<String> {
    let size = if unit.round { half_rounded_numeric(&size)? } else { size };
    Ok(format!("{} {}", numeric_io::format_numeric(&size)?, unit.name))
}

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn invalid_size(arg: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid size: \"{arg}\""))
}

/// PG `numeric_int8`: round half away from zero, then range-check.
fn numeric_to_int8(n: &Numeric) -> Result<i64> {
    match n {
        Numeric::NaN => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot convert NaN to bigint")),
        Numeric::PosInf | Numeric::NegInf => {
            Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot convert infinity to bigint"))
        }
        Numeric::Finite(d) => {
            let rounded = d.with_scale_round(0, RoundingMode::HalfUp);
            rounded
                .to_i64()
                .ok_or_else(|| PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))
        }
    }
}

/// `pg_size_bytes(text)`: parse `<number> [unit]` into bytes, following PG's scanner exactly.
fn size_bytes_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let text = text_at(args, 0)?;
    let bytes = text.as_bytes();
    let n = bytes.len();

    // Part 0: leading whitespace.
    let mut start = 0;
    while start < n && is_c_space(bytes[start]) {
        start += 1;
    }

    // Parts 1-3: sign, digits, optional '.' and fraction. The '.' is consumed even without digits.
    let mut end = start;
    if end < n && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    let mut have_digits = false;
    if end < n && bytes[end].is_ascii_digit() {
        have_digits = true;
        while end < n && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if end < n && bytes[end] == b'.' {
        end += 1;
        if end < n && bytes[end].is_ascii_digit() {
            have_digits = true;
            while end < n && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
    }
    if !have_digits {
        return Err(invalid_size(text));
    }

    // Part 4: optional exponent, consumed only when strtol(3) would convert digits after it.
    if end < n && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut q = end + 1;
        while q < n && is_c_space(bytes[q]) {
            q += 1;
        }
        if q < n && (bytes[q] == b'-' || bytes[q] == b'+') {
            q += 1;
        }
        let digits_start = q;
        while q < n && bytes[q].is_ascii_digit() {
            q += 1;
        }
        if q > digits_start {
            end = q;
        }
    }

    // The number text ends on an ASCII byte boundary, so these slices are on char boundaries.
    let mut num = numeric_io::parse_numeric(&text[start..end])?;

    // Optional unit, after whitespace.
    let mut rest = end;
    while rest < n && is_c_space(bytes[rest]) {
        rest += 1;
    }
    if rest < n {
        let unit_text = text[rest..].trim_end_matches(|c: char| c.is_ascii() && is_c_space(c as u8));
        let idx = SIZE_UNITS
            .iter()
            .position(|u| u.name.eq_ignore_ascii_case(unit_text))
            .or_else(|| unit_text.eq_ignore_ascii_case("B").then_some(0));
        let Some(idx) = idx else {
            return Err(invalid_size(text)
                .with_detail(format!("Invalid size unit: \"{unit_text}\"."))
                .with_hint("Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\"."));
        };
        let bits = SIZE_UNITS[idx].unitbits;
        if bits > 0 {
            num = arith::mul(&Numeric::from_i64(1i64 << bits), &num)?;
        }
    }

    Ok(Datum::Int8(numeric_to_int8(&num)?))
}

// ---------------------------------------------------------------------------------------------
// to_regtype / to_regtypemod
// ---------------------------------------------------------------------------------------------

/// `to_regtype(text)`: NULL when the type does not exist (42704), other errors propagate.
fn to_regtype_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let text = text_at(args, 0)?;
    match names::parse_type_string(ctx.env, text) {
        Ok(tid) => Ok(Datum::Oid(tid.oid)),
        Err(e) if e.sqlstate == SqlState::UNDEFINED_OBJECT => Ok(Datum::Null),
        Err(e) => Err(e),
    }
}

/// `to_regtypemod(text)`: the typmod of the parsed type name, NULL when the type does not exist.
fn to_regtypemod_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let text = text_at(args, 0)?;
    match names::parse_type_string(ctx.env, text) {
        Ok(tid) => Ok(Datum::Int4(tid.typmod)),
        Err(e) if e.sqlstate == SqlState::UNDEFINED_OBJECT => Ok(Datum::Null),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// timeofday
// ---------------------------------------------------------------------------------------------

/// Days since 1970-01-01 → (year, month 1..=12, day 1..=31). Civil-calendar conversion (proleptic
/// Gregorian), written from the standard days-to-date algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const USECS_PER_DAY: i64 = 86_400_000_000;
/// Days from 1970-01-01 to 2000-01-01.
const DAYS_1970_TO_2000: i64 = 10_957;

/// `timeofday()`: `Thu Feb 29 13:01:02.500000 2024 UTC` in the session time zone.
fn timeofday_sql(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    let (offset_secs, zone) = match &ctx.io.time_zone {
        TimeZoneSetting::Utc => (0i64, "UTC".to_string()),
        TimeZoneSetting::Fixed { name, secs_east } => (i64::from(*secs_east), name.clone()),
        TimeZoneSetting::Named(name) => {
            return Err(PgError::unimplemented(LEASE, format!("timeofday in named time zone {name}")));
        }
    };
    // Timestamps are µs since 2000-01-01 (UTC); shift to local wall-clock time.
    let local = ctx.session.clock_timestamp().saturating_add(offset_secs.saturating_mul(1_000_000));
    let days_2000 = local.div_euclid(USECS_PER_DAY);
    let rem = local.rem_euclid(USECS_PER_DAY);
    let days_1970 = days_2000 + DAYS_1970_TO_2000;
    let (year, month, day) = civil_from_days(days_1970);
    // 1970-01-01 was a Thursday (index 4 with Sunday = 0).
    let wday = (days_1970 + 4).rem_euclid(7) as usize;
    let hh = rem / 3_600_000_000;
    let mi = (rem / 60_000_000) % 60;
    let ss = (rem / 1_000_000) % 60;
    let us = rem % 1_000_000;
    let dow = DAYS.get(wday).copied().unwrap_or("???");
    let mon = MONTHS.get((month - 1) as usize).copied().unwrap_or("???");
    Ok(Datum::Text(format!(
        "{dow} {mon} {day:02} {hh:02}:{mi:02}:{ss:02}.{us:06} {year} {zone}"
    )))
}

// ---------------------------------------------------------------------------------------------
// parse_ident (PG `utils/adt/misc.c`)
// ---------------------------------------------------------------------------------------------

fn scanner_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

fn ident_error(text: &str, detail: Option<&str>) -> PgError {
    let e = PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("string is not a valid identifier: \"{text}\""),
    );
    match detail {
        Some(d) => e.with_detail(d),
        None => e,
    }
}

/// Split a possibly qualified identifier into its parts (`"Foo".bar` → `["Foo", "bar"]`).
fn parse_ident_parts(text: &str, strict: bool) -> Result<Vec<String>> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut pos = 0;
    let mut parts: Vec<String> = Vec::new();
    let mut after_dot = false;

    while pos < n && scanner_isspace(bytes[pos]) {
        pos += 1;
    }

    loop {
        let mut missing_ident = true;
        if pos < n && bytes[pos] == b'"' {
            // Quoted identifier; a doubled quote stands for one quote character.
            let mut out: Vec<u8> = Vec::new();
            let mut p = pos + 1;
            loop {
                if p >= n {
                    return Err(ident_error(text, Some("String has unclosed double quotes.")));
                }
                if bytes[p] == b'"' {
                    if p + 1 < n && bytes[p + 1] == b'"' {
                        out.push(b'"');
                        p += 2;
                        continue;
                    }
                    break;
                }
                out.push(bytes[p]);
                p += 1;
            }
            pos = p + 1;
            if out.is_empty() {
                return Err(ident_error(text, Some("Quoted identifier must not be empty.")));
            }
            parts.push(String::from_utf8(out).map_err(|_| PgError::internal("identifier is not UTF-8"))?);
            missing_ident = false;
        } else if pos < n && is_ident_start(bytes[pos]) {
            // Unquoted identifier: folded to lower case (ASCII only; multibyte characters unchanged).
            let start = pos;
            pos += 1;
            while pos < n && is_ident_cont(bytes[pos]) {
                pos += 1;
            }
            let mut part = bytes[start..pos].to_vec();
            part.make_ascii_lowercase();
            parts.push(String::from_utf8(part).map_err(|_| PgError::internal("identifier is not UTF-8"))?);
            missing_ident = false;
        }

        if missing_ident {
            if pos < n && bytes[pos] == b'.' {
                return Err(ident_error(text, Some("No valid identifier before \".\".")));
            } else if after_dot {
                return Err(ident_error(text, Some("No valid identifier after \".\".")));
            }
            return Err(ident_error(text, None));
        }

        while pos < n && scanner_isspace(bytes[pos]) {
            pos += 1;
        }
        if pos < n && bytes[pos] == b'.' {
            after_dot = true;
            pos += 1;
            while pos < n && scanner_isspace(bytes[pos]) {
                pos += 1;
            }
        } else if pos >= n {
            break;
        } else {
            if strict {
                return Err(ident_error(text, None));
            }
            break;
        }
    }
    Ok(parts)
}

/// `parse_ident(text, strict bool)` → `text[]`.
fn parse_ident_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let parts = parse_ident_parts(text_at(args, 0)?, bool_at(args, 1)?)?;
    let items = parts.into_iter().map(Datum::Text).collect();
    Ok(Datum::array(Array::from_vec(oids::TEXT, items)))
}

// ---------------------------------------------------------------------------------------------
// Unicode / settings
// ---------------------------------------------------------------------------------------------

fn unicode_version_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text("16.0".to_string()))
}

/// PGArbor is not built against ICU, so PG's `icu_unicode_version()` is NULL.
fn icu_unicode_version_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Null)
}

/// `pg_settings_get_flags(text)`: PGArbor exposes no GUC flag table, so NULL (the documented answer).
fn settings_get_flags_sql(_args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Null)
}

// ---------------------------------------------------------------------------------------------
// information_schema helpers (`information_schema.sql`). Pure functions of (typid, typmod).
// Callers pass the true (domain-resolved) type already.
// ---------------------------------------------------------------------------------------------

/// `_pg_char_max_length(typid, typmod)`.
fn char_max_length_of(typid: Oid, typmod: i32) -> Option<i32> {
    if typmod == -1 {
        return None;
    }
    match typid {
        oids::BPCHAR | oids::VARCHAR => Some(typmod - 4),
        1560 | 1562 => Some(typmod), // bit, varbit
        _ => None,
    }
}

fn char_max_length_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    Ok(int4_or_null(char_max_length_of(typid, typmod)))
}

fn char_octet_length_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    if !matches!(typid, 25 | oids::BPCHAR | oids::VARCHAR) {
        return Ok(Datum::Null);
    }
    if typmod == -1 {
        return Ok(Datum::Int4(1 << 30));
    }
    match char_max_length_of(typid, typmod) {
        None => Ok(Datum::Null),
        Some(max_chars) => match max_chars.checked_mul(DB_MAX_MBLEN) {
            Some(v) => Ok(Datum::Int4(v)),
            None => Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")),
        },
    }
}

/// `pg_encoding_max_length(database encoding)`.
const DB_MAX_MBLEN: i32 = ENCODINGS[DB_ENCODING_ID].1;

fn numeric_precision_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    let v = match typid {
        21 => Some(16),
        23 => Some(32),
        20 => Some(64),
        1700 => {
            if typmod == -1 {
                None
            } else {
                Some(((typmod - 4) >> 16) & 0xFFFF)
            }
        }
        700 => Some(24),
        701 => Some(53),
        _ => None,
    };
    Ok(int4_or_null(v))
}

fn numeric_precision_radix_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let typid = oid_at(args, 0)?;
    let v = match typid {
        21 | 23 | 20 | 700 | 701 => Some(2),
        1700 => Some(10),
        _ => None,
    };
    Ok(int4_or_null(v))
}

fn numeric_scale_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    let v = match typid {
        21 | 23 | 20 => Some(0),
        1700 => {
            if typmod == -1 {
                None
            } else {
                Some((typmod - 4) & 0xFFFF)
            }
        }
        _ => None,
    };
    Ok(int4_or_null(v))
}

fn datetime_precision_sql(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    let v = match typid {
        1082 => Some(0), // date
        1083 | 1114 | 1184 | 1266 => {
            // time, timestamp, timestamptz, timetz
            Some(if typmod < 0 { 6 } else { typmod })
        }
        1186 => {
            // interval
            if typmod < 0 || (typmod & 0xFFFF) == 0xFFFF {
                Some(6)
            } else {
                Some(typmod & 0xFFFF)
            }
        }
        _ => None,
    };
    Ok(int4_or_null(v))
}

/// `_pg_interval_type(typid, mod)`: the upper-cased field part of `format_type`, e.g. `YEAR`
/// for `interval year`. Matches `interval[()0-9]* (.*)` on the format_type text; NULL otherwise.
fn interval_type_sql(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (typid, typmod) = (oid_at(args, 0)?, int4_at(args, 1)?);
    if typid != 1186 {
        return Ok(Datum::Null);
    }
    let text = names::format_type(ctx.env, typid, typmod)?;
    let Some(rest) = text.strip_prefix("interval") else {
        return Ok(Datum::Null);
    };
    let rest = rest.trim_start_matches(|c: char| c == '(' || c == ')' || c.is_ascii_digit());
    match rest.strip_prefix(' ') {
        Some(fields) => Ok(Datum::Text(fields.to_uppercase())),
        None => Ok(Datum::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::func::ScalarFn;
    use crate::registry::builtin_env;

    /// Run `f` with a call context over the builtin catalog, pinned GUCs and a fixed clock (0).
    fn with_ctx<R>(tys: &[TypeId], f: impl FnOnce(&FuncCtx<'_>) -> R) -> R {
        let io = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &io,
            session: &sess,
            arg_types: tys,
            result_type: TypeId::TEXT,
            collation: 0,
        };
        f(&ctx)
    }

    fn text(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    #[test]
    fn pg_typeof_reports_argument_type() {
        let r = with_ctx(&[TypeId::INT4], |c| pg_typeof_sql(&[Datum::Int4(1)], c));
        assert_eq!(r.expect("ok"), Datum::Oid(23));
    }

    #[test]
    fn format_type_acceptance() {
        let r = with_ctx(&[], |c| format_type_sql(&[Datum::Oid(1043), Datum::Int4(14)], c));
        assert_eq!(r.expect("ok"), text("character varying(10)"));
        let r = with_ctx(&[], |c| format_type_sql(&[Datum::Null, Datum::Int4(14)], c));
        assert_eq!(r.expect("ok"), Datum::Null);
    }

    #[test]
    fn version_prefix() {
        let r = with_ctx(&[], |c| version_sql(&[], c)).expect("ok");
        let s = r.as_str().unwrap_or_default();
        assert!(s.starts_with("PostgreSQL 18.6 (PGArbor"), "{s}");
    }

    #[test]
    fn size_pretty_int8_matches_pg() {
        let pretty = |v: i64| with_ctx(&[], |c| size_pretty_int8_sql(&[Datum::Int8(v)], c)).expect("ok");
        assert_eq!(pretty(10240), text("10 kB"));
        assert_eq!(pretty(1024), text("1024 bytes"));
        assert_eq!(pretty(-10240), text("-10 kB"));
        assert_eq!(pretty(1_048_576), text("1024 kB"));
        assert_eq!(pretty(20 * 1024 * 1024), text("20 MB"));
    }

    #[test]
    fn size_pretty_numeric_keeps_display_scale() {
        let n = numeric_io::parse_numeric("1.50").expect("numeric");
        let r = with_ctx(&[], |c| size_pretty_numeric_sql(&[Datum::Numeric(n)], c));
        assert_eq!(r.expect("ok"), text("1.50 bytes"));
    }

    #[test]
    fn size_bytes_parses_units() {
        let parse = |s: &str| with_ctx(&[], |c| size_bytes_sql(&[text(s)], c));
        assert_eq!(parse("1 MB").expect("ok"), Datum::Int8(1_048_576));
        assert_eq!(parse("  2kB ").expect("ok"), Datum::Int8(2048));
        assert_eq!(parse("3 B").expect("ok"), Datum::Int8(3));
        assert_eq!(parse("1.5e3").expect("ok"), Datum::Int8(1500));
        let e = parse("x").expect_err("invalid");
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "invalid size: \"x\"");
        let e = parse("1 furlong").expect_err("bad unit");
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn encodings() {
        assert_eq!(with_ctx(&[], |c| database_encoding_sql(&[], c)).expect("ok"), text("UTF8"));
        assert_eq!(with_ctx(&[], |c| client_encoding_sql(&[], c)).expect("ok"), text("UTF8"));
        assert_eq!(
            with_ctx(&[], |c| encoding_to_char_sql(&[Datum::Int4(6)], c)).expect("ok"),
            text("UTF8")
        );
        assert_eq!(
            with_ctx(&[], |c| char_to_encoding_sql(&[text("UTF-8")], c)).expect("ok"),
            Datum::Int4(6)
        );
        assert_eq!(
            with_ctx(&[], |c| char_to_encoding_sql(&[text("nosuch")], c)).expect("ok"),
            Datum::Int4(-1)
        );
        assert_eq!(
            with_ctx(&[], |c| encoding_max_length_sql(&[Datum::Int4(6)], c)).expect("ok"),
            Datum::Int4(4)
        );
        assert_eq!(
            with_ctx(&[], |c| encoding_max_length_sql(&[Datum::Int4(99)], c)).expect("ok"),
            Datum::Null
        );
    }

    #[test]
    fn information_schema_helpers() {
        let call = |f: ScalarFn, a: &[Datum]| with_ctx(&[], |c| f(a, c)).expect("ok");
        assert_eq!(call(char_max_length_sql, &[Datum::Oid(1043), Datum::Int4(14)]), Datum::Int4(10));
        assert_eq!(call(char_octet_length_sql, &[Datum::Oid(1043), Datum::Int4(14)]), Datum::Int4(40));
        assert_eq!(call(numeric_precision_sql, &[Datum::Oid(23), Datum::Int4(-1)]), Datum::Int4(32));
        assert_eq!(call(numeric_precision_radix_sql, &[Datum::Oid(1700), Datum::Int4(-1)]), Datum::Int4(10));
        assert_eq!(
            call(numeric_precision_sql, &[Datum::Oid(1700), Datum::Int4(((10 << 16) | 2) + 4)]),
            Datum::Int4(10)
        );
        assert_eq!(call(numeric_scale_sql, &[Datum::Oid(1700), Datum::Int4(((10 << 16) | 2) + 4)]), Datum::Int4(2));
        assert_eq!(call(datetime_precision_sql, &[Datum::Oid(1082), Datum::Int4(-1)]), Datum::Int4(0));
        assert_eq!(call(datetime_precision_sql, &[Datum::Oid(1184), Datum::Int4(-1)]), Datum::Int4(6));
        assert_eq!(call(char_max_length_sql, &[Datum::Oid(1043), Datum::Int4(-1)]), Datum::Null);
    }

    #[test]
    fn to_regtype_and_typmod() {
        assert_eq!(with_ctx(&[], |c| to_regtype_sql(&[text("nosuch")], c)).expect("ok"), Datum::Null);
        assert_eq!(with_ctx(&[], |c| to_regtype_sql(&[text("integer")], c)).expect("ok"), Datum::Oid(23));
        assert_eq!(
            with_ctx(&[], |c| to_regtypemod_sql(&[text("varchar(10)")], c)).expect("ok"),
            Datum::Int4(14)
        );
        assert_eq!(with_ctx(&[], |c| to_regtypemod_sql(&[text("nosuch")], c)).expect("ok"), Datum::Null);
    }

    #[test]
    fn timeofday_format() {
        assert_eq!(
            with_ctx(&[], |c| timeofday_sql(&[], c)).expect("ok"),
            text("Sat Jan 01 00:00:00.000000 2000 UTC")
        );
    }

    #[test]
    fn parse_ident_cases() {
        assert_eq!(parse_ident_parts("\"Foo\".Bar ", false).expect("ok"), vec!["Foo".to_string(), "bar".to_string()]);
        assert_eq!(parse_ident_parts("\"a\"\"b\"", true).expect("ok"), vec!["a\"b".to_string()]);
        assert_eq!(parse_ident_parts("abc junk", false).expect("ok"), vec!["abc".to_string()]);
        let e = parse_ident_parts("abc junk", true).expect_err("strict");
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "string is not a valid identifier: \"abc junk\"");
        assert!(parse_ident_parts("", false).is_err());
        let e = parse_ident_parts("\"open", false).expect_err("unclosed");
        assert_eq!(e.detail.as_deref(), Some("String has unclosed double quotes."));
    }

    #[test]
    fn unicode_and_settings() {
        assert_eq!(with_ctx(&[], |c| unicode_version_sql(&[], c)).expect("ok"), text("16.0"));
        assert_eq!(with_ctx(&[], |c| icu_unicode_version_sql(&[], c)).expect("ok"), Datum::Null);
    }

    #[test]
    fn num_nulls_counts_arguments() {
        let args = [Datum::Int4(1), Datum::Null, Datum::Null];
        assert_eq!(with_ctx(&[], |c| num_nulls_sql(&args, c)).expect("ok"), Datum::Int4(2));
        assert_eq!(with_ctx(&[], |c| num_nonnulls_sql(&args, c)).expect("ok"), Datum::Int4(1));
    }

    #[test]
    fn oidvectortypes_joins_names() {
        let arr = Array::from_vec(oids::OID, vec![Datum::Oid(23), Datum::Oid(1043)]);
        let r = with_ctx(&[], |c| oidvectortypes_sql(&[Datum::array(arr)], c));
        assert_eq!(r.expect("ok"), text("integer, character varying"));
    }
}
