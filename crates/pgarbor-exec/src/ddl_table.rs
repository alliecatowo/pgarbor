//! CREATE TABLE and ALTER TABLE execution (lease EXE-33, north-star). Catalog operations:
//! `pgarbor_catalog::ddl::{table, alter_table, alter_column, constraint, index}`.
//!
//! ## `exec_create_table(cx, spec) -> ExecResult`
//! `run_ddl(cx, |ddl| ddl.create_table(&spec))`. `Ok(None)` (IF NOT EXISTS skip) -> tag `CREATE TABLE`
//! (the catalog queued the `relation "t" already exists, skipping` NOTICE). `Some(created)`: a new table
//! is empty, so no index build is needed; create the SEQUENCE STATE of every `created.sequences` entry
//! (serial/identity) through `ddl_followup::run_followups` with the outcome's follow-ups (the catalog
//! queues `CreateSequenceState`). Tag `CREATE TABLE`.
//!
//! ## `exec_alter_table(cx, alter) -> ExecResult` (`BoundAlterTable`)
//! One `run_ddl` for the WHOLE statement (single catalog version bump, atomic). Apply
//! `alter.commands` in order; each variant calls the `CatalogTxn` method named in its doc
//! (`pgarbor_sql::ir::AlterTableCmd`), passing `&pgarbor_sql::rewrite::SqlExprRewriter` where the method
//! takes an `ExprRewriter`, `DropBehavior::{Cascade, Restrict}` from the `cascade` flag.
//! Data-level work comes back as `FollowUp`s in the outcome and is executed AFTER the closure by
//! `ddl_followup::run_followups`, in order, with `ColumnRewrite`s built here:
//! * `AddColumn`: `ddl.add_column(table, &spec, if_not_exists)`; a volatile default makes the catalog emit
//!   `RewriteTable`; supply `ColumnRewrite::FromDefault{attnum}` (the attnum returned by add_column);
//!   a STORED generated column supplies `Generated{attnum}`. A constant default needs no rewrite
//!   (`Column::missing_value`).
//! * `AlterColumnType{..}`: `ddl.alter_column_type(table, &column, new_type, new_collation, new_ndims,
//!   new_default, needs_rewrite)`; if `needs_rewrite` supply `Using{attnum, expr: using}` (binder
//!   guarantees `using` is `Some` then).
//! * `AddConstraint(spec)`: `ddl.add_constraint(table, &spec)` (PK/UNIQUE queue `BuildIndex`, NOT NULL /
//!   CHECK / FK queue the matching `Validate*` unless `NOT VALID`).
//! * the remaining commands map one-to-one (`drop_column`, `rename_column(.., rewriter)`,
//!   `set_column_default`, `set_not_null`, `drop_not_null`, `add_identity`, `set_identity`,
//!   `drop_identity`, `set_generation_expression`, `drop_expression`, `set_column_attribute`,
//!   `drop_constraint`, `rename_constraint`, `validate_constraint`, `alter_constraint`,
//!   `set_relation_options`, `set_persistence`, `set_replica_identity`, `set_row_security`).
//! * DROP COLUMN needs no data work (slots stay; `DropRelationData` for dropped indexes arrives as a
//!   follow-up).
//! Tag: `alter.keyword` -> `ALTER TABLE` / `ALTER VIEW` / `ALTER MATERIALIZED VIEW` / `ALTER INDEX` /
//! `ALTER SEQUENCE` / `ALTER FOREIGN TABLE` / `ALTER TYPE` (same as `BoundStatement::tag()`).
//! `SET NOT NULL` over NULLs: 23502 `column "a" of relation "t" contains null values`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundAlterTable;
use pgarbor_catalog::ddl::spec::CreateTableSpec;

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_create_table(cx: &mut ExecContext, spec: &CreateTableSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-33", "CREATE TABLE (ddl_table::exec_create_table)"))
}

#[allow(unused_variables)]
pub fn exec_alter_table(cx: &mut ExecContext, alter: &BoundAlterTable) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-33", "ALTER TABLE (ddl_table::exec_alter_table)"))
}
