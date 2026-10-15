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
use crate::catalog_data::OprKind;
use crate::context::TypeEnv;
use crate::datum::TypeId;
use pgarbor_common::{Oid, PgError, Result};

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
#[allow(unused_variables)]
pub fn builtin_operator_candidates(name: &str) -> Result<Vec<OperCandidate>> {
    Err(PgError::unimplemented("TYP-56", "builtin operator candidates"))
}

/// Resolve an operator (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn resolve_operator(
    env: &dyn TypeEnv,
    name: &str,
    left: Option<Oid>,
    right: Oid,
    candidates: &[OperCandidate],
) -> Result<ResolvedOperator> {
    Err(PgError::unimplemented("TYP-56", "operator resolution"))
}

/// Convenience: resolve against builtin operators only.
pub fn resolve_builtin_operator(env: &dyn TypeEnv, name: &str, left: Option<Oid>, right: Oid) -> Result<ResolvedOperator> {
    let cands = builtin_operator_candidates(name)?;
    resolve_operator(env, name, left, right, &cands)
}
