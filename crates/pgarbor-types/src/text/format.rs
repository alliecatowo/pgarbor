//! format(), quoting, concat and radix conversions (TYPE-13). Lease TYP-15.
//!
//! PG18 reference: `utils/adt/varlena.c` (`text_format`, `text_concat`, `text_concat_ws`,
//! `to_hex32/64`, `to_oct32/64`, `to_bin32/64`), `quote.c`, `ruleutils.c` (`quote_identifier`),
//! keyword list: `contracts/reference/pg18/pg_keywords.csv`.
//! Owned procs (ImplModule::TextFormat): `text_format`, `text_format_nv` (format with only the format
//! string), `quote_ident`, `quote_literal`, `quote_nullable`, SQL-language `quote_literal(anyelement)`
//! (oid 1285) and `quote_nullable(anyelement)` (oid 1290) (convert with `crate::io::output` of
//! `ctx.arg_types[0]` first), `text_concat` (`concat(VARIADIC "any")`), `text_concat_ws`,
//! `to_hex32 to_hex64 to_oct32 to_oct64 to_bin32 to_bin64` (two's complement of the unsigned
//! reinterpretation: `to_hex(-1)` = `ffffffff`, lowercase).
//!
//! `concat`/`concat_ws`/`format` are `VARIADIC "any"`: they receive the individual argument Datums and
//! their types in `ctx.arg_types`; non-text values are rendered with `crate::io::output`; NULLs are
//! skipped by concat/concat_ws (concat_ws with NULL separator → NULL; it is not strict).
//! `concat('a', NULL, 1)` = `a1`; `concat_ws(',', 'a', NULL, 'b')` = `a,b`.
//!
//! `format(fmt, ...)`: specifiers `%[argpos$][flags][width]type` with types `s` (value as text, NULL →
//! empty), `I` (`quote_ident`; NULL → 22004 `null values cannot be formatted as an SQL identifier`),
//! `L` (`quote_literal`, NULL → `NULL` unquoted), `%%` literal. Flag `-` left-justifies; width may be
//! `*` / `*n$`. Positional and sequential args may mix: after `%2$s` the next plain `%s` uses arg 3.
//! Errors (22023): `too few arguments for format()`, `unrecognized format() type specifier "z"` with
//! HINT `For a single "%" use "%%".`, `unterminated format() type specifier`, and
//! `format specifies argument 0, but arguments are numbered from 1`.
//! Oracle: `format('%2$s %1$s %s', 'a', 'b')` = `b a b`.
//!
//! `quote_ident(s)`: returned unquoted only if s matches `[a-z_][a-z0-9_$]*` AND is not a keyword
//! whose category (pg_keywords.csv `catcode`) is anything but `U` (unreserved); otherwise wrapped in `"`
//! with embedded `"` doubled. `quote_ident('select')` = `"select"`, `quote_ident('Abc')` = `"Abc"`.
//! `quote_literal(s)`: `'` + s with `'` doubled + `'`; if s contains a backslash, backslashes are doubled
//! and the result is prefixed with `E` (`quote_literal(E'a''b\\c')` = `E'a''b\\\\c'`).
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, IoCtx};
use crate::datum::{Datum, TypeId, type_mismatch};
use crate::func::{FuncImpl, ScalarFn};
use crate::io;
use pgarbor_common::{PgError, Result, SqlState};
use std::borrow::Cow;
use std::num::IntErrorKind;

/// Keywords whose `catcode` is not `U` (unreserved), sorted in byte order, generated from
/// `contracts/reference/pg18/pg_keywords.csv`. A safe-looking identifier found here must be quoted.
const QUOTED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

const PERCENT_HINT: &str = "For a single \"%\" use \"%%\".";

