//! SQL three-valued logic (TYPE-04). Fully implemented (no lease).
//!
//! `Option<bool>`: `None` = SQL NULL (unknown). Truth tables per SQL standard / PG:
//! AND: false dominates (`false AND NULL` = false), `true AND NULL` = NULL.
//! OR: true dominates (`true OR NULL` = true), `false OR NULL` = NULL. NOT NULL = NULL.
//! `IS TRUE`/`IS FALSE`/`IS UNKNOWN` (and NOT variants) never return NULL.
//! WHERE / JOIN ON / CHECK: a row qualifies only when the predicate is TRUE (`qualifies`); CHECK
//! constraints PASS when the predicate is TRUE or NULL (`check_passes`).
//! `IS [NOT] DISTINCT FROM` lives in `compare::not_distinct` (type-aware).
// CONTRACT: all functions below are frozen (v1).
use crate::datum::{type_mismatch, Datum};
use pgarbor_common::Result;

/// Three-valued AND.
pub fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// Three-valued OR.
pub fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// Three-valued NOT.
pub fn not3(a: Option<bool>) -> Option<bool> {
    a.map(|v| !v)
}

/// AND over many operands (empty = TRUE), short-circuit semantics are the evaluator's business.
pub fn and_all<I: IntoIterator<Item = Option<bool>>>(it: I) -> Option<bool> {
    it.into_iter().fold(Some(true), and3)
}

/// OR over many operands (empty = FALSE).
pub fn or_all<I: IntoIterator<Item = Option<bool>>>(it: I) -> Option<bool> {
    it.into_iter().fold(Some(false), or3)
}

/// `x IS TRUE`.
pub fn is_true(a: Option<bool>) -> bool {
    a == Some(true)
}

/// `x IS FALSE`.
pub fn is_false(a: Option<bool>) -> bool {
    a == Some(false)
}

/// `x IS UNKNOWN`.
pub fn is_unknown(a: Option<bool>) -> bool {
    a.is_none()
}

/// WHERE/ON/HAVING semantics: only TRUE qualifies.
pub fn qualifies(a: Option<bool>) -> bool {
    a == Some(true)
}

/// CHECK constraint semantics: TRUE or NULL passes.
pub fn check_passes(a: Option<bool>) -> bool {
    a != Some(false)
}

/// View a boolean Datum as `Option<bool>` (`Null` → None). Non-bool variants are an internal error.
pub fn datum_to_bool3(d: &Datum) -> Result<Option<bool>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(*b)),
        other => Err(type_mismatch("Bool", other)),
    }
}

/// `Option<bool>` → boolean Datum.
pub fn bool3_to_datum(v: Option<bool>) -> Datum {
    match v {
        Some(b) => Datum::Bool(b),
        None => Datum::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truth_tables() {
        let vals = [Some(true), Some(false), None];
        for a in vals {
            for b in vals {
                assert_eq!(and3(a, b), and3(b, a));
                assert_eq!(or3(a, b), or3(b, a));
                // De Morgan holds in 3VL
                assert_eq!(not3(and3(a, b)), or3(not3(a), not3(b)));
            }
        }
        assert_eq!(and3(Some(false), None), Some(false));
        assert_eq!(and3(Some(true), None), None);
        assert_eq!(or3(Some(true), None), Some(true));
        assert_eq!(or3(Some(false), None), None);
        assert!(check_passes(None) && !qualifies(None));
        assert_eq!(and_all([]), Some(true));
        assert_eq!(or_all([]), Some(false));
    }
}
