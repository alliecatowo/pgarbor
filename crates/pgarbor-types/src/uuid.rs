//! `uuid` I/O and functions (TYPE-07). Lease TYP-20.
//!
//! PG18 reference: `utils/adt/uuid.c` (`string_to_uuid`, `uuid_out`, `gen_random_uuid`, `uuidv7`,
//! `uuid_extract_version`, `uuid_extract_timestamp`).
//!
//! Text input (`string_to_uuid`): optional surrounding `{` `}`; 32 hex digits (either case); a `-`
//! may follow ANY group of 4 hex digits (after digit 4, 8, 12, ... — so `a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11`
//! and `a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11` are valid); no other characters, no surrounding whitespace.
//! Error → 22P02 `invalid input syntax for type uuid: "<input>"`.
//! Output: lowercase `8-4-4-4-12`. Binary: 16 raw bytes (recv: exactly 16, else 22P03).
//!
//! Owned procs (ImplModule::Uuid): `uuid_eq uuid_ne uuid_lt uuid_le uuid_gt uuid_ge uuid_cmp`
//! (memcmp order), `uuid_hash(_extended)`, `gen_random_uuid` and `uuidv4` (16 bytes from
//! `ctx.session.fill_random`, then set version nibble 4 (byte 6 = (b & 0x0f) | 0x40) and RFC 4122
//! variant (byte 8 = (b & 0x3f) | 0x80)), `uuidv7` / `uuidv7_interval` (PG18: 48-bit big-endian Unix ms
//! from `ctx.session.clock_timestamp()` (+ interval shift), 12-bit sub-millisecond fraction in
//! `rand_a` per PG's method, version 7, variant, random tail), `uuid_extract_version` (NULL unless RFC
//! variant), `uuid_extract_timestamp` (v1 and v7 only, else NULL; returns timestamptz).
//!
//! Dependencies (frozen signatures of other leases): `uuid_hash(_extended)` call
//! `compare::pg_hash_bytes(_extended)` (TYP-49); `uuidv7_interval` calls
//! `datetime::timestamp::timestamp_add_interval` (TYP-28) and, for a non-UTC `TimeZone`, the
//! `datetime::tz` offset functions (TYP-25). Until those land they return their own 0A000 errors.
use crate::catalog_data::BuiltinProc;
use crate::compare::{pg_hash_bytes, pg_hash_bytes_extended};
use crate::context::{FuncCtx, IoCtx, TimeZoneSetting};
use crate::datetime::{timestamp, tz};
use crate::datum::{Datum, Interval, TypeId, type_mismatch};
use crate::func::FuncImpl;
use crate::io::invalid_binary;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicI64, Ordering as MemOrdering};

/// `POSTGRES_EPOCH_JDATE` (2000-01-01).
const POSTGRES_EPOCH_JDATE: i64 = 2451545;
/// `UNIX_EPOCH_JDATE` (1970-01-01).
const UNIX_EPOCH_JDATE: i64 = 2440588;
/// `GREGORIAN_EPOCH_JDATE` (1582-10-15), the origin of UUID version 1 timestamps.
const GREGORIAN_EPOCH_JDATE: i64 = 2299161;
const NS_PER_MS: i64 = 1_000_000;
const NS_PER_US: i64 = 1_000;
const US_PER_MS: i64 = 1_000;
/// `PG_UNIX_EPOCH_OFFSET_US`: microseconds from 1970-01-01 to 2000-01-01.
const PG_UNIX_EPOCH_OFFSET_US: i64 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * 86_400 * 1_000_000;
/// Microseconds from 1582-10-15 to 2000-01-01 (UUID v1 origin, `uuid_extract_timestamp`).
const GREGORIAN_TO_PG_EPOCH_US: i64 = (POSTGRES_EPOCH_JDATE - GREGORIAN_EPOCH_JDATE) * 86_400 * 1_000_000;
/// `UUIDV7_MIN_TIMESTAMP` / `UUIDV7_MAX_TIMESTAMP` in PostgreSQL-epoch microseconds.
const UUIDV7_MIN_TIMESTAMP: i64 = -PG_UNIX_EPOCH_OFFSET_US;
const UUIDV7_MAX_TIMESTAMP: i64 = ((1i64 << 48) - 1) * US_PER_MS - PG_UNIX_EPOCH_OFFSET_US;
/// Sub-millisecond precision bits kept in `rand_a` (PG `SUBMS_BITS`).
const SUBMS_BITS: u32 = 12;
/// `SUBMS_MINIMAL_STEP_NS` for `SUBMS_MINIMAL_STEP_BITS == 12` (the non-Darwin build).
const SUBMS_MINIMAL_STEP_NS: i64 = NS_PER_MS / (1 << 12) + 1;

