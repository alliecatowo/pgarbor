//! Parsing, validation and display formatting of GUC values by type (lease API-01).
//! PG sources: `utils/misc/guc.c` (`parse_bool`, `parse_int`, `parse_real`, `convert_to_base_unit`,
//! `convert_int_from_base_unit`, `config_enum_lookup_by_name`, `parse_and_validate_value`),
//! `guc_tables.c` unit tables.
//!
//! ## Booleans
//! Accepted (case-insensitive, unambiguous prefixes allowed): `true`, `false`, `on`, `off`, `yes`, `no`,
//! `1`, `0`, `t`, `f`, `y`, `n` (`o` is ambiguous). Display: `on` / `off`. Invalid value:
//! 22023 `parameter "x" requires a Boolean value` (oracle: `set standard_conforming_strings = 'x'`).
//!
//! ## Integers and reals (with units)
//! Text is an integer (or, for reals, a float), optionally followed by a unit (whitespace allowed). Without a
//! unit the number is in the parameter's BASE unit (`def.unit`: `kB`, `8kB`, `B`, `MB`, `ms`, `s`,
//! `min`; oracle: `set work_mem = 100` shows `100kB`; `set statement_timeout = 1500` shows `1500ms`).
//! Memory units (case-sensitive): `B`, `kB`, `MB`, `GB`, `TB` (1024-based). Time units: `us`, `ms`, `s`,
//! `min`, `h`, `d`. A fractional value with a unit is rounded to the base unit (`0.5s` for a ms parameter =
//! 500). Errors: not a number -> 22023 `invalid value for parameter "work_mem": "abc"`; bad unit ->
//! the same message plus HINT `Valid units for this parameter are "B", "kB", "MB", "GB", and "TB".`
//! (time: `"us", "ms", "s", "min", "h", and "d"`); outside `[min_val, max_val]` -> 22023 `5 is outside the
//! valid range for parameter "extra_float_digits" (-15 .. 3)` (message with the converted value in the
//! parameter's BASE unit; for unit parameters PG prints e.g. `-1 is outside the valid range for
//! parameter "work_mem" (64 .. 2147483647)`). Display ([`format_int`]): the largest unit of the
//! parameter's family for which the value is an exact multiple (`4096` kB -> `4MB`, `1500` ms -> `1500ms`,
//! `1000` ms -> `1s`, `60000` ms -> `1min`, `0` -> `0`); parameters without a unit print the plain
//! number. Special: `-1` and `0` print without unit.
//!
//! ## Enums
//! Case-insensitive match against `def.enumvals`; the canonical (lower-case as listed) label is stored.
//! `true/false/on/off/yes/no/1/0` aliases are accepted for enums that list `on`/`off`. Invalid: 22023
//! `invalid value for parameter "bytea_output": "x"` + HINT `Available values: escape, hex.`.
//!
//! ## Strings
//! Stored as given, except the special normalizations done by `normalize` for `DateStyle`, `TimeZone`,
//! `client_encoding`, `search_path` (see `guc.rs` hooks).
//!
//! ## Implementation notes (deviations from the text above, taken from the PG 18.6 source)
//! - Range errors on unit parameters append the unit name exactly as `guc.c` does
//!   (`parse_and_validate_value`, `get_config_unit_name`): `-1 kB is outside the valid range for parameter
//!   "work_mem" (64 kB .. 2147483647 kB)`.
//! - Real values are displayed with C `%g` (6 significant digits, `ShowGUCOption` in `guc.c`), not 17.
use pgarbor_common::{PgError, Result, SqlState};

use crate::guc_def::{GucDef, GucType};

/// Unit family of a base unit (`GUC_UNIT_MEMORY` / time).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Memory,
    Time,
}

/// Units of a family, largest first (the order of `guc_tables.c`). Value = bytes (memory) or
/// microseconds (time), so that every conversion is exact integer arithmetic.
const MEMORY_UNITS: &[(&str, i128)] = &[
    ("TB", 1i128 << 40),
    ("GB", 1i128 << 30),
    ("MB", 1i128 << 20),
    ("kB", 1i128 << 10),
    ("B", 1),
];

