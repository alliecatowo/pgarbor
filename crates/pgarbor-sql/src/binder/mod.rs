//! The binder: parse-analysis. Turns a `sqlparser` AST statement into a [`BoundStatement`]
//! (typed, name-resolved IR in `crate::ir`). PostgreSQL's `src/backend/parser/analyze.c` and its
//! `parse_*.c` files are the reference for every behavior; each module's docs name its files.
//!
//! ## Shape
//! * [`bind`] is the only entry point (a trivial wrapper; lib.rs/mod.rs are not leased).
//!   [`dispatch::bind_statement`] does the work: classify, create the [`Binder`] (per-statement
//!   mutable state), bind, finalize parameters, return a [`BindOutput`].
//! * Binding is PURE: no catalog writes, no side effects, no execution. It reads the catalog
//!   snapshot inside `BindContext::env` only.
//! * Every module exposes free functions taking `b: &mut Binder` (state: scope stack, CTE frames,
//!   parameter table, notices). Functions never panic; user errors are `PgError`s built with
//!   `crate::diag` (PG-exact wording, SQLSTATE, position).
//!
//! ## Module map
//! | area | modules |
//! |---|---|
//! | state, entry | [`context`], [`dispatch`], [`params`], [`scope`] |
//! | types, literals, coercion | [`types`], [`literal`], [`coerce`] |
//! | expressions | [`expr`], [`expr_ops`], [`expr_func`], [`expr_agg`], [`expr_misc`], [`expr_sublink`] |
//! | queries | [`select`], [`from`], [`join`], [`target`], [`group`], [`order`], [`window`], [`setop`], [`values`], [`cte`] |
//! | DML | [`insert`], [`on_conflict`], [`update_delete`], [`target_table`] |
//! | DDL | [`stored`], [`ddl_table`], [`ddl_constraint`], [`ddl_index`], [`ddl_alter`], [`ddl_alter_other`], [`ddl_objects`], [`ddl_view`], [`ddl_function`], [`ddl_drop`], [`ddl_grant`] |
//! | utility | [`utility`], [`utility_session`], [`copy`] |
pub mod coerce;
pub mod context;
pub mod copy;
pub mod cte;
pub mod ddl_alter;
pub mod ddl_alter_other;
pub mod ddl_constraint;
pub mod ddl_drop;
pub mod ddl_ext;
pub mod ddl_function;
pub mod ddl_grant;
pub mod ddl_index;
pub mod ddl_objects;
pub mod ddl_policy;
pub mod ddl_partition;
pub mod ddl_table;
pub mod ddl_view;
pub mod dispatch;
pub mod expr;
pub mod expr_agg;
pub mod expr_func;
pub mod expr_json;
pub mod expr_misc;
pub mod expr_ops;
pub mod expr_sublink;
pub mod from;
pub mod group;
pub mod insert;
pub mod join;
pub mod literal;
pub mod merge;
pub mod on_conflict;
pub mod order;
pub mod params;
pub mod rls;
pub mod plhook;
pub mod scope;
pub mod select;
pub mod setop;
pub mod stored;
pub mod target;
pub mod target_table;
pub mod types;
pub mod update_delete;
pub mod utility;
pub mod utility_session;
pub mod values;
pub mod view_dml;
pub mod window;

pub use context::{BindContext, BindOutput, BindSession, Binder, BoundArg, ExprCtx};

use crate::parser::RawStatement;
use pgarbor_common::Result;

/// Bind one parsed statement against the catalog snapshot in `ctx.env`.
///
/// Returns the bound statement, the final parameter types (for Describe /
/// ParameterDescription) and the notices raised while binding (identifier truncation, ...).
/// See [`dispatch::bind_statement`].
pub fn bind(stmt: &RawStatement, ctx: &BindContext) -> Result<BindOutput> {
    dispatch::bind_statement(stmt, ctx)
}
