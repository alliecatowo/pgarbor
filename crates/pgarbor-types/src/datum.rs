//! Runtime SQL values (ARCHITECTURE §8, TYPE-01).
//!
//! A `Datum` never carries its SQL type: the type always travels next to it as a `TypeId`
//! (column descriptors, bound expression types, function signatures). Several SQL types share one
//! variant; the table below is the frozen mapping every crate relies on.
//!
//! | SQL type (OID)                                   | Datum variant                       |
//! |--------------------------------------------------|-------------------------------------|
//! | bool (16)                                        | `Bool`                              |
//! | int2 (21)                                        | `Int2`                              |
//! | int4 (23)                                        | `Int4`                              |
//! | int8 (20), xid8 (5069, bits reinterpreted as u64), pg_lsn (3220, u64 bits) | `Int8`    |
//! | float4 (700) / float8 (701)                      | `Float4` / `Float8`                 |
//! | numeric (1700)                                   | `Numeric`                           |
//! | text (25), varchar (1043), bpchar (1042, stored WITH its blank padding), name (19), unknown (705), cstring (2275), pg_node_tree (194), aclitem (1033, canonical text), enum labels (any enum type) | `Text` |
//! | bytea (17)                                       | `Bytea`                             |
//! | uuid (2950)                                      | `Uuid`                              |
//! | date (1082)                                      | `Date` days since 2000-01-01; `i32::MAX` = 'infinity', `i32::MIN` = '-infinity' |
//! | time (1083)                                      | `Time` µs since midnight, 0..=86_400_000_000 |
//! | timetz (1266)                                    | `TimeTz(µs since midnight, zone)` — zone uses PostgreSQL's convention: seconds WEST of UTC (`+05:30` is stored as `-19800`) |
//! | timestamp (1114) / timestamptz (1184)            | `Timestamp` / `TimestampTz` µs since 2000-01-01 00:00:00 (UTC for tz); `i64::MAX` = 'infinity', `i64::MIN` = '-infinity' |
//! | interval (1186)                                  | `Interval`                          |
//! | json (114)                                       | `Json` (the exact input text, validated) |
//! | jsonb (3802)                                     | `Jsonb` (normalized tree)           |
//! | any array type, int2vector (22), oidvector (30)  | `Array` (vectors have lower bound 0) |
//! | oid (26), regproc (24), regprocedure, regoper, regoperator, regclass, regtype, regrole, regnamespace, regcollation, regconfig, regdictionary, xid (28), cid (29) | `Oid` |
//! | "char" (18)                                      | `Char` (one raw byte; 0 means the empty string) |
//! | record (2249) and composite types                | `Record` (field values in attribute order) |
//!
//! Equality: `#[derive(PartialEq)]` is STRUCTURAL equality for tests and caches only (for floats it
//! is IEEE equality, so `NaN != NaN`). SQL semantics (`NaN = NaN`, `1.0 = 1.00`, `'a ' = 'a'` for
//! bpchar, interval `'1 mon' = '30 days'`) live in `crate::compare`. `Datum` is deliberately not
//! `Eq`/`Hash`; use `compare::hash_datum` for hash tables.
use crate::array::Array;
use crate::json::Jsonb;
use crate::numeric::Numeric;
use crate::oids;
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A SQL type reference: OID plus type modifier (`-1` = none).
///
/// Typmod encodings follow PostgreSQL exactly (see `crate::typmod`):
/// `varchar(n)`/`bpchar(n)` → `n + 4`; `numeric(p,s)` → `((p << 16) | (s & 0x7ff)) + 4`;
/// `time(p)`/`timetz(p)`/`timestamp(p)`/`timestamptz(p)` → `p`; `interval` → `(range << 16) | precision`.
// CONTRACT
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TypeId {
    pub oid: Oid,
    pub typmod: i32,
}

