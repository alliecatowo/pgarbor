//! Type input/output dispatch (TYPE-01, TYPE-15): text and binary I/O for any type OID.
//!
//! ```ignore
//! let cfg = IoConfig::default();
//! let ctx = IoCtx::new(registry::builtin_env(), &cfg);
//! let d = io::input(&ctx, TypeId::INT4, "42")?;           // Datum::Int4(42)
//! assert_eq!(io::output(&ctx, TypeId::INT4, &d)?, "42");
//! let bytes = io::send(&ctx, TypeId::INT4, &d)?;          // [0,0,0,42]
//! ```
//!
//! Contract for every family module (`scalar::bool`, `scalar::int`, `scalar::float`,
//! `scalar::oid_types`, `numeric::io`, `text::io`, `bytea`, `uuid`, `datetime::{date,time,timestamp,interval}`,
//! `json::io`, `array::io`, `record`, `enum_type`, `extra_types`), each exporting:
//! ```ignore
//! pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum>;     // PG typinput; applies ty.typmod
//! pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String>; // PG typoutput
//! pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum>;  // PG typreceive; applies ty.typmod
//! pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>>; // PG typsend
//! ```
//! `ty.oid` tells a family which member type is meant (e.g. int2 vs int4). NULL never reaches
//! these functions: callers handle SQL NULL (the wire sends length -1); passing `Datum::Null` to
//! `output`/`send` is an internal error. Binary formats are PostgreSQL's network byte order formats
//! (`*send`/`*recv` in PG source); `recv` must consume the WHOLE buffer, otherwise 22P03
//! `incorrect binary data format` (use `invalid_binary("")`; the wire layer reports PG's
//! "incorrect binary data format in bind parameter N", also 22P03).
//!
//! CONTRACT: the public functions of this file are frozen (v1). This file is fully implemented
//! (routing only); behavior lives in the family modules.
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::registry::{self, TypeInfo};
use crate::{array, bytea, datetime, enum_type, extra_types, json, numeric, record, scalar, text, uuid};
use pgarbor_common::{PgError, Result};

/// Implementation family of a type, decided by its `typinput` routine name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoFamily {
    Bool,
    Int,
    Float,
    Numeric,
    OidLike,
    Text,
    Bytea,
    Uuid,
    Date,
    Time,
    Timestamp,
    Interval,
    Json,
    Array,
    Record,
    Enum,
    Domain,
    Void,
    Other,
}

/// Family for a type's `typinput` routine name.
pub fn family_of(info: &TypeInfo) -> IoFamily {
    match info.input {
        "boolin" => IoFamily::Bool,
        "int2in" | "int4in" | "int8in" => IoFamily::Int,
        "float4in" | "float8in" => IoFamily::Float,
        "numeric_in" => IoFamily::Numeric,
        "oidin" | "xidin" | "cidin" | "xid8in" | "regprocin" | "regprocedurein" | "regoperin"
        | "regoperatorin" | "regclassin" | "regtypein" | "regrolein" | "regnamespacein"
        | "regcollationin" | "regconfigin" | "regdictionaryin" | "int2vectorin" | "oidvectorin"
        | "pg_lsn_in" => IoFamily::OidLike,
        "textin" | "varcharin" | "bpcharin" | "namein" | "charin" | "unknownin" | "cstring_in"
        | "pg_node_tree_in" => IoFamily::Text,
        "byteain" => IoFamily::Bytea,
        "uuid_in" => IoFamily::Uuid,
        "date_in" => IoFamily::Date,
        "time_in" | "timetz_in" => IoFamily::Time,
        "timestamp_in" | "timestamptz_in" => IoFamily::Timestamp,
        "interval_in" => IoFamily::Interval,
        "json_in" | "jsonb_in" => IoFamily::Json,
        "array_in" => IoFamily::Array,
        "record_in" => IoFamily::Record,
        "enum_in" => IoFamily::Enum,
        "domain_in" => IoFamily::Domain,
        "void_in" => IoFamily::Void,
        _ => IoFamily::Other,
    }
}