/// Last nanosecond timestamp handed out by `uuidv7`. PG keeps the same static in
/// `get_real_time_ns_ascending` so that successive UUIDs never move backwards.
static UUIDV7_PREV_NS: AtomicI64 = AtomicI64::new(0);

/// Parse uuid text per module rules.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    parse_uuid_bytes(s).ok_or_else(|| invalid_uuid_text(s))
}

/// Canonical lowercase hyphenated form.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// `uuid_in`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Ok(Datum::Uuid(parse_uuid(s)?))
}

/// `uuid_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    match d {
        Datum::Uuid(u) => Ok(format_uuid(u)),
        other => Err(type_mismatch("uuid", other)),
    }
}

/// `uuid_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    if buf.len() != 16 {
        return Err(invalid_binary(""));
    }
    let mut u = [0u8; 16];
    u.copy_from_slice(buf);
    Ok(Datum::Uuid(u))
}

/// `uuid_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match d {
        Datum::Uuid(u) => Ok(u.to_vec()),
        other => Err(type_mismatch("uuid", other)),
    }
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    // None of the Uuid-module procs is SQL-language (empty prosrc), so the C symbol name is the key.
    // `uuidv4` and `gen_random_uuid` share prosrc; `uuidv7` (0 args) and `uuidv7_interval` differ by prosrc.
    let imp = match proc.prosrc {
        "uuid_eq" => FuncImpl::Scalar(uuid_eq),
        "uuid_ne" => FuncImpl::Scalar(uuid_ne),
        "uuid_lt" => FuncImpl::Scalar(uuid_lt),
        "uuid_le" => FuncImpl::Scalar(uuid_le),
        "uuid_gt" => FuncImpl::Scalar(uuid_gt),
        "uuid_ge" => FuncImpl::Scalar(uuid_ge),
        "uuid_cmp" => FuncImpl::Scalar(uuid_cmp),
        "uuid_hash" => FuncImpl::Scalar(uuid_hash),
        "uuid_hash_extended" => FuncImpl::Scalar(uuid_hash_extended),
        "gen_random_uuid" => FuncImpl::Scalar(gen_random_uuid),
        "uuidv7" => FuncImpl::Scalar(uuidv7),
        "uuidv7_interval" => FuncImpl::Scalar(uuidv7_interval),
        "uuid_extract_version" => FuncImpl::Scalar(uuid_extract_version),
        "uuid_extract_timestamp" => FuncImpl::Scalar(uuid_extract_timestamp),
        _ => return None,
    };
    Some(imp)
}

/// 0A000 for a Uuid-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-20", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Text input
// ---------------------------------------------------------------------------------------------

fn invalid_uuid_text(s: &str) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type uuid: \"{s}\""))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Port of `string_to_uuid`: returns None on any syntax error.
fn parse_uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    let braces = b.first() == Some(&b'{');
    let mut p = usize::from(braces);
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(*b.get(p)?)?;
        let lo = hex_val(*b.get(p + 1)?)?;
        *slot = (hi << 4) | lo;
        p += 2;
        // A hyphen may follow every group of four hex digits, except after the last byte.
        if b.get(p) == Some(&b'-') && i % 2 == 1 && i < 15 {
            p += 1;
        }
    }
    if braces {
        if b.get(p) != Some(&b'}') {
            return None;
        }
        p += 1;
    }
    if p != b.len() {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Argument helpers (arguments of strict procs are non-NULL; a mismatch is an internal error)
// ---------------------------------------------------------------------------------------------

fn uuid_arg(args: &[Datum], i: usize) -> Result<&[u8; 16]> {
    match args.get(i) {
        Some(Datum::Uuid(u)) => Ok(u),
        Some(other) => Err(type_mismatch("uuid", other)),
        None => Err(PgError::internal("uuid function called without its argument")),
    }
}

fn int8_arg(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Int8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("int8", other)),
        None => Err(PgError::internal("uuid function called without its argument")),
    }
}