impl TypeId {
    pub const BOOL: TypeId = TypeId::of(oids::BOOL);
    pub const INT2: TypeId = TypeId::of(oids::INT2);
    pub const INT4: TypeId = TypeId::of(oids::INT4);
    pub const INT8: TypeId = TypeId::of(oids::INT8);
    pub const FLOAT4: TypeId = TypeId::of(oids::FLOAT4);
    pub const FLOAT8: TypeId = TypeId::of(oids::FLOAT8);
    pub const NUMERIC: TypeId = TypeId::of(oids::NUMERIC);
    pub const TEXT: TypeId = TypeId::of(oids::TEXT);
    pub const VARCHAR: TypeId = TypeId::of(oids::VARCHAR);
    pub const BPCHAR: TypeId = TypeId::of(oids::BPCHAR);
    pub const NAME: TypeId = TypeId::of(oids::NAME);
    pub const CHAR: TypeId = TypeId::of(oids::CHAR);
    pub const BYTEA: TypeId = TypeId::of(oids::BYTEA);
    pub const UUID: TypeId = TypeId::of(oids::UUID);
    pub const DATE: TypeId = TypeId::of(oids::DATE);
    pub const TIME: TypeId = TypeId::of(oids::TIME);
    pub const TIMETZ: TypeId = TypeId::of(oids::TIMETZ);
    pub const TIMESTAMP: TypeId = TypeId::of(oids::TIMESTAMP);
    pub const TIMESTAMPTZ: TypeId = TypeId::of(oids::TIMESTAMPTZ);
    pub const INTERVAL: TypeId = TypeId::of(oids::INTERVAL);
    pub const JSON: TypeId = TypeId::of(oids::JSON);
    pub const JSONB: TypeId = TypeId::of(oids::JSONB);
    pub const OID: TypeId = TypeId::of(oids::OID);
    pub const REGCLASS: TypeId = TypeId::of(oids::REGCLASS);
    pub const REGTYPE: TypeId = TypeId::of(oids::REGTYPE);
    pub const UNKNOWN: TypeId = TypeId::of(oids::UNKNOWN);
    pub const RECORD: TypeId = TypeId::of(oids::RECORD);
    pub const VOID: TypeId = TypeId::of(oids::VOID);
    pub const TEXT_ARRAY: TypeId = TypeId::of(oids::TEXT_ARRAY);
    pub const INT4_ARRAY: TypeId = TypeId::of(oids::INT4_ARRAY);

    /// Type with explicit typmod.
    pub const fn new(oid: Oid, typmod: i32) -> TypeId {
        TypeId { oid, typmod }
    }
    /// Type without typmod (`typmod = -1`).
    pub const fn of(oid: Oid) -> TypeId {
        TypeId { oid, typmod: -1 }
    }
    /// True when a typmod is attached.
    pub const fn has_typmod(&self) -> bool {
        self.typmod >= 0
    }
    /// Same OID, typmod removed.
    pub const fn without_typmod(&self) -> TypeId {
        TypeId::of(self.oid)
    }
}

impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.typmod >= 0 {
            write!(f, "TypeId({}, typmod {})", self.oid, self.typmod)
        } else {
            write!(f, "TypeId({})", self.oid)
        }
    }
}

/// PostgreSQL interval: three independent fields, never normalized on construction
/// (`'1 mon'` and `'30 days'` are distinct values that compare equal).
///
/// PG17+ infinite intervals: `+infinity` = all three fields at MAX (`i32::MAX`, `i32::MAX`,
/// `i64::MAX`), `-infinity` = all at MIN.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    pub const ZERO: Interval = Interval { months: 0, days: 0, micros: 0 };
    pub const INFINITY: Interval = Interval { months: i32::MAX, days: i32::MAX, micros: i64::MAX };
    pub const NEG_INFINITY: Interval = Interval { months: i32::MIN, days: i32::MIN, micros: i64::MIN };

    pub const fn new(months: i32, days: i32, micros: i64) -> Interval {
        Interval { months, days, micros }
    }
    pub fn is_infinite(&self) -> bool {
        *self == Interval::INFINITY || *self == Interval::NEG_INFINITY
    }
    /// PostgreSQL's `interval_cmp_value`: the 128-bit span used for comparison, hashing and keys,
    /// with 1 month = 30 days and 1 day = 24 hours. Infinities map to their field values naturally
    /// (they are larger/smaller than every finite span).
    pub fn cmp_span(&self) -> i128 {
        const USECS_PER_DAY: i128 = 86_400_000_000;
        let days = self.months as i128 * 30 + self.days as i128;
        days * USECS_PER_DAY + self.micros as i128
    }
}

