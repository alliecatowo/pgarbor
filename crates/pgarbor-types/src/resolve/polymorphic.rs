//! Polymorphic type resolution (TYPE-12). Lease TYP-57 (sonnet).
//!
//! PG18 reference: `src/backend/parser/parse_coerce.c` — `check_generic_type_consistency`,
//! `enforce_generic_type_consistency`, `resolve_polymorphic_argtypes`,
//! `check_valid_polymorphic_signature`; docs "38.2.5 Polymorphic Types".
//!
//! Families: (1) `anyelement`, `anyarray`, `anynonarray`, `anyenum`, `anyrange`, `anymultirange` must all
//! resolve to ONE element type T (anyarray = T[]); (2) `anycompatible`, `anycompatiblearray`,
//! `anycompatiblenonarray`, `anycompatiblerange`, `anycompatiblemultirange` resolve to the common type of
//! their inputs (`coerce::select_common_type`, context = function name), e.g. `array_append(array[1], 1.5)`
//! → numeric[] result `{1,1.5}` (oracle).
//! Rules: unknown inputs at polymorphic positions do not determine T (if nothing determines T for an
//! `anyelement` family, assume `text` when all such inputs are unknown — PG resolves unknown → text);
//! anynonarray rejects arrays; anyenum requires an enum; a domain over an array counts as its base array.
//! During function resolution an inconsistent candidate is simply NOT a candidate (`check_*` returns
//! false), so the user-visible error is the resolver's 42883 (oracle: `array_append(array[1], 'x'::text)`
//! → `function array_append(integer[], text) does not exist`; `array_cat(array[1], 2)` →
//! `function array_cat(integer[], integer) does not exist`). Unknown literals adopt the resolved type:
//! `array_append(array[1], 'x')` → 22P02 `invalid input syntax for type integer: "x"`; all-unknown
//! inputs resolve to text: `pg_typeof(array_append(null, null))` = `text[]`, `array_append(null, 1)` →
//! `integer[]`. `enforce_generic_type_consistency` (called after selection) raises 42804
//! `arguments declared "anyelement" are not all alike` with DETAIL `integer versus text`, or
//! `argument declared anyarray is not an array but type integer`, only for direct callers
//! (e.g. operator/aggregate paths that skip candidate filtering).
use crate::context::TypeEnv;
use crate::names::display_name;
use crate::oids;
use crate::registry::{array_type_of, base_type, element_type_of, is_array_type, is_polymorphic};
use crate::resolve::coerce::select_common_type;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// Outcome of matching declared polymorphic positions against actual types.
struct Resolution {
    /// Family 1 element type (0 = none of the family appeared).
    elem: Oid,
    /// Family 1 array type (base type, 0 = not given by an actual array).
    array: Oid,
    /// Any family-1 polymorphic declared type appeared.
    have1: bool,
    /// Family 2 common type (0 = none).
    common: Oid,
    /// Any family-2 polymorphic declared type appeared.
    have2: bool,
}

fn declared_name(d: Oid) -> &'static str {
    match d {
        oids::ANYELEMENT => "anyelement",
        oids::ANYARRAY => "anyarray",
        oids::ANYNONARRAY => "anynonarray",
        oids::ANYENUM => "anyenum",
        oids::ANYRANGE => "anyrange",
        oids::ANYMULTIRANGE => "anymultirange",
        oids::ANYCOMPATIBLE => "anycompatible",
        oids::ANYCOMPATIBLEARRAY => "anycompatiblearray",
        oids::ANYCOMPATIBLENONARRAY => "anycompatiblenonarray",
        oids::ANYCOMPATIBLERANGE => "anycompatiblerange",
        oids::ANYCOMPATIBLEMULTIRANGE => "anycompatiblemultirange",
        _ => "any",
    }
}

fn is_range_family(d: Oid) -> bool {
    matches!(d, oids::ANYRANGE | oids::ANYMULTIRANGE | oids::ANYCOMPATIBLERANGE | oids::ANYCOMPATIBLEMULTIRANGE)
}

/// In strict mode a failure is an error; otherwise the candidate is simply rejected.
fn bail<T>(strict: bool, err: impl FnOnce() -> PgError) -> Result<Option<T>> {
    if strict {
        Err(err())
    } else {
        Ok(None)
    }
}