fn interval_arg(args: &[Datum], i: usize) -> Result<Interval> {
    match args.get(i) {
        Some(Datum::Interval(iv)) => Ok(*iv),
        Some(other) => Err(type_mismatch("interval", other)),
        None => Err(PgError::internal("uuid function called without its argument")),
    }
}

/// `memcmp` result of two 16-byte values: the difference of the first differing bytes, else 0
/// (glibc's `memcmp` returns the byte difference).
fn memcmp_result(a: &[u8; 16], b: &[u8; 16]) -> i32 {
    for (x, y) in a.iter().zip(b.iter()) {
        if x != y {
            return i32::from(*x) - i32::from(*y);
        }
    }
    0
}

fn uuid_order(args: &[Datum]) -> Result<Ordering> {
    let a = uuid_arg(args, 0)?;
    let b = uuid_arg(args, 1)?;
    Ok(a.cmp(b))
}

// ---------------------------------------------------------------------------------------------
// Comparison and hashing
// ---------------------------------------------------------------------------------------------

fn uuid_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_eq()))
}

fn uuid_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_ne()))
}

fn uuid_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_lt()))
}

fn uuid_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_le()))
}

fn uuid_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_gt()))
}

fn uuid_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(uuid_order(args)?.is_ge()))
}

fn uuid_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = uuid_arg(args, 0)?;
    let b = uuid_arg(args, 1)?;
    Ok(Datum::Int4(memcmp_result(a, b)))
}

fn uuid_hash(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let u = uuid_arg(args, 0)?;
    // hash_any(data, 16) returns uint32; the int4 result carries its bits.
    Ok(Datum::Int4(pg_hash_bytes(u)? as i32))
}

fn uuid_hash_extended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let u = uuid_arg(args, 0)?;
    let seed = int8_arg(args, 1)?;
    Ok(Datum::Int8(pg_hash_bytes_extended(u, seed as u64)? as i64))
}

// ---------------------------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------------------------

/// RFC 9562 version and variant bits, as `uuid_set_version`.
fn set_uuid_version(u: &mut [u8; 16], version: u8) {
    u[6] = (u[6] & 0x0f) | (version << 4);
    u[8] = (u[8] & 0x3f) | 0x80;
}

fn gen_random_uuid(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let mut u = [0u8; 16];
    ctx.session.fill_random(&mut u);
    set_uuid_version(&mut u, 4);
    Ok(Datum::Uuid(u))
}

/// `get_real_time_ns_ascending`: wall clock in nanoseconds since the Unix epoch, forced to advance
/// by at least `SUBMS_MINIMAL_STEP_NS` per call. The clock is the session clock (microsecond
/// resolution), so the nanosecond part is only the monotonic step.
fn real_time_ns_ascending(ctx: &FuncCtx) -> i64 {
    let unix_us = ctx.session.clock_timestamp().saturating_add(PG_UNIX_EPOCH_OFFSET_US);
    let now_ns = unix_us.saturating_mul(NS_PER_US);
    let mut prev = UUIDV7_PREV_NS.load(MemOrdering::Relaxed);
    loop {
        let next = if prev.saturating_add(SUBMS_MINIMAL_STEP_NS) >= now_ns {
            prev.saturating_add(SUBMS_MINIMAL_STEP_NS)
        } else {
            now_ns
        };
        match UUIDV7_PREV_NS.compare_exchange_weak(prev, next, MemOrdering::Relaxed, MemOrdering::Relaxed) {
            Ok(_) => return next,
            Err(current) => prev = current,
        }
    }
}

/// `generate_uuidv7`: 48-bit big-endian Unix milliseconds, a 12-bit sub-millisecond fraction in
/// `rand_a`, version 7, RFC variant and a random tail.
fn generate_uuidv7(ctx: &FuncCtx, unix_ts_ms: u64, sub_ms_ns: u32) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0] = (unix_ts_ms >> 40) as u8;
    u[1] = (unix_ts_ms >> 32) as u8;
    u[2] = (unix_ts_ms >> 24) as u8;
    u[3] = (unix_ts_ms >> 16) as u8;
    u[4] = (unix_ts_ms >> 8) as u8;
    u[5] = unix_ts_ms as u8;
    let increased = (u64::from(sub_ms_ns) << SUBMS_BITS) / (NS_PER_MS as u64);
    u[6] = (increased >> 8) as u8;
    u[7] = increased as u8;
    ctx.session.fill_random(&mut u[8..]);
    set_uuid_version(&mut u, 7);
    u
}