/// A runtime SQL value. See the module docs for the type → variant mapping.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Numeric),
    Text(String),
    Bytea(Vec<u8>),
    Uuid([u8; 16]),
    Date(i32),
    Time(i64),
    /// (µs since midnight, zone in seconds WEST of UTC).
    TimeTz(i64, i32),
    Timestamp(i64),
    TimestampTz(i64),
    Interval(Interval),
    Json(String),
    Jsonb(Jsonb),
    Array(Box<Array>),
    Oid(u32),
    Char(u8),
    Record(Vec<Datum>),
}

/// `Date` value for 'infinity'.
pub const DATE_INFINITY: i32 = i32::MAX;
/// `Date` value for '-infinity'.
pub const DATE_NEG_INFINITY: i32 = i32::MIN;
/// `Timestamp`/`TimestampTz` value for 'infinity'.
pub const TIMESTAMP_INFINITY: i64 = i64::MAX;
/// `Timestamp`/`TimestampTz` value for '-infinity'.
pub const TIMESTAMP_NEG_INFINITY: i64 = i64::MIN;

impl Datum {
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }
    /// SQL NULL-aware boolean view: `Null` → `None`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Datum::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_i16(&self) -> Option<i16> {
        match self {
            Datum::Int2(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Datum::Int4(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Datum::Float8(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Datum::Text(s) | Datum::Json(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Datum::Bytea(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_numeric(&self) -> Option<&Numeric> {
        match self {
            Datum::Numeric(n) => Some(n),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Array> {
        match self {
            Datum::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_jsonb(&self) -> Option<&Jsonb> {
        match self {
            Datum::Jsonb(j) => Some(j),
            _ => None,
        }
    }
    pub fn as_oid(&self) -> Option<u32> {
        match self {
            Datum::Oid(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_interval(&self) -> Option<Interval> {
        match self {
            Datum::Interval(v) => Some(*v),
            _ => None,
        }
    }
    /// Wrap a string as `Text`.
    pub fn text(s: impl Into<String>) -> Datum {
        Datum::Text(s.into())
    }
    /// Wrap an array.
    pub fn array(a: Array) -> Datum {
        Datum::Array(Box::new(a))
    }
    /// Short variant name for diagnostics ("Int4", "Text", ...).
    pub fn variant_name(&self) -> &'static str {
        match self {
            Datum::Null => "Null",
            Datum::Bool(_) => "Bool",
            Datum::Int2(_) => "Int2",
            Datum::Int4(_) => "Int4",
            Datum::Int8(_) => "Int8",
            Datum::Float4(_) => "Float4",
            Datum::Float8(_) => "Float8",
            Datum::Numeric(_) => "Numeric",
            Datum::Text(_) => "Text",
            Datum::Bytea(_) => "Bytea",
            Datum::Uuid(_) => "Uuid",
            Datum::Date(_) => "Date",
            Datum::Time(_) => "Time",
            Datum::TimeTz(..) => "TimeTz",
            Datum::Timestamp(_) => "Timestamp",
            Datum::TimestampTz(_) => "TimestampTz",
            Datum::Interval(_) => "Interval",
            Datum::Json(_) => "Json",
            Datum::Jsonb(_) => "Jsonb",
            Datum::Array(_) => "Array",
            Datum::Oid(_) => "Oid",
            Datum::Char(_) => "Char",
            Datum::Record(_) => "Record",
        }
    }
}

/// Internal-error helper for "the binder promised type X but the executor passed variant Y".
/// This is a PGArbor bug (XX000), never a user error.
pub fn type_mismatch(expected: &str, got: &Datum) -> pgarbor_common::PgError {
    pgarbor_common::PgError::internal(format!(
        "datum type mismatch: expected {expected}, got {}",
        got.variant_name()
    ))
}
