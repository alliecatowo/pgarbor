//! Type families and builtin procs PGArbor does not implement yet (TYPE-01 follow-ups). Lease TYP-42.
//!
//! Covers I/O for every type whose `typinput` is not routed by `crate::io` (geometric types, inet/cidr/
//! macaddr, bit/varbit, ranges/multiranges, tsvector/tsquery, xml, jsonpath, txid_snapshot,
//! pg_snapshot, tid, and the pseudo types), plus the procs classified `ImplModule::Unsupported` /
//! `Window` / `External` that reach the scalar path.
//!
//! Implemented here (fit the frozen `Datum` without an amendment):
//! - `money` (`cash_in`/`cash_out`/`cash_recv`/`cash_send`): `Datum::Int8` cents, PG's C-locale output
//!   `$1,234.56` / `-$1.00`, and PG's `cash_in` parsing rules.
//! - `aclitem` (`aclitemin`/`aclitemout`): `Datum::Text` in canonical `grantee=privs/grantor` form.
//!   Role existence is not checked (no role catalog reaches this crate), and a missing `/grantor`
//!   is 0A000 because PG would default it to the bootstrap superuser's name, which is unknown here.
//! - Pseudo types: `anyarray_out`/`anycompatiblearray_out`/`anyarray_send`/`anycompatiblearray_send`
//!   delegate to the array I/O (`Datum::Array`); every other pseudo type rejects I/O with PG's
//!   0A000 texts (`cannot accept a value of type X` / `cannot display a value of type X`).
//! - `refcursor` needs no code here: its typinput is `textin`, so `crate::io` routes it to `text::io`.
//!
//! Everything else is 0A000 via `PgError::unimplemented("TYP-42", ...)` naming the type or function
//! (e.g. `type point is not supported by PGArbor yet`). Most of these need new `Datum` variants: see
//! amendment request `contracts/amendments/pgarbor-types-03-datum-variants.md`.
use crate::array;
use crate::catalog_data::{BuiltinAggregate, BuiltinProc};
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId, type_mismatch};
use crate::func::FuncImpl;
use crate::io as pg_io;
use crate::registry::{self, TypeInfo};
use pgarbor_common::{PgError, Result, SqlState};

/// Lease ID used in every `unimplemented` error of this module.
const LEASE: &str = "TYP-42";

/// `pg_type.typtype` for pseudo types.
const PSEUDO_TYPTYPE: u8 = b'p';

/// `money`: C-locale `lconv` values as PostgreSQL sees them under `lc_monetary = C`.
/// `frac_digits` is CHAR_MAX there, which PG maps to 2; the currency symbol falls back to `$`.
const MONEY_FRAC_DIGITS: usize = 2;
const MONEY_DSYMBOL: u8 = b'.';
const MONEY_SSYMBOL: &[u8] = b",";
const MONEY_CSYMBOL: &[u8] = b"$";
const MONEY_PSYMBOL: &[u8] = b"+";
const MONEY_NSYMBOL: &[u8] = b"-";
/// `mon_grouping` is empty in C, so PG groups by 3.
const MONEY_GROUP: i64 = 3;

/// `aclitem` privilege letters in bit order (`ACL_ALL_RIGHTS_STR`).
const ACL_ALL_RIGHTS: &[u8] = b"arwdDxtXUCTcsAm";
/// `NAMEDATALEN - 1`: the longest identifier `getid` accepts.
const ACL_NAME_MAX: usize = 63;

fn type_name(ctx: &IoCtx, ty: TypeId) -> String {
    registry::type_info(ctx.env, ty.oid).map(|t| t.name.into_owned()).unwrap_or_else(|_| ty.oid.to_string())
}

fn unsupported_type(ctx: &IoCtx, ty: TypeId) -> PgError {
    PgError::unimplemented(LEASE, format!("type {}", type_name(ctx, ty)))
}

/// Catalog metadata for the type, or the 0A000 error when the OID is unknown.
fn routine_info(ctx: &IoCtx, ty: TypeId) -> std::result::Result<TypeInfo, PgError> {
    registry::type_info(ctx.env, ty.oid).map_err(|_| unsupported_type(ctx, ty))
}