fn info_for(ctx: &IoCtx, ty: TypeId) -> Result<TypeInfo> {
    registry::type_info(ctx.env, ty.oid)
}

/// Domain → (base type with the domain's typmod unless the caller supplied one).
fn domain_base(ctx: &IoCtx, ty: TypeId) -> TypeId {
    let (base, tm) = registry::base_type_and_typmod(ctx.env, ty.oid, ty.typmod);
    TypeId::new(base, if ty.typmod >= 0 { ty.typmod } else { tm })
}

fn null_error(what: &str) -> PgError {
    PgError::internal(format!("{what} called with a NULL datum"))
}

/// Text input (PG `typinput`, e.g. `int4in`), applying `ty.typmod` (varchar length, numeric scale, ...).
/// Domain constraints (NOT NULL / CHECK) are NOT checked here: the executor checks them.
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    let info = info_for(ctx, ty)?;
    match family_of(&info) {
        IoFamily::Bool => scalar::bool::input(ctx, ty, s),
        IoFamily::Int => scalar::int::input(ctx, ty, s),
        IoFamily::Float => scalar::float::input(ctx, ty, s),
        IoFamily::Numeric => numeric::io::input(ctx, ty, s),
        IoFamily::OidLike => scalar::oid_types::input(ctx, ty, s),
        IoFamily::Text => text::io::input(ctx, ty, s),
        IoFamily::Bytea => bytea::input(ctx, ty, s),
        IoFamily::Uuid => uuid::input(ctx, ty, s),
        IoFamily::Date => datetime::date::input(ctx, ty, s),
        IoFamily::Time => datetime::time::input(ctx, ty, s),
        IoFamily::Timestamp => datetime::timestamp::input(ctx, ty, s),
        IoFamily::Interval => datetime::interval::input(ctx, ty, s),
        IoFamily::Json => json::io::input(ctx, ty, s),
        IoFamily::Array => array::io::input(ctx, ty, s),
        IoFamily::Record => record::input(ctx, ty, s),
        IoFamily::Enum => enum_type::input(ctx, ty, s),
        IoFamily::Domain => input(ctx, domain_base(ctx, ty), s),
        IoFamily::Void => Ok(Datum::Null),
        IoFamily::Other => extra_types::input(ctx, ty, s),
    }
}

/// Text output (PG `typoutput`) under `ctx.config` (DateStyle, IntervalStyle, extra_float_digits, ...).
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    if d.is_null() {
        return Err(null_error("type output"));
    }
    let info = info_for(ctx, ty)?;
    match family_of(&info) {
        IoFamily::Bool => scalar::bool::output(ctx, ty, d),
        IoFamily::Int => scalar::int::output(ctx, ty, d),
        IoFamily::Float => scalar::float::output(ctx, ty, d),
        IoFamily::Numeric => numeric::io::output(ctx, ty, d),
        IoFamily::OidLike => scalar::oid_types::output(ctx, ty, d),
        IoFamily::Text => text::io::output(ctx, ty, d),
        IoFamily::Bytea => bytea::output(ctx, ty, d),
        IoFamily::Uuid => uuid::output(ctx, ty, d),
        IoFamily::Date => datetime::date::output(ctx, ty, d),
        IoFamily::Time => datetime::time::output(ctx, ty, d),
        IoFamily::Timestamp => datetime::timestamp::output(ctx, ty, d),
        IoFamily::Interval => datetime::interval::output(ctx, ty, d),
        IoFamily::Json => json::io::output(ctx, ty, d),
        IoFamily::Array => array::io::output(ctx, ty, d),
        IoFamily::Record => record::output(ctx, ty, d),
        IoFamily::Enum => enum_type::output(ctx, ty, d),
        IoFamily::Domain => output(ctx, domain_base(ctx, ty), d),
        IoFamily::Void => Ok(String::new()),
        IoFamily::Other => extra_types::output(ctx, ty, d),
    }
}

