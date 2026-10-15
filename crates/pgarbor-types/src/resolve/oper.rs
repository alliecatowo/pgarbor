//! Operator resolution (TYPE-03, TYPE-12). Lease TYP-56 (sonnet). Re-exported as `crate::ops`.
//!
//! PG18 reference: `src/backend/parser/parse_oper.c` — `oper`, `left_oper`, `oper_select_candidate`,
//! `binary_oper_exact`, `op_error`; docs "10.2 Operators".
//!
//! `resolve_operator(env, name, left, right, candidates)`; `left = None` for prefix operators:
//! 1. Exact match (left, right) among candidates. Special case (PG `binary_oper_exact`): if one input is
//!    `unknown` and the other is type T, first look for an exact `T op T` operator (so `'1' + 1` →
//!    int4 + int4, and `now() - '1 day'` → timestamptz - timestamptz → the literal then fails input with
//!    22007 `invalid input syntax for type timestamp with time zone: "1 day"` — oracle-verified).
//!    Domains: also try with domain inputs reduced to base types.
//! 2. Otherwise filter candidates by implicit coercibility (`coerce::can_coerce_type`) and run
//!    `func::select_candidate` (same heuristic as functions).
//! 3. Errors: none → 42883 `operator does not exist: integer + text` (prefix: `operator does not exist:
//!    - text`) + HINT `No operator matches the given name and argument types. You might need to add explicit
//!    type casts.`; ambiguous → 42725 `operator is not unique: unknown + unknown` (prefix: `- unknown`)
//!    + HINT `Could not choose a best candidate operator. You might need to add explicit type casts.`
//!    (oracle: `'a' + 'b'` → not unique; `-'1'` → `operator is not unique: - unknown`).
//!    If the selected operator's proc is missing (oprcode = 0) → 42883 `operator is only a shell: ...`.
//! The result carries the operator's proc OID (`BuiltinOperator::proc`) which the executor calls via
//! `func::implementation`, and the (possibly polymorphic-resolved) result type.
//! Also provides `IS DISTINCT FROM`/`= ANY(array)` support helpers: `resolve_operator` with `right` =
//! element type is the binder's job.
use crate::catalog_data::{self, CastContext, OprKind};
use crate::context::TypeEnv;
use crate::datum::TypeId;
use crate::oids;
use crate::registry;
use crate::resolve::{coerce, func, polymorphic};
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// A candidate operator (builtin from `catalog_data::OPERATORS` or user-defined).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperCandidate {
    pub oid: Oid,
    pub name: String,
    pub kind: OprKind,
    /// 0 for prefix operators.
    pub left: Oid,
    pub right: Oid,
    pub result: Oid,
    /// Implementing proc OID.
    pub proc: Oid,
    pub commutator: Oid,
    pub negator: Oid,
    /// Search-path position (lower wins on identical signatures).
    pub path_pos: u32,
}

/// Successful operator resolution.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOperator {
    pub oid: Oid,
    pub proc: Oid,
    /// Types the left (None for prefix) and right operands must be coerced to.
    pub left: Option<Oid>,
    pub right: Oid,
    /// Resolved result type.
    pub result: TypeId,
    pub commutator: Oid,
    pub negator: Oid,
}

/// All builtin operators with this name.
// CONTRACT
pub fn builtin_operator_candidates(name: &str) -> Result<Vec<OperCandidate>> {
    Ok(catalog_data::operators_named(name)
        .map(|o| OperCandidate {
            oid: o.oid,
            name: o.name.to_string(),
            kind: o.kind,
            left: o.left,
            right: o.right,
            result: o.result,
            proc: o.proc,
            commutator: o.commutator,
            negator: o.negator,
            path_pos: 0,
        })
        .collect())
}

/// `a + b` / `- a` display form used in error messages (PG `op_signature_string`).
fn op_signature(env: &dyn TypeEnv, name: &str, left: Option<Oid>, right: Oid) -> String {
    use crate::names::display_name;
    match left {
        Some(l) => format!("{} {} {}", display_name(env, l), name, display_name(env, right)),
        None => format!("{} {}", name, display_name(env, right)),
    }
}