fn uuidv7(_args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ns = real_time_ns_ascending(ctx);
    Ok(Datum::Uuid(generate_uuidv7(ctx, (ns / NS_PER_MS) as u64, (ns % NS_PER_MS) as u32)))
}

fn uuidv7_interval(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let shift = interval_arg(args, 0)?;
    if shift.is_infinite() {
        return Err(PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "interval out of range for UUID version 7",
        )
        .with_detail("UUID version 7 does not support infinite intervals."));
    }
    let ns = real_time_ns_ascending(ctx);
    // Current time as a TimestampTz (PostgreSQL epoch, microseconds), then shifted.
    let ts = (ns / NS_PER_US).wrapping_sub(PG_UNIX_EPOCH_OFFSET_US);
    let ts = timestamptz_add_interval(ctx, ts, &shift)?;
    if !(UUIDV7_MIN_TIMESTAMP..=UUIDV7_MAX_TIMESTAMP).contains(&ts) {
        return Err(PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "timestamp out of range for UUID version 7",
        )
        .with_detail("UUID version 7 supports timestamps from 1970-01-01 to approximately year 10889."));
    }
    let us = ts.wrapping_add(PG_UNIX_EPOCH_OFFSET_US);
    let sub_ms = ((us % US_PER_MS) * NS_PER_US + ns % NS_PER_US) as u32;
    Ok(Datum::Uuid(generate_uuidv7(ctx, (us / US_PER_MS) as u64, sub_ms)))
}

/// `timestamptz_pl_interval` in the session zone: months and days are applied to the local wall
/// clock, then the microseconds, then the result is converted back to UTC.
fn timestamptz_add_interval(ctx: &FuncCtx, ts: i64, iv: &Interval) -> Result<i64> {
    let off_utc = zone_offset_at_utc(&ctx.io.time_zone, ts)?;
    let local = ts.wrapping_add(i64::from(off_utc) * 1_000_000);
    let new_local = timestamp::timestamp_add_interval(local, iv)?;
    let off_local = zone_offset_at_local(&ctx.io.time_zone, new_local)?;
    Ok(new_local.wrapping_sub(i64::from(off_local) * 1_000_000))
}

/// Seconds east of UTC at a UTC instant. UTC needs no tz rules; other settings use the zone code.
fn zone_offset_at_utc(zone: &TimeZoneSetting, ts: i64) -> Result<i32> {
    match zone {
        TimeZoneSetting::Utc => Ok(0),
        TimeZoneSetting::Fixed { secs_east, .. } => Ok(*secs_east),
        TimeZoneSetting::Named(_) => tz::offset_at_utc(zone, ts),
    }
}

/// Seconds east of UTC for a local wall-clock time.
fn zone_offset_at_local(zone: &TimeZoneSetting, local_ts: i64) -> Result<i32> {
    match zone {
        TimeZoneSetting::Utc => Ok(0),
        TimeZoneSetting::Fixed { secs_east, .. } => Ok(*secs_east),
        TimeZoneSetting::Named(_) => tz::offset_at_local(zone, local_ts),
    }
}

// ---------------------------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------------------------

fn uuid_extract_version(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let u = uuid_arg(args, 0)?;
    if (u[8] & 0xc0) != 0x80 {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int2(i16::from(u[6] >> 4)))
}

