//! Evaluation contexts: the ONLY way type code reaches catalog state, session state, clocks,
//! randomness or engine services. pgarbor-types never depends on pgarbor-catalog; instead the
//! catalog implements `TypeEnv` and the session/executor implements `SessionContext`.
//!
//! - `TypeEnv`: type metadata (builtins + user enums/domains/composites), reg* name lookups.
//!   Needed at bind time (resolution, casts) and at run time (I/O, comparison of enums/records).
//! - `SessionContext`: impure run-time services (now(), random(), notices, external functions).
//! - `IoConfig`: the output-affecting GUCs (DateStyle, IntervalStyle, TimeZone, extra_float_digits,
//!   bytea_output). `IoConfig::default()` is the pinned oracle configuration (ARCHITECTURE §0.6).
//! - `IoCtx` / `FuncCtx`: cheap borrowed bundles passed to I/O and function implementations.
//!
//! CONTRACT: all items in this file are frozen (v1).
use crate::catalog_data::{BuiltinCast, BuiltinProc};
use crate::datum::{Datum, TypeId};
use crate::func::SetIter;
use crate::registry::TypeInfo;
use pgarbor_common::{Oid, PgError, Result};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

/// One label of an enum type (`pg_enum` row).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct EnumLabel {
    /// pg_enum row OID.
    pub oid: Oid,
    pub label: String,
    /// `enumsortorder`: labels compare by this value, not by OID or text.
    pub sort_order: f32,
}

/// One attribute of a composite type (in attnum order, dropped columns included and flagged).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CompositeField {
    pub name: String,
    pub type_id: TypeId,
    pub dropped: bool,
}

/// Which reg* namespace a name lookup targets.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegKind {
    /// regclass: relation name (search_path aware, may be schema-qualified, may be quoted).
    Class,
    /// regtype: type name in SQL syntax (`integer`, `character varying`, `public.mytype`).
    Type,
    /// regproc: function name only (must be unique).
    Proc,
    /// regprocedure: `name(argtypes)`.
    Procedure,
    /// regoper: operator name (must be unique).
    Oper,
    /// regoperator: `+(integer,integer)`.
    Operator,
    Namespace,
    Role,
    Collation,
    Config,
    Dictionary,
}

/// Type metadata provider. Implemented by `registry::BuiltinTypeEnv` (builtins only) and by
/// pgarbor-catalog's snapshot (builtins + user types). All methods must be cheap and pure.
// CONTRACT
pub trait TypeEnv {
    /// Metadata for a type OID (base, array, enum, domain, composite, pseudo), or None if unknown.
    fn type_info(&self, oid: Oid) -> Option<TypeInfo>;
    /// Resolve a type by its catalog `typname` (NOT SQL aliases; see `crate::names` for those),
    /// optionally schema-qualified. Unqualified names follow the session search_path
    /// (pg_catalog implicitly first). Returns None if not found.
    fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid>;
    /// Enum labels in `enumsortorder` order, or None if `oid` is not an enum type.
    fn enum_labels(&self, _oid: Oid) -> Option<Arc<[EnumLabel]>> {
        None
    }
    /// Attributes of a composite type (by the composite TYPE oid), or None.
    fn composite_fields(&self, _type_oid: Oid) -> Option<Arc<[CompositeField]>> {
        None
    }
    /// User-defined (`CREATE CAST`) casts; builtin pg_cast rows are consulted separately.
    fn user_cast(&self, _source: Oid, _target: Oid) -> Option<BuiltinCast> {
        None
    }
    /// Resolve a reg* input name (`'pg_class'::regclass`). `Ok(None)` = not found (the caller raises
    /// the PG error, e.g. 42P01 `relation "x" does not exist`). The default resolves nothing.
    fn reg_lookup(&self, _kind: RegKind, _name: &str) -> Result<Option<Oid>> {
        Ok(None)
    }
    /// Display name for a reg* output (`'1259'::regclass` → `pg_class`), quoted/qualified as PG
    /// would print it, or None if the OID does not exist (output then prints the bare number).
    fn reg_name(&self, _kind: RegKind, _oid: Oid) -> Option<String> {
        None
    }
}