const TIME_UNITS: &[(&str, i128)] = &[
    ("d", 86_400_000_000),
    ("h", 3_600_000_000),
    ("min", 60_000_000),
    ("s", 1_000_000),
    ("ms", 1_000),
    ("us", 1),
];

const MEMORY_UNIT_HINT: &str = "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".";
const TIME_UNIT_HINT: &str = "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\".";

/// `GUC_LIST_QUOTE` parameters: list elements that are not plain identifiers are double-quoted.
const LIST_QUOTE_PARAMS: &[&str] = &[
    "search_path",
    "temp_tablespaces",
    "session_preload_libraries",
    "shared_preload_libraries",
    "local_preload_libraries",
    "dynamic_library_path",
    "unix_socket_directories",
    "log_destination",
];

/// C `isspace` in the C locale.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Base unit of a parameter: its family and its size in bytes (memory) or microseconds (time).
#[derive(Clone, Copy, Debug)]
struct BaseUnit {
    family: Family,
    scale: i128,
}

fn units_of(family: Family) -> &'static [(&'static str, i128)] {
    match family {
        Family::Memory => MEMORY_UNITS,
        Family::Time => TIME_UNITS,
    }
}

fn find_unit(table: &[(&str, i128)], name: &str) -> Option<i128> {
    table.iter().find(|e| e.0 == name).map(|e| e.1)
}

/// Base unit of `def.unit` (`kB`, `8kB`, `B`, `MB`, `ms`, `s`, `min`, ...); `None` for unitless parameters.
fn base_unit(def: &GucDef) -> Option<BaseUnit> {
    let unit = def.unit;
    if unit.is_empty() {
        return None;
    }
    let digits = unit.bytes().take_while(|b| b.is_ascii_digit()).count();
    let (num, name) = (unit.get(..digits).unwrap_or(""), unit.get(digits..).unwrap_or(""));
    let factor: i128 = if digits == 0 { 1 } else { num.parse::<i128>().ok()? };
    if let Some(scale) = find_unit(MEMORY_UNITS, name) {
        return Some(BaseUnit { family: Family::Memory, scale: scale * factor });
    }
    if let Some(scale) = find_unit(TIME_UNITS, name) {
        return Some(BaseUnit { family: Family::Time, scale: scale * factor });
    }
    None
}

/// C `rint` in the default rounding mode (round half to even).
fn rint(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    let diff = x - f;
    if diff < 0.5 {
        f
    } else if diff > 0.5 {
        f + 1.0
    } else if (f / 2.0).floor() * 2.0 == f {
        f
    } else {
        f + 1.0
    }
}

/// Lexical result of the leading number of a GUC text (`strtol`/`strtod` behaviour as used by guc.c).
enum Scan {
    /// No number at the start of the text.
    NoNumber,
    /// The number overflows a double (`ERANGE`): rejected without a HINT.
    Overflow,
    /// The value and the byte index just after the number.
    Number(f64, usize),
}

fn digit_val(c: u8) -> u128 {
    (c as char).to_digit(16).unwrap_or(0) as u128
}

/// Safe substring by byte offsets (empty when the offsets are not on char boundaries).
fn sub(s: &str, from: usize, to: usize) -> &str {
    s.get(from..to).unwrap_or("")
}

/// Scan the leading number the way `strtol(base 0)` does, falling back to `strtod` syntax when the
/// integer scan stops at `.`/`e`/`E` or overflows (`parse_int` in guc.c).
fn scan_number(s: &str) -> Scan {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut neg = false;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut acc: Option<u128> = Some(0);
    let has_digits: bool;
    if i + 2 < n && b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X') && b[i + 2].is_ascii_hexdigit() {
        i += 2;
        while i < n && b[i].is_ascii_hexdigit() {
            acc = acc
                .and_then(|a| a.checked_mul(16))
                .and_then(|a| a.checked_add(digit_val(b[i])));
            i += 1;
        }
        has_digits = true;
    } else if i < n && b[i] == b'0' {
        // Leading zero: octal digits (the zero itself is a valid octal number).
        while i < n && (b'0'..=b'7').contains(&b[i]) {
            acc = acc
                .and_then(|a| a.checked_mul(8))
                .and_then(|a| a.checked_add(digit_val(b[i])));
            i += 1;
        }
        has_digits = true;
    } else {
        let dstart = i;
        while i < n && b[i].is_ascii_digit() {
            acc = acc
                .and_then(|a| a.checked_mul(10))
                .and_then(|a| a.checked_add(digit_val(b[i])));
            i += 1;
        }
        has_digits = i > dstart;
    }
    let next = if i < n { b[i] } else { 0 };
    if let (true, Some(a)) = (has_digits && next != b'.' && next != b'e' && next != b'E', acc) {
        let v = a as f64;
        return Scan::Number(if neg { -v } else { v }, i);
    }
    scan_float(s)
}