/// PG `quote_identifier` (also used by reg* output and catalog DDL rendering downstream).
// CONTRACT
pub fn quote_identifier(ident: &str) -> Result<String> {
    // ruleutils.c: safe = [a-z_] first, then [a-z0-9_] only. Note that `$` is NOT in the safe set
    // in PG 18 (the module doc above is looser); PG's source is followed here.
    let bytes = ident.as_bytes();
    let first = bytes.first().copied();
    let mut safe = matches!(first, Some(b'a'..=b'z') | Some(b'_'))
        && bytes.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_');
    if safe && QUOTED_KEYWORDS.binary_search(&ident).is_ok() {
        safe = false;
    }
    if safe {
        return Ok(ident.to_string());
    }
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

/// PG `quote_literal` text form.
// CONTRACT
pub fn quote_literal(s: &str) -> Result<String> {
    let has_backslash = s.contains('\\');
    let mut out = String::with_capacity(s.len() + 3);
    if has_backslash {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    // The two SQL-language builtins have an empty prosrc, so they are matched by OID.
    let f: ScalarFn = match proc.oid {
        1285 => quote_literal_any,
        1290 => quote_nullable_any,
        _ => match proc.prosrc {
            "quote_ident" => quote_ident_fn,
            "quote_literal" => quote_literal_fn,
            "quote_nullable" => quote_nullable_fn,
            "to_hex32" => to_hex32,
            "to_hex64" => to_hex64,
            "to_oct32" => to_oct32,
            "to_oct64" => to_oct64,
            "to_bin32" => to_bin32,
            "to_bin64" => to_bin64,
            "text_concat" => concat_fn,
            "text_concat_ws" => concat_ws_fn,
            "text_format" | "text_format_nv" => format_fn,
            _ => return None,
        },
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a TextFormat-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-15", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------------

fn first(args: &[Datum]) -> Result<&Datum> {
    args.first().ok_or_else(|| PgError::internal("missing function argument"))
}

fn text_of(d: &Datum) -> Result<&str> {
    d.as_str().ok_or_else(|| type_mismatch("text", d))
}

fn int4_arg(args: &[Datum]) -> Result<i32> {
    match first(args)? {
        Datum::Int4(v) => Ok(*v),
        d => Err(type_mismatch("int4", d)),
    }
}

fn int8_arg(args: &[Datum]) -> Result<i64> {
    match first(args)? {
        Datum::Int8(v) => Ok(*v),
        d => Err(type_mismatch("int8", d)),
    }
}

/// Text rendering of one argument of declared/actual type `ty`. Text values are borrowed; any other
/// non-NULL value goes through its type's output function. NULL must be handled by the caller.
fn value_str<'a>(ioc: &IoCtx, ty: TypeId, d: &'a Datum) -> Result<Cow<'a, str>> {
    match d {
        Datum::Text(s) => Ok(Cow::Borrowed(s.as_str())),
        other => io::output(ioc, ty, other).map(Cow::Owned),
    }
}

// ---------------------------------------------------------------------------------------------
// quote_ident / quote_literal / quote_nullable
// ---------------------------------------------------------------------------------------------

fn quote_ident_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(quote_identifier(text_of(first(args)?)?)?))
}

fn quote_literal_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(quote_literal(text_of(first(args)?)?)?))
}

/// `quote_nullable(text)`: NULL → unquoted `NULL`. Not strict.
fn quote_nullable_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    match args.first() {
        None | Some(Datum::Null) => Ok(Datum::Text("NULL".to_string())),
        Some(d) => Ok(Datum::Text(quote_literal(text_of(d)?)?)),
    }
}

/// `quote_literal(anyelement)` (oid 1285, strict): the value is rendered as text first.
fn quote_literal_any(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let d = first(args)?;
    if d.is_null() {
        return Ok(Datum::Null);
    }
    let s = value_str(&ctx.io_ctx(), ctx.arg_type(0), d)?;
    Ok(Datum::Text(quote_literal(&s)?))
}

