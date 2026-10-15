//! Bound intermediate representation (module docs of the three files).
//!
//! * [`expr`]: `BoundExpr` and every expression node.
//! * [`expr_walk`]: traversal/rewrite helpers (the only exhaustive child enumeration).
//! * [`query`]: `BoundQuery`, range table, FROM tree, target list, windows, CTEs.
//! * [`stmt`]: `BoundStatement`, DML and utility statements, ALTER actions.
pub mod expr;
pub mod expr_walk;
pub mod query;
pub mod stmt;

pub use expr::*;
pub use query::*;
pub use stmt::*;