/// Run-time services. Implementations use interior mutability (methods take `&self`).
// CONTRACT
pub trait SessionContext {
    /// `statement_timestamp()` as a TimestampTz value (µs since 2000-01-01 UTC).
    fn statement_timestamp(&self) -> i64;
    /// `transaction_timestamp()` / `now()` / `CURRENT_TIMESTAMP`.
    fn transaction_timestamp(&self) -> i64;
    /// `clock_timestamp()`: wall clock at call time.
    fn clock_timestamp(&self) -> i64;
    /// Fill `buf` with cryptographically strong random bytes (`gen_random_uuid`, `uuidv4/7`).
    fn fill_random(&self, buf: &mut [u8]);
    /// Session PRNG for `random()` & friends: uniform in [0, 1).
    fn random_f64(&self) -> f64;
    /// `setseed(seed)`: reseed the session PRNG (`seed` already validated to [-1, 1]).
    fn set_seed(&self, seed: f64);
    /// Emit a NOTICE/WARNING (PgError with severity Notice/Warning).
    fn notice(&self, notice: PgError);
    /// Evaluate a proc whose `module` is `ImplModule::External` (catalog/session/engine functions such
    /// as `nextval`, `current_user`, `pg_get_expr`, `has_table_privilege`). Strictness is already
    /// handled by the caller (`func::FuncHandle::call`).
    fn call_external(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum>;
    /// Set-returning variant of `call_external` (e.g. `pg_get_keywords`, `pg_options_to_table`).
    fn call_external_set(&self, proc: &BuiltinProc, args: &[Datum]) -> Result<SetIter>;
}

/// `DateStyle` output format component.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateStyle {
    #[default]
    Iso,
    Postgres,
    Sql,
    German,
}

/// `DateStyle` field-order component (input disambiguation + Postgres/SQL output).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateOrder {
    #[default]
    Mdy,
    Dmy,
    Ymd,
}

/// `IntervalStyle`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IntervalStyle {
    #[default]
    Postgres,
    PostgresVerbose,
    SqlStandard,
    Iso8601,
}

/// `bytea_output`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ByteaOutput {
    #[default]
    Hex,
    Escape,
}

/// Session `TimeZone` setting (parsed by `datetime::tz::parse_timezone_setting`).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum TimeZoneSetting {
    /// `UTC` / `GMT` / `Etc/UTC` / `Z` (the pinned oracle setting).
    #[default]
    Utc,
    /// A fixed offset, e.g. `SET TIME ZONE '+05:30'` or `INTERVAL '-08:00' HOUR TO MINUTE`.
    /// `secs_east` is ISO-sign (east of Greenwich positive: `+05:30` → 19800). `name` is what SHOW prints.
    Fixed { name: String, secs_east: i32 },
    /// An IANA zone name (`America/New_York`). Requires tz rules PGArbor does not ship yet: every
    /// conversion through it returns 0A000 (see contracts/crates/pgarbor-types.md, known gaps).
    Named(String),
}

/// GUC-derived I/O configuration. `Default` = pinned oracle settings:
/// `DateStyle=ISO, MDY`, `IntervalStyle=postgres`, `TimeZone=UTC`, `extra_float_digits=1`, `bytea_output=hex`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoConfig {
    pub date_style: DateStyle,
    pub date_order: DateOrder,
    pub interval_style: IntervalStyle,
    /// -15..=3. Values > 0 select shortest-round-trip float output (PG12+); <= 0 select `%.{15+n}g`.
    pub extra_float_digits: i32,
    pub bytea_output: ByteaOutput,
    pub time_zone: TimeZoneSetting,
}

impl Default for IoConfig {
    fn default() -> IoConfig {
        IoConfig {
            date_style: DateStyle::Iso,
            date_order: DateOrder::Mdy,
            interval_style: IntervalStyle::Postgres,
            extra_float_digits: 1,
            bytea_output: ByteaOutput::Hex,
            time_zone: TimeZoneSetting::Utc,
        }
    }
}