/// PG's `PSEUDOTYPE_DUMMY_*` rejection: `cannot <action> a value of type <name>`, SQLSTATE 0A000.
fn pseudo_error(name: &str, action: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot {action} a value of type {name}"))
}

/// PG's `getTypeBinaryInputInfo` / `getTypeBinaryOutputInfo` error for a type without `typreceive`/`typsend`.
fn no_binary_function(name: &str, input: bool) -> PgError {
    let dir = if input { "input" } else { "output" };
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("no binary {dir} function available for type {name}"))
}

/// The C string view of a Rust string: PG strings cannot hold NUL, so input stops at the first one.
fn c_string_bytes(s: &str) -> &[u8] {
    let b = s.as_bytes();
    match b.iter().position(|&c| c == 0) {
        Some(p) => &b[..p],
        None => b,
    }
}

/// C-locale `isspace`.
fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    i
}

/// `strncmp(s + i, pat, strlen(pat)) == 0`.
fn starts_at(b: &[u8], i: usize, pat: &[u8]) -> bool {
    b.get(i..i + pat.len()) == Some(pat)
}

/// Text input for unsupported types.
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let info = routine_info(ctx, ty)?;
    match info.input {
        "cash_in" => money_in(s),
        "aclitemin" => aclitem_in(s),
        _ if info.typtype == PSEUDO_TYPTYPE => Err(pseudo_error(&type_name(ctx, ty), "accept")),
        _ => Err(unsupported_type(ctx, ty)),
    }
}

/// Text output for unsupported types.
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    let info = routine_info(ctx, ty)?;
    match info.output {
        "cash_out" => money_out(d),
        "aclitemout" => aclitem_out(d),
        "anyarray_out" | "anycompatiblearray_out" => array_out(ctx, d),
        _ if info.typtype == PSEUDO_TYPTYPE => Err(pseudo_error(&type_name(ctx, ty), "display")),
        _ => Err(unsupported_type(ctx, ty)),
    }
}

/// Binary input for unsupported types.
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let info = routine_info(ctx, ty)?;
    match info.receive {
        "cash_recv" => money_recv(buf),
        "-" => Err(no_binary_function(&type_name(ctx, ty), true)),
        _ if info.typtype == PSEUDO_TYPTYPE => Err(pseudo_error(&type_name(ctx, ty), "accept")),
        _ => Err(unsupported_type(ctx, ty)),
    }
}

/// Binary output for unsupported types.
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    let info = routine_info(ctx, ty)?;
    match info.send {
        "cash_send" => money_send(d),
        "anyarray_send" | "anycompatiblearray_send" => array_send(ctx, d),
        "-" => Err(no_binary_function(&type_name(ctx, ty), false)),
        _ if info.typtype == PSEUDO_TYPTYPE => Err(pseudo_error(&type_name(ctx, ty), "display")),
        _ => Err(unsupported_type(ctx, ty)),
    }
}

/// Function registry hook for `ImplModule::Unsupported` procs (none implemented).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for unsupported procs.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented(LEASE, format!("function {}", proc.name))
}

/// 0A000 for unsupported aggregates (xmlagg, range_agg, range_intersect_agg).
pub(crate) fn unsupported_aggregate(agg: &BuiltinAggregate) -> PgError {
    PgError::unimplemented(LEASE, format!("aggregate {}", agg.name))
}

// ---------------------------------------------------------------------------------------------
// Pseudo types: anyarray / anycompatiblearray
// ---------------------------------------------------------------------------------------------

/// Array type OID of a `Datum::Array`'s element type, as the array I/O needs it.
fn array_type_for(ctx: &IoCtx, d: &Datum) -> Result<TypeId> {
    let a = match d {
        Datum::Array(a) => a,
        _ => return Err(type_mismatch("Array", d)),
    };
    let array_oid = registry::array_type_of(ctx.env, a.elem).ok_or_else(|| {
        PgError::internal(format!("element type {} of an array has no array type", a.elem))
    })?;
    Ok(TypeId::of(array_oid))
}

/// `anyarray_out` is PG's `array_out`.
fn array_out(ctx: &IoCtx, d: &Datum) -> Result<String> {
    array::io::output(ctx, array_type_for(ctx, d)?, d)
}

