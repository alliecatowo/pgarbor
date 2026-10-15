//! Type modifiers: typmodin/typmodout semantics and length/precision coercion (TYPE-03). Lease TYP-02.
//!
//! PG18 reference: `utils/adt/varchar.c` (`anychar_typmodin`, `varchar`, `bpchar` length coercion
//! functions), `numeric.c` (`numerictypmodin`, `numeric` coercion), `date.c`/`timestamp.c`
//! (`anytime_typmodin`, `anytimestamp_typmod_check`, `intervaltypmodin`, `time_scale`,
//! `timestamp_scale`, `interval_scale`), `varbit.c` (bit lengths).
//!
//! Encodings (frozen, PG-identical): varchar/bpchar(n) → n + 4 (VARHDRSZ); numeric(p,s) →
//! `((p << 16) | (s & 0x7ff)) + 4` (s may be negative in PG15+: sign-extend 11 bits when decoding);
//! numeric(p) → scale 0; time/timetz/timestamp/timestamptz(p) → p; interval → `(range << 16) | precision`
//! with range bits from `datetime::interval_fields` and precision 0xFFFF = unspecified; bit(n) → n.
//!
//! `typmod_in(oid, mods)` errors/warnings (oracle-verified):
//! - varchar(0) → 22023 `length for type varchar must be at least 1`; > 10485760 → 22023
//!   `length for type varchar cannot exceed 10485760` (bpchar: same with `char`).
//! - numeric: `NUMERIC precision 1001 must be between 1 and 1000` (22023); scale outside -1000..=1000 →
//!   `NUMERIC scale %d must be between -1000 and 1000`; more than 2 mods → `invalid NUMERIC type modifier`.
//!   (numeric(5,6) is VALID in PG18.)
//! - time/timestamp(p): p < 0 → 22023 `TIMESTAMP(-1) precision must not be negative`
//!   (`TIME(-1) WITH TIME ZONE precision must not be negative`); p > 6 → WARNING (returned, not an
//!   error; severity Warning, SQLSTATE 01000) `TIMESTAMP(7) precision reduced to maximum allowed, 6`,
//!   `TIMESTAMP(7) WITH TIME ZONE precision reduced to maximum allowed, 6`, `TIME(7) precision reduced to
//!   maximum allowed, 6`, and typmod 6 (all oracle-verified).
//! - types without typmodin (text, int4, ...) → 42601 `type modifier is not allowed for type "text"`.
//!
//! `typmod_out(oid, typmod)`: the suffix PG prints: `(10)`, `(10,2)`, `(3)`, interval
//! ` year to month`, `(2)`, ` day to second(3)`.
//!
//! `enforce_typmod(ctx, ty, value, explicit)` (PG length-coercion functions):
//! varchar/bpchar → `text::io::apply_char_typmod`; numeric → `numeric::io::apply_typmod`;
//! time/timetz → `datetime::time::adjust_time_precision`; timestamp/tz →
//! `datetime::timestamp::adjust_timestamp_precision`; interval → `datetime::interval::apply_interval_typmod`;
//! other types → unchanged. NULL → NULL.
//! Owned procs (ImplModule::Typmod): `varchar(varchar, int4, bool)` (prosrc `varchar`), `bpchar(bpchar,
//! int4, bool)`, `numeric(numeric, int4)`, `time_scale`, `timetz_scale`, `timestamp_scale`,
//! `timestamptz_scale`, `interval_scale` — thin wrappers over `enforce_typmod`.
use crate::catalog_data::{self, BuiltinProc};
use crate::context::FuncCtx;
use crate::datetime::interval::apply_interval_typmod;
use crate::datetime::interval_fields as iv;
use crate::datetime::time::adjust_time_precision;
use crate::datetime::timestamp::adjust_timestamp_precision;
use crate::datetime::{MAX_INTERVAL_PRECISION, MAX_TIMESTAMP_PRECISION};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::{FuncImpl, ScalarFn};
use crate::numeric::io::apply_typmod;
use crate::oids;
use crate::text::io::apply_char_typmod;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};