/// Decimal floating syntax: `[+-]digits[.digits][(e|E)[+-]digits]` (exponent only when it has digits).
fn scan_float(s: &str) -> Scan {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut sign = "";
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        sign = if b[i] == b'-' { "-" } else { "" };
        i += 1;
    }
    let int_start = i;
    while i < n && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_part = sub(s, int_start, i);
    let mut frac_part = "";
    if i < n && b[i] == b'.' {
        i += 1;
        let fstart = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_part = sub(s, fstart, i);
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return Scan::NoNumber;
    }
    let mut exp_part = "";
    if i < n && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < n && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let dstart = k;
        while k < n && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > dstart {
            exp_part = sub(s, i + 1, k);
            i = k;
        }
    }
    let normalized = format!(
        "{}{}.{}e{}",
        sign,
        if int_part.is_empty() { "0" } else { int_part },
        if frac_part.is_empty() { "0" } else { frac_part },
        if exp_part.is_empty() { "0" } else { exp_part },
    );
    match normalized.parse::<f64>() {
        Ok(v) if v.is_finite() => Scan::Number(v, i),
        Ok(_) => Scan::Overflow,
        Err(_) => Scan::NoNumber,
    }
}

/// Convert `value` given in unit `unit` to the base unit of `base` (`convert_to_base_unit` in guc.c):
/// the unit must be an exact table name followed only by whitespace; fractional values are rounded to the
/// next smaller unit of the same family first. `None` when the unit is unknown or garbage follows it.
fn convert_to_base(value: f64, unit: &str, base: BaseUnit) -> Option<f64> {
    let table = units_of(base.family);
    let end = unit.find(is_space).unwrap_or(unit.len());
    let token = unit.get(..end)?;
    if !unit.get(end..)?.trim_matches(is_space).is_empty() {
        return None;
    }
    let idx = table.iter().position(|e| e.0 == token)?;
    let entry = table.get(idx)?.1 as f64;
    let mut cvalue = value * entry;
    if let Some(next) = table.get(idx + 1) {
        let nx = next.1 as f64;
        cvalue = rint(cvalue / nx) * nx;
    }
    Some(cvalue / base.scale as f64)
}

/// Lexical stage shared by integers and reals: the number and its unit, converted to the base unit.
/// `Err(None)` = invalid without HINT, `Err(Some(hint))` = invalid with that HINT.
fn lex_value(def: &GucDef, text: &str) -> std::result::Result<f64, Option<&'static str>> {
    let trimmed = text.trim_start_matches(is_space);
    let (value, end) = match scan_number(trimmed) {
        Scan::Number(v, e) => (v, e),
        Scan::NoNumber | Scan::Overflow => return Err(None),
    };
    let rest = sub(trimmed, end, trimmed.len()).trim_start_matches(is_space);
    if rest.is_empty() {
        return Ok(value);
    }
    let base = match base_unit(def) {
        Some(b) => b,
        None => return Err(None),
    };
    match convert_to_base(value, rest, base) {
        Some(v) => Ok(v),
        None => Err(Some(match base.family {
            Family::Memory => MEMORY_UNIT_HINT,
            Family::Time => TIME_UNIT_HINT,
        })),
    }
}

fn invalid_value(def: &GucDef, text: &str, hint: Option<&str>) -> PgError {
    let err = PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{}\": \"{}\"", def.name, text),
    );
    match hint {
        Some(h) => err.with_hint(h),
        None => err,
    }
}

