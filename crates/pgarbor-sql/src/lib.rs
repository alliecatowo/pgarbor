//! PGArbor SQL front end: `sqlparser` 0.63 adapter, binder, bound IR and deparser.
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor-sql.md` for the design and `contracts/leases/pgarbor-sql.json` for the
//! implementation leases (SQL-01..).
//!
//! ## Pipeline
//! ```text
//! &str --parser::parse--> Vec<RawStatement> --binder::bind(&BindContext)--> BindOutput
//!        (whole string first, 42601)             (per statement, at execution time)
//!   BindOutput { statement: BoundStatement, param_types, notices }
//! ```
//! ## Module map
//! | area | modules | role |
//! |---|---|---|
//! | text -> AST | [`parser`], [`parser_extra`], [`extra_stmt`], [`syntax_error`], [`classify`], [`source`] | tokenizing, statement splitting, PG-style syntax errors, per-variant classification |
//! | bound IR | [`ir`] | `BoundStatement`, `BoundQuery`, `BoundExpr` (typed, name resolved) + traversal helpers |
//! | binder | [`binder`] | name resolution, type resolution/coercion, parameter inference, grouping rules, DDL specs |
//! | deparse | [`deparse`], [`rewrite`] | canonical `pg_get_expr` / `pg_get_viewdef` text; catalog `ExprRewriter` |
//! | shared | [`names`], [`diag`] | identifier folding, PG-exact parse-analysis errors |
#![allow(clippy::too_many_arguments)]

pub mod binder;
pub mod classify;
pub mod deparse;
pub mod diag;
pub mod extra_stmt;
pub mod identity_opts;
pub mod ir;
pub mod names;
pub mod parser;
pub mod parser_extra;
pub mod rewrite;
pub mod source;
pub mod syntax_error;

pub use binder::{bind, BindContext, BindOutput, BindSession};
pub use ir::{BoundExpr, BoundQuery, BoundStatement, ExprKind};
pub use parser::{parse, parse_expr_text, parse_one, Parsed, RawStatement};
pub use source::SourceMap;