/// PG `VARHDRSZ`: the offset between a varchar/bpchar/numeric typmod and its logical value.
const VARHDRSZ: i32 = 4;
/// PG `MaxAttrSize` (10 MiB).
const MAX_ATTR_SIZE: i32 = 10 * 1024 * 1024;
const NUMERIC_MAX_PRECISION: i32 = 1000;
const NUMERIC_MIN_SCALE: i32 = -1000;
const NUMERIC_MAX_SCALE: i32 = 1000;
/// PG `MAX_TIME_PRECISION`.
const MAX_TIME_PRECISION: i32 = 6;
/// PG `INTERVAL_RANGE_MASK`.
const INTERVAL_RANGE_MASK: i32 = 0x7FFF;
/// PG `INTERVAL_PRECISION_MASK` / `INTERVAL_FULL_PRECISION`.
const INTERVAL_PRECISION_MASK: i32 = 0xFFFF;

/// Interval range masks accepted by `intervaltypmodin` (PG `switch (tl[0])`).
const VALID_INTERVAL_RANGES: [i32; 14] = [
    iv::YEAR,
    iv::MONTH,
    iv::DAY,
    iv::HOUR,
    iv::MINUTE,
    iv::SECOND,
    iv::YEAR | iv::MONTH,
    iv::DAY | iv::HOUR,
    iv::DAY | iv::HOUR | iv::MINUTE,
    iv::DAY | iv::HOUR | iv::MINUTE | iv::SECOND,
    iv::HOUR | iv::MINUTE,
    iv::HOUR | iv::MINUTE | iv::SECOND,
    iv::MINUTE | iv::SECOND,
    iv::FULL_RANGE,
];

/// Interval range → suffix printed by `intervaltypmodout` (PG order of cases).
const INTERVAL_FIELD_NAMES: [(i32, &str); 14] = [
    (iv::YEAR, " year"),
    (iv::MONTH, " month"),
    (iv::DAY, " day"),
    (iv::HOUR, " hour"),
    (iv::MINUTE, " minute"),
    (iv::SECOND, " second"),
    (iv::YEAR | iv::MONTH, " year to month"),
    (iv::DAY | iv::HOUR, " day to hour"),
    (iv::DAY | iv::HOUR | iv::MINUTE, " day to minute"),
    (iv::DAY | iv::HOUR | iv::MINUTE | iv::SECOND, " day to second"),
    (iv::HOUR | iv::MINUTE, " hour to minute"),
    (iv::HOUR | iv::MINUTE | iv::SECOND, " hour to second"),
    (iv::MINUTE | iv::SECOND, " minute to second"),
    (iv::FULL_RANGE, ""),
];

/// Decode a numeric typmod into (precision, scale); None for typmod < 4 (unconstrained).
pub fn numeric_typmod_parts(typmod: i32) -> Option<(i32, i32)> {
    if typmod < 4 {
        return None;
    }
    let t = typmod - 4;
    let precision = (t >> 16) & 0xffff;
    // scale is an 11-bit two's complement field
    let scale = ((t & 0x7ff) ^ 1024) - 1024;
    Some((precision, scale))
}

/// Encode numeric (precision, scale) as a typmod.
pub fn numeric_typmod(precision: i32, scale: i32) -> i32 {
    ((precision << 16) | (scale & 0x7ff)) + 4
}

/// Character length n for a varchar/bpchar typmod, None when unconstrained.
pub fn char_typmod_length(typmod: i32) -> Option<i32> {
    if typmod >= 4 { Some(typmod - 4) } else { None }
}

/// PG typmodin semantics for `oid` with integer modifiers; returns (typmod, warnings).
// CONTRACT
#[allow(unused_variables)]
pub fn typmod_in(oid: Oid, mods: &[i32]) -> Result<(i32, Vec<PgError>)> {
    let mut warnings = Vec::new();
    let typmod = match oid {
        oids::VARCHAR => char_typmod_in(mods, "varchar")?,
        oids::BPCHAR => char_typmod_in(mods, "char")?,
        oids::BIT => bit_typmod_in(mods, "bit")?,
        oids::VARBIT => bit_typmod_in(mods, "varbit")?,
        oids::NUMERIC => numeric_typmod_in(mods)?,
        oids::TIME => datetime_precision_in(mods, "TIME", false, MAX_TIME_PRECISION, &mut warnings)?,
        oids::TIMETZ => datetime_precision_in(mods, "TIME", true, MAX_TIME_PRECISION, &mut warnings)?,
        oids::TIMESTAMP => {
            datetime_precision_in(mods, "TIMESTAMP", false, MAX_TIMESTAMP_PRECISION, &mut warnings)?
        }
        oids::TIMESTAMPTZ => {
            datetime_precision_in(mods, "TIMESTAMP", true, MAX_TIMESTAMP_PRECISION, &mut warnings)?
        }
        oids::INTERVAL => interval_typmod_in(mods, &mut warnings)?,
        _ => return Err(type_modifier_not_allowed(oid)),
    };
    Ok((typmod, warnings))
}