fn not_found(env: &dyn TypeEnv, name: &str, left: Option<Oid>, right: Oid) -> PgError {
    PgError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {}", op_signature(env, name, left, right)),
    )
    .with_hint("No operator matches the given name and argument types. You might need to add explicit type casts.")
}

fn ambiguous(env: &dyn TypeEnv, name: &str, left: Option<Oid>, right: Oid) -> PgError {
    PgError::new(
        SqlState::AMBIGUOUS_FUNCTION,
        format!("operator is not unique: {}", op_signature(env, name, left, right)),
    )
    .with_hint("Could not choose a best candidate operator. You might need to add explicit type casts.")
}

fn is_polymorphic(oid: Oid) -> bool {
    matches!(
        oid,
        oids::ANYELEMENT
            | oids::ANYARRAY
            | oids::ANYNONARRAY
            | oids::ANYENUM
            | oids::ANYRANGE
            | oids::ANYMULTIRANGE
            | oids::ANYCOMPATIBLE
            | oids::ANYCOMPATIBLEARRAY
            | oids::ANYCOMPATIBLENONARRAY
            | oids::ANYCOMPATIBLERANGE
            | oids::ANYCOMPATIBLEMULTIRANGE
    )
}

/// Finish resolution for a chosen candidate: shell check and polymorphic resolution.
fn finish(
    env: &dyn TypeEnv,
    name: &str,
    left: Option<Oid>,
    right: Oid,
    c: &OperCandidate,
) -> Result<ResolvedOperator> {
    if c.proc == 0 {
        return Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("operator is only a shell: {}", op_signature(env, name, left, right)),
        ));
    }
    let poly = is_polymorphic(c.left) || is_polymorphic(c.right) || is_polymorphic(c.result);
    let (rl, rr, res) = if poly {
        let (actual, declared): (Vec<Oid>, Vec<Oid>) = match left {
            Some(l) => (vec![l, right], vec![c.left, c.right]),
            None => (vec![right], vec![c.right]),
        };
        let (decl, ret) = polymorphic::enforce_generic_type_consistency(env, &actual, &declared, c.result)?;
        match left {
            Some(_) => (Some(decl[0]), decl[1], ret),
            None => (None, decl[0], ret),
        }
    } else {
        (left.map(|_| c.left), c.right, c.result)
    };
    Ok(ResolvedOperator {
        oid: c.oid,
        proc: c.proc,
        left: rl,
        right: rr,
        result: TypeId::of(res),
        commutator: c.commutator,
        negator: c.negator,
    })
}

fn exact<'a>(cands: &[&'a OperCandidate], l: Option<Oid>, r: Oid) -> Option<&'a OperCandidate> {
    cands.iter().copied().find(|c| match l {
        Some(l) => c.left == l && c.right == r,
        None => c.right == r,
    })
}