/// `quote_nullable(anyelement)` (oid 1290, not strict): NULL → `NULL`.
fn quote_nullable_any(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let d = first(args)?;
    if d.is_null() {
        return Ok(Datum::Text("NULL".to_string()));
    }
    let s = value_str(&ctx.io_ctx(), ctx.arg_type(0), d)?;
    Ok(Datum::Text(quote_literal(&s)?))
}

// ---------------------------------------------------------------------------------------------
// concat / concat_ws
// ---------------------------------------------------------------------------------------------

/// `concat(VARIADIC "any")`: NULL arguments are skipped; the result is never NULL.
fn concat_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ioc = ctx.io_ctx();
    let mut out = String::new();
    for (i, d) in args.iter().enumerate() {
        if d.is_null() {
            continue;
        }
        out.push_str(&value_str(&ioc, ctx.arg_type(i), d)?);
    }
    Ok(Datum::Text(out))
}

/// `concat_ws(sep text, VARIADIC "any")`: NULL separator → NULL; NULL values are skipped entirely
/// (no separator is emitted for them).
fn concat_ws_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (sep_arg, rest) = match args.split_first() {
        Some(parts) => parts,
        None => return Ok(Datum::Null),
    };
    let ioc = ctx.io_ctx();
    let sep = match sep_arg {
        Datum::Null => return Ok(Datum::Null),
        d => value_str(&ioc, ctx.arg_type(0), d)?,
    };
    let mut out = String::new();
    let mut first_value = true;
    for (k, d) in rest.iter().enumerate() {
        if d.is_null() {
            continue;
        }
        if first_value {
            first_value = false;
        } else {
            out.push_str(&sep);
        }
        out.push_str(&value_str(&ioc, ctx.arg_type(k + 1), d)?);
    }
    Ok(Datum::Text(out))
}

// ---------------------------------------------------------------------------------------------
// to_hex / to_oct / to_bin (two's complement of the unsigned reinterpretation, no leading zeros)
// ---------------------------------------------------------------------------------------------

fn to_hex32(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:x}", int4_arg(args)? as u32)))
}

fn to_hex64(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:x}", int8_arg(args)? as u64)))
}

fn to_oct32(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:o}", int4_arg(args)? as u32)))
}

fn to_oct64(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:o}", int8_arg(args)? as u64)))
}

fn to_bin32(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:b}", int4_arg(args)? as u32)))
}

fn to_bin64(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Text(format!("{:b}", int8_arg(args)? as u64)))
}

// ---------------------------------------------------------------------------------------------
// format(): port of varlena.c text_format (PG 18)
// ---------------------------------------------------------------------------------------------

fn unterminated() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "unterminated format() type specifier").with_hint(PERCENT_HINT)
}

fn too_few_arguments() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "too few arguments for format()")
}

fn argument_zero() -> PgError {
    PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "format specifies argument 0, but arguments are numbered from 1",
    )
}

/// Step past the byte at `*i`; the format string must not end there (PG ADVANCE_PARSE_POINTER).
fn advance(b: &[u8], i: &mut usize) -> Result<()> {
    *i += 1;
    if *i >= b.len() { Err(unterminated()) } else { Ok(()) }
}

/// Byte at `i`, or 0 past the end (callers keep the invariant that `i` is in range).
fn peek(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// Parse contiguous decimal digits at `*i` (PG text_format_parse_digits).
fn parse_digits(b: &[u8], i: &mut usize) -> Result<Option<i32>> {
    let mut val: i32 = 0;
    let mut found = false;
    while *i < b.len() && b[*i].is_ascii_digit() {
        let digit = i32::from(b[*i] - b'0');
        val = val
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(|| PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "number is out of range"))?;
        advance(b, i)?;
        found = true;
    }
    Ok(if found { Some(val) } else { None })
}

/// A parsed `[argpos$][flags][width]` prefix (the type character is not consumed here).
#[derive(Default)]
struct Spec {
    /// Explicit argument position (`%2$s`).
    argpos: Option<usize>,
    /// Indirect width position: `Some(0)` = next argument (`*`), `Some(n)` = `*n$`, `None` = direct.
    width_arg: Option<usize>,
    /// `-` flag: left-justify.
    minus: bool,
    /// Direct width (`%10s`), 0 when omitted or indirect.
    width: i32,
}