/// PG typmodout suffix for `oid` (empty string when typmod < 0).
// CONTRACT
#[allow(unused_variables)]
pub fn typmod_out(oid: Oid, typmod: i32) -> Result<String> {
    Ok(match oid {
        oids::VARCHAR | oids::BPCHAR => {
            if typmod > VARHDRSZ {
                format!("({})", typmod - VARHDRSZ)
            } else {
                String::new()
            }
        }
        oids::BIT | oids::VARBIT => {
            if typmod >= 0 {
                format!("({typmod})")
            } else {
                String::new()
            }
        }
        oids::NUMERIC => match numeric_typmod_parts(typmod) {
            Some((p, s)) => format!("({p},{s})"),
            None => String::new(),
        },
        // PG `anytime_typmodout` / `anytimestamp_typmodout`: the zone phrase is kept even without a
        // precision, so an unconstrained `time` prints as " without time zone".
        oids::TIME => zone_typmod_out(typmod, " without time zone"),
        oids::TIMETZ => zone_typmod_out(typmod, " with time zone"),
        oids::TIMESTAMP => zone_typmod_out(typmod, " without time zone"),
        oids::TIMESTAMPTZ => zone_typmod_out(typmod, " with time zone"),
        oids::INTERVAL => return interval_typmod_out(typmod),
        _ => String::new(),
    })
}

/// Coerce `value` (already of type `ty.oid`) to `ty.typmod` (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn enforce_typmod(ctx: &FuncCtx, ty: TypeId, value: Datum, explicit: bool) -> Result<Datum> {
    if value.is_null() || ty.typmod < 0 {
        return Ok(value);
    }
    let typmod = ty.typmod;
    match ty.oid {
        oids::VARCHAR | oids::BPCHAR => match value {
            Datum::Text(s) => {
                Ok(Datum::Text(apply_char_typmod(s, typmod, ty.oid == oids::BPCHAR, explicit)?))
            }
            other => Err(type_mismatch("text", &other)),
        },
        oids::NUMERIC => match value {
            Datum::Numeric(n) => Ok(Datum::Numeric(apply_typmod(n, typmod)?)),
            other => Err(type_mismatch("numeric", &other)),
        },
        oids::TIME => match value {
            Datum::Time(t) => Ok(Datum::Time(adjust_time_precision(t, typmod)?)),
            other => Err(type_mismatch("time", &other)),
        },
        oids::TIMETZ => match value {
            Datum::TimeTz(t, zone) => Ok(Datum::TimeTz(adjust_time_precision(t, typmod)?, zone)),
            other => Err(type_mismatch("timetz", &other)),
        },
        oids::TIMESTAMP => match value {
            Datum::Timestamp(ts) => Ok(Datum::Timestamp(adjust_timestamp_precision(ts, typmod)?)),
            other => Err(type_mismatch("timestamp", &other)),
        },
        oids::TIMESTAMPTZ => match value {
            Datum::TimestampTz(ts) => Ok(Datum::TimestampTz(adjust_timestamp_precision(ts, typmod)?)),
            other => Err(type_mismatch("timestamptz", &other)),
        },
        oids::INTERVAL => match value {
            Datum::Interval(v) => Ok(Datum::Interval(apply_interval_typmod(v, typmod)?)),
            other => Err(type_mismatch("interval", &other)),
        },
        _ => Ok(value),
    }
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "varchar" => varchar_scale,
        "bpchar" => bpchar_scale,
        "numeric" => numeric_scale,
        "time_scale" => time_scale,
        "timetz_scale" => timetz_scale,
        "timestamp_scale" => timestamp_scale,
        "timestamptz_scale" => timestamptz_scale,
        "interval_scale" => interval_scale,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Typmod-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-02", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// typmodin helpers
// ---------------------------------------------------------------------------------------------