/// `anyarray_send` is PG's `array_send`.
fn array_send(ctx: &IoCtx, d: &Datum) -> Result<Vec<u8>> {
    array::io::send(ctx, array_type_for(ctx, d)?, d)
}

// ---------------------------------------------------------------------------------------------
// money (cash.c, C locale)
// ---------------------------------------------------------------------------------------------

fn money_range(s: &str) -> PgError {
    PgError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("value \"{s}\" is out of range for type money"),
    )
}

/// Port of PG `cash_in` under `lc_monetary = C`. Values are accumulated as negatives so that
/// `-9223372036854775808` cents is representable, then the sign is applied.
fn money_in(s: &str) -> Result<Datum> {
    let b = c_string_bytes(s);
    let mut i = skip_space(b, 0);
    if starts_at(b, i, MONEY_CSYMBOL) {
        i += MONEY_CSYMBOL.len();
    }
    i = skip_space(b, i);

    let mut negative = false;
    if starts_at(b, i, MONEY_NSYMBOL) {
        negative = true;
        i += MONEY_NSYMBOL.len();
    } else if b.get(i) == Some(&b'(') {
        negative = true;
        i += 1;
    } else if starts_at(b, i, MONEY_PSYMBOL) {
        i += MONEY_PSYMBOL.len();
    }
    i = skip_space(b, i);
    if starts_at(b, i, MONEY_CSYMBOL) {
        i += MONEY_CSYMBOL.len();
    }
    i = skip_space(b, i);

    let mut value: i64 = 0;
    let mut dec: usize = 0;
    let mut seen_dot = false;
    while let Some(&c) = b.get(i) {
        if c.is_ascii_digit() && (!seen_dot || dec < MONEY_FRAC_DIGITS) {
            let digit = i64::from(c - b'0');
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_sub(digit))
                .ok_or_else(|| money_range(s))?;
            if seen_dot {
                dec += 1;
            }
            i += 1;
        } else if c == MONEY_DSYMBOL && !seen_dot {
            seen_dot = true;
            i += 1;
        } else if starts_at(b, i, MONEY_SSYMBOL) {
            i += MONEY_SSYMBOL.len();
        } else {
            break;
        }
    }

    // Round off on one more digit (value is built negative, so "round up" means subtract).
    if b.get(i).is_some_and(|c| c.is_ascii_digit() && *c >= b'5') {
        value = value.checked_sub(1).ok_or_else(|| money_range(s))?;
    }

    // Adjust for fewer than the required fractional digits.
    while dec < MONEY_FRAC_DIGITS {
        value = value.checked_mul(10).ok_or_else(|| money_range(s))?;
        dec += 1;
    }

    // Only trailing digits, whitespace, ')', a trailing sign or a trailing currency symbol may follow.
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    while let Some(&c) = b.get(i) {
        if c_isspace(c) || c == b')' {
            i += 1;
        } else if starts_at(b, i, MONEY_NSYMBOL) {
            negative = true;
            i += MONEY_NSYMBOL.len();
        } else if starts_at(b, i, MONEY_PSYMBOL) {
            i += MONEY_PSYMBOL.len();
        } else if starts_at(b, i, MONEY_CSYMBOL) {
            i += MONEY_CSYMBOL.len();
        } else {
            return Err(pg_io::invalid_input_syntax("money", s));
        }
    }

    let cents = if negative { value } else { value.checked_neg().ok_or_else(|| money_range(s))? };
    Ok(Datum::Int8(cents))
}