fn parse_spec(b: &[u8], i: &mut usize) -> Result<Spec> {
    let mut spec = Spec::default();
    if let Some(n) = parse_digits(b, i)? {
        if peek(b, *i) != b'$' {
            // Only a width, followed by the type character.
            spec.width = n;
            return Ok(spec);
        }
        if n == 0 {
            return Err(argument_zero());
        }
        spec.argpos = Some(n as usize);
        advance(b, i)?;
    }
    while peek(b, *i) == b'-' {
        spec.minus = true;
        advance(b, i)?;
    }
    if peek(b, *i) == b'*' {
        advance(b, i)?;
        match parse_digits(b, i)? {
            Some(n) => {
                if peek(b, *i) != b'$' {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "width argument position must be ended by \"$\"",
                    ));
                }
                if n == 0 {
                    return Err(argument_zero());
                }
                spec.width_arg = Some(n as usize);
                advance(b, i)?;
            }
            None => spec.width_arg = Some(0),
        }
    } else if let Some(n) = parse_digits(b, i)? {
        spec.width = n;
    }
    Ok(spec)
}

/// Integer value of a non-int4/int2 width argument, via its text form (PG pg_strtoint32).
fn parse_width_text(s: &str) -> Result<i32> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    t.parse::<i32>().map_err(|e| match e.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => PgError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{s}\" is out of range for type integer"),
        ),
        _ => PgError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type integer: \"{s}\""),
        ),
    })
}

/// Append `s` padded to `width` characters (PG text_format_append_string). A negative width
/// implies left justification.
fn push_padded(out: &mut String, s: &str, minus: bool, width: i32) -> Result<()> {
    if width == 0 {
        out.push_str(s);
        return Ok(());
    }
    let mut left = minus;
    let w: usize = if width < 0 {
        if width == i32::MIN {
            return Err(PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "number is out of range"));
        }
        left = true;
        width.unsigned_abs() as usize
    } else {
        width as usize
    };
    let pad = w.saturating_sub(s.chars().count());
    if left {
        out.push_str(s);
        for _ in 0..pad {
            out.push(' ');
        }
    } else {
        for _ in 0..pad {
            out.push(' ');
        }
        out.push_str(s);
    }
    Ok(())
}