fn invalid_type_modifier() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid type modifier")
}

fn typmod_warning(message: String) -> PgError {
    PgError::new(SqlState::WARNING, message).with_severity(Severity::Warning)
}

/// 42601 for a type that has no typmodin.
fn type_modifier_not_allowed(oid: Oid) -> PgError {
    let name = catalog_data::builtin_type(oid).map(|t| t.name).unwrap_or("unknown");
    PgError::new(SqlState::SYNTAX_ERROR, format!("type modifier is not allowed for type \"{name}\""))
}

/// `anychar_typmodin`: varchar(n) / char(n) → n + VARHDRSZ.
fn char_typmod_in(mods: &[i32], typename: &str) -> Result<i32> {
    let &[n] = mods else {
        return Err(invalid_type_modifier());
    };
    if n < 1 {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("length for type {typename} must be at least 1"),
        ));
    }
    if n > MAX_ATTR_SIZE {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("length for type {typename} cannot exceed {MAX_ATTR_SIZE}"),
        ));
    }
    Ok(VARHDRSZ + n)
}

/// `anybit_typmodin`: bit(n) / varbit(n) → n (no VARHDRSZ).
fn bit_typmod_in(mods: &[i32], typename: &str) -> Result<i32> {
    let &[n] = mods else {
        return Err(invalid_type_modifier());
    };
    if n < 1 {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("length for type {typename} must be at least 1"),
        ));
    }
    if n > MAX_ATTR_SIZE * 8 {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("length for type {typename} cannot exceed {}", MAX_ATTR_SIZE * 8),
        ));
    }
    Ok(n)
}

/// `numerictypmodin` + `make_numeric_typmod_safe`.
fn numeric_typmod_in(mods: &[i32]) -> Result<i32> {
    let (p, s) = match mods {
        [p, s] => (*p, *s),
        [p] => (*p, 0),
        _ => {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid NUMERIC type modifier"));
        }
    };
    if !(1..=NUMERIC_MAX_PRECISION).contains(&p) {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("NUMERIC precision {p} must be between 1 and {NUMERIC_MAX_PRECISION}"),
        ));
    }
    if !(NUMERIC_MIN_SCALE..=NUMERIC_MAX_SCALE).contains(&s) {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("NUMERIC scale {s} must be between {NUMERIC_MIN_SCALE} and {NUMERIC_MAX_SCALE}"),
        ));
    }
    Ok(numeric_typmod(p, s))
}

/// `anytime_typmodin` / `anytimestamp_typmodin` with `anytime_typmod_check`: the precision is the
/// typmod itself. Values above `max` produce a WARNING and are clamped to `max`.
fn datetime_precision_in(
    mods: &[i32],
    kind: &str,
    istz: bool,
    max: i32,
    warnings: &mut Vec<PgError>,
) -> Result<i32> {
    let &[p] = mods else {
        return Err(invalid_type_modifier());
    };
    let tz = if istz { " WITH TIME ZONE" } else { "" };
    if p < 0 {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{kind}({p}){tz} precision must not be negative"),
        ));
    }
    if p > max {
        warnings.push(typmod_warning(format!("{kind}({p}){tz} precision reduced to maximum allowed, {max}")));
        return Ok(max);
    }
    Ok(p)
}

/// Interval typmod: `(range << 16) | precision`, PG `INTERVAL_TYPMOD`.
fn interval_typmod_encode(range: i32, precision: i32) -> i32 {
    ((range & INTERVAL_RANGE_MASK) << 16) | (precision & INTERVAL_PRECISION_MASK)
}

/// `intervaltypmodin`: `[range]` or `[range, precision]`.
fn interval_typmod_in(mods: &[i32], warnings: &mut Vec<PgError>) -> Result<i32> {
    if let Some(&range) = mods.first() {
        if !VALID_INTERVAL_RANGES.contains(&range) {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid INTERVAL type modifier"));
        }
    }
    match mods {
        [range] => {
            if *range != iv::FULL_RANGE {
                Ok(interval_typmod_encode(*range, iv::FULL_PRECISION))
            } else {
                Ok(-1)
            }
        }
        [range, p] => {
            let (range, p) = (*range, *p);
            if p < 0 {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("INTERVAL({p}) precision must not be negative"),
                ));
            }
            if p > MAX_INTERVAL_PRECISION {
                warnings.push(typmod_warning(format!(
                    "INTERVAL({p}) precision reduced to maximum allowed, {MAX_INTERVAL_PRECISION}"
                )));
                return Ok(interval_typmod_encode(range, MAX_INTERVAL_PRECISION));
            }
            Ok(interval_typmod_encode(range, p))
        }
        _ => Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid INTERVAL type modifier")),
    }
}

