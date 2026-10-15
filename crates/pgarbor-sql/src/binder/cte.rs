//! WITH clauses: non-recursive, recursive, data-modifying CTEs (SQL-46).
//! PG references: `parse_cte.c` (`transformWithClause`, `analyzeCTE`, `checkWellFormedRecursion`),
//! `parse_clause.c` (`transformRangeTableRef` for CTE references), `analyze.c`.
//!
//! ## Visibility rules
//! * Names are visible to later CTEs of the same `WITH` and to the main query; with `WITH RECURSIVE`
//!   a CTE is visible to itself and (in order of appearance after dependency sorting) to all
//!   siblings; without RECURSIVE it is not visible to itself (a self reference then resolves to a
//!   table of that name, or fails with 42P01).
//! * Duplicate names in one WITH: 42712 `WITH query name "x" specified more than once`.
//! * Inner WITH shadows outer ones; CTE names shadow tables in FROM (`schema.x` is never a CTE).
//! * `CteFrame::entries` is pushed on `Binder::cte_frames` before binding the bodies so references
//!   resolve through [`find_cte`], which returns `RangeKind::Cte { cte_index, levels_up, .. }` data.
//!
//! ## Recursive CTEs
//! Shape required: `non_recursive_term UNION [ALL] recursive_term`. Errors (all 42P19 except where
//! noted): `recursive query "x" does not have the form non-recursive-term UNION [ALL] recursive-term`;
//! self reference in the non-recursive term -> `recursive reference to query "x" must not appear
//! within its non-recursive term` (42P19); in a subquery/outer join right side/EXCEPT/aggregate ->
//! the matching `recursive reference to query "x" must not appear within ...` messages
//! (`checkWellFormedRecursion`); more than one self reference -> `recursive reference to query "x"
//! must not appear more than once`; column types of the two terms must match after coercion
//! (42804 `recursive query "x" column 1 has type integer in non-recursive term but type numeric
//! overall` + HINT `Cast the output of the non-recursive term to the correct type.`).
//! The bound CTE has `recursive = true`; its body is `SetOp(Union)` with the non-recursive term
//! first. The self reference binds as `RangeKind::Cte { recursive_self: true }` with the columns
//! of the non-recursive term.
//!
//! ## Data-modifying CTEs
//! `WITH t AS (INSERT/UPDATE/DELETE ... RETURNING ...)` -> `CteBody::Modify(BoundStatement)`;
//! allowed only at the top level of the statement (inner WITH: 0A000 `WITH clause containing a
//! data-modifying statement must be at the top level`); without RETURNING the CTE has no columns
//! and cannot be referenced (42P01 on reference, `WITH query "x" does not have a RETURNING clause`).
//! `MATERIALIZED` / `NOT MATERIALIZED` hints are recorded; `SEARCH`/`CYCLE` clauses: 0A000
//! (`SEARCH`/`CYCLE` in WITH is not supported by PGArbor yet).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::With;

use super::context::Binder;
use crate::ir::{BoundCte, OutputColumn};

/// One visible CTE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteEntry {
    pub name: String,
    /// Output columns once analyzed (after the column alias list); empty while the body is being
    /// analyzed for a not-yet-typed recursive reference (then `recursive_columns` is used).
    pub columns: Vec<OutputColumn>,
    /// Index in the owning query's `BoundQuery::ctes` / `BoundInsert::ctes`.
    pub index: u32,
    pub recursive: bool,
    /// Currently inside the body of this CTE (self reference allowed iff `recursive`).
    pub in_progress: bool,
    /// Data-modifying CTE without RETURNING: cannot be referenced.
    pub no_returning: bool,
    /// References seen so far (incremented by `find_cte`); copied into `BoundCte::ref_count`.
    pub ref_count: u32,
}

/// The CTEs of one WITH clause (one frame per `bind_with` call; innermost last on the Binder).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CteFrame {
    pub entries: Vec<CteEntry>,
}

/// Result of resolving a name against the CTE frames.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteRef {
    pub cte_index: u32,
    /// How many WITH-owning query levels out the CTE lives (0 = the query being bound or its own WITH).
    pub levels_up: u32,
    pub recursive_self: bool,
    pub columns: Vec<OutputColumn>,
}

/// Bind a `WITH` clause: pushes a `CteFrame` on `b.cte_frames` (the caller pops it with
/// [`pop_with`] after the main body is bound), analyzes every CTE in dependency order for
/// RECURSIVE, and returns the bound CTEs in written order. `top_level` is true when the owning
/// statement is the outermost one (data-modifying CTEs allowed). Uses `select::bind_query` for
/// query bodies and `dispatch::bind_inner_statement` for DML bodies.
#[allow(unused_variables)]
pub fn bind_with(b: &mut Binder, with: &With, top_level: bool) -> Result<Vec<BoundCte>> {
    Err(PgError::unimplemented("SQL-46", "WITH clause binding (cte::bind_with)"))
}

/// Remove the frame pushed by `bind_with` and compute each bound CTE's `ref_count`
/// (from `CteEntry::ref_count`; `materialized` hint semantics are the planner's).
pub fn pop_with(b: &mut Binder) {
    b.cte_frames.pop();
}

/// Resolve an unqualified relation name against the visible CTEs (innermost frame first).
/// Returns `None` if no CTE has that name. A hit increments `CteEntry::ref_count` (the final count goes into `BoundCte::ref_count`). Errors: 42P01 for a
/// data-modifying CTE without RETURNING; 42P19-family for illegal recursive references is raised
/// by the checker inside `bind_with`, not here.
#[allow(unused_variables)]
pub fn find_cte(b: &mut Binder, name: &str, pos: Option<u32>) -> Result<Option<CteRef>> {
    Err(PgError::unimplemented("SQL-46", "CTE name lookup (cte::find_cte)"))
}
