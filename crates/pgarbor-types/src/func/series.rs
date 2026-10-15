//! `generate_series` (TYPE-14). Lease TYP-44.
//!
//! PG18 reference: `utils/adt/int.c` (`generate_series_int4`, `_step_int4`), `int8.c`, `numeric.c`
//! (`generate_series_numeric`, `_step_numeric`), `timestamp.c` (`generate_series_timestamp`,
//! `generate_series_timestamptz`, `generate_series_timestamptz_at_zone`).
//! Owned procs (ImplModule::Series, all `FuncImpl::Set`, lazily iterating — `generate_series(1, 1e9)`
//! must not allocate the whole set): `generate_series_int4 generate_series_step_int4 generate_series_int8
//! generate_series_step_int8 generate_series_numeric generate_series_step_numeric generate_series_timestamp
//! generate_series_timestamptz generate_series_timestamptz_at_zone`.
//!
//! Semantics: inclusive bounds; step default 1; step 0 → 22023 `step size cannot equal zero`; negative
//! step counts down; empty when start > stop with positive step. Integer iteration stops before
//! overflow (no error: `generate_series(2147483646, 2147483647)` yields 2 rows). numeric: NaN start →
//! 22023 `start value cannot be NaN`, NaN stop → `stop value cannot be NaN`, NaN step → `step size cannot be NaN`;
//! infinite start/stop → 22023 `start value cannot be infinity` / `stop value cannot be infinity`
//! (step infinity → `step size cannot be infinity`); result dscale = max(start, step dscales).
//! timestamp: interval step (zero interval → 22023 `step size cannot equal zero`; infinite step →
//! `step size cannot be infinite`); timestamptz arithmetic in the session zone (or the given zone).
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, TimeZoneSetting};
use crate::datetime::timestamp::timestamp_add_interval;
use crate::datetime::tz::{self, ZoneSpec};
use crate::datetime::{dt_parse_error, out_of_range, DtErr};
use crate::datum::{type_mismatch, Datum, Interval};
use crate::func::{FuncImpl, SetFn, SetIter};
use crate::numeric::arith;
use crate::numeric::Numeric;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: SetFn = match proc.prosrc {
        "generate_series_int4" | "generate_series_step_int4" => series_int4,
        "generate_series_int8" | "generate_series_step_int8" => series_int8,
        "generate_series_numeric" | "generate_series_step_numeric" => series_numeric,
        "generate_series_timestamp" => series_timestamp,
        "generate_series_timestamptz" => series_timestamptz,
        "generate_series_timestamptz_at_zone" => series_timestamptz_at_zone,
        _ => return None,
    };
    Some(FuncImpl::Set(f))
}

/// 0A000 for a Series-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-44", format!("function {}", proc.name))
}

const USECS_PER_SEC: i64 = 1_000_000;

fn invalid_param(msg: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg)
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("generate_series: missing argument {}", i + 1)))
}

// ---------------------------------------------------------------------------------------------
// integer series (int4 / int8)
// ---------------------------------------------------------------------------------------------

fn int_arg(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Int4(v) => Ok(i64::from(*v)),
        Datum::Int8(v) => Ok(*v),
        other => Err(type_mismatch("integer", other)),
    }
}

#[derive(Clone, Copy)]
enum IntKind {
    Int4,
    Int8,
}

/// Integer iterator. `cur` is kept in i64, which is wide enough for both widths: every emitted value
/// lies between `start` and `finish`, and an int8 step that would overflow ends the series, as in PG.
struct IntSeries {
    kind: IntKind,
    cur: i64,
    finish: i64,
    step: i64,
    done: bool,
}

impl Iterator for IntSeries {
    type Item = Result<Datum>;