fn mismatch(msg: String) -> PgError {
    PgError::new(SqlState::DATATYPE_MISMATCH, msg)
}

fn range_unsupported(d: Oid) -> PgError {
    PgError::unimplemented("TYP-57", format!("polymorphic resolution for {}", declared_name(d)))
}

fn no_array_type(env: &dyn TypeEnv, t: Oid) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("could not find array type for data type {}", display_name(env, t)))
}

fn analyze(env: &dyn TypeEnv, actual: &[Oid], declared: &[Oid], strict: bool) -> Result<Option<Resolution>> {
    let mut elem: Oid = 0;
    let mut array: Oid = 0;
    let mut have1 = false;
    let mut have2 = false;
    let mut nonarray1 = false;
    let mut anyenum = false;
    let mut nonarray2 = false;
    let mut types2: Vec<Oid> = Vec::new();

    for (&a, &d) in actual.iter().zip(declared.iter()) {
        match d {
            oids::ANYELEMENT | oids::ANYNONARRAY | oids::ANYENUM => {
                have1 = true;
                nonarray1 |= d == oids::ANYNONARRAY;
                anyenum |= d == oids::ANYENUM;
                if a == oids::UNKNOWN {
                    continue;
                }
                if elem != 0 && elem != a {
                    return bail(strict, || {
                        mismatch(format!("arguments declared \"{}\" are not all alike", declared_name(d)))
                            .with_detail(format!("{} versus {}", display_name(env, elem), display_name(env, a)))
                    });
                }
                elem = a;
            }
            oids::ANYARRAY => {
                have1 = true;
                if a == oids::UNKNOWN {
                    continue;
                }
                let b = base_type(env, a);
                if array != 0 && array != b {
                    return bail(strict, || {
                        mismatch("arguments declared \"anyarray\" are not all alike".to_string())
                            .with_detail(format!("{} versus {}", display_name(env, array), display_name(env, b)))
                    });
                }
                array = b;
            }
            oids::ANYCOMPATIBLE | oids::ANYCOMPATIBLENONARRAY => {
                have2 = true;
                nonarray2 |= d == oids::ANYCOMPATIBLENONARRAY;
                if a != oids::UNKNOWN {
                    types2.push(a);
                }
            }
            oids::ANYCOMPATIBLEARRAY => {
                have2 = true;
                if a == oids::UNKNOWN {
                    continue;
                }
                let b = base_type(env, a);
                match element_type_of(env, b) {
                    Some(e) => types2.push(e),
                    None => {
                        return bail(strict, || {
                            mismatch(format!(
                                "argument declared anycompatiblearray is not an array but type {}",
                                display_name(env, a)
                            ))
                        });
                    }
                }
            }
            _ if is_range_family(d) => {
                return bail(strict, || range_unsupported(d));
            }
            _ => {}
        }
    }

    // Family 1: reconcile the array type with the element type.
    if array != 0 {
        let te = match element_type_of(env, array) {
            Some(e) => e,
            None => {
                return bail(strict, || {
                    mismatch(format!("argument declared anyarray is not an array but type {}", display_name(env, array)))
                });
            }
        };
        if elem != 0 && elem != te {
            return bail(strict, || {
                mismatch("argument declared anyarray is not consistent with argument declared anyelement".to_string())
                    .with_detail(format!("{} versus {}", display_name(env, te), display_name(env, elem)))
            });
        }
        elem = te;
    }
    if elem == 0 && have1 {
        if anyenum {
            if strict {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    "could not determine polymorphic type because input has type unknown",
                ));
            }
        } else {
            // All polymorphic inputs are unknown: PG resolves them as text.
            elem = oids::TEXT;
        }
    }
    if elem != 0 {
        if nonarray1 && is_array_type(env, base_type(env, elem)) {
            return bail(strict, || {
                mismatch(format!("type matched to anynonarray is an array type: {}", display_name(env, elem)))
            });
        }
        if anyenum {
            let is_enum = env.type_info(elem).map(|t| t.is_enum()).unwrap_or(false);
            if !is_enum {
                return bail(strict, || {
                    mismatch(format!("type matched to anyenum is not an enum type: {}", display_name(env, elem)))
                });
            }
        }
    }

    // Family 2: common type of all inputs.
    let mut common: Oid = 0;
    if have2 {
        common = if types2.is_empty() {
            oids::TEXT
        } else {
            match select_common_type(env, &types2, "anycompatible") {
                Ok(c) => c,
                Err(e) if !strict && e.sqlstate == SqlState::DATATYPE_MISMATCH => return Ok(None),
                Err(e) => return Err(e),
            }
        };
        if nonarray2 && is_array_type(env, base_type(env, common)) {
            return bail(strict, || {
                mismatch(format!(
                    "type matched to anycompatiblenonarray is an array type: {}",
                    display_name(env, common)
                ))
            });
        }
        if declared.iter().any(|&d| d == oids::ANYCOMPATIBLEARRAY) && array_type_of(env, common).is_none() {
            return bail(strict, || no_array_type(env, common));
        }
    }

    Ok(Some(Resolution { elem, array, have1, common, have2 }))
}

