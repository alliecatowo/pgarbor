//! Builtin function registry and dispatch (TYPE-12).
//!
//! Every builtin `pg_proc` row (generated `catalog_data::PROCS`) carries the `ImplModule` that
//! implements it. `implementation(proc_oid)` asks that module's `lookup(proc)` for a function
//! pointer once (at bind/plan time); executors keep the returned `FuncHandle` and call it per row.
//!
//! ```ignore
//! let h = func::implementation(177)?;            // int4pl, i.e. `int4 + int4`
//! let v = h.call(&[Datum::Int4(1), Datum::Int4(2)], &ctx)?;   // Datum::Int4(3)
//! ```
//!
//! Rules enforced HERE (implementations must not re-implement them):
//! - Strict procs (`proisstrict`): any `Datum::Null` argument → result `Datum::Null` (scalar) or an
//!   empty set (SRF) WITHOUT calling the implementation.
//! - Calling a scalar handle on a set-returning proc (or vice versa), an aggregate or a window
//!   function is an internal error (XX000): the binder must route those elsewhere.
//! - A proc whose module has no implementation yet → `0A000` via `<module>::unsupported(proc)`
//!   (never 42883: the function exists in PostgreSQL).
//!
//! Module contract (every module listed in `ImplModule`):
//! ```ignore
//! pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl>;   // match on proc.prosrc (lang 12)
//!                                                                 // or proc.oid (SQL-language builtins)
//! pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError;       // PgError::unimplemented(<lease>, ..)
//! ```
//! Implementations receive exactly `proc.nargs` arguments in declared order, already coerced to the
//! declared argument types (after polymorphic resolution: see `FuncCtx::arg_types`), with
//! defaults filled in and VARIADIC arguments collected into one array argument (except
//! `VARIADIC "any"` procs such as `concat`, `format`, `json_build_object`, which receive the
//! individual values and read their types from `FuncCtx::arg_types`).
//!
//! CONTRACT: all public items are frozen (v1).
use crate::agg;
use crate::array;
use crate::bytea;
use crate::catalog_data::{self, BuiltinProc, ImplModule, ProcKind};
use crate::context::FuncCtx;
use crate::crypto;
use crate::datetime;
use crate::datum::Datum;
use crate::enum_type;
use crate::extra_types;
use crate::formatting;
use crate::json;
use crate::numeric;
use crate::record;
use crate::scalar;
use crate::system_funcs;
use crate::text;
use crate::typmod;
use crate::uuid;
use pgarbor_common::{Oid, PgError, Result, SqlState};

pub mod series;

/// Scalar implementation: `args.len() == proc.nargs` (after default/variadic processing).
pub type ScalarFn = fn(&[Datum], &FuncCtx) -> Result<Datum>;
/// Rows of a set-returning function. Scalar SRFs (`generate_series`, `unnest`) yield plain values;
/// SRFs with OUT parameters (`jsonb_each` → (key text, value jsonb)) yield `Datum::Record`.
pub type SetIter = Box<dyn Iterator<Item = Result<Datum>>>;
/// Set-returning implementation. The iterator must own everything it needs ('static).
pub type SetFn = fn(&[Datum], &FuncCtx) -> Result<SetIter>;

/// How a builtin proc is evaluated.
// CONTRACT
#[derive(Clone, Copy)]
pub enum FuncImpl {
    Scalar(ScalarFn),
    Set(SetFn),
    /// prokind 'a': evaluate with `crate::agg::make_aggregate`.
    Aggregate,
    /// prokind 'w': evaluated by pgarbor-exec's window operator.
    Window,
    /// `ImplModule::External`: evaluate with `SessionContext::call_external(_set)`.
    External,
}

impl std::fmt::Debug for FuncImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FuncImpl::Scalar(_) => "Scalar",
            FuncImpl::Set(_) => "Set",
            FuncImpl::Aggregate => "Aggregate",
            FuncImpl::Window => "Window",
            FuncImpl::External => "External",
        })
    }
}

/// A resolved, callable builtin proc.
// CONTRACT
#[derive(Clone, Copy, Debug)]
pub struct FuncHandle {
    pub proc: &'static BuiltinProc,
    pub imp: FuncImpl,
}

impl FuncHandle {
    /// Call a scalar (non-set) function. Applies strictness.
    pub fn call(&self, args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
        if self.proc.strict && args.iter().any(|a| a.is_null()) {
            return Ok(Datum::Null);
        }
        match self.imp {
            FuncImpl::Scalar(f) => f(args, ctx),
            FuncImpl::External if !self.proc.retset => ctx.session.call_external(self.proc, args),
            other => Err(PgError::internal(format!(
                "function {} ({:?}) cannot be called as a scalar function",
                self.proc.name, other
            ))),
        }
    }
    /// Call a set-returning function. Applies strictness (NULL argument → empty set).
    pub fn call_set(&self, args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
        if self.proc.strict && args.iter().any(|a| a.is_null()) {
            return Ok(Box::new(std::iter::empty()));
        }
        match self.imp {
            FuncImpl::Set(f) => f(args, ctx),
            FuncImpl::External if self.proc.retset => ctx.session.call_external_set(self.proc, args),
            other => Err(PgError::internal(format!(
                "function {} ({:?}) cannot be called as a set-returning function",
                self.proc.name, other
            ))),
        }
    }
    pub fn is_set_returning(&self) -> bool {
        self.proc.retset
    }
}