    fn next(&mut self) -> Option<Result<Datum>> {
        if self.done {
            return None;
        }
        let more = if self.step > 0 { self.cur <= self.finish } else { self.cur >= self.finish };
        if !more {
            self.done = true;
            return None;
        }
        let value = self.cur;
        match self.cur.checked_add(self.step) {
            Some(next) => self.cur = next,
            // Overflow: this is the final value (PG sets step to 0).
            None => self.done = true,
        }
        Some(Ok(match self.kind {
            IntKind::Int4 => Datum::Int4(value as i32),
            IntKind::Int8 => Datum::Int8(value),
        }))
    }
}

fn int_series(kind: IntKind, args: &[Datum]) -> Result<SetIter> {
    let start = int_arg(args, 0)?;
    let finish = int_arg(args, 1)?;
    let step = if args.len() >= 3 { int_arg(args, 2)? } else { 1 };
    if step == 0 {
        return Err(invalid_param("step size cannot equal zero"));
    }
    Ok(Box::new(IntSeries { kind, cur: start, finish, step, done: false }))
}

fn series_int4(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    int_series(IntKind::Int4, args)
}

fn series_int8(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    int_series(IntKind::Int8, args)
}

// ---------------------------------------------------------------------------------------------
// numeric series
// ---------------------------------------------------------------------------------------------

fn numeric_arg(args: &[Datum], i: usize) -> Result<&Numeric> {
    match arg(args, i)? {
        Datum::Numeric(n) => Ok(n),
        other => Err(type_mismatch("numeric", other)),
    }
}

/// Numeric iterator. Values are finite (specials are rejected up front). Each emitted value keeps the
/// display scale of `cur`; `arith::add` produces max(dscale) for the next value, as PG's `add_var` does.
struct NumSeries {
    cur: Numeric,
    stop: Numeric,
    step: Numeric,
    ascending: bool,
    done: bool,
}

impl Iterator for NumSeries {
    type Item = Result<Datum>;

    fn next(&mut self) -> Option<Result<Datum>> {
        if self.done {
            return None;
        }
        let ord = self.cur.pg_cmp(&self.stop);
        let more = if self.ascending { ord != Ordering::Greater } else { ord != Ordering::Less };
        if !more {
            self.done = true;
            return None;
        }
        // Compute the increment before emitting, so an overflow error replaces the row (as in PG).
        let next = match arith::add(&self.cur, &self.step) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        let value = std::mem::replace(&mut self.cur, next);
        Some(Ok(Datum::Numeric(value)))
    }
}

fn series_numeric(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let start = numeric_arg(args, 0)?;
    let stop = numeric_arg(args, 1)?;
    if start.is_nan() {
        return Err(invalid_param("start value cannot be NaN"));
    }
    if start.is_infinite() {
        return Err(invalid_param("start value cannot be infinity"));
    }
    if stop.is_nan() {
        return Err(invalid_param("stop value cannot be NaN"));
    }
    if stop.is_infinite() {
        return Err(invalid_param("stop value cannot be infinity"));
    }
    let step = if args.len() >= 3 {
        let s = numeric_arg(args, 2)?;
        if s.is_nan() {
            return Err(invalid_param("step size cannot be NaN"));
        }
        if s.is_infinite() {
            return Err(invalid_param("step size cannot be infinity"));
        }
        s.clone()
    } else {
        Numeric::from_i64(1)
    };
    let sign = step.pg_cmp(&Numeric::zero());
    if sign == Ordering::Equal {
        return Err(invalid_param("step size cannot equal zero"));
    }
    Ok(Box::new(NumSeries {
        cur: start.clone(),
        stop: stop.clone(),
        step,
        ascending: sign == Ordering::Greater,
        done: false,
    }))
}

// ---------------------------------------------------------------------------------------------
// timestamp / timestamptz series
// ---------------------------------------------------------------------------------------------

/// The zone used for timestamptz interval arithmetic.
enum Zone {
    /// The session `TimeZone` setting (`IoConfig::time_zone`).
    Session(TimeZoneSetting),
    /// A fixed offset from `AT TIME ZONE`, in seconds EAST of UTC.
    Fixed(i32),
}