// ---------------------------------------------------------------------------------------------
// typmodout helpers
// ---------------------------------------------------------------------------------------------

/// `anytime_typmodout` / `anytimestamp_typmodout`.
fn zone_typmod_out(typmod: i32, zone: &str) -> String {
    if typmod >= 0 { format!("({typmod}){zone}") } else { zone.to_string() }
}

/// `intervaltypmodout`.
fn interval_typmod_out(typmod: i32) -> Result<String> {
    if typmod < 0 {
        return Ok(String::new());
    }
    let fields = (typmod >> 16) & INTERVAL_RANGE_MASK;
    let precision = typmod & INTERVAL_PRECISION_MASK;
    let Some(&(_, fieldstr)) = INTERVAL_FIELD_NAMES.iter().find(|(mask, _)| *mask == fields) else {
        return Err(PgError::internal(format!("invalid INTERVAL typmod: 0x{:x}", typmod as u32)));
    };
    if precision != iv::FULL_PRECISION {
        Ok(format!("{fieldstr}({precision})"))
    } else {
        Ok(fieldstr.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Owned builtin procs (ImplModule::Typmod): thin wrappers over enforce_typmod's helpers.
// Argument 0 is the value; argument 1 is the encoded typmod.
// ---------------------------------------------------------------------------------------------

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("typmod function: missing argument {}", i + 1)))
}

fn typmod_arg(args: &[Datum], i: usize) -> Result<i32> {
    let d = arg(args, i)?;
    d.as_i32().ok_or_else(|| type_mismatch("integer", d))
}

fn explicit_arg(args: &[Datum], i: usize) -> Result<bool> {
    let d = arg(args, i)?;
    d.as_bool().ok_or_else(|| type_mismatch("boolean", d))
}

fn char_scale_impl(args: &[Datum], is_bpchar: bool) -> Result<Datum> {
    let typmod = typmod_arg(args, 1)?;
    let explicit = explicit_arg(args, 2)?;
    match arg(args, 0)? {
        Datum::Text(s) => {
            Ok(Datum::Text(apply_char_typmod(s.clone(), typmod, is_bpchar, explicit)?))
        }
        other => Err(type_mismatch("text", other)),
    }
}

/// `varchar(varchar, int4, bool)`.
fn varchar_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    char_scale_impl(args, false)
}

/// `bpchar(bpchar, int4, bool)`.
fn bpchar_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    char_scale_impl(args, true)
}

/// `numeric(numeric, int4)`.
fn numeric_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let typmod = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::Numeric(n) => Ok(Datum::Numeric(apply_typmod(n.clone(), typmod)?)),
        other => Err(type_mismatch("numeric", other)),
    }
}

/// `time_scale(time, int4)`.
fn time_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let p = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::Time(t) => Ok(Datum::Time(adjust_time_precision(*t, p)?)),
        other => Err(type_mismatch("time", other)),
    }
}

/// `timetz_scale(timetz, int4)`: the zone is kept, only the time part is rounded.
fn timetz_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let p = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::TimeTz(t, zone) => Ok(Datum::TimeTz(adjust_time_precision(*t, p)?, *zone)),
        other => Err(type_mismatch("timetz", other)),
    }
}

/// `timestamp_scale(timestamp, int4)`.
fn timestamp_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let p = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::Timestamp(ts) => Ok(Datum::Timestamp(adjust_timestamp_precision(*ts, p)?)),
        other => Err(type_mismatch("timestamp", other)),
    }
}

/// `timestamptz_scale(timestamptz, int4)`.
fn timestamptz_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let p = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::TimestampTz(ts) => Ok(Datum::TimestampTz(adjust_timestamp_precision(*ts, p)?)),
        other => Err(type_mismatch("timestamptz", other)),
    }
}