/// Resolve the implementation of builtin proc `proc_oid`.
///
/// Errors: unknown OID → 42883 `function with OID N does not exist`; known but not implemented →
/// 0A000 from the owning module.
pub fn implementation(proc_oid: Oid) -> Result<FuncHandle> {
    let proc = catalog_data::builtin_proc(proc_oid).ok_or_else(|| {
        PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function with OID {proc_oid} does not exist"))
    })?;
    implementation_for(proc)
}

/// Same as `implementation` for an already-fetched row.
pub fn implementation_for(proc: &'static BuiltinProc) -> Result<FuncHandle> {
    let imp = match proc.kind {
        ProcKind::Aggregate => Some(FuncImpl::Aggregate),
        ProcKind::Window => Some(FuncImpl::Window),
        _ => match proc.module {
            ImplModule::External => Some(FuncImpl::External),
            _ => module_lookup(proc),
        },
    };
    match imp {
        Some(imp) => Ok(FuncHandle { proc, imp }),
        None => Err(module_unsupported(proc)),
    }
}

/// Convenience: resolve + call a scalar builtin (prefer caching the `FuncHandle`).
pub fn call(proc_oid: Oid, args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    implementation(proc_oid)?.call(args, ctx)
}

/// Convenience: resolve + call a set-returning builtin.
pub fn call_set(proc_oid: Oid, args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    implementation(proc_oid)?.call_set(args, ctx)
}

fn module_lookup(p: &BuiltinProc) -> Option<FuncImpl> {
    match p.module {
        ImplModule::Bool => scalar::bool::lookup(p),
        ImplModule::IntOps => scalar::int_ops::lookup(p),
        ImplModule::FloatOps => scalar::float_ops::lookup(p),
        ImplModule::Numeric => numeric::funcs::lookup(p),
        ImplModule::OidTypes => scalar::oid_types::lookup(p),
        ImplModule::Text => text::funcs::lookup(p),
        ImplModule::TextFormat => text::format::lookup(p),
        ImplModule::Like => text::like::lookup(p),
        ImplModule::Regex => text::regex::lookup(p),
        ImplModule::Bytea => bytea::lookup(p),
        ImplModule::Crypto => crypto::lookup(p),
        ImplModule::Uuid => uuid::lookup(p),
        ImplModule::Date => datetime::date::lookup(p),
        ImplModule::Time => datetime::time::lookup(p),
        ImplModule::Timestamp => datetime::timestamp::lookup(p),
        ImplModule::Interval => datetime::interval::lookup(p),
        ImplModule::DatetimeFuncs => datetime::funcs::lookup(p),
        ImplModule::Formatting => formatting::lookup(p),
        ImplModule::JsonOps => json::ops::lookup(p),
        ImplModule::JsonBuild => json::build::lookup(p),
        ImplModule::JsonSrf => json::srf::lookup(p),
        ImplModule::ArrayFuncs => array::funcs::lookup(p),
        ImplModule::Record => record::lookup(p),
        ImplModule::Enum => enum_type::lookup(p),
        ImplModule::System => system_funcs::lookup(p),
        ImplModule::Series => series::lookup(p),
        ImplModule::Typmod => typmod::lookup(p),
        ImplModule::AggBasic
        | ImplModule::AggCollect
        | ImplModule::AggStats
        | ImplModule::AggOrderedSet => Some(FuncImpl::Aggregate),
        ImplModule::Window => Some(FuncImpl::Window),
        ImplModule::External => Some(FuncImpl::External),
        ImplModule::Unsupported => extra_types::lookup(p),
    }
}

fn module_unsupported(p: &BuiltinProc) -> PgError {
    match p.module {
        ImplModule::Bool => scalar::bool::unsupported(p),
        ImplModule::IntOps => scalar::int_ops::unsupported(p),
        ImplModule::FloatOps => scalar::float_ops::unsupported(p),
        ImplModule::Numeric => numeric::funcs::unsupported(p),
        ImplModule::OidTypes => scalar::oid_types::unsupported(p),
        ImplModule::Text => text::funcs::unsupported(p),
        ImplModule::TextFormat => text::format::unsupported(p),
        ImplModule::Like => text::like::unsupported(p),
        ImplModule::Regex => text::regex::unsupported(p),
        ImplModule::Bytea => bytea::unsupported(p),
        ImplModule::Crypto => crypto::unsupported(p),
        ImplModule::Uuid => uuid::unsupported(p),
        ImplModule::Date => datetime::date::unsupported(p),
        ImplModule::Time => datetime::time::unsupported(p),
        ImplModule::Timestamp => datetime::timestamp::unsupported(p),
        ImplModule::Interval => datetime::interval::unsupported(p),
        ImplModule::DatetimeFuncs => datetime::funcs::unsupported(p),
        ImplModule::Formatting => formatting::unsupported(p),
        ImplModule::JsonOps => json::ops::unsupported(p),
        ImplModule::JsonBuild => json::build::unsupported(p),
        ImplModule::JsonSrf => json::srf::unsupported(p),
        ImplModule::ArrayFuncs => array::funcs::unsupported(p),
        ImplModule::Record => record::unsupported(p),
        ImplModule::Enum => enum_type::unsupported(p),
        ImplModule::System => system_funcs::unsupported(p),
        ImplModule::Series => series::unsupported(p),
        ImplModule::Typmod => typmod::unsupported(p),
        ImplModule::AggBasic
        | ImplModule::AggCollect
        | ImplModule::AggStats
        | ImplModule::AggOrderedSet => agg::unsupported_as_function(p),
        ImplModule::Window | ImplModule::External | ImplModule::Unsupported => extra_types::unsupported(p),
    }
}