/// `%g`-style rendering with 6 significant digits (C `printf`, as `ShowGUCOption` uses).
fn format_g(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".to_string() } else { "-inf".to_string() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    // Rust's `{:.5e}` yields e.g. `1.23457e-5`; the exponent is taken after rounding, as C does.
    let sci = format!("{:.5e}", v);
    let (mant, exp) = match sci.split_once('e') {
        Some((m, e)) => match e.parse::<i32>() {
            Ok(x) => (m.to_string(), x),
            Err(_) => return format!("{:.5e}", v),
        },
        None => return format!("{:.5e}", v),
    };
    if (-4..6).contains(&exp) {
        let decimals = (5 - exp) as usize;
        trim_fraction_zeros(&format!("{:.*}", decimals, v))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", trim_fraction_zeros(&mant), sign, exp.unsigned_abs())
    }
}

fn trim_fraction_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

fn parse_bound_i64(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<i64>().ok()
    }
}

fn parse_bound_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<f64>().ok().filter(|v| v.is_finite())
    }
}

/// Parse a PG boolean literal (module docs): `Ok(Some(b))`, or `Ok(None)` when `s` is not a boolean.
pub fn parse_bool(s: &str) -> Result<Option<bool>> {
    let t = s.trim_matches(is_space).to_ascii_lowercase();
    if t.is_empty() {
        return Ok(None);
    }
    let v = match t.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ if "true".starts_with(t.as_str()) => Some(true),
        _ if "false".starts_with(t.as_str()) => Some(false),
        _ if "yes".starts_with(t.as_str()) => Some(true),
        _ if "no".starts_with(t.as_str()) => Some(false),
        // `o` alone is ambiguous; `on` needs two characters, `off` three (guc.c parse_bool_with_len).
        _ if t.len() >= 2 && "on".starts_with(t.as_str()) => Some(true),
        _ if t.len() >= 3 && "off".starts_with(t.as_str()) => Some(false),
        _ => None,
    };
    Ok(v)
}

/// Parse an integer GUC value with optional unit into the parameter's base unit.
pub fn parse_int(def: &GucDef, text: &str) -> Result<i64> {
    let value = match lex_value(def, text) {
        Ok(v) => v,
        Err(hint) => return Err(invalid_value(def, text, hint)),
    };
    let rounded = rint(value);
    if rounded > i32::MAX as f64 || rounded < i32::MIN as f64 {
        return Err(invalid_value(def, text, Some("Value exceeds integer range.")));
    }
    let v = rounded as i64;
    let lo = parse_bound_i64(def.min_val).unwrap_or(i32::MIN as i64);
    let hi = parse_bound_i64(def.max_val).unwrap_or(i32::MAX as i64);
    if v < lo || v > hi {
        let u = if def.unit.is_empty() { String::new() } else { format!(" {}", def.unit) };
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "{}{} is outside the valid range for parameter \"{}\" ({}{} .. {}{})",
                v, u, def.name, lo, u, hi, u
            ),
        ));
    }
    Ok(v)
}

/// Parse a real GUC value.
pub fn parse_real(def: &GucDef, text: &str) -> Result<f64> {
    let value = match lex_value(def, text) {
        Ok(v) => v,
        Err(hint) => return Err(invalid_value(def, text, hint)),
    };
    let lo = parse_bound_f64(def.min_val);
    let hi = parse_bound_f64(def.max_val);
    let below = lo.map_or(false, |l| value < l);
    let above = hi.map_or(false, |h| value > h);
    if below || above {
        let u = if def.unit.is_empty() { String::new() } else { format!(" {}", def.unit) };
        let lo_text = format_g(lo.unwrap_or(value));
        let hi_text = format_g(hi.unwrap_or(value));
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "{}{} is outside the valid range for parameter \"{}\" ({}{} .. {}{})",
                format_g(value),
                u,
                def.name,
                lo_text,
                u,
                hi_text,
                u
            ),
        ));
    }
    Ok(value)
}