impl Zone {
    /// Offset (seconds east) at UTC instant `utc` (µs since 2000-01-01).
    fn offset_at_utc(&self, utc: i64) -> Result<i64> {
        match self {
            Zone::Session(setting) => Ok(i64::from(tz::offset_at_utc(setting, utc)?)),
            Zone::Fixed(secs) => Ok(i64::from(*secs)),
        }
    }

    /// Offset (seconds east) for local wall-clock time `local` (µs since 2000-01-01).
    fn offset_at_local(&self, local: i64) -> Result<i64> {
        match self {
            Zone::Session(setting) => Ok(i64::from(tz::offset_at_local(setting, local)?)),
            Zone::Fixed(secs) => Ok(i64::from(*secs)),
        }
    }
}

/// PG `timestamptz_pl_interval_internal`: months and days move the wall-clock time in `zone`, the
/// micro part is added to the instant. Infinite values are returned unchanged.
///
/// Single conversion round trip: PG converts after the months step and again after the days step.
/// The two differ only across a DST transition inside the interval, and zones with DST rules are
/// not shipped yet (they fail with 0A000), so fixed and session-UTC zones give identical results.
fn timestamptz_add_interval(zone: &Zone, ts: i64, iv: &Interval) -> Result<i64> {
    if ts == i64::MAX || ts == i64::MIN {
        return Ok(ts);
    }
    let mut result = ts;
    if iv.months != 0 || iv.days != 0 {
        let off = zone.offset_at_utc(ts)?.checked_mul(USECS_PER_SEC).ok_or_else(|| out_of_range("timestamp"))?;
        let local = ts.checked_add(off).ok_or_else(|| out_of_range("timestamp"))?;
        let shifted = timestamp_add_interval(local, &Interval::new(iv.months, iv.days, 0))?;
        let back = zone.offset_at_local(shifted)?.checked_mul(USECS_PER_SEC).ok_or_else(|| out_of_range("timestamp"))?;
        result = shifted.checked_sub(back).ok_or_else(|| out_of_range("timestamp"))?;
    }
    result.checked_add(iv.micros).ok_or_else(|| out_of_range("timestamp"))
}

#[derive(Clone, Copy)]
enum TsKind {
    Timestamp,
    TimestampTz,
}

/// Timestamp iterator. Each increment is computed before the current value is emitted, so an overflow
/// error replaces the row (as in PG). `zone` is only used for `TimestampTz`.
struct TsSeries {
    kind: TsKind,
    zone: Option<Zone>,
    cur: i64,
    finish: i64,
    step: Interval,
    ascending: bool,
    done: bool,
}

impl TsSeries {
    fn advance(&self, ts: i64) -> Result<i64> {
        match self.kind {
            TsKind::Timestamp => timestamp_add_interval(ts, &self.step),
            TsKind::TimestampTz => match &self.zone {
                Some(zone) => timestamptz_add_interval(zone, ts, &self.step),
                None => Err(PgError::internal("generate_series: timestamptz series without a zone")),
            },
        }
    }
}

impl Iterator for TsSeries {
    type Item = Result<Datum>;

    fn next(&mut self) -> Option<Result<Datum>> {
        if self.done {
            return None;
        }
        let more = if self.ascending { self.cur <= self.finish } else { self.cur >= self.finish };
        if !more {
            self.done = true;
            return None;
        }
        let next = match self.advance(self.cur) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        let value = std::mem::replace(&mut self.cur, next);
        Some(Ok(match self.kind {
            TsKind::Timestamp => Datum::Timestamp(value),
            TsKind::TimestampTz => Datum::TimestampTz(value),
        }))
    }
}

fn ts_series(kind: TsKind, zone: Option<Zone>, start: i64, finish: i64, step: Interval) -> Result<SetIter> {
    let span = step.cmp_span();
    if span == 0 {
        return Err(invalid_param("step size cannot equal zero"));
    }
    if step.is_infinite() {
        return Err(invalid_param("step size cannot be infinite"));
    }
    Ok(Box::new(TsSeries { kind, zone, cur: start, finish, step, ascending: span > 0, done: false }))
}