/// Borrowed context for type input/output.
// CONTRACT
#[derive(Clone, Copy)]
pub struct IoCtx<'a> {
    pub env: &'a dyn TypeEnv,
    pub config: &'a IoConfig,
    /// Transaction start timestamp (TimestampTz µs) for the special inputs 'now', 'today',
    /// 'tomorrow', 'yesterday'. `None` (e.g. decoding outside a session) makes those inputs fail with
    /// 0A000 rather than guessing a clock.
    pub now: Option<i64>,
}

impl<'a> IoCtx<'a> {
    pub fn new(env: &'a dyn TypeEnv, config: &'a IoConfig) -> IoCtx<'a> {
        IoCtx { env, config, now: None }
    }
    /// Same context with the transaction timestamp set.
    pub fn with_now(self, now: i64) -> IoCtx<'a> {
        IoCtx { now: Some(now), ..self }
    }
}

/// Borrowed context for a function / operator / cast / aggregate invocation.
// CONTRACT
#[derive(Clone, Copy)]
pub struct FuncCtx<'a> {
    pub env: &'a dyn TypeEnv,
    pub io: &'a IoConfig,
    pub session: &'a dyn SessionContext,
    /// Actual (resolved, non-polymorphic) argument types of THIS call, parallel to `args`.
    /// Polymorphic implementations (`to_jsonb(anyelement)`, `array_append`, `pg_typeof`) read them.
    pub arg_types: &'a [TypeId],
    /// Resolved result type of this call (typmod included when known).
    pub result_type: TypeId,
    /// Input collation OID (0 = none/default). v1 treats every collation as C/codepoint order.
    pub collation: Oid,
}

impl<'a> FuncCtx<'a> {
    /// The I/O context derived from this call context.
    pub fn io_ctx(&self) -> IoCtx<'a> {
        IoCtx { env: self.env, config: self.io, now: Some(self.session.transaction_timestamp()) }
    }
    /// Same context with different argument / result types (used when one implementation calls another).
    pub fn with_types(&self, arg_types: &'a [TypeId], result_type: TypeId) -> FuncCtx<'a> {
        FuncCtx { arg_types, result_type, ..*self }
    }
    /// Actual type of argument `i`, or `unknown` if the caller did not supply it.
    pub fn arg_type(&self, i: usize) -> TypeId {
        self.arg_types.get(i).copied().unwrap_or(TypeId::UNKNOWN)
    }
}

/// A deterministic `SessionContext` for unit tests and for embedders that need no engine services:
/// fixed timestamps, xorshift PRNG, notices collected in memory, external functions unsupported.
#[derive(Debug)]
pub struct TestSession {
    pub now: i64,
    pub rng_state: Cell<u64>,
    pub notices: RefCell<Vec<PgError>>,
}

impl Default for TestSession {
    fn default() -> TestSession {
        TestSession { now: 0, rng_state: Cell::new(0x9E37_79B9_7F4A_7C15), notices: RefCell::new(Vec::new()) }
    }
}

impl TestSession {
    fn next_u64(&self) -> u64 {
        let mut x = self.rng_state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state.set(x);
        x
    }
}

impl SessionContext for TestSession {
    fn statement_timestamp(&self) -> i64 {
        self.now
    }
    fn transaction_timestamp(&self) -> i64 {
        self.now
    }
    fn clock_timestamp(&self) -> i64 {
        self.now
    }
    fn fill_random(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    fn random_f64(&self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn set_seed(&self, seed: f64) {
        let bits = ((seed + 1.0) * 1e15) as u64;
        self.rng_state.set(bits | 1);
    }
    fn notice(&self, notice: PgError) {
        self.notices.borrow_mut().push(notice);
    }
    fn call_external(&self, proc: &BuiltinProc, _args: &[Datum]) -> Result<Datum> {
        Err(PgError::new(
            pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
            format!("function {} requires an engine session", proc.name),
        ))
    }
    fn call_external_set(&self, proc: &BuiltinProc, _args: &[Datum]) -> Result<SetIter> {
        Err(PgError::new(
            pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
            format!("function {} requires an engine session", proc.name),
        ))
    }
}