/// `interval_scale(interval, int4)`.
fn interval_scale(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let typmod = typmod_arg(args, 1)?;
    match arg(args, 0)? {
        Datum::Interval(v) => Ok(Datum::Interval(apply_interval_typmod(*v, typmod)?)),
        other => Err(type_mismatch("interval", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_data::ImplModule;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::Interval;

    fn with_ctx<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let env = crate::registry::builtin_env();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &io,
            session: &session,
            arg_types: &[],
            result_type: TypeId::UNKNOWN,
            collation: 0,
        };
        f(&ctx)
    }

    #[test]
    fn numeric_typmod_roundtrip() {
        assert_eq!(numeric_typmod_parts(numeric_typmod(10, 2)), Some((10, 2)));
        assert_eq!(numeric_typmod_parts(numeric_typmod(5, -3)), Some((5, -3)));
        assert_eq!(numeric_typmod(10, 2), (10 << 16 | 2) + 4);
        assert_eq!(numeric_typmod_parts(-1), None);
        assert_eq!(char_typmod_length(14), Some(10));
    }

    #[test]
    fn char_typmod_errors_and_encoding() {
        let e = typmod_in(oids::VARCHAR, &[0]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "length for type varchar must be at least 1");
        let e = typmod_in(oids::BPCHAR, &[10_485_761]).unwrap_err();
        assert_eq!(e.message, "length for type char cannot exceed 10485760");
        assert_eq!(typmod_in(oids::VARCHAR, &[3]).unwrap().0, 7);
        assert_eq!(typmod_in(oids::VARCHAR, &[1, 2]).unwrap_err().message, "invalid type modifier");
    }

    #[test]
    fn numeric_typmod_errors() {
        let e = typmod_in(oids::NUMERIC, &[1001]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "NUMERIC precision 1001 must be between 1 and 1000");
        let e = typmod_in(oids::NUMERIC, &[10, 1001]).unwrap_err();
        assert_eq!(e.message, "NUMERIC scale 1001 must be between -1000 and 1000");
        let e = typmod_in(oids::NUMERIC, &[1, 2, 3]).unwrap_err();
        assert_eq!(e.message, "invalid NUMERIC type modifier");
        // numeric(5,6) is valid in PG18
        assert_eq!(typmod_in(oids::NUMERIC, &[5, 6]).unwrap().0, numeric_typmod(5, 6));
        assert_eq!(typmod_in(oids::NUMERIC, &[10]).unwrap().0, numeric_typmod(10, 0));
        assert_eq!(typmod_out(oids::NUMERIC, numeric_typmod(10, 2)).unwrap(), "(10,2)");
        assert_eq!(typmod_out(oids::NUMERIC, -1).unwrap(), "");
    }

    #[test]
    fn timestamp_precision_warnings_and_errors() {
        let (t, w) = typmod_in(oids::TIMESTAMPTZ, &[7]).unwrap();
        assert_eq!(t, 6);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].message, "TIMESTAMP(7) WITH TIME ZONE precision reduced to maximum allowed, 6");
        assert_eq!(w[0].severity, Severity::Warning);
        assert_eq!(w[0].sqlstate, SqlState::WARNING);

        let (t, w) = typmod_in(oids::TIMESTAMP, &[7]).unwrap();
        assert_eq!(t, 6);
        assert_eq!(w[0].message, "TIMESTAMP(7) precision reduced to maximum allowed, 6");

        let (t, w) = typmod_in(oids::TIME, &[7]).unwrap();
        assert_eq!(t, 6);
        assert_eq!(w[0].message, "TIME(7) precision reduced to maximum allowed, 6");

        let e = typmod_in(oids::TIMESTAMP, &[-1]).unwrap_err();
        assert_eq!(e.message, "TIMESTAMP(-1) precision must not be negative");
        let e = typmod_in(oids::TIMETZ, &[-1]).unwrap_err();
        assert_eq!(e.message, "TIME(-1) WITH TIME ZONE precision must not be negative");

        assert_eq!(typmod_in(oids::TIMESTAMPTZ, &[3]).unwrap(), (3, vec![]));
        assert_eq!(typmod_out(oids::TIMESTAMPTZ, 3).unwrap(), "(3) with time zone");
        assert_eq!(typmod_out(oids::TIMESTAMPTZ, -1).unwrap(), " with time zone");
        assert_eq!(typmod_out(oids::TIME, 3).unwrap(), "(3) without time zone");
    }

    #[test]
    fn no_typmodin_is_42601() {
        let e = typmod_in(oids::TEXT, &[1]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "type modifier is not allowed for type \"text\"");
    }

    #[test]
    fn interval_typmod_in_and_out() {
        let (t, w) = typmod_in(oids::INTERVAL, &[iv::YEAR | iv::MONTH]).unwrap();
        assert!(w.is_empty());
        assert_eq!(t, interval_typmod_encode(iv::YEAR | iv::MONTH, iv::FULL_PRECISION));
        assert_eq!(typmod_out(oids::INTERVAL, t).unwrap(), " year to month");

        let (t, _) = typmod_in(oids::INTERVAL, &[iv::DAY | iv::HOUR | iv::MINUTE | iv::SECOND, 3]).unwrap();
        assert_eq!(typmod_out(oids::INTERVAL, t).unwrap(), " day to second(3)");

        assert_eq!(typmod_in(oids::INTERVAL, &[iv::FULL_RANGE]).unwrap().0, -1);
        assert_eq!(typmod_out(oids::INTERVAL, -1).unwrap(), "");

        let e = typmod_in(oids::INTERVAL, &[0]).unwrap_err();
        assert_eq!(e.message, "invalid INTERVAL type modifier");
        let e = typmod_in(oids::INTERVAL, &[iv::YEAR, -1]).unwrap_err();
        assert_eq!(e.message, "INTERVAL(-1) precision must not be negative");

        let (t, w) = typmod_in(oids::INTERVAL, &[iv::SECOND, 9]).unwrap();
        assert_eq!(t, interval_typmod_encode(iv::SECOND, 6));
        assert_eq!(w[0].message, "INTERVAL(9) precision reduced to maximum allowed, 6");
    }

    #[test]
    fn every_typmod_proc_has_an_implementation() {
        let mut n = 0;
        for p in catalog_data::PROCS.iter().filter(|p| p.module == ImplModule::Typmod) {
            n += 1;
            assert!(lookup(p).is_some(), "no implementation for {}", p.name);
        }
        assert_eq!(n, 8);
    }

    #[test]
    fn varchar_length_coercion() {
        with_ctx(|ctx| {
            let ty = TypeId::new(oids::VARCHAR, 7);
            assert_eq!(enforce_typmod(ctx, ty, Datum::text("abcd"), true).unwrap(), Datum::text("abc"));
            // trailing spaces beyond the limit are dropped silently on assignment
            assert_eq!(enforce_typmod(ctx, ty, Datum::text("abc   "), false).unwrap(), Datum::text("abc"));
            let e = enforce_typmod(ctx, ty, Datum::text("abcd"), false).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::STRING_DATA_RIGHT_TRUNCATION);
            assert_eq!(e.message, "value too long for type character varying(3)");
            assert_eq!(enforce_typmod(ctx, ty, Datum::Null, false).unwrap(), Datum::Null);
        });
    }

    #[test]
    fn numeric_coercion_rounds_to_scale() {
        with_ctx(|ctx| {
            let ty = TypeId::new(oids::NUMERIC, numeric_typmod(5, 2));
            let v = crate::io::input(&ctx.io_ctx(), TypeId::NUMERIC, "123.456").unwrap();
            let out = enforce_typmod(ctx, ty, v, false).unwrap();
            assert_eq!(crate::io::output(&ctx.io_ctx(), ty, &out).unwrap(), "123.46");
        });
    }

    #[test]
    #[ignore = "needs TYP-27"]
    fn time_precision_coercion() {
        with_ctx(|ctx| {
            let ty = TypeId::new(oids::TIME, 0);
            assert_eq!(enforce_typmod(ctx, ty, Datum::Time(1_000_001), false).unwrap(), Datum::Time(1_000_000));
        });
    }

    #[test]
    #[ignore = "needs TYP-29"]
    fn interval_typmod_coercion() {
        with_ctx(|ctx| {
            let ty = TypeId::new(oids::INTERVAL, interval_typmod_encode(iv::YEAR, iv::FULL_PRECISION));
            let v = Datum::Interval(Interval::new(14, 3, 3_723_456_789));
            assert_eq!(enforce_typmod(ctx, ty, v, false).unwrap(), Datum::Interval(Interval::new(12, 0, 0)));
        });
    }
}