fn undetermined_result(d: Oid, fam1: bool) -> PgError {
    let code = SqlState::from_code("42P13").unwrap_or(SqlState::INDETERMINATE_DATATYPE);
    let detail = if fam1 {
        format!(
            "A result of type {} requires at least one input of type anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange.",
            declared_name(d)
        )
    } else {
        format!(
            "A result of type {} requires at least one input of type anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange.",
            declared_name(d)
        )
    };
    PgError::new(code, "cannot determine result data type").with_detail(detail)
}

/// Replace one polymorphic type by its resolved type.
fn substitute(env: &dyn TypeEnv, r: &Resolution, d: Oid, is_result: bool) -> Result<Oid> {
    match d {
        oids::ANYELEMENT | oids::ANYNONARRAY | oids::ANYENUM => {
            if is_result && !r.have1 {
                return Err(undetermined_result(d, true));
            }
            Ok(r.elem)
        }
        oids::ANYARRAY => {
            if is_result && !r.have1 {
                return Err(undetermined_result(d, true));
            }
            if r.array != 0 {
                Ok(r.array)
            } else {
                array_type_of(env, r.elem).ok_or_else(|| no_array_type(env, r.elem))
            }
        }
        oids::ANYCOMPATIBLE | oids::ANYCOMPATIBLENONARRAY => {
            if is_result && !r.have2 {
                return Err(undetermined_result(d, false));
            }
            Ok(r.common)
        }
        oids::ANYCOMPATIBLEARRAY => {
            if is_result && !r.have2 {
                return Err(undetermined_result(d, false));
            }
            array_type_of(env, r.common).ok_or_else(|| no_array_type(env, r.common))
        }
        _ if is_range_family(d) => Err(range_unsupported(d)),
        _ => Ok(d),
    }
}

/// PG `check_generic_type_consistency`: can `actual` inputs satisfy the polymorphic `declared` args?
// CONTRACT
pub fn check_generic_type_consistency(env: &dyn TypeEnv, actual: &[Oid], declared: &[Oid]) -> Result<bool> {
    Ok(analyze(env, actual, declared, false)?.is_some())
}