fn ts_arg(args: &[Datum], i: usize) -> Result<i64> {
    match arg(args, i)? {
        Datum::Timestamp(v) | Datum::TimestampTz(v) => Ok(*v),
        other => Err(type_mismatch("timestamp", other)),
    }
}

fn interval_arg(args: &[Datum], i: usize) -> Result<Interval> {
    match arg(args, i)? {
        Datum::Interval(iv) => Ok(*iv),
        other => Err(type_mismatch("interval", other)),
    }
}

fn series_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let start = ts_arg(args, 0)?;
    let finish = ts_arg(args, 1)?;
    let step = interval_arg(args, 2)?;
    ts_series(TsKind::Timestamp, None, start, finish, step)
}

fn series_timestamptz(args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    let start = ts_arg(args, 0)?;
    let finish = ts_arg(args, 1)?;
    let step = interval_arg(args, 2)?;
    let zone = Zone::Session(ctx.io.time_zone.clone());
    ts_series(TsKind::TimestampTz, Some(zone), start, finish, step)
}

/// PG `lookup_timezone` for the zone argument of `generate_series(timestamptz, timestamptz, interval, text)`.
/// Bare numeric zones use POSIX signs (`'+05'` is 5 hours WEST), which `tz::lookup_zone` implements.
fn lookup_at_zone(name: &str) -> Result<Zone> {
    let lower = name.to_ascii_lowercase();
    match tz::lookup_zone(&lower)? {
        Ok(ZoneSpec::Fixed { secs_east, .. }) => Ok(Zone::Fixed(secs_east)),
        Ok(ZoneSpec::Region(region)) => Err(dt_parse_error(&DtErr::UnsupportedTimezone(region), name, "timestamp with time zone")),
        Err(e) => Err(dt_parse_error(&e, name, "timestamp with time zone")),
    }
}

