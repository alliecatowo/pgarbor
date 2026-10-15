//! Aggregate functions (TYPE-12): native Rust state machines keyed by the aggregate's pg_proc OID.
//!
//! PostgreSQL's transition/final procs (`int4_sum`, `numeric_avg_accum`, ...) are NOT used; each
//! aggregate is a Rust `AggState`. The executor:
//! 1. resolves the aggregate call with `resolve::func` (prokind 'a' candidates),
//! 2. calls `make_aggregate(&spec, ctx)` once per group,
//! 3. calls `transition(args)` once per input row (after FILTER, and after DISTINCT / ORDER BY
//!    processing, which the executor performs by buffering/sorting rows itself),
//! 4. calls `finalize()` once.
//!
//! NULL handling is the aggregate's job (it mirrors PG's strict-transition semantics):
//! `count(x)`, `sum`, `avg`, `min`, `max`, `bool_and`, `string_agg`, `json_agg_strict`... skip rows
//! where (any) argument is NULL; `count(*)` (zero args) counts every row; `array_agg`, `json_agg`,
//! `jsonb_agg` include NULLs. An aggregate that saw no non-NULL input returns NULL, except
//! `count` → 0 (and `json_agg`/`array_agg` over zero rows → NULL).
//!
//! CONTRACT: all public items are frozen (v1).
use crate::catalog_data::{self, BuiltinAggregate, BuiltinProc, ImplModule};
use crate::context::FuncCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{Oid, PgError, Result, SqlState};

pub mod basic;
pub mod collect;
pub mod ordered_set;
pub mod stats;

/// Running state of one aggregate over one group.
// CONTRACT
pub trait AggState {
    /// Feed one input row. `args.len()` = number of aggregated arguments (0 for `count(*)`).
    /// For ordered-set aggregates, rows arrive already sorted by the WITHIN GROUP ordering.
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()>;
    /// Remove one previously-added row (moving-window frames). Return `Ok(false)` when the aggregate
    /// has no inverse (the executor then recomputes the frame from scratch). Default: no inverse.
    fn retract(&mut self, _args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        Ok(false)
    }
    /// Produce the result. May be called more than once (window frames): must not consume state.
    fn finalize(&self, ctx: &FuncCtx) -> Result<Datum>;
}

/// Everything needed to instantiate an aggregate.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AggSpec {
    /// pg_proc OID of the aggregate (= `pg_aggregate.aggfnoid`), e.g. 2108 = `sum(int4)`.
    pub agg_oid: Oid,
    /// Actual (resolved) types of the aggregated arguments.
    pub arg_types: Vec<TypeId>,
    /// Resolved result type (e.g. `sum(int4)` → int8, `avg(int4)` → numeric, `array_agg(text)` → text[]).
    pub result_type: TypeId,
    /// Evaluated direct arguments of ordered-set aggregates (`percentile_cont(0.5)` → `[0.5]`), else empty.
    pub direct_args: Vec<Datum>,
}

/// Instantiate a fresh state for `spec.agg_oid`.
///
/// Errors: unknown OID / not an aggregate → 42883 `function with OID N does not exist`-style internal
/// error; aggregate known but not implemented → 0A000 from the owning module.
pub fn make_aggregate(spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    let agg = catalog_data::builtin_aggregate(spec.agg_oid).ok_or_else(|| {
        PgError::new(SqlState::UNDEFINED_FUNCTION, format!("aggregate with OID {} does not exist", spec.agg_oid))
    })?;
    match agg.module {
        ImplModule::AggBasic => basic::make(agg, spec, ctx),
        ImplModule::AggCollect => collect::make(agg, spec, ctx),
        ImplModule::AggStats => stats::make(agg, spec, ctx),
        ImplModule::AggOrderedSet => ordered_set::make(agg, spec, ctx),
        _ => Err(crate::extra_types::unsupported_aggregate(agg)),
    }
}

/// The `pg_aggregate` row for an aggregate proc OID.
pub fn aggregate_info(agg_oid: Oid) -> Option<&'static BuiltinAggregate> {
    catalog_data::builtin_aggregate(agg_oid)
}

/// Error for an aggregate proc invoked through the scalar function path (binder bug).
pub(crate) fn unsupported_as_function(p: &BuiltinProc) -> PgError {
    PgError::internal(format!("aggregate function {} called as a plain function", p.name))
}
