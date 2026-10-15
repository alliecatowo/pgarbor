//! Stored expressions: binding catalog expression text and producing `StoredExpr` (SQL-29, sonnet).
//! The catalog stores defaults, CHECKs, index expressions/predicates, generation expressions and view
//! queries as CANONICAL TEXT (`pg_get_expr` format) plus a dependency list (`StoredExpr { sql, bound:
//! None, depends_on }`, catalog contract invariant 5 and amendment 04).
//!
//! ## Two directions
//! 1. **DDL time** ([`store_expr`]): after binding a user expression (`CREATE TABLE t(a int DEFAULT
//!    nextval('s'), CHECK (a > 0))`) deparse it with `deparse::expr::deparse_expr` (canonical PG text,
//!    e.g. `nextval('s'::regclass)`, `(a > 0)`) and collect dependencies with [`collect_dependencies`].
//! 2. **Use time** ([`bind_stored_expr`]): re-parse `StoredExpr::sql` (`parser::parse_expr_text`) and bind it
//!    again in the scope of its table (DML defaults/checks/generated columns, ALTER COLUMN TYPE USING
//!    fallback, partial index predicate matching). Stored text came out of our own deparser, so
//!    bind errors there are internal (XX000 `could not bind stored expression`).
//!
//! ## Binding scopes for DDL expressions
//! A table that does not exist yet (CREATE TABLE) or changes (ALTER) has no `Relation` to open: use
//! [`table_expr_scope`], which pushes a `ScopeKind::TableExpr` scope whose single range entry (rt 1)
//! is a synthetic relation named `table_name` with the given `(column name, type)` list in attnum order.
//! Column references bind to `Column{rt_index: 1, attnum}`. The caller pops the scope with
//! `Binder::scopes.pop()`.
//!
//! ## Dependencies ([`collect_dependencies`])
//! Walk the bound expression (`BoundExpr::walk`): `Column{rt_index: 1}` -> `ObjectAddress::column(table,
//! attnum)` (when the table OID is known; for CREATE TABLE the catalog adds the self-column deps itself:
//! pass `table: None`), `FuncCall`/`Op`/`Aggref` of user functions (OID >= 16384) -> `ObjClass::Proc`,
//! user types in casts/consts/arrays (`ty.oid >= 16384`, enum/domain/composite) -> `ObjClass::Type`, a
//! `regclass` constant (`'s'::regclass`, created by `nextval('s')`) -> the sequence/relation, `SubLink`
//! bodies are rejected before this point. Builtin objects (OID < 16384) are never dependencies. Order:
//! first occurrence, de-duplicated.
use pgarbor_catalog::{ObjectAddress, StoredExpr};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::TypeId;

use super::context::{Binder, ExprCtx};
use crate::ir::BoundExpr;

/// Bind catalog expression text in the scope of an existing relation (`rel_oid`): opens a
/// `TableExpr` scope over the relation's columns (`from::relation_range_entry`), parses and binds the
/// text with `ec`, pops the scope. The caller applies the final coercion to the column type
/// (`coerce::coerce_assignment`).
#[allow(unused_variables)]
pub fn bind_stored_expr(b: &mut Binder, sql: &str, rel_oid: Oid, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-29", "stored expression binding (stored::bind_stored_expr)"))
}

/// Push a `TableExpr` scope over a synthetic relation (module docs). `columns` = (name, type) in attnum
/// order; dropped/system columns not representable here.
#[allow(unused_variables)]
pub fn table_expr_scope(b: &mut Binder, table_name: &str, columns: &[(String, TypeId)]) -> Result<()> {
    Err(PgError::unimplemented("SQL-29", "synthetic table scope (stored::table_expr_scope)"))
}

/// Deparse + dependencies -> `StoredExpr` (module docs, direction 1). `table`: OID of the owning table
/// when it exists (ALTER), `None` for CREATE TABLE (catalog supplies those edges).
#[allow(unused_variables)]
pub fn store_expr(b: &mut Binder, bound: &BoundExpr, table: Option<Oid>, column_names: &dyn Fn(i16) -> Option<String>) -> Result<StoredExpr> {
    Err(PgError::unimplemented("SQL-29", "StoredExpr construction (stored::store_expr)"))
}

/// Dependency list of a bound expression (module docs).
#[allow(unused_variables)]
pub fn collect_dependencies(b: &Binder, e: &BoundExpr, table: Option<Oid>) -> Result<Vec<ObjectAddress>> {
    Err(PgError::unimplemented("SQL-29", "dependency collection (stored::collect_dependencies)"))
}

/// Convenience for DDL binders: bind AST expression `e` in the current `TableExpr` scope with `ec`,
/// coerce it (assignment context) to `target` if given, and return (bound, stored). `columns_ref`
/// receives the attnums of columns referenced, in order of first reference (CHECK `conkey`).
#[allow(unused_variables)]
pub fn bind_ddl_expr(
    b: &mut Binder,
    e: &sqlparser::ast::Expr,
    ec: ExprCtx,
    target: Option<TypeId>,
    table: Option<Oid>,
    columns_ref: &mut Vec<i16>,
    column_names: &dyn Fn(i16) -> Option<String>,
) -> Result<(BoundExpr, StoredExpr)> {
    Err(PgError::unimplemented("SQL-29", "DDL expression binding (stored::bind_ddl_expr)"))
}