fn series_timestamptz_at_zone(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let start = ts_arg(args, 0)?;
    let finish = ts_arg(args, 1)?;
    let step = interval_arg(args, 2)?;
    let zone = match arg(args, 3)? {
        Datum::Text(name) => lookup_at_zone(name)?,
        other => return Err(type_mismatch("text", other)),
    };
    ts_series(TsKind::TimestampTz, Some(zone), start, finish, step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::func;
    use crate::registry;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;

    const GS_INT4_2: u32 = 1067;
    const GS_INT4_3: u32 = 1066;
    const GS_INT8_2: u32 = 1069;
    const GS_INT8_3: u32 = 1068;
    const GS_NUMERIC_2: u32 = 3260;
    const GS_TIMESTAMP: u32 = 938;
    const GS_TIMESTAMPTZ: u32 = 939;
    const GS_TIMESTAMPTZ_ZONE: u32 = 6274;

    fn collect(oid: u32, args: &[Datum]) -> Result<Vec<Datum>> {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::INT4,
            collation: 0,
        };
        func::implementation(oid)?.call_set(args, &ctx)?.collect::<Result<Vec<Datum>>>()
    }

    fn num(s: &str) -> Numeric {
        Numeric::Finite(BigDecimal::from_str(s).unwrap())
    }

    #[test]
    fn int4_ascending_and_empty() {
        let rows = collect(GS_INT4_2, &[Datum::Int4(1), Datum::Int4(3)]).unwrap();
        assert_eq!(rows, vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3)]);
        let rows = collect(GS_INT4_2, &[Datum::Int4(3), Datum::Int4(1)]).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn int4_negative_step() {
        let rows = collect(GS_INT4_3, &[Datum::Int4(3), Datum::Int4(1), Datum::Int4(-1)]).unwrap();
        assert_eq!(rows, vec![Datum::Int4(3), Datum::Int4(2), Datum::Int4(1)]);
    }

    #[test]
    fn int4_zero_step_is_22023() {
        let err = collect(GS_INT4_3, &[Datum::Int4(1), Datum::Int4(10), Datum::Int4(0)]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "step size cannot equal zero");
    }

    #[test]
    fn int4_stops_before_overflow_without_error() {
        let rows = collect(GS_INT4_2, &[Datum::Int4(2147483646), Datum::Int4(2147483647)]).unwrap();
        assert_eq!(rows, vec![Datum::Int4(2147483646), Datum::Int4(2147483647)]);
    }

    #[test]
    fn int8_stops_before_overflow_without_error() {
        let rows = collect(GS_INT8_2, &[Datum::Int8(i64::MAX - 1), Datum::Int8(i64::MAX)]).unwrap();
        assert_eq!(rows, vec![Datum::Int8(i64::MAX - 1), Datum::Int8(i64::MAX)]);
        let rows = collect(GS_INT8_3, &[Datum::Int8(1), Datum::Int8(10), Datum::Int8(4)]).unwrap();
        assert_eq!(rows, vec![Datum::Int8(1), Datum::Int8(5), Datum::Int8(9)]);
    }

    #[test]
    fn numeric_nan_and_infinity_are_rejected() {
        let err = collect(GS_NUMERIC_2, &[Datum::Numeric(Numeric::NaN), Datum::Numeric(num("1"))]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "start value cannot be NaN");
        let err = collect(GS_NUMERIC_2, &[Datum::Numeric(num("1")), Datum::Numeric(Numeric::PosInf)]).unwrap_err();
        assert_eq!(err.message, "stop value cannot be infinity");
    }

    #[test]
    #[ignore = "needs TYP-10"]
    fn numeric_fractional_start() {
        let rows = collect(GS_NUMERIC_2, &[Datum::Numeric(num("1.5")), Datum::Numeric(num("3"))]).unwrap();
        assert_eq!(rows, vec![Datum::Numeric(num("1.5")), Datum::Numeric(num("2.5"))]);
    }

    #[test]
    #[ignore = "needs TYP-28"]
    fn timestamp_by_one_day_is_inclusive() {
        let day = 86_400 * USECS_PER_SEC;
        let rows = collect(
            GS_TIMESTAMP,
            &[Datum::Timestamp(0), Datum::Timestamp(2 * day), Datum::Interval(Interval::new(0, 1, 0))],
        )
        .unwrap();
        assert_eq!(rows, vec![Datum::Timestamp(0), Datum::Timestamp(day), Datum::Timestamp(2 * day)]);
    }

    #[test]
    #[ignore = "needs TYP-28"]
    fn timestamptz_session_utc_by_one_day() {
        let day = 86_400 * USECS_PER_SEC;
        let rows = collect(
            GS_TIMESTAMPTZ,
            &[Datum::TimestampTz(0), Datum::TimestampTz(day), Datum::Interval(Interval::new(0, 1, 0))],
        )
        .unwrap();
        assert_eq!(rows, vec![Datum::TimestampTz(0), Datum::TimestampTz(day)]);
    }

    #[test]
    fn timestamp_zero_interval_is_22023() {
        let err = collect(
            GS_TIMESTAMP,
            &[Datum::Timestamp(0), Datum::Timestamp(1), Datum::Interval(Interval::ZERO)],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "step size cannot equal zero");
    }

    #[test]
    fn timestamp_infinite_interval_is_22023() {
        let err = collect(
            GS_TIMESTAMPTZ,
            &[Datum::TimestampTz(0), Datum::TimestampTz(1), Datum::Interval(Interval::INFINITY)],
        )
        .unwrap_err();
        assert_eq!(err.message, "step size cannot be infinite");
    }

    #[test]
    #[ignore = "needs TYP-25"]
    fn timestamptz_unknown_zone_is_22023() {
        let err = collect(
            GS_TIMESTAMPTZ_ZONE,
            &[
                Datum::TimestampTz(0),
                Datum::TimestampTz(1),
                Datum::Interval(Interval::new(0, 1, 0)),
                Datum::Text("Nowhere/Nothing".to_string()),
            ],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }
}
