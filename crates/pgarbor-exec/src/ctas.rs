//! `CREATE TABLE AS`, `SELECT INTO`, `CREATE MATERIALIZED VIEW` population and `REFRESH MATERIALIZED
//! VIEW` (lease EXE-38). PG: `commands/createas.c`, `commands/matview.c`.
//!
//! ## `exec_create_table_as(cx, bound, plan)` (`BoundCreateTableAs`, plan of kind `Populate`)
//! 1. `ddl_table::exec_create_table(&bound.table)` creates the (empty) table from the query's column
//!    list (the CreateTableSpec was built by the binder). `IF NOT EXISTS` skip: if the table existed
//!    return tag `CREATE TABLE AS` WITHOUT running the query (PG notice `relation "t" already exists,
//!    skipping`).
//! 2. If `bound.with_data`: `open_plan(plan)`, fetch all rows (streaming in batches of 1000) and insert
//!    each with `dml_insert::insert_row` (full constraint/index pipeline; the table has none besides
//!    NOT NULL implied by nothing), counting rows. Plan output columns map 1:1 to the table's columns
//!    (attnum order).
//! 3. Tag `SELECT n` (PG tags CTAS as `SELECT n`), `SELECT 0` for `WITH NO DATA`.
//! `SELECT ... INTO t` arrives as the same node.
//!
//! ## `populate_matview(cx, relation)` / `exec_refresh_matview_data`
//! Re-run the materialized view's stored query: the view row's `ViewDef::query` text is bound to a
//! `BoundQuery` (`pgarbor_sql::parse` + `bind` with the owner's env) and planned
//! (`pgarbor_plan::plan` with `PlanOptions::default()`), the matview's existing rows and index entries
//! are deleted (`rowstore::delete_relation_data` for the relation and each index), the plan's rows are
//! inserted (`dml_insert::insert_row`), and the catalog `populated` flag is already set by
//! `refresh_matview`. Unique index violations surface as 23505.
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_plan::plan::Plan;
use pgarbor_sql::ir::BoundCreateTableAs;

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_create_table_as(cx: &mut ExecContext, bound: &BoundCreateTableAs, plan: Option<&Plan>) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-38", "CREATE TABLE AS (ctas::exec_create_table_as)"))
}

#[allow(unused_variables)]
pub fn populate_matview(cx: &mut ExecContext, relation: Oid) -> Result<u64> {
    Err(PgError::unimplemented("EXE-38", "materialized view population (ctas::populate_matview)"))
}
