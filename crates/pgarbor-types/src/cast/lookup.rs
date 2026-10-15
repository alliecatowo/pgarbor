//! Coercion pathway lookup and cast planning (TYPE-03). Lease TYP-52 (sonnet).
//!
//! PG18 reference: `src/backend/parser/parse_coerce.c` — `find_coercion_pathway`,
//! `find_typmod_coercion_function`, `IsBinaryCoercible`, `coerce_type` (structure only), and the
//! `pg_cast` rows (`catalog_data::CASTS`, plus `TypeEnv::user_cast`).
//!
//! `find_cast(env, source, target, ctx)` (PG `find_coercion_pathway`), in order:
//! 1. `source == target` → `Relabel`.
//! 2. Domains: replace a domain source by its base type (`registry::base_type`); a domain target is
//!    handled by `plan_cast` (cast to the base, then `CastStep::DomainCheck`).
//! 3. `pg_cast` row (builtin or user) with `context <= ctx`: method Function → `Func(castfunc)`,
//!    Binary → `Relabel`, InOut → `ViaIo`. A row whose context is too strong means "no pathway in this
//!    context" (return `None`; do NOT fall through to IO for that pair, PG semantics).
//! 4. No row: if both source and target are array types and `find_cast(elem_src, elem_dst, ctx)`
//!    returns a pathway P → `ArrayCoerce(P)` (`'{1,2}'::int[]::text[]`, `int4[] → int8[]` implicitly).
//! 5. Automatic I/O conversion: target in string category ('S') and `ctx >= Assignment` → `ViaIo`;
//!    source in string category and `ctx == Explicit` → `ViaIo`. (E.g. `int4 → text` is assignment-only:
//!    `INSERT INTO t(textcol) VALUES (1)` works, `'1'::text + 1` does not coerce.)
//! 6. Composite/record → record and record → composite by field-wise coercion: `RecordCoerce`.
//! 7. Otherwise `None`.
//! Unknown-type literals (`unknown`, OID 705) are NOT handled here: see `resolve::coerce`.
//!
//! `plan_cast(env, from, to, ctx)` produces the executable `CastPlan`: pathway steps, then a
//! `Typmod` step when `to.typmod >= 0` and the target has a length-coercion function
//! (varchar, bpchar, numeric, time, timetz, timestamp, timestamptz, interval, bit, varbit) and the
//! typmods differ, then a `DomainCheck` for domain targets. No pathway → 42846
//! `cannot cast type integer to interval` (type names via `names::format_type`; oracle-verified for
//! `x::interval` with x int). `Explicit` controls typmod semantics (explicit `::varchar(3)` truncates,
//! assignment errors 22001).
use crate::catalog_data::CastContext;
use crate::context::TypeEnv;
use crate::datum::TypeId;
use pgarbor_common::{Oid, PgError, Result};

/// PG `CoercionPathType`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CastPathway {
    /// Binary coercible: no runtime work (e.g. varchar → text, text → varchar).
    Relabel,
    /// Call a cast function (pg_proc OID); it may take 1, 2 (typmod) or 3 (typmod, is_explicit) args.
    Func(Oid),
    /// Output with the source type's output function, input with the target's input function.
    ViaIo,
    /// Element-wise cast of an array.
    ArrayCoerce(Box<CastPathway>),
    /// Field-wise record/composite coercion.
    RecordCoerce,
}

/// One runtime step of a cast.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CastStep {
    Relabel,
    /// Call cast function `proc` with `nargs` arguments: (value[, target typmod int4[, explicit bool]]).
    Func { proc: Oid, nargs: u8 },
    /// I/O conversion from `from` to `to` (typmod applied by the input function).
    ViaIo { from: TypeId, to: TypeId },
    /// Element-wise application of `elem_plan` producing an array of `elem_plan.to.oid`.
    ArrayCoerce { elem_plan: Box<CastPlan> },
    /// Field-wise coercion to the target composite / record.
    RecordCoerce { to: TypeId },
    /// Length / precision coercion (`typmod::enforce_typmod`).
    Typmod { to: TypeId, explicit: bool },
    /// Domain constraint check for `domain` (NOT NULL / CHECK): performed by pgarbor-exec; the type
    /// crate only marks where it must happen (`apply_cast` treats it as identity).
    DomainCheck { domain: Oid },
}

/// An executable cast.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastPlan {
    pub from: TypeId,
    pub to: TypeId,
    pub context: CastContext,
    pub steps: Vec<CastStep>,
}

impl CastPlan {
    /// True when the cast does nothing at runtime.
    pub fn is_noop(&self) -> bool {
        self.steps.iter().all(|s| matches!(s, CastStep::Relabel))
    }
}

/// PG `find_coercion_pathway` (see module docs). `Ok(None)` = no pathway in this context.
// CONTRACT
#[allow(unused_variables)]
pub fn find_cast(env: &dyn TypeEnv, source: Oid, target: Oid, ctx: CastContext) -> Result<Option<CastPathway>> {
    Err(PgError::unimplemented("TYP-52", "cast lookup"))
}

/// Build an executable plan (see module docs). Errors: 42846 `cannot cast type X to Y`.
// CONTRACT
#[allow(unused_variables)]
pub fn plan_cast(env: &dyn TypeEnv, from: TypeId, to: TypeId, ctx: CastContext) -> Result<CastPlan> {
    Err(PgError::unimplemented("TYP-52", "cast planning"))
}

/// PG `IsBinaryCoercible(source, target)`.
// CONTRACT
#[allow(unused_variables)]
pub fn is_binary_coercible(env: &dyn TypeEnv, source: Oid, target: Oid) -> Result<bool> {
    Err(PgError::unimplemented("TYP-52", "binary coercibility"))
}