/// Resolve an operator (see module docs).
// CONTRACT
pub fn resolve_operator(
    env: &dyn TypeEnv,
    name: &str,
    left: Option<Oid>,
    right: Oid,
    candidates: &[OperCandidate],
) -> Result<ResolvedOperator> {
    // Arity filter, then drop duplicates of the same signature in favour of the lowest path_pos.
    let want = if left.is_some() { OprKind::Infix } else { OprKind::Prefix };
    let mut cands: Vec<&OperCandidate> = Vec::new();
    for c in candidates.iter().filter(|c| c.name == name && c.kind == want) {
        match cands.iter().position(|e| e.left == c.left && e.right == c.right) {
            Some(i) => {
                if c.path_pos < cands[i].path_pos {
                    cands[i] = c;
                }
            }
            None => cands.push(c),
        }
    }

    // Step 1: exact match.
    match left {
        Some(l) => {
            let (mut a, mut b) = (l, right);
            let was_unknown = a == oids::UNKNOWN || b == oids::UNKNOWN;
            if a == oids::UNKNOWN {
                a = b;
            } else if b == oids::UNKNOWN {
                b = a;
            }
            if let Some(c) = exact(&cands, Some(a), b) {
                return finish(env, name, left, right, c);
            }
            if was_unknown {
                let base = registry::base_type(env, a);
                if base != a {
                    if let Some(c) = exact(&cands, Some(base), base) {
                        return finish(env, name, left, right, c);
                    }
                }
            }
        }
        None => {
            if let Some(c) = exact(&cands, None, right) {
                return finish(env, name, left, right, c);
            }
        }
    }

    // Step 2: implicit-coercibility filter, then the shared selection heuristic.
    let inputs: Vec<Oid> = match left {
        Some(l) => vec![l, right],
        None => vec![right],
    };
    let mut ok: Vec<&OperCandidate> = Vec::new();
    for c in cands.iter().copied() {
        let targets: Vec<Oid> = match left {
            Some(_) => vec![c.left, c.right],
            None => vec![c.right],
        };
        if coerce::can_coerce_type(env, &inputs, &targets, CastContext::Implicit)? {
            ok.push(c);
        }
    }
    match ok.len() {
        0 => Err(not_found(env, name, left, right)),
        1 => finish(env, name, left, right, ok[0]),
        _ => {
            let arg_types: Vec<Vec<Oid>> = ok
                .iter()
                .map(|c| match left {
                    Some(_) => vec![c.left, c.right],
                    None => vec![c.right],
                })
                .collect();
            let picked = match func::select_candidate(env, &inputs, &arg_types) {
                Ok(p) => p,
                Err(e) if e.sqlstate == SqlState::AMBIGUOUS_FUNCTION => return Err(ambiguous(env, name, left, right)),
                Err(e) => return Err(e),
            };
            if picked.len() == 1 && picked[0] < ok.len() {
                finish(env, name, left, right, ok[picked[0]])
            } else {
                Err(ambiguous(env, name, left, right))
            }
        }
    }
}

/// Convenience: resolve against builtin operators only.
pub fn resolve_builtin_operator(env: &dyn TypeEnv, name: &str, left: Option<Oid>, right: Oid) -> Result<ResolvedOperator> {
    let cands = builtin_operator_candidates(name)?;
    resolve_operator(env, name, left, right, &cands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oids::*;

    fn r(name: &str, l: Option<Oid>, rt: Oid) -> Result<ResolvedOperator> {
        resolve_builtin_operator(registry::builtin_env(), name, l, rt)
    }

    #[test]
    fn int4_plus_int4() {
        let o = r("+", Some(INT4), INT4).unwrap();
        assert_eq!(o.oid, 551);
        assert_eq!(o.result, TypeId::INT4);
    }

    #[test]
    fn unknown_plus_int4() {
        let o = r("+", Some(UNKNOWN), INT4).unwrap();
        assert_eq!((o.left, o.right), (Some(INT4), INT4));
    }

    #[test]
    fn timestamptz_minus_unknown() {
        let o = r("-", Some(TIMESTAMPTZ), UNKNOWN).unwrap();
        assert_eq!((o.left, o.right), (Some(TIMESTAMPTZ), TIMESTAMPTZ));
    }

    #[test]
    fn unknown_unknown_ambiguous() {
        let e = r("+", Some(UNKNOWN), UNKNOWN).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::AMBIGUOUS_FUNCTION);
        assert_eq!(e.message, "operator is not unique: unknown + unknown");
        assert!(e.hint.is_some());
    }

    #[test]
    fn int4_plus_text_missing() {
        let e = r("+", Some(INT4), TEXT).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "operator does not exist: integer + text");
        assert!(e.hint.is_some());
    }

    #[test]
    fn prefix_unknown_ambiguous() {
        let e = r("-", None, UNKNOWN).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::AMBIGUOUS_FUNCTION);
        assert_eq!(e.message, "operator is not unique: - unknown");
    }

    #[test]
    fn numeric_plus_int4() {
        let o = r("+", Some(NUMERIC), INT4).unwrap();
        assert_eq!((o.left, o.right), (Some(NUMERIC), NUMERIC));
    }

    #[test]
    fn candidates_nonempty() {
        assert!(!builtin_operator_candidates("+").unwrap().is_empty());
        assert!(builtin_operator_candidates("no_such_op").unwrap().is_empty());
    }
}