/// Display form of an integer in base units (module docs).
pub fn format_int(def: &GucDef, base_value: i64) -> Result<String> {
    if base_value <= 0 {
        return Ok(base_value.to_string());
    }
    let base = match base_unit(def) {
        Some(b) => b,
        None => return Ok(base_value.to_string()),
    };
    let total = (base_value as i128) * base.scale;
    for &(name, mult) in units_of(base.family) {
        // Greatest unit that divides the value exactly (or any unit not larger than the base unit).
        if mult <= base.scale || total % mult == 0 {
            let q = rint(base_value as f64 * base.scale as f64 / mult as f64);
            return Ok(format!("{}{}", q as i64, name));
        }
    }
    Ok(base_value.to_string())
}

/// Display form of a real (PG prints with up to 17 significant digits trimmed: `0.1`, `100`, `1.5`).
pub fn format_real(def: &GucDef, value: f64) -> Result<String> {
    let base = match base_unit(def) {
        Some(b) if value > 0.0 => b,
        _ => return Ok(format_g(value)),
    };
    // convert_real_from_base_unit: first unit whose quotient is within 1e-8 of an integer; otherwise the
    // smallest unit of the family.
    let mut chosen: Option<(f64, &'static str)> = None;
    for &(name, mult) in units_of(base.family) {
        let q = value * base.scale as f64 / mult as f64;
        chosen = Some((q, name));
        if q > 0.0 && ((rint(q) / q) - 1.0).abs() <= 1e-8 {
            break;
        }
    }
    match chosen {
        Some((q, name)) => Ok(format!("{}{}", format_g(q), name)),
        None => Ok(format_g(value)),
    }
}

fn enum_error(def: &GucDef, text: &str) -> PgError {
    let mut hint = String::from("Available values: ");
    for (i, label) in def.enumvals.iter().enumerate() {
        if i > 0 {
            hint.push_str(", ");
        }
        hint.push_str(label);
    }
    hint.push('.');
    invalid_value(def, text, Some(&hint))
}

/// Canonical enum label for `text`, or the 22023 error with the `Available values` hint.
pub fn parse_enum(def: &GucDef, text: &str) -> Result<&'static str> {
    for label in def.enumvals.iter() {
        if label.eq_ignore_ascii_case(text) {
            return Ok(*label);
        }
    }
    // Boolean aliases for enums that have on/off labels.
    let has_on = def.enumvals.iter().any(|l| *l == "on");
    let has_off = def.enumvals.iter().any(|l| *l == "off");
    if has_on && has_off {
        let lower = text.to_ascii_lowercase();
        let target = match lower.as_str() {
            "true" | "yes" | "on" | "1" => "on",
            "false" | "no" | "off" | "0" => "off",
            _ => "",
        };
        if !target.is_empty() {
            for label in def.enumvals.iter() {
                if *label == target {
                    return Ok(*label);
                }
            }
        }
    }
    Err(enum_error(def, text))
}

/// Validate `text` for `def` and return the canonical SHOW-form string to store: bool -> `on`/`off`,
/// int -> `format_int(parse_int(..))`, real -> `format_real`, enum -> canonical label, string -> `text`.
pub fn normalize(def: &GucDef, text: &str) -> Result<String> {
    match def.vartype {
        GucType::Bool => match parse_bool(text)? {
            Some(true) => Ok("on".to_string()),
            Some(false) => Ok("off".to_string()),
            None => Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("parameter \"{}\" requires a Boolean value", def.name),
            )),
        },
        GucType::Int => {
            let v = parse_int(def, text)?;
            format_int(def, v)
        }
        GucType::Real => {
            let v = parse_real(def, text)?;
            format_real(def, v)
        }
        GucType::Enum => Ok(parse_enum(def, text)?.to_string()),
        GucType::String => Ok(text.to_string()),
    }
}

/// A plain lower-case identifier that needs no quoting in a list value.
fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A numeric literal (`[+-]digits[.digits]`), which passes through unquoted.
fn is_numeric_literal(s: &str) -> bool {
    let t = s.strip_prefix('-').or_else(|| s.strip_prefix('+')).unwrap_or(s);
    let (int_part, frac_part) = match t.split_once('.') {
        Some((a, b)) => (a, b),
        None => (t, ""),
    };
    let all_digits = |x: &str| x.bytes().all(|c| c.is_ascii_digit());
    (!int_part.is_empty() || !frac_part.is_empty()) && all_digits(int_part) && all_digits(frac_part)
}