/// Port of PG `cash_out` under `lc_monetary = C`: `$1,234.56`, negative `-$1.00`.
fn money_out(d: &Datum) -> Result<String> {
    let cents = match d {
        Datum::Int8(v) => *v,
        _ => return Err(type_mismatch("Int8", d)),
    };
    let negative = cents < 0;
    let mut uvalue = cents.unsigned_abs();

    // Digits are produced right to left, then reversed.
    let mut rev: Vec<u8> = Vec::with_capacity(32);
    let mut digit_pos: i64 = MONEY_FRAC_DIGITS as i64;
    loop {
        if digit_pos == 0 {
            rev.push(MONEY_DSYMBOL);
        } else if digit_pos < 0 && digit_pos % MONEY_GROUP == 0 {
            rev.extend_from_slice(MONEY_SSYMBOL);
        }
        rev.push(b'0' + (uvalue % 10) as u8);
        uvalue /= 10;
        digit_pos -= 1;
        if uvalue == 0 && digit_pos < 0 {
            break;
        }
    }
    rev.reverse();
    let body = String::from_utf8_lossy(&rev).into_owned();

    // Sign precedes the currency symbol; no spaces (sep_by_space is CHAR_MAX in C).
    let mut out = String::with_capacity(body.len() + 2);
    if negative {
        out.push('-');
    }
    out.push('$');
    out.push_str(&body);
    Ok(out)
}

/// `cash_recv`: one big-endian i64, nothing left over.
fn money_recv(buf: &[u8]) -> Result<Datum> {
    if buf.len() < 8 {
        return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
    }
    if buf.len() > 8 {
        return Err(pg_io::invalid_binary(""));
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(buf);
    Ok(Datum::Int8(i64::from_be_bytes(raw)))
}

/// `cash_send`: one big-endian i64.
fn money_send(d: &Datum) -> Result<Vec<u8>> {
    match d {
        Datum::Int8(v) => Ok(v.to_be_bytes().to_vec()),
        _ => Err(type_mismatch("Int8", d)),
    }
}

// ---------------------------------------------------------------------------------------------
// aclitem (acl.c)
// ---------------------------------------------------------------------------------------------

/// `is_safe_acl_char`: ASCII alphanumerics and `_`; high-bit bytes only when reading identifiers.
fn is_safe_acl_char(c: u8, is_getid: bool) -> bool {
    if c >= 0x80 {
        return is_getid;
    }
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Port of `getid`: one identifier (bare or double-quoted, `""` escapes a quote), then whitespace.
/// Returns the name bytes and the position after the trailing whitespace. An empty name means none.
fn acl_getid(b: &[u8], mut i: usize) -> Result<(Vec<u8>, usize)> {
    i = skip_space(b, i);
    let mut name: Vec<u8> = Vec::new();
    let mut in_quotes = false;
    while let Some(&c) = b.get(i) {
        if !(in_quotes || c == b'"' || is_safe_acl_char(c, true)) {
            break;
        }
        if c == b'"' {
            if !in_quotes {
                in_quotes = true;
                i += 1;
                continue;
            }
            if b.get(i + 1) != Some(&b'"') {
                in_quotes = false;
                i += 1;
                continue;
            }
            // Escaped double quote: skip the first one and keep the second.
            i += 1;
        }
        if name.len() >= ACL_NAME_MAX {
            return Err(PgError::new(SqlState::NAME_TOO_LONG, "identifier too long")
                .with_detail(format!("Identifier must be less than {} characters.", ACL_NAME_MAX + 1)));
        }
        name.push(b[i]);
        i += 1;
    }
    i = skip_space(b, i);
    Ok((name, i))
}

/// Port of `putid`: quote the role name when it has characters outside the safe set.
fn acl_put_id(out: &mut String, name: &str) {
    let safe = name.bytes().all(|c| is_safe_acl_char(c, false));
    if !safe {
        out.push('"');
    }
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    if !safe {
        out.push('"');
    }
}

fn bytes_to_name(v: Vec<u8>) -> Result<String> {
    String::from_utf8(v).map_err(|_| PgError::internal("aclitem name is not valid UTF-8"))
}

/// Port of `aclitemin` (through `aclparse`), syntax only. The result is the canonical text that
/// `aclitemout` prints, held as `Datum::Text`.
fn aclitem_in(s: &str) -> Result<Datum> {
    let b = c_string_bytes(s);
    let (mut name, mut i) = acl_getid(b, 0)?;
    if b.get(i) != Some(&b'=') {
        // A keyword ("group" / "user") precedes the name.
        if name.as_slice() != b"group" && name.as_slice() != b"user" {
            return Err(PgError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("unrecognized key word: \"{}\"", String::from_utf8_lossy(&name)),
            )
            .with_hint("ACL key word must be \"group\" or \"user\"."));
        }
        let (next_name, j) = acl_getid(b, i)?;
        if next_name.is_empty() {
            return Err(PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "missing name")
                .with_hint("A name must follow the \"group\" or \"user\" key word."));
        }
        name = next_name;
        i = j;
    }
    if b.get(i) != Some(&b'=') {
        return Err(PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "missing \"=\" sign"));
    }
    let grantee = bytes_to_name(name)?;

    // Privilege letters, each optionally followed by '*' (grant option for the preceding letter).
    let mut privs: u32 = 0;
    let mut goption: u32 = 0;
    let mut read: u32 = 0;
    i += 1;
    while let Some(&c) = b.get(i) {
        if !(c.is_ascii_alphabetic() || c == b'*') {
            break;
        }
        if c == b'*' {
            goption |= read;
        } else {
            match ACL_ALL_RIGHTS.iter().position(|&r| r == c) {
                Some(p) => read = 1u32 << p,
                None => {
                    return Err(PgError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        format!(
                            "invalid mode character: must be one of \"{}\"",
                            String::from_utf8_lossy(ACL_ALL_RIGHTS)
                        ),
                    ));
                }
            }
        }
        privs |= read;
        i += 1;
    }

    // Grantor. PG defaults a missing grantor to the bootstrap superuser with a WARNING; that name is
    // not known to this crate, so the input stays unimplemented rather than guessing it.
    let after_privs = skip_space(b, i);
    if after_privs >= b.len() {
        return Err(PgError::unimplemented(
            LEASE,
            "aclitem without a grantor (PostgreSQL defaults it to the bootstrap superuser)",
        ));
    }
    if b[after_privs] != b'/' {
        return Err(PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "extra garbage at the end of the ACL specification"));
    }
    let (grantor_bytes, j) = acl_getid(b, after_privs + 1)?;
    if grantor_bytes.is_empty() {
        return Err(PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "a name must follow the \"/\" sign"));
    }
    let grantor = bytes_to_name(grantor_bytes)?;
    if j < b.len() {
        return Err(PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "extra garbage at the end of the ACL specification"));
    }

    // Canonical text, as aclitemout prints it: PUBLIC grantee is empty; each privilege letter is
    // followed by '*' when it has a grant option.
    let mut out = String::with_capacity(grantee.len() + grantor.len() + 20);
    if !grantee.is_empty() {
        acl_put_id(&mut out, &grantee);
    }
    out.push('=');
    for (bit, &letter) in ACL_ALL_RIGHTS.iter().enumerate() {
        let mask = 1u32 << bit;
        if privs & mask != 0 {
            out.push(char::from(letter));
        }
        if goption & mask != 0 {
            out.push('*');
        }
    }
    out.push('/');
    acl_put_id(&mut out, &grantor);
    Ok(Datum::Text(out))
}

