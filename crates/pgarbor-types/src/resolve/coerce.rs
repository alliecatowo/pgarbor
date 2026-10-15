//! Implicit coercibility, unknown-literal coercion and common-type selection (TYPE-03).
//! Lease TYP-54 (sonnet).
//!
//! PG18 reference: `src/backend/parser/parse_coerce.c` — `can_coerce_type`, `coerce_type` (unknown
//! literals: Const of type unknown is converted by the target type's input function at bind time),
//! `select_common_type`, `select_common_type_from_oids`, `verify_common_type`, `select_common_typmod`,
//! `TypeCategory`, `IsPreferredType`.
//!
//! `can_coerce_type(env, inputs, targets, ctx)` — every pair must satisfy one of:
//! input == target; target is `any` (2276); input is `unknown` and target is not a pseudo type that rejects
//! it (unknown → any non-pseudo type is OK); polymorphic targets (anyelement/anyarray/...) defer to
//! `resolve::polymorphic::check_generic_type_consistency` over the whole call; `find_cast(...) != None`
//! in `ctx`; input `record`/composite → `record` target; domain input → its base type rules.
//!
//! `select_common_type(env, types, context)` (UNION/INTERSECT/EXCEPT, CASE, COALESCE, GREATEST/LEAST,
//! VALUES, ARRAY[], IN lists) — PG algorithm:
//! 1. Pick the first non-unknown input type as candidate (if all unknown → `text`).
//! 2. For each later non-unknown type T (domains reduced to base types first when they differ):
//!    if T != candidate: if categories differ → 42804 `<context> types <cand> and <T> cannot be matched`
//!    (names via `names::format_type`; oracle: `UNION types integer and text cannot be matched`,
//!    `CASE types text and integer cannot be matched` — note the order is candidate then new type);
//!    else if candidate is not preferred AND candidate can be implicitly cast to T AND T cannot be
//!    implicitly cast to candidate → candidate = T (also prefer T if it is the preferred type of the category).
//! 3. Return the candidate. `context` strings: "UNION", "INTERSECT", "EXCEPT", "CASE", "COALESCE",
//!    "GREATEST", "LEAST", "VALUES", "ARRAY", "IN".
//! Oracle: `select 1 union select 2.5` → numeric; `coalesce(1, 2.5::float4)` → real;
//! `case when true then 1 else 2::int8 end` → bigint; `array[1, 2.5]` → numeric[].
//! `verify_common_type`: every input must be implicitly coercible to the result, else 42846
//! `<context> could not convert type X to Y` (PG `coerce_to_common_type`).
//! `select_common_typmod`: all inputs share the typmod → it, else -1.
use crate::catalog_data::CastContext;
use crate::context::TypeEnv;
use crate::datum::TypeId;
use pgarbor_common::{Oid, PgError, Result};

/// PG `can_coerce_type`.
// CONTRACT
#[allow(unused_variables)]
pub fn can_coerce_type(env: &dyn TypeEnv, inputs: &[Oid], targets: &[Oid], ctx: CastContext) -> Result<bool> {
    Err(PgError::unimplemented("TYP-54", "coercibility check"))
}

/// PG `select_common_type` (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn select_common_type(env: &dyn TypeEnv, types: &[Oid], context: &str) -> Result<Oid> {
    Err(PgError::unimplemented("TYP-54", "common type selection"))
}

/// PG `select_common_typmod`.
// CONTRACT
#[allow(unused_variables)]
pub fn select_common_typmod(types: &[TypeId], common: Oid) -> Result<i32> {
    Err(PgError::unimplemented("TYP-54", "common typmod selection"))
}

/// PG `coerce_to_common_type` check: error 42846 `<context> could not convert type X to Y` if some input
/// cannot be implicitly coerced to `common`.
// CONTRACT
#[allow(unused_variables)]
pub fn verify_common_type(env: &dyn TypeEnv, types: &[Oid], common: Oid, context: &str) -> Result<()> {
    Err(PgError::unimplemented("TYP-54", "common type verification"))
}
