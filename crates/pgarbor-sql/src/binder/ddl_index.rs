//! CREATE INDEX (SQL-32). PG references: `indexcmds.c` (`DefineIndex`, `ComputeIndexAttrs`,
//! `ResolveOpClass`, `CheckPredicate`), `parse_utilcmd.c` (`transformIndexStmt`), `gram.y`
//! (`IndexStmt`, `index_elem`).
//!
//! `bind_create_index(b, ci)` -> `BoundStatement::CreateIndex(Box<IndexSpec>)` (sqlparser `CreateIndex {
//! name, table_name, using, columns: Vec<IndexColumn>, unique, concurrently, if_not_exists, include,
//! nulls_distinct, with, predicate, index_options, alter_options }`):
//! * Table: `env.resolve_relation` (42P01); must be a table or materialized view (42809 `"v" is not a
//!   table or materialized view`); system catalog: 42501 `permission denied: "pg_class" is a system
//!   catalog`.
//! * Name: optional; `None` -> `IndexSpec.name = None` (catalog generates `<table>_<cols>_idx`). Index
//!   lives in the TABLE's schema. A qualified index name is a GRAMMAR error in PG (oracle):
//!   `create index public.zi on t(b)` -> 42601 `syntax error at or near "."` (position of the dot).
//! * Method (`USING`): `btree` (default) -> `IndexMethod::Btree`; `hash`/`gist`/`gin`/`brin`/`spgist`
//!   -> 0A000 `index method "gin"` unsupported by PGArbor (still recognized, catalog supports only btree);
//!   unknown -> 42704 `access method "foo" does not exist`.
//! * Keys (`IndexColumn { column: OrderByExpr, operator_class }`): a bare column name -> `column:
//!   Some(name)` (42703 `column "x" does not exist`); anything else is an expression key: bind in the
//!   table scope with `ExprCtx::index_expr()` (functions must be IMMUTABLE: 42P17 `functions in index
//!   expression must be marked IMMUTABLE`; no aggregates/windows/sub-selects/SRFs), store with
//!   `stored::store_expr` (canonical text like `lower(name)`), `expr_type` = result type,
//!   `expr_func_name` = function name for a top-level call (drives the attribute name). `COLLATE`,
//!   operator class (`name_ops` — resolve by name among builtin btree opclasses: `text_pattern_ops`,
//!   `varchar_pattern_ops`, `bpchar_pattern_ops`, `int4_ops`...; unknown 42704 `operator class "x" does
//!   not exist for access method "btree"`), `ASC|DESC`, `NULLS FIRST|LAST` (explicit only).
//!   More than 32 key columns: 54011 `cannot use more than 32 columns in an index`.
//! * `INCLUDE (cols)`, `NULLS [NOT] DISTINCT` (`nulls_not_distinct`), `WHERE predicate` (boolean,
//!   immutable functions only: 42P17 `functions in index predicate must be marked IMMUTABLE`; column refs
//!   of the table; stored canonical text), `WITH (opt=value)` -> `options` (`fillfactor`,
//!   `deduplicate_items`; others 22023 `unrecognized parameter "x"`), `CONCURRENTLY` -> `concurrently`,
//!   `IF NOT EXISTS`, `UNIQUE`. Tablespace, `ONLY`, `CREATE INDEX .. ON ONLY`: 0A000.
//! * Duplicate index definitions are allowed (PG warns nothing).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::CreateIndex;

use super::context::Binder;
use crate::ir::BoundStatement;

/// Bind CREATE INDEX (module docs).
#[allow(unused_variables)]
pub fn bind_create_index(b: &mut Binder, ci: &CreateIndex) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-32", "CREATE INDEX binding (ddl_index::bind_create_index)"))
}
