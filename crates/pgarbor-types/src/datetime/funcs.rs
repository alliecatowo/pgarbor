//! Date/time SQL functions: extract/date_part, date_trunc, date_bin, age, make_*, now(),
//! AT TIME ZONE, overlaps, to_timestamp(float8) (TYPE-14). Lease TYP-30.
//!
//! PG18 reference: `utils/adt/timestamp.c` (`timestamp_part_common`, `timestamp_trunc`,
//! `timestamptz_trunc_internal`, `timestamp_age`, `timestamp_bin`, `make_interval`, `make_timestamp(tz)`,
//! `timestamp_zone`, `timestamptz_zone`, `timestamp_izone`, `overlaps_timestamp`, `float8_timestamptz`,
//! `now`, `statement_timestamp`, `clock_timestamp`), `date.c` (`extract_date`, `time_part`, `timetz_zone`).
//!
//! Owned procs (ImplModule::DatetimeFuncs; `grep 'module: ImplModule::DatetimeFuncs' src/catalog_data/procs.rs`):
//! `extract_date extract_time extract_timetz extract_timestamp extract_timestamptz extract_interval`
//! (EXTRACT → numeric, scale per unit: `extract(epoch from timestamptz '2024-01-01')` = `1704067200.000000`,
//! `extract(second from time '1:2:3.5')` = `3.500000`, `extract(year from date '2024-02-29')` = `2024`,
//! `extract(julian from date '2024-01-01')` = `2460311`), `timestamp_part timestamptz_part time_part
//! timetz_part interval_part` and SQL-language `date_part(text, date)` (oid 1384) (date_part → float8),
//! units: microseconds milliseconds second minute hour day dow isodow doy week month quarter year
//! isoyear decade century millennium epoch julian timezone timezone_hour timezone_minute; unknown unit
//! → 22023 `unit "foo" not recognized for type date` (type display names: `date`, `time without time zone`,
//! `timestamp without time zone`, `timestamp with time zone`, `interval`); unit valid but not supported for
//! the type → 0A000 `unit "x" not supported for type y`;
//! `timestamp_trunc timestamptz_trunc timestamptz_trunc_zone interval_trunc` (`date_trunc('month', ts)`);
//! `timestamp_bin timestamptz_bin` (`date_bin('15 minutes', ts, origin)`; stride with months → 22023
//! `timestamps cannot be binned into intervals containing months or years`);
//! `timestamp_age timestamptz_age` (`age(ts, ts)`: symbolic y/m/d result, `age('2024-02-29','2023-03-31')`
//! = `10 mons 29 days`) and SQL-language `age(timestamp)` (oid 2059) / `age(timestamptz)` (1386)
//! (= age(current date midnight, ts));
//! `make_interval make_timestamp make_timestamptz make_timestamptz_at_timezone`;
//! `now statement_timestamp clock_timestamp` (from `ctx.session`); `float8_timestamptz` (`to_timestamp(double)`;
//! NaN → 22008 `timestamp cannot be NaN`; out of range → 22008 `timestamp out of range: "-1e+20"`);
//! AT TIME ZONE: `timestamp_zone(text, timestamp)` → timestamptz, `timestamptz_zone(text, timestamptz)`
//! → timestamp, `timetz_zone`, interval variants `*_izone`, PG17 `*_at_local`; zone text resolved by
//! `tz::lookup_zone` (POSIX sign for bare numeric strings: `AT TIME ZONE '+05'` means 5h WEST:
//! `timestamptz '2024-02-29 12:00+00' at time zone '+05'` = `2024-02-29 07:00:00`; `'PST'` →
//! `2024-02-29 04:00:00`); `timestamptz_pl_interval_at_zone`, `timestamptz_mi_interval_at_zone`
//! (PG16 date_add/date_subtract); `overlaps_timestamp overlaps_time overlaps_timetz` and SQL-language
//! `overlaps(...)` variants (oids 1305-1311, 2042-2044).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a DatetimeFuncs-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-30", format!("function {}", proc.name))
}