/// `format(text, VARIADIC "any")` and `format(text)`. A NULL format string yields NULL.
fn format_fn(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let fmt_arg = match args.first() {
        None | Some(Datum::Null) => return Ok(Datum::Null),
        Some(d) => d,
    };
    let fmt = text_of(fmt_arg)?;
    let ioc = ctx.io_ctx();
    let b = fmt.as_bytes();
    let mut out = String::with_capacity(fmt.len() + 16);
    // Next argument position to print (PG `arg`); position 0 is the format string itself.
    let mut arg: usize = 1;
    let mut i: usize = 0;
    while i < b.len() {
        if b[i] != b'%' {
            let end = b[i..].iter().position(|&c| c == b'%').map_or(b.len(), |p| i + p);
            let chunk = fmt.get(i..end).ok_or_else(|| PgError::internal("format() scan out of bounds"))?;
            out.push_str(chunk);
            i = end;
            continue;
        }
        advance(b, &mut i)?;
        if peek(b, i) == b'%' {
            out.push('%');
            i += 1;
            continue;
        }
        let spec = parse_spec(b, &mut i)?;
        let conv = peek(b, i);
        if !matches!(conv, b's' | b'I' | b'L') {
            let ch = fmt.get(i..).and_then(|s| s.chars().next()).unwrap_or('?');
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized format() type specifier \"{ch}\""),
            )
            .with_hint(PERCENT_HINT));
        }

        // Indirect width, taken before the value.
        let mut width = spec.width;
        if let Some(wp) = spec.width_arg {
            if wp > 0 {
                arg = wp;
            }
            if arg >= args.len() {
                return Err(too_few_arguments());
            }
            let d = &args[arg];
            width = match d {
                Datum::Null => 0,
                Datum::Int4(v) => *v,
                Datum::Int2(v) => i32::from(*v),
                other => parse_width_text(&value_str(&ioc, ctx.arg_type(arg), other)?)?,
            };
            arg += 1;
        }

        // The value.
        if let Some(p) = spec.argpos {
            arg = p;
        }
        if arg >= args.len() {
            return Err(too_few_arguments());
        }
        let d = &args[arg];
        let ty = ctx.arg_type(arg);
        arg += 1;
        i += 1; // the conversion character is ASCII

        let text: Cow<str> = if d.is_null() {
            match conv {
                b'I' => {
                    return Err(PgError::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        "null values cannot be formatted as an SQL identifier",
                    ));
                }
                b'L' => Cow::Borrowed("NULL"),
                _ => Cow::Borrowed(""),
            }
        } else {
            let s = value_str(&ioc, ty, d)?;
            match conv {
                b'I' => Cow::Owned(quote_identifier(&s)?),
                b'L' => Cow::Owned(quote_literal(&s)?),
                _ => s,
            }
        };
        push_padded(&mut out, &text, spec.minus, width)?;
    }
    Ok(Datum::Text(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry;
    use pgarbor_common::Oid;

    fn run(oid: Oid, args: &[Datum], types: &[TypeId]) -> Result<Datum> {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: types,
            result_type: TypeId::TEXT,
            collation: 0,
        };
        crate::func::implementation(oid)?.call(args, &ctx)
    }

    fn text(d: Result<Datum>) -> String {
        match d {
            Ok(Datum::Text(s)) => s,
            Ok(_) => panic!("expected text result"),
            Err(e) => panic!("unexpected error: {}", e.message),
        }
    }

    fn err(d: Result<Datum>) -> (String, String) {
        let e = d.err().expect("expected an error");
        (e.sqlstate.code().to_string(), e.message)
    }

    fn t(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    #[test]
    fn quote_ident_rules() {
        assert_eq!(quote_identifier("abc").unwrap(), "abc");
        assert_eq!(quote_identifier("_x1").unwrap(), "_x1");
        assert_eq!(quote_identifier("select").unwrap(), "\"select\"");
        assert_eq!(quote_identifier("Abc").unwrap(), "\"Abc\"");
        assert_eq!(quote_identifier("").unwrap(), "\"\"");
        assert_eq!(quote_identifier("a\"b").unwrap(), "\"a\"\"b\"");
        assert_eq!(quote_identifier("a$b").unwrap(), "\"a$b\"");
        assert_eq!(quote_identifier("1abc").unwrap(), "\"1abc\"");
    }

    #[test]
    fn quote_literal_rules() {
        assert_eq!(quote_literal("ab").unwrap(), "'ab'");
        assert_eq!(quote_literal("a'b").unwrap(), "'a''b'");
        assert_eq!(quote_literal("a'b\\c").unwrap(), "E'a''b\\\\c'");
    }

    #[test]
    fn format_positional_and_sequential() {
        let r = run(3539, &[t("%2$s %1$s %s"), t("a"), t("b")], &[TypeId::TEXT; 3]);
        assert_eq!(text(r), "b a b");
    }

    #[test]
    fn format_errors() {
        let (code, msg) = err(run(3539, &[t("%z"), Datum::Int4(1)], &[TypeId::TEXT, TypeId::INT4]));
        assert_eq!(code, "22023");
        assert_eq!(msg, "unrecognized format() type specifier \"z\"");
        let (code, msg) = err(run(3539, &[t("%s %s"), t("a")], &[TypeId::TEXT; 2]));
        assert_eq!(code, "22023");
        assert_eq!(msg, "too few arguments for format()");
        let (code, _) = err(run(3539, &[t("%")], &[TypeId::TEXT]));
        assert_eq!(code, "22023");
        let (code, msg) = err(run(3539, &[t("%I"), Datum::Null], &[TypeId::TEXT, TypeId::UNKNOWN]));
        assert_eq!(code, "22004");
        assert_eq!(msg, "null values cannot be formatted as an SQL identifier");
    }

    #[test]
    fn format_width_flags_and_percent() {
        assert_eq!(text(run(3539, &[t("%-5s|"), t("ab")], &[TypeId::TEXT; 2])), "ab   |");
        assert_eq!(text(run(3539, &[t("%5s|"), t("ab")], &[TypeId::TEXT; 2])), "   ab|");
        assert_eq!(
            text(run(3539, &[t("%*s|"), Datum::Int4(5), t("ab")], &[TypeId::TEXT, TypeId::INT4, TypeId::TEXT])),
            "   ab|"
        );
        assert_eq!(text(run(3539, &[t("%%")], &[TypeId::TEXT])), "%");
        assert_eq!(text(run(3540, &[t("100%%")], &[TypeId::TEXT])), "100%");
    }

    #[test]
    fn format_null_handling() {
        assert_eq!(text(run(3539, &[t("[%s][%L]"), Datum::Null, Datum::Null], &[TypeId::TEXT, TypeId::UNKNOWN, TypeId::UNKNOWN])), "[][NULL]");
        assert!(matches!(run(3539, &[Datum::Null, t("x")], &[TypeId::TEXT, TypeId::TEXT]), Ok(Datum::Null)));
    }

    #[test]
    fn concat_skips_nulls() {
        let r = run(3058, &[t("a"), Datum::Null, Datum::Int4(1)], &[TypeId::TEXT, TypeId::UNKNOWN, TypeId::INT4]);
        assert_eq!(text(r), "a1");
        assert_eq!(text(run(3058, &[], &[])), "");
    }

    #[test]
    fn concat_ws_skips_nulls() {
        let types = [TypeId::TEXT, TypeId::TEXT, TypeId::UNKNOWN, TypeId::TEXT];
        let r = run(3059, &[t(","), t("a"), Datum::Null, t("b")], &types);
        assert_eq!(text(r), "a,b");
        assert!(matches!(run(3059, &[Datum::Null, t("a")], &[TypeId::TEXT, TypeId::TEXT]), Ok(Datum::Null)));
    }

    #[test]
    fn radix_conversions() {
        assert_eq!(text(run(2089, &[Datum::Int4(-1)], &[TypeId::INT4])), "ffffffff");
        assert_eq!(text(run(2090, &[Datum::Int8(-1)], &[TypeId::INT8])), "ffffffffffffffff");
        assert_eq!(text(run(2089, &[Datum::Int4(0)], &[TypeId::INT4])), "0");
        assert_eq!(text(run(6332, &[Datum::Int4(8)], &[TypeId::INT4])), "10");
        assert_eq!(text(run(6330, &[Datum::Int4(5)], &[TypeId::INT4])), "101");
    }

    #[test]
    fn quote_nullable_null_is_unquoted() {
        assert_eq!(text(run(1289, &[Datum::Null], &[TypeId::UNKNOWN])), "NULL");
        assert_eq!(text(run(1289, &[t("a")], &[TypeId::TEXT])), "'a'");
        assert_eq!(text(run(1282, &[t("select")], &[TypeId::TEXT])), "\"select\"");
    }

    #[test]
    fn lookup_covers_owned_procs() {
        for oid in [1282u32, 1283, 1285, 1289, 1290, 2089, 2090, 3058, 3059, 3539, 3540, 6330, 6331, 6332, 6333] {
            let p = crate::catalog_data::builtin_proc(oid).expect("proc row");
            assert!(lookup(p).is_some(), "proc {oid} should be implemented");
        }
    }
}
