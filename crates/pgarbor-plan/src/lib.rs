//! PGArbor planner: bound IR -> logical plan -> optimized physical plan, plus EXPLAIN.
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor-plan.md` and `contracts/leases/pgarbor-plan.json` (PLN-01..).
//!
//! ## Pipeline (`planner::plan_statement`)
//! ```text
//! BoundStatement --build/build_dml--> LogicalPlan --simplify/pushdown/subplan--> LogicalPlan
//!   --planner (index, join, agg, sort, window, setop, cte)--> PlanNode tree (bind-time exprs)
//!   --explain::annotate (only for EXPLAIN)--> --setrefs::finalize--> executable Plan
//! ```
//! ## Module map
//! | area | modules |
//! |---|---|
//! | data | [`plan`] (physical plan, the exec contract), [`logical`], [`context`] |
//! | logical build | [`build`], [`build_dml`], [`cte`] |
//! | rewrites | [`simplify`], [`pushdown`], [`subplan`] |
//! | physical choices | [`planner`], [`index`], [`join`], [`agg`], [`window`], [`sort_limit`], [`setop`], [`cost`] |
//! | finalization | [`setrefs`] |
//! | EXPLAIN | [`explain`], [`explain_fmt`] |
#![allow(clippy::too_many_arguments)]

pub mod agg;
pub mod build;
pub mod build_dml;
pub mod context;
pub mod cost;
pub mod cte;
pub mod explain;
pub mod explain_fmt;
pub mod index;
pub mod join;
pub mod logical;
pub mod plan;
pub mod planner;
pub mod pushdown;
pub mod setop;
pub mod setrefs;
pub mod simplify;
pub mod sort_limit;
pub mod subplan;
pub mod window;

pub use context::{PlanContext, PlanOptions};
pub use plan::{ColumnDesc, Plan, PlanKind, PlanNode};

use pgarbor_common::Result;
use pgarbor_sql::ir::BoundStatement;

/// Plan a bound statement (ARCHITECTURE §10): `SELECT`/`VALUES`/`INSERT`/`UPDATE`/`DELETE`/
/// `CREATE TABLE AS`. Utility statements are the executor's: passing one is an internal error
/// (XX000 `statement is not plannable`). See [`planner::plan_statement`].
pub fn plan(stmt: &BoundStatement, ctx: &PlanContext) -> Result<Plan> {
    planner::plan_statement(stmt, ctx)
}

/// Render a plan as EXPLAIN output lines (text format) or the requested format's lines.
/// See [`explain::explain_plan`].
pub fn explain_plan(plan: &Plan, options: &pgarbor_sql::ir::ExplainOptions) -> Result<Vec<String>> {
    explain::explain_plan(plan, options)
}
