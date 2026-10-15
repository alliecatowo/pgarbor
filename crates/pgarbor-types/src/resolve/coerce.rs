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
use crate::cast::lookup::find_cast;
use crate::catalog_data::CastContext;
use crate::context::TypeEnv;
use crate::datum::TypeId;
use crate::names::display_name;
use crate::oids;
use crate::registry::{base_type, is_polymorphic};
use crate::resolve::polymorphic::check_generic_type_consistency;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// Category and preferred flag of a type, as PG `get_type_category_preferred`
/// (unknown OIDs report category 'X', not preferred).
fn category_preferred(env: &dyn TypeEnv, oid: Oid) -> (u8, bool) {
    env.type_info(oid).map(|t| (t.category, t.preferred)).unwrap_or((b'X', false))
}

/// PG `ISCOMPLEX`: a stand-alone composite type.
fn is_complex(env: &dyn TypeEnv, oid: Oid) -> bool {
    env.type_info(oid).map(|t| t.is_composite()).unwrap_or(false)
}

/// PG `is_complex_array`: an array whose element type is composite.
fn is_complex_array(env: &dyn TypeEnv, oid: Oid) -> bool {
    match env.type_info(oid) {
        Some(t) if t.is_array() => is_complex(env, t.elem),
        _ => false,
    }
}

/// PG `can_coerce_type`.
// CONTRACT
pub fn can_coerce_type(env: &dyn TypeEnv, inputs: &[Oid], targets: &[Oid], ctx: CastContext) -> Result<bool> {
    if inputs.len() != targets.len() {
        return Ok(false);
    }
    let mut have_generics = false;
    for (&input, &target) in inputs.iter().zip(targets.iter()) {
        // `any` accepts everything.
        if target == oids::ANY {
            continue;
        }
        if input == target {
            continue;
        }
        // Polymorphic targets are checked together after the loop.
        if is_polymorphic(target) {
            have_generics = true;
            continue;
        }
        // Unknown literals can be converted to any (non-polymorphic) type by its input function.
        if input == oids::UNKNOWN {
            continue;
        }
        if find_cast(env, input, target, ctx)?.is_some() {
            continue;
        }
        // Record <-> composite coercions are possible field-wise.
        if input == oids::RECORD && is_complex(env, target) {
            continue;
        }
        if target == oids::RECORD && is_complex(env, input) {
            continue;
        }
        if target == oids::RECORD_ARRAY && is_complex_array(env, input) {
            continue;
        }
        return Ok(false);
    }
    if have_generics && !check_generic_type_consistency(env, inputs, targets)? {
        return Ok(false);
    }
    Ok(true)
}

/// PG `select_common_type` (see module docs).
// CONTRACT
pub fn select_common_type(env: &dyn TypeEnv, types: &[Oid], context: &str) -> Result<Oid> {
    let first = match types.first() {
        Some(&t) => t,
        None => return Err(PgError::internal("select_common_type called with no input types")),
    };
    // All valid and identical: keep that type (the only way a domain survives).
    if first != oids::UNKNOWN && types.iter().all(|&t| t == first) {
        return Ok(first);
    }
    let mut ptype = base_type(env, first);
    let (mut pcategory, mut pispreferred) = category_preferred(env, ptype);
    for &raw in &types[1..] {
        let ntype = base_type(env, raw);
        if ntype == oids::UNKNOWN || ntype == ptype {
            continue;
        }
        let (ncategory, nispreferred) = category_preferred(env, ntype);
        if ptype == oids::UNKNOWN {
            ptype = ntype;
            pcategory = ncategory;
            pispreferred = nispreferred;
        } else if ncategory != pcategory {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "{} types {} and {} cannot be matched",
                    context,
                    display_name(env, ptype),
                    display_name(env, ntype)
                ),
            ));
        } else if !pispreferred
            && can_coerce_type(env, &[ptype], &[ntype], CastContext::Implicit)?
            && !can_coerce_type(env, &[ntype], &[ptype], CastContext::Implicit)?
        {
            ptype = ntype;
            pcategory = ncategory;
            pispreferred = nispreferred;
        }
    }
    if ptype == oids::UNKNOWN {
        ptype = oids::TEXT;
    }
    Ok(ptype)
}