/// Binary input (PG `typreceive`), applying `ty.typmod`.
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    let info = info_for(ctx, ty)?;
    match family_of(&info) {
        IoFamily::Bool => scalar::bool::recv(ctx, ty, buf),
        IoFamily::Int => scalar::int::recv(ctx, ty, buf),
        IoFamily::Float => scalar::float::recv(ctx, ty, buf),
        IoFamily::Numeric => numeric::io::recv(ctx, ty, buf),
        IoFamily::OidLike => scalar::oid_types::recv(ctx, ty, buf),
        IoFamily::Text => text::io::recv(ctx, ty, buf),
        IoFamily::Bytea => bytea::recv(ctx, ty, buf),
        IoFamily::Uuid => uuid::recv(ctx, ty, buf),
        IoFamily::Date => datetime::date::recv(ctx, ty, buf),
        IoFamily::Time => datetime::time::recv(ctx, ty, buf),
        IoFamily::Timestamp => datetime::timestamp::recv(ctx, ty, buf),
        IoFamily::Interval => datetime::interval::recv(ctx, ty, buf),
        IoFamily::Json => json::io::recv(ctx, ty, buf),
        IoFamily::Array => array::io::recv(ctx, ty, buf),
        IoFamily::Record => record::recv(ctx, ty, buf),
        IoFamily::Enum => enum_type::recv(ctx, ty, buf),
        IoFamily::Domain => recv(ctx, domain_base(ctx, ty), buf),
        IoFamily::Void => Ok(Datum::Null),
        IoFamily::Other => extra_types::recv(ctx, ty, buf),
    }
}

/// Binary output (PG `typsend`).
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    if d.is_null() {
        return Err(null_error("type send"));
    }
    let info = info_for(ctx, ty)?;
    match family_of(&info) {
        IoFamily::Bool => scalar::bool::send(ctx, ty, d),
        IoFamily::Int => scalar::int::send(ctx, ty, d),
        IoFamily::Float => scalar::float::send(ctx, ty, d),
        IoFamily::Numeric => numeric::io::send(ctx, ty, d),
        IoFamily::OidLike => scalar::oid_types::send(ctx, ty, d),
        IoFamily::Text => text::io::send(ctx, ty, d),
        IoFamily::Bytea => bytea::send(ctx, ty, d),
        IoFamily::Uuid => uuid::send(ctx, ty, d),
        IoFamily::Date => datetime::date::send(ctx, ty, d),
        IoFamily::Time => datetime::time::send(ctx, ty, d),
        IoFamily::Timestamp => datetime::timestamp::send(ctx, ty, d),
        IoFamily::Interval => datetime::interval::send(ctx, ty, d),
        IoFamily::Json => json::io::send(ctx, ty, d),
        IoFamily::Array => array::io::send(ctx, ty, d),
        IoFamily::Record => record::send(ctx, ty, d),
        IoFamily::Enum => enum_type::send(ctx, ty, d),
        IoFamily::Domain => send(ctx, domain_base(ctx, ty), d),
        IoFamily::Void => Ok(Vec::new()),
        IoFamily::Other => extra_types::send(ctx, ty, d),
    }
}

/// Output for possibly-NULL values: `None` for SQL NULL.
pub fn output_opt(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Option<String>> {
    if d.is_null() { Ok(None) } else { output(ctx, ty, d).map(Some) }
}

/// Standard 22P02 error: `invalid input syntax for type <typname>: "<input>"`.
/// `type_display` is the SQL display name used by PG (e.g. "integer", "double precision", "boolean").
pub fn invalid_input_syntax(type_display: &str, input: &str) -> PgError {
    PgError::new(
        pgarbor_common::SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {type_display}: \"{input}\""),
    )
}

/// Standard 22P03 error for malformed binary input (`incorrect binary data format`).
pub fn invalid_binary(what: &str) -> PgError {
    PgError::new(pgarbor_common::SqlState::INVALID_BINARY_REPRESENTATION, format!("incorrect binary data format{what}"))
}