/// `aclitemout`. The stored `Datum::Text` is already the canonical text, so it passes through.
fn aclitem_out(d: &Datum) -> Result<String> {
    match d {
        Datum::Text(s) => Ok(s.clone()),
        _ => Err(type_mismatch("Text", d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;

    const MONEY: TypeId = TypeId::of(790);
    const ACLITEM: TypeId = TypeId::of(1033);
    const POINT: TypeId = TypeId::of(600);
    const ANYELEMENT: TypeId = TypeId::of(2283);
    const ANYARRAY: TypeId = TypeId::of(2277);

    fn with_ctx<R>(f: impl FnOnce(&IoCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        f(&ctx)
    }

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> PgError {
        match r {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn money_input_accepts_symbol_grouping_and_plain_forms() {
        with_ctx(|ctx| {
            assert_eq!(pg_io::input(ctx, MONEY, "$1,234.56").unwrap(), Datum::Int8(123_456));
            assert_eq!(pg_io::input(ctx, MONEY, "1234.56").unwrap(), Datum::Int8(123_456));
            assert_eq!(pg_io::input(ctx, MONEY, "  $ 1,234.56  ").unwrap(), Datum::Int8(123_456));
        });
    }

    #[test]
    fn money_negative_forms() {
        with_ctx(|ctx| {
            assert_eq!(pg_io::input(ctx, MONEY, "-$1").unwrap(), Datum::Int8(-100));
            assert_eq!(pg_io::input(ctx, MONEY, "(1.00)").unwrap(), Datum::Int8(-100));
            assert_eq!(pg_io::input(ctx, MONEY, "$-1.00").unwrap(), Datum::Int8(-100));
            assert_eq!(pg_io::output(ctx, MONEY, &Datum::Int8(-100)).unwrap(), "-$1.00");
        });
    }

    #[test]
    fn money_rounding_and_fraction_padding() {
        with_ctx(|ctx| {
            // One extra digit (>= 5) rounds the value away from zero when built negative.
            assert_eq!(pg_io::input(ctx, MONEY, "1.005").unwrap(), Datum::Int8(101));
            assert_eq!(pg_io::input(ctx, MONEY, "1.004").unwrap(), Datum::Int8(100));
            assert_eq!(pg_io::input(ctx, MONEY, "1.5").unwrap(), Datum::Int8(150));
            assert_eq!(pg_io::input(ctx, MONEY, "7").unwrap(), Datum::Int8(700));
        });
    }

    #[test]
    fn money_output_grouping() {
        with_ctx(|ctx| {
            assert_eq!(pg_io::output(ctx, MONEY, &Datum::Int8(123_456)).unwrap(), "$1,234.56");
            assert_eq!(pg_io::output(ctx, MONEY, &Datum::Int8(0)).unwrap(), "$0.00");
            assert_eq!(pg_io::output(ctx, MONEY, &Datum::Int8(100_000)).unwrap(), "$1,000.00");
            assert_eq!(pg_io::output(ctx, MONEY, &Datum::Int8(5)).unwrap(), "$0.05");
            assert_eq!(
                pg_io::output(ctx, MONEY, &Datum::Int8(i64::MIN)).unwrap(),
                "-$92,233,720,368,547,758.08"
            );
        });
    }

    #[test]
    fn money_roundtrip() {
        with_ctx(|ctx| {
            let d = pg_io::input(ctx, MONEY, "$1,234.56").unwrap();
            let text = pg_io::output(ctx, MONEY, &d).unwrap();
            assert_eq!(text, "$1,234.56");
            assert_eq!(pg_io::input(ctx, MONEY, &text).unwrap(), d);
        });
    }

    #[test]
    fn money_input_errors() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::input(ctx, MONEY, "abc"));
            assert!(e.sqlstate == SqlState::INVALID_TEXT_REPRESENTATION);
            assert_eq!(e.message, "invalid input syntax for type money: \"abc\"");

            let e = err_of(pg_io::input(ctx, MONEY, "92233720368547758.08"));
            assert!(e.sqlstate == SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
            assert_eq!(e.message, "value \"92233720368547758.08\" is out of range for type money");
        });
    }

    #[test]
    fn money_binary_roundtrip_and_errors() {
        with_ctx(|ctx| {
            let d = Datum::Int8(-123_456);
            let bytes = pg_io::send(ctx, MONEY, &d).unwrap();
            assert_eq!(bytes, (-123_456i64).to_be_bytes().to_vec());
            assert_eq!(pg_io::recv(ctx, MONEY, &bytes).unwrap(), d);

            let e = err_of(pg_io::recv(ctx, MONEY, &bytes[..7]));
            assert!(e.sqlstate == SqlState::PROTOCOL_VIOLATION);
            let mut long = bytes.clone();
            long.push(0);
            let e = err_of(pg_io::recv(ctx, MONEY, &long));
            assert!(e.sqlstate == SqlState::INVALID_BINARY_REPRESENTATION);
        });
    }

    #[test]
    fn aclitem_passes_through_canonical_text() {
        with_ctx(|ctx| {
            assert_eq!(
                pg_io::input(ctx, ACLITEM, "alice=r*w/bob").unwrap(),
                Datum::Text("alice=r*w/bob".to_string())
            );
            assert_eq!(pg_io::output(ctx, ACLITEM, &Datum::Text("alice=r*w/bob".into())).unwrap(), "alice=r*w/bob");
            assert_eq!(pg_io::input(ctx, ACLITEM, "=r/postgres").unwrap(), Datum::Text("=r/postgres".into()));
            assert_eq!(pg_io::input(ctx, ACLITEM, "alice=/bob").unwrap(), Datum::Text("alice=/bob".into()));
        });
    }

    #[test]
    fn aclitem_canonicalizes_privilege_order_and_keyword() {
        with_ctx(|ctx| {
            assert_eq!(pg_io::input(ctx, ACLITEM, "alice=wr/bob").unwrap(), Datum::Text("alice=rw/bob".into()));
            assert_eq!(pg_io::input(ctx, ACLITEM, "group staff=r/bob").unwrap(), Datum::Text("staff=r/bob".into()));
            assert_eq!(pg_io::input(ctx, ACLITEM, "alice=rr/bob").unwrap(), Datum::Text("alice=r/bob".into()));
        });
    }

    #[test]
    fn aclitem_quotes_unsafe_names() {
        with_ctx(|ctx| {
            assert_eq!(pg_io::input(ctx, ACLITEM, "\"a\"\"b\"=r/x").unwrap(), Datum::Text("\"a\"\"b\"=r/x".into()));
            assert_eq!(pg_io::input(ctx, ACLITEM, "\"Bob Smith\"=r/x").unwrap(), Datum::Text("\"Bob Smith\"=r/x".into()));
        });
    }

    #[test]
    fn aclitem_errors() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::input(ctx, ACLITEM, "alice=z/bob"));
            assert!(e.sqlstate == SqlState::INVALID_TEXT_REPRESENTATION);
            assert_eq!(e.message, "invalid mode character: must be one of \"arwdDxtXUCTcsAm\"");

            let e = err_of(pg_io::input(ctx, ACLITEM, "alice=r/bob x"));
            assert!(e.sqlstate == SqlState::INVALID_TEXT_REPRESENTATION);

            let e = err_of(pg_io::input(ctx, ACLITEM, "alice=r/"));
            assert_eq!(e.message, "a name must follow the \"/\" sign");

            let e = err_of(pg_io::input(ctx, ACLITEM, "alice r/bob"));
            assert_eq!(e.message, "unrecognized key word: \"alice\"");

            let e = err_of(pg_io::input(ctx, ACLITEM, "group alice"));
            assert_eq!(e.message, "missing \"=\" sign");

            let e = err_of(pg_io::input(ctx, ACLITEM, "alice=r"));
            assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
        });
    }

    #[test]
    fn aclitem_has_no_binary_functions() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::recv(ctx, ACLITEM, &[]));
            assert!(e.sqlstate == SqlState::UNDEFINED_FUNCTION);
            assert_eq!(e.message, "no binary input function available for type aclitem");
            let e = err_of(pg_io::send(ctx, ACLITEM, &Datum::Text("alice=r/bob".into())));
            assert_eq!(e.message, "no binary output function available for type aclitem");
        });
    }

    #[test]
    fn unsupported_types_keep_the_lease_error() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::input(ctx, POINT, "(1,2)"));
            assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
            assert_eq!(e.message, "type point is not supported by PGArbor yet");
            assert_eq!(e.detail.as_deref(), Some("pgarbor-unimplemented: TYP-42"));
        });
    }

    #[test]
    fn pseudo_types_reject_io_with_pg_texts() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::input(ctx, ANYELEMENT, "x"));
            assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
            assert_eq!(e.message, "cannot accept a value of type anyelement");

            let e = err_of(pg_io::output(ctx, ANYELEMENT, &Datum::Int4(1)));
            assert_eq!(e.message, "cannot display a value of type anyelement");

            let e = err_of(pg_io::recv(ctx, ANYARRAY, &[]));
            assert_eq!(e.message, "cannot accept a value of type anyarray");
        });
    }

    #[test]
    fn anyarray_output_rejects_non_array_datum() {
        with_ctx(|ctx| {
            let e = err_of(pg_io::output(ctx, ANYARRAY, &Datum::Int4(1)));
            assert!(e.sqlstate == SqlState::INTERNAL_ERROR);
        });
    }
}