fn push_quoted_identifier(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

/// `SET name = v1, v2` values joined the way PG stores a list value: elements separated by `, `;
/// for `search_path`-style list-quote parameters (`search_path`, `temp_tablespaces`,
/// `session_preload_libraries`, ...) an element that is not a plain lower-case identifier is
/// double-quoted (`a, "B", "$user"` - `$user` stays unquoted as `"$user"` with quotes, exactly as PG
/// prints the default `"$user", public`). Numbers/booleans pass through.
pub fn join_set_values(def: &GucDef, values: &[String]) -> Result<String> {
    let list_quote = LIST_QUOTE_PARAMS.iter().any(|p| p.eq_ignore_ascii_case(def.name));
    let mut out = String::new();
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if list_quote && !is_plain_identifier(v) && !is_numeric_literal(v) {
            push_quoted_identifier(&mut out, v);
        } else {
            out.push_str(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guc_def::{find_def, GucContext};

    fn def(name: &str) -> &'static GucDef {
        find_def(name).expect("parameter exists in the generated table")
    }

    fn custom(
        vartype: GucType,
        unit: &'static str,
        min: &'static str,
        max: &'static str,
        enumvals: &'static [&'static str],
    ) -> GucDef {
        GucDef {
            name: "test_param",
            category: "",
            short_desc: "",
            extra_desc: "",
            context: GucContext::User,
            vartype,
            unit,
            min_val: min,
            max_val: max,
            enumvals,
            boot_val: "",
            report: false,
        }
    }

    fn expect_err(r: Result<String>) -> PgError {
        match r {
            Err(e) => e,
            Ok(v) => panic!("expected an error, got {v:?}"),
        }
    }

    #[test]
    fn bool_prefixes() {
        assert_eq!(parse_bool("of").unwrap(), None);
        assert_eq!(parse_bool("o").unwrap(), None);
        assert_eq!(parse_bool("on").unwrap(), Some(true));
        assert_eq!(parse_bool("off").unwrap(), Some(false));
        assert_eq!(parse_bool("tr").unwrap(), Some(true));
        assert_eq!(parse_bool("t").unwrap(), Some(true));
        assert_eq!(parse_bool("f").unwrap(), Some(false));
        assert_eq!(parse_bool("y").unwrap(), Some(true));
        assert_eq!(parse_bool("n").unwrap(), Some(false));
        assert_eq!(parse_bool("1").unwrap(), Some(true));
        assert_eq!(parse_bool("0").unwrap(), Some(false));
        assert_eq!(parse_bool("  OFF ").unwrap(), Some(false));
        assert_eq!(parse_bool("").unwrap(), None);
        assert_eq!(parse_bool("x").unwrap(), None);
    }

    #[test]
    fn bool_error_text() {
        let e = expect_err(normalize(def("standard_conforming_strings"), "x"));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "parameter \"standard_conforming_strings\" requires a Boolean value");
    }

    #[test]
    fn memory_units_normalize() {
        assert_eq!(normalize(def("work_mem"), "4MB").unwrap(), "4MB");
        assert_eq!(normalize(def("work_mem"), "100").unwrap(), "100kB");
        assert_eq!(normalize(def("work_mem"), "0.5MB").unwrap(), "512kB");
        assert_eq!(format_int(def("work_mem"), 0).unwrap(), "0");
        assert_eq!(format_int(def("work_mem"), -1).unwrap(), "-1");
        assert_eq!(format_int(def("work_mem"), 1025).unwrap(), "1025kB");
    }

    #[test]
    fn time_units_normalize() {
        let st = def("statement_timeout");
        assert_eq!(normalize(st, "1s").unwrap(), "1s");
        assert_eq!(normalize(st, "1500").unwrap(), "1500ms");
        assert_eq!(parse_int(st, "0.5s").unwrap(), 500);
        assert_eq!(format_int(st, 1000).unwrap(), "1s");
        assert_eq!(format_int(st, 60000).unwrap(), "1min");
        assert_eq!(format_int(st, 1500).unwrap(), "1500ms");
        assert_eq!(format_int(st, 3_600_000).unwrap(), "1h");
    }

    #[test]
    fn range_error_text() {
        let e = expect_err(normalize(def("extra_float_digits"), "5"));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "5 is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)");
        let e = expect_err(normalize(def("work_mem"), "-1"));
        assert_eq!(
            e.message,
            "-1 kB is outside the valid range for parameter \"work_mem\" (64 kB .. 2147483647 kB)"
        );
    }

    #[test]
    fn unit_and_syntax_errors() {
        let e = expect_err(normalize(def("work_mem"), "4XB"));
        assert_eq!(e.message, "invalid value for parameter \"work_mem\": \"4XB\"");
        assert_eq!(
            e.hint.as_deref(),
            Some("Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".")
        );
        let e = expect_err(normalize(def("statement_timeout"), "5 xx"));
        assert_eq!(
            e.hint.as_deref(),
            Some("Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\".")
        );
        let e = expect_err(normalize(def("work_mem"), "abc"));
        assert_eq!(e.message, "invalid value for parameter \"work_mem\": \"abc\"");
        assert_eq!(e.hint, None);
        let e = expect_err(normalize(def("work_mem"), "99999999999"));
        assert_eq!(e.hint.as_deref(), Some("Value exceeds integer range."));
    }

    #[test]
    fn enum_normalize_and_hint() {
        let e = expect_err(normalize(def("bytea_output"), "x"));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "invalid value for parameter \"bytea_output\": \"x\"");
        assert_eq!(e.hint.as_deref(), Some("Available values: escape, hex."));
        assert_eq!(normalize(def("bytea_output"), "HEX").unwrap(), "hex");

        static VALS: &[&str] = &["on", "off", "partial"];
        let d = custom(GucType::Enum, "", "", "", VALS);
        assert_eq!(normalize(&d, "TRUE").unwrap(), "on");
        assert_eq!(normalize(&d, "no").unwrap(), "off");
        assert_eq!(normalize(&d, "PARTIAL").unwrap(), "partial");
    }

    #[test]
    fn real_formatting() {
        let d = custom(GucType::Real, "", "0", "1000", &[]);
        assert_eq!(format_real(&d, 0.1).unwrap(), "0.1");
        assert_eq!(format_real(&d, 100.0).unwrap(), "100");
        assert_eq!(format_real(&d, 1.5).unwrap(), "1.5");
        assert_eq!(format_real(&d, 1234567.0).unwrap(), "1.23457e+06");
        assert_eq!(format_real(&d, 0.0001).unwrap(), "0.0001");
        assert_eq!(format_real(&d, 0.00001).unwrap(), "1e-05");
        assert_eq!(normalize(&d, "2.50").unwrap(), "2.5");
        let e = expect_err(normalize(&d, "1001"));
        assert_eq!(e.message, "1001 is outside the valid range for parameter \"test_param\" (0 .. 1000)");
    }

    #[test]
    fn list_join() {
        let sp = def("search_path");
        let vals: Vec<String> = vec!["a".into(), "B".into(), "$user".into()];
        assert_eq!(join_set_values(sp, &vals).unwrap(), "a, \"B\", \"$user\"");
        let vals: Vec<String> = vec!["public".into()];
        assert_eq!(join_set_values(sp, &vals).unwrap(), "public");
        let vals: Vec<String> = vec!["we\"ird".into()];
        assert_eq!(join_set_values(sp, &vals).unwrap(), "\"we\"\"ird\"");
        let vals: Vec<String> = Vec::new();
        assert_eq!(join_set_values(sp, &vals).unwrap(), "");
    }

    #[test]
    fn scan_edge_cases() {
        // 0x and octal follow strtol base-0 rules; 08 stops after the zero and leaves "8" as a bad unit.
        assert_eq!(parse_int(def("work_mem"), "0x40").unwrap(), 64);
        assert!(matches!(lex_value(def("work_mem"), "08"), Err(Some(_))));
        assert!(matches!(lex_value(def("work_mem"), ".5"), Ok(v) if v == 0.5));
    }
}