/// PG `enforce_generic_type_consistency`: returns (declared args with polymorphics replaced by actual
/// types, resolved return type). `ret` may be non-polymorphic (returned unchanged).
// CONTRACT
pub fn enforce_generic_type_consistency(
    env: &dyn TypeEnv,
    actual: &[Oid],
    declared: &[Oid],
    ret: Oid,
) -> Result<(Vec<Oid>, Oid)> {
    let r = analyze(env, actual, declared, true)?
        .ok_or_else(|| PgError::internal("polymorphic resolution failed without an error"))?;
    let mut out = Vec::with_capacity(declared.len());
    for &d in declared {
        out.push(if is_polymorphic(d) { substitute(env, &r, d, false)? } else { d });
    }
    let ret_t = if is_polymorphic(ret) { substitute(env, &r, ret, true)? } else { ret };
    Ok((out, ret_t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_env;

    #[test]
    fn append_int_int() {
        let env = builtin_env();
        let d = [oids::ANYARRAY, oids::ANYELEMENT];
        let a = [oids::INT4_ARRAY, oids::INT4];
        assert!(check_generic_type_consistency(env, &a, &d).unwrap());
        let (args, ret) = enforce_generic_type_consistency(env, &a, &d, oids::ANYARRAY).unwrap();
        assert_eq!(args, vec![oids::INT4_ARRAY, oids::INT4]);
        assert_eq!(ret, oids::INT4_ARRAY);
    }

    #[test]
    fn anyelement_inconsistent() {
        let env = builtin_env();
        let d = [oids::ANYELEMENT, oids::ANYELEMENT];
        assert!(!check_generic_type_consistency(env, &[oids::INT4, oids::TEXT], &d).unwrap());
        let e = enforce_generic_type_consistency(env, &[oids::INT4, oids::TEXT], &d, oids::ANYELEMENT).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATATYPE_MISMATCH);
        assert_eq!(e.message, "arguments declared \"anyelement\" are not all alike");
    }

    #[test]
    fn unknowns_resolve_to_text() {
        let env = builtin_env();
        let d = [oids::ANYELEMENT, oids::ANYELEMENT];
        let (args, ret) =
            enforce_generic_type_consistency(env, &[oids::UNKNOWN, oids::UNKNOWN], &d, oids::ANYELEMENT).unwrap();
        assert_eq!(args, vec![oids::TEXT, oids::TEXT]);
        assert_eq!(ret, oids::TEXT);
        let d = [oids::ANYARRAY, oids::ANYELEMENT];
        let (_, ret) =
            enforce_generic_type_consistency(env, &[oids::UNKNOWN, oids::UNKNOWN], &d, oids::ANYARRAY).unwrap();
        assert_eq!(ret, oids::TEXT_ARRAY);
        let (_, ret) = enforce_generic_type_consistency(env, &[oids::UNKNOWN, oids::INT4], &d, oids::ANYARRAY).unwrap();
        assert_eq!(ret, oids::INT4_ARRAY);
    }

    #[test]
    fn anynonarray_rejects_array() {
        let env = builtin_env();
        let d = [oids::ANYNONARRAY];
        assert!(!check_generic_type_consistency(env, &[oids::INT4_ARRAY], &d).unwrap());
        assert!(check_generic_type_consistency(env, &[oids::INT4], &d).unwrap());
    }

    #[test]
    fn anyarray_requires_array() {
        let env = builtin_env();
        let d = [oids::ANYARRAY];
        assert!(!check_generic_type_consistency(env, &[oids::INT4], &d).unwrap());
        let e = enforce_generic_type_consistency(env, &[oids::INT4], &d, oids::ANYELEMENT).unwrap_err();
        assert_eq!(e.message, "argument declared anyarray is not an array but type integer");
    }

    #[test]
    fn anyarray_element_mismatch() {
        let env = builtin_env();
        let d = [oids::ANYARRAY, oids::ANYELEMENT];
        assert!(!check_generic_type_consistency(env, &[oids::INT4_ARRAY, oids::TEXT], &d).unwrap());
    }

    #[test]
    fn anyenum_requires_enum() {
        let env = builtin_env();
        assert!(!check_generic_type_consistency(env, &[oids::INT4], &[oids::ANYENUM]).unwrap());
    }

    #[test]
    fn result_without_input_family() {
        let env = builtin_env();
        let e = enforce_generic_type_consistency(env, &[oids::INT4], &[oids::INT4], oids::ANYELEMENT).unwrap_err();
        assert_eq!(e.message, "cannot determine result data type");
    }

    #[test]
    #[ignore = "needs TYP-54"]
    fn compatible_append_numeric() {
        let env = builtin_env();
        let d = [oids::ANYCOMPATIBLEARRAY, oids::ANYCOMPATIBLE];
        let a = [oids::INT4_ARRAY, oids::NUMERIC];
        assert!(check_generic_type_consistency(env, &a, &d).unwrap());
        let (_, ret) = enforce_generic_type_consistency(env, &a, &d, oids::ANYCOMPATIBLEARRAY).unwrap();
        assert_eq!(ret, oids::NUMERIC_ARRAY);
    }

    #[test]
    #[ignore = "needs TYP-54"]
    fn compatible_unknown_and_mismatch() {
        let env = builtin_env();
        let d = [oids::ANYCOMPATIBLEARRAY, oids::ANYCOMPATIBLE];
        let (_, ret) =
            enforce_generic_type_consistency(env, &[oids::UNKNOWN, oids::UNKNOWN], &d, oids::ANYCOMPATIBLEARRAY)
                .unwrap();
        assert_eq!(ret, oids::TEXT_ARRAY);
        assert!(!check_generic_type_consistency(env, &[oids::INT4_ARRAY, oids::TEXT], &d).unwrap());
    }
}