/// PG `select_common_typmod`.
// CONTRACT
pub fn select_common_typmod(types: &[TypeId], common: Oid) -> Result<i32> {
    let mut result = -1;
    let mut first = true;
    for t in types {
        if t.oid != common {
            return Ok(-1);
        }
        if first {
            result = t.typmod;
            first = false;
        } else if result != t.typmod {
            return Ok(-1);
        }
    }
    Ok(result)
}

/// PG `coerce_to_common_type` check: error 42846 `<context> could not convert type X to Y` if some input
/// cannot be implicitly coerced to `common`.
// CONTRACT
pub fn verify_common_type(env: &dyn TypeEnv, types: &[Oid], common: Oid, context: &str) -> Result<()> {
    for &t in types {
        if t == common || t == oids::UNKNOWN {
            continue;
        }
        if !can_coerce_type(env, &[t], &[common], CastContext::Implicit)? {
            return Err(PgError::new(
                SqlState::CANNOT_COERCE,
                format!(
                    "{} could not convert type {} to {}",
                    context,
                    display_name(env, t),
                    display_name(env, common)
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_env;

    fn sct(types: &[Oid], ctx: &str) -> Result<Oid> {
        select_common_type(builtin_env(), types, ctx)
    }

    #[test]
    fn numeric_promotion() {
        assert_eq!(sct(&[oids::INT4, oids::NUMERIC], "UNION").unwrap(), oids::NUMERIC);
        assert_eq!(sct(&[oids::INT4, oids::FLOAT4], "COALESCE").unwrap(), oids::FLOAT4);
        assert_eq!(sct(&[oids::INT4, oids::INT8], "CASE").unwrap(), oids::INT8);
        assert_eq!(sct(&[oids::INT4, oids::NUMERIC], "ARRAY").unwrap(), oids::NUMERIC);
    }

    #[test]
    fn all_unknown_is_text() {
        assert_eq!(sct(&[oids::UNKNOWN, oids::UNKNOWN], "UNION").unwrap(), oids::TEXT);
        assert_eq!(sct(&[oids::UNKNOWN, oids::INT4], "CASE").unwrap(), oids::INT4);
    }

    #[test]
    fn category_mismatch() {
        let e = sct(&[oids::INT4, oids::TEXT], "UNION").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42804");
        assert_eq!(e.message, "UNION types integer and text cannot be matched");
        let e = sct(&[oids::TEXT, oids::INT4], "CASE").unwrap_err();
        assert_eq!(e.message, "CASE types text and integer cannot be matched");
    }

    #[test]
    fn typmod_selection() {
        let a = TypeId { oid: oids::VARCHAR, typmod: 7 };
        let b = TypeId { oid: oids::VARCHAR, typmod: 7 };
        let c = TypeId { oid: oids::VARCHAR, typmod: 9 };
        assert_eq!(select_common_typmod(&[a, b], oids::VARCHAR).unwrap(), 7);
        assert_eq!(select_common_typmod(&[a, c], oids::VARCHAR).unwrap(), -1);
        assert_eq!(select_common_typmod(&[a, TypeId::INT4], oids::VARCHAR).unwrap(), -1);
    }

    #[test]
    fn coercibility_basics() {
        let env = builtin_env();
        assert!(can_coerce_type(env, &[oids::INT4], &[oids::INT8], CastContext::Implicit).unwrap());
        assert!(!can_coerce_type(env, &[oids::INT8], &[oids::INT4], CastContext::Implicit).unwrap());
        assert!(can_coerce_type(env, &[oids::UNKNOWN], &[oids::DATE], CastContext::Implicit).unwrap());
        assert!(can_coerce_type(env, &[oids::INT4], &[oids::ANY], CastContext::Implicit).unwrap());
    }

    #[test]
    fn verify_reports_42846() {
        let env = builtin_env();
        verify_common_type(env, &[oids::INT4, oids::UNKNOWN], oids::INT8, "UNION").unwrap();
        let e = verify_common_type(env, &[oids::INT8], oids::INT4, "CASE").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42846");
        assert_eq!(e.message, "CASE could not convert type bigint to integer");
    }
}