fn uuid_extract_timestamp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let u = uuid_arg(args, 0)?;
    if (u[8] & 0xc0) != 0x80 {
        return Ok(Datum::Null);
    }
    match u[6] >> 4 {
        1 => {
            // 60-bit count of 100 ns intervals since 1582-10-15, split as in RFC 9562.
            let tms: u64 = (u64::from(u[0]) << 24)
                + (u64::from(u[1]) << 16)
                + (u64::from(u[2]) << 8)
                + u64::from(u[3])
                + (u64::from(u[4]) << 40)
                + (u64::from(u[5]) << 32)
                + ((u64::from(u[6]) & 0xf) << 56)
                + (u64::from(u[7]) << 48);
            let ts = (tms / 10) as i64 - GREGORIAN_TO_PG_EPOCH_US;
            Ok(Datum::TimestampTz(ts))
        }
        7 => {
            // 48-bit big-endian Unix milliseconds.
            let tms: u64 = u64::from(u[5])
                + (u64::from(u[4]) << 8)
                + (u64::from(u[3]) << 16)
                + (u64::from(u[2]) << 24)
                + (u64::from(u[1]) << 32)
                + (u64::from(u[0]) << 40);
            let ts = (tms as i64) * US_PER_MS - PG_UNIX_EPOCH_OFFSET_US;
            Ok(Datum::TimestampTz(ts))
        }
        _ => Ok(Datum::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry;

    const SAMPLE: [u8; 16] = [
        0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a, 0x11,
    ];

    fn uuid_of(d: Datum) -> [u8; 16] {
        match d {
            Datum::Uuid(u) => u,
            other => panic!("expected uuid, got {other:?}"),
        }
    }

    #[test]
    fn parse_accepted_forms() {
        assert_eq!(parse_uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap(), SAMPLE);
        assert_eq!(parse_uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11").unwrap(), SAMPLE);
        assert_eq!(parse_uuid("{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}").unwrap(), SAMPLE);
        assert_eq!(parse_uuid("a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11").unwrap(), SAMPLE);
        assert_eq!(parse_uuid("a0eebc99-9c0b4ef8-bb6d-6bb9bd380a11").unwrap(), SAMPLE);
        assert_eq!(parse_uuid("a0eebc999c0b4ef8bb6d6bb9bd380a11").unwrap(), SAMPLE);
    }

    #[test]
    fn parse_rejected_forms() {
        for bad in [
            "a0eebc999c0b4ef8bb6d6bb9bd380a1",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11 ",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "g0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "",
        ] {
            let e = parse_uuid(bad).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22P02", "input {bad:?}");
            assert_eq!(e.message, format!("invalid input syntax for type uuid: \"{bad}\""));
        }
    }

    #[test]
    fn text_roundtrip() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let d = input(&ctx, TypeId::UUID, "{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}").unwrap();
        assert_eq!(output(&ctx, TypeId::UUID, &d).unwrap(), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    }

    #[test]
    fn binary_io() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let d = Datum::Uuid(SAMPLE);
        let bytes = send(&ctx, TypeId::UUID, &d).unwrap();
        assert_eq!(bytes, SAMPLE.to_vec());
        assert_eq!(recv(&ctx, TypeId::UUID, &bytes).unwrap(), d);
        let e = recv(&ctx, TypeId::UUID, &bytes[..15]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22P03");
    }

    #[test]
    fn gen_random_uuid_v4_bits() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        for _ in 0..16 {
            let u = uuid_of(gen_random_uuid(&[], &ctx).unwrap());
            assert_eq!(u[6] >> 4, 4);
            assert_eq!(u[8] & 0xc0, 0x80);
        }
    }

    #[test]
    fn uuidv7_version_and_order() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let mut prev: Option<[u8; 16]> = None;
        for _ in 0..50 {
            let u = uuid_of(uuidv7(&[], &ctx).unwrap());
            assert_eq!(u[6] >> 4, 7);
            assert_eq!(u[8] & 0xc0, 0x80);
            if let Some(p) = prev {
                assert!(p[..8] <= u[..8], "uuidv7 moved backwards");
            }
            prev = Some(u);
            let v = uuid_of(Datum::Uuid(u));
            let version = uuid_extract_version(&[Datum::Uuid(v)], &ctx).unwrap();
            assert_eq!(version, Datum::Int2(7));
        }
    }

    #[test]
    fn uuidv7_timestamp_from_session_clock() {
        let sess = TestSession { now: 0, ..TestSession::default() };
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let u = uuid_of(uuidv7(&[], &ctx).unwrap());
        match uuid_extract_timestamp(&[Datum::Uuid(u)], &ctx).unwrap() {
            // Session clock 0 is 2000-01-01; the UUID carries that millisecond (plus a tiny step).
            Datum::TimestampTz(ts) => assert!((0..1_000_000).contains(&ts), "ts {ts}"),
            other => panic!("expected timestamptz, got {other:?}"),
        }
    }

    #[test]
    fn uuidv7_interval_infinite_rejected() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let e = uuidv7_interval(&[Datum::Interval(Interval::INFINITY)], &ctx).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22008");
    }

    #[test]
    #[ignore = "needs TYP-28 (timestamp_add_interval)"]
    fn uuidv7_interval_shifts_by_a_day() {
        let sess = TestSession { now: 0, ..TestSession::default() };
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let u = uuid_of(uuidv7_interval(&[Datum::Interval(Interval::new(0, 1, 0))], &ctx).unwrap());
        match uuid_extract_timestamp(&[Datum::Uuid(u)], &ctx).unwrap() {
            Datum::TimestampTz(ts) => assert!((86_400_000_000..86_400_000_000 + 1_000_000).contains(&ts)),
            other => panic!("expected timestamptz, got {other:?}"),
        }
    }

    #[test]
    fn extract_version_and_timestamp_null_cases() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        // SAMPLE has variant bits 10 and version 4: version is 4, no timestamp.
        assert_eq!(uuid_extract_version(&[Datum::Uuid(SAMPLE)], &ctx).unwrap(), Datum::Int2(4));
        assert_eq!(uuid_extract_timestamp(&[Datum::Uuid(SAMPLE)], &ctx).unwrap(), Datum::Null);
        // Non-RFC variant (byte 8 top bits not 10) is NULL for both.
        let mut nonrfc = SAMPLE;
        nonrfc[8] = 0x00;
        assert_eq!(uuid_extract_version(&[Datum::Uuid(nonrfc)], &ctx).unwrap(), Datum::Null);
        assert_eq!(uuid_extract_timestamp(&[Datum::Uuid(nonrfc)], &ctx).unwrap(), Datum::Null);
    }

    #[test]
    fn extract_timestamp_v1() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        // Version 1 UUID whose 100 ns count is the Unix epoch (0x01B21DD213814000) plus 946684800 s,
        // i.e. exactly 2000-01-01 00:00:00 UTC, which is PostgreSQL timestamp 0.
        let u: [u8; 16] = [
            0x63, 0xb0, 0x00, 0x00, 0xbf, 0xde, 0x11, 0xd3, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(uuid_extract_version(&[Datum::Uuid(u)], &ctx).unwrap(), Datum::Int2(1));
        assert_eq!(uuid_extract_timestamp(&[Datum::Uuid(u)], &ctx).unwrap(), Datum::TimestampTz(0));
    }

    #[test]
    fn comparisons_are_memcmp_order() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let mut hi = SAMPLE;
        hi[0] = 0xff;
        let args = [Datum::Uuid(SAMPLE), Datum::Uuid(hi)];
        assert_eq!(uuid_lt(&args, &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(uuid_gt(&args, &ctx).unwrap(), Datum::Bool(false));
        assert_eq!(uuid_eq(&args, &ctx).unwrap(), Datum::Bool(false));
        assert_eq!(uuid_ne(&args, &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(uuid_le(&args, &ctx).unwrap(), Datum::Bool(true));
        assert_eq!(uuid_ge(&args, &ctx).unwrap(), Datum::Bool(false));
        assert_eq!(uuid_cmp(&args, &ctx).unwrap(), Datum::Int4(0xa0 - 0xff));
        let same = [Datum::Uuid(SAMPLE), Datum::Uuid(SAMPLE)];
        assert_eq!(uuid_cmp(&same, &ctx).unwrap(), Datum::Int4(0));
        assert_eq!(uuid_eq(&same, &ctx).unwrap(), Datum::Bool(true));
    }

    #[test]
    #[ignore = "needs TYP-49 (pg_hash_bytes)"]
    fn hash_procs_run() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::UUID,
            collation: 0,
        };
        let a = uuid_hash(&[Datum::Uuid(SAMPLE)], &ctx).unwrap();
        let b = uuid_hash(&[Datum::Uuid(SAMPLE)], &ctx).unwrap();
        assert_eq!(a, b);
        assert!(matches!(uuid_hash_extended(&[Datum::Uuid(SAMPLE), Datum::Int8(0)], &ctx).unwrap(), Datum::Int8(_)));
    }

    #[test]
    fn registry_resolves_uuid_procs() {
        // uuidv4 (6428) and gen_random_uuid (3432) share the C symbol; uuidv7 0-arg is 6429.
        for oid in [2956u32, 2960, 3432, 6428, 6429, 6430, 6343, 6342] {
            assert!(crate::func::implementation(oid).is_ok(), "oid {oid}");
        }
    }
}
