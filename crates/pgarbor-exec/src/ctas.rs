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
use std::collections::HashMap;

use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::CatalogEnv;
use pgarbor_catalog::RelKind;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_plan::context::{PlanContext, PlanOptions};
use pgarbor_plan::explain_fmt::NodeActual;
use pgarbor_plan::plan::Plan;
use pgarbor_sql::ir::BoundCreateTableAs;
use pgarbor_sql::{BindContext, BindSession};
use pgarbor_types::Datum;

use crate::build::open_plan;
use crate::context::ExecContext;
use crate::ddl_table;
use crate::dml_insert;
use crate::result::ExecResult;
use crate::rowstore;
use crate::runtime::{Executor, Runtime};
use crate::tabledesc::{self, TableDesc};

/// Rows fetched from the query per batch (module docs).
const FETCH_BATCH: usize = 1000;

/// Interrupt poll interval in inserted rows (`ExecTxn::check_interrupts` contract: at least once per
/// 1024 rows in every long loop).
const INTERRUPT_EVERY: u64 = 1024;

/// `CREATE TABLE AS` / `SELECT INTO` (module docs). Tag `SELECT n`, or `SELECT 0` for
/// `WITH NO DATA`. The `IF NOT EXISTS` skip returns tag `CREATE TABLE AS` and never runs the query
/// (module docs; `BoundCreateTableAs` does not say whether the statement was `SELECT INTO`, so the
/// skip tag is always the CTAS one).
pub fn exec_create_table_as(cx: &mut ExecContext, bound: &BoundCreateTableAs, plan: Option<&Plan>) -> Result<ExecResult> {
    let spec = &bound.table;
    if bound.with_data && plan.is_none() {
        return Err(PgError::internal("CREATE TABLE AS requires a plan"));
    }
    let existed = cx.se.env.snapshot().class_in_namespace(spec.namespace, &spec.name).is_some();
    // Creates the table, or (IF NOT EXISTS and already present) skips it and queues the NOTICE. Any
    // other existing relation fails here with 42P07.
    ddl_table::exec_create_table(&mut *cx, spec)?;
    if existed {
        return Ok(ExecResult::command("CREATE TABLE AS"));
    }
    let plan = match plan {
        Some(p) if bound.with_data => p,
        _ => return Ok(ExecResult::command("SELECT 0")),
    };
    let oid = match cx.se.env.snapshot().class_in_namespace(spec.namespace, &spec.name) {
        Some(ClassRef::Relation(oid)) => oid,
        _ => return Err(PgError::internal(format!("table \"{}\" was not created", spec.name))),
    };
    let td = tabledesc::table_desc(&*cx, oid)?;
    let (n, _) = insert_query_rows(&mut *cx, plan, &td)?;
    Ok(ExecResult::command(format!("SELECT {n}")))
}

/// `EXPLAIN ANALYZE` of `CREATE TABLE AS ... WITH DATA` (PG: `ExplainOneUtility` hands the query to
/// `ExplainOnePlan` with the INTO clause, so the table is created and filled while the plan runs). The
/// caller has set `collect_stats`. Returns `None` when `IF NOT EXISTS` skipped the table (PG prints no
/// plan for it), else the actuals of the query plan.
pub fn analyze_create_table_as(
    cx: &mut ExecContext,
    bound: &BoundCreateTableAs,
    plan: &Plan,
) -> Result<Option<Vec<NodeActual>>> {
    let spec = &bound.table;
    let existed = cx.se.env.snapshot().class_in_namespace(spec.namespace, &spec.name).is_some();
    ddl_table::exec_create_table(&mut *cx, spec)?;
    if existed {
        return Ok(None);
    }
    let oid = match cx.se.env.snapshot().class_in_namespace(spec.namespace, &spec.name) {
        Some(ClassRef::Relation(oid)) => oid,
        _ => return Err(PgError::internal(format!("table \"{}\" was not created", spec.name))),
    };
    let td = tabledesc::table_desc(&*cx, oid)?;
    match insert_query_rows(&mut *cx, plan, &td)? {
        (_, Some(actuals)) => Ok(Some(actuals)),
        (_, None) => Err(PgError::internal("EXPLAIN ANALYZE: operator statistics were not collected for the plan")),
    }
}

/// `REFRESH MATERIALIZED VIEW` data step and the population after `CREATE MATERIALIZED VIEW ... WITH
/// DATA` (module docs). Returns the number of rows stored.
pub fn populate_matview(cx: &mut ExecContext, relation: Oid) -> Result<u64> {
    let query_text = {
        let snap = cx.se.env.snapshot();
        let rel = match snap.relation(relation) {
            Some(r) => r,
            None => {
                return Err(PgError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation with OID {relation} does not exist"),
                ))
            }
        };
        if !matches!(rel.kind, RelKind::MatView) {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a materialized view", rel.name)));
        }
        let view = match rel.view.as_ref() {
            Some(v) => v,
            None => return Err(PgError::internal(format!("materialized view \"{}\" has no definition", rel.name))),
        };
        view.query.sql.clone()
    };

    // Bind the stored definition with the owner's env (module docs: parse + bind, then plan with
    // default options). The owner env keeps the snapshot and the search path, with the owner as the
    // current user.
    let owner_env = {
        let snap = cx.se.env.snapshot();
        let owner = match snap.relation(relation) {
            Some(r) => r.owner,
            None => return Err(PgError::internal("materialized view vanished during refresh")),
        };
        CatalogEnv::with_resolved(snap.clone(), cx.se.env.path().clone(), owner)
    };
    let raw = pgarbor_sql::parse_one(&query_text)?;
    let bound = {
        let bctx = BindContext {
            env: &owner_env,
            session: BindSession { database: cx.se.services.database_name() },
            io_config: cx.se.io.clone(),
            statement_timestamp: cx.se.txn_ts,
            declared_params: Vec::new(),
        };
        pgarbor_sql::bind(&raw, &bctx)?
    };
    for n in &bound.notices {
        cx.notice(n.clone());
    }
    let plan = {
        let pctx = PlanContext {
            env: &owner_env,
            param_types: bound.param_types.clone(),
            param_values: None,
            options: PlanOptions::default(),
            table_rows: HashMap::new(),
        };
        pgarbor_plan::plan(&bound.statement, &pctx)?
    };

    // Remove the old contents: heap rows, then the entries of every index of the matview.
    let td = tabledesc::table_desc(&*cx, relation)?;
    {
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut *cx);
        rowstore::delete_relation_data(&mut ex, relation)?;
        for idx in &td.indexes {
            rowstore::delete_relation_data(&mut ex, idx.oid)?;
        }
    }

    let (n, _) = insert_query_rows(&mut *cx, &plan, &td)?;
    Ok(n)
}

/// Run `plan` in batches and insert every output row into `td` (through `dml_insert::insert_row`, so
/// constraints, indexes and foreign keys are all applied). Returns the number of rows inserted.
fn insert_query_rows(cx: &mut ExecContext, plan: &Plan, td: &TableDesc) -> Result<(u64, Option<Vec<NodeActual>>)> {
    let mut stream = open_plan(plan, &mut *cx)?;
    let dropped = live_mask(td);
    let mut rt = Runtime::default();
    let mut inserted: u64 = 0;
    loop {
        let batch = stream.fetch(&mut *cx, Some(FETCH_BATCH))?;
        if batch.is_empty() {
            break;
        }
        for row in batch {
            if inserted % INTERRUPT_EVERY == 0 {
                cx.check_interrupts()?;
            }
            let full = expand_row(&dropped, &td.name, row)?;
            let mut ex = Executor::new(&mut rt, &mut *cx);
            dml_insert::insert_row(&mut ex, td, full)?;
            inserted += 1;
        }
    }
    Ok((inserted, stream.actuals()))
}

/// `dropped[i]` for every stored column slot of `td` (attnum order).
fn live_mask(td: &TableDesc) -> Vec<bool> {
    td.cols.iter().map(|c| c.dropped).collect()
}

/// Map a query output row (one value per live column, in order) onto the stored image of
/// `dropped.len()` slots: dropped slots are NULL. A width mismatch is an internal error.
fn expand_row(dropped: &[bool], table: &str, row: Vec<Datum>) -> Result<Vec<Datum>> {
    let live = dropped.iter().filter(|d| !**d).count();
    if live != row.len() {
        return Err(PgError::internal(format!(
            "query produced {} columns for relation \"{table}\" which has {live} live columns",
            row.len()
        )));
    }
    if live == dropped.len() {
        return Ok(row);
    }
    let mut out: Vec<Datum> = Vec::with_capacity(dropped.len());
    let mut values = row.into_iter();
    for is_dropped in dropped {
        if *is_dropped {
            out.push(Datum::Null);
        } else {
            out.push(values.next().unwrap_or(Datum::Null));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i32) -> Datum {
        Datum::Int4(v)
    }

    #[test]
    fn expand_row_without_dropped_slots_is_identity() {
        let out = expand_row(&[false, false], "t", vec![int(1), int(2)]).unwrap();
        assert_eq!(out, vec![int(1), int(2)]);
    }

    #[test]
    fn expand_row_fills_dropped_slots_with_null() {
        let out = expand_row(&[false, true, false], "t", vec![int(1), int(2)]).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], int(1));
        assert!(out[1].is_null());
        assert_eq!(out[2], int(2));
    }

    #[test]
    fn expand_row_width_mismatch_is_internal_error() {
        let err = expand_row(&[false, false], "t", vec![int(1)]).unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX000");
    }

    /// Bind, plan (when the statement has one) and execute one SQL statement on `cx`.
    fn run_sql(cx: &mut ExecContext, sql: &str) -> Result<ExecResult> {
        let raw = pgarbor_sql::parse_one(sql)?;
        let bound = {
            let bctx = BindContext {
                env: &cx.se.env,
                session: BindSession { database: "postgres".to_string() },
                io_config: cx.se.io.clone(),
                statement_timestamp: cx.se.txn_ts,
                declared_params: Vec::new(),
            };
            pgarbor_sql::bind(&raw, &bctx)?
        };
        let plan = {
            let pctx = PlanContext {
                env: &cx.se.env,
                param_types: bound.param_types.clone(),
                param_values: None,
                options: PlanOptions::default(),
                table_rows: HashMap::new(),
            };
            // Utility statements have no plan; `execute` takes `None` for them.
            pgarbor_plan::plan(&bound.statement, &pctx).ok()
        };
        crate::execute::execute(&bound.statement, plan.as_ref(), cx)
    }

    /// Run `f` over a fresh bootstrapped catalog in an in-memory transaction.
    fn with_session<T>(f: impl FnOnce(&mut ExecContext) -> Result<T>) -> Result<T> {
        let guc = crate::context::NoGuc;
        let services = crate::context::NoServices;
        let rng = std::cell::Cell::new(12345u64);
        let (kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut txn = crate::memtxn::MemTxn::new();
        txn.kv = kv;
        let se = crate::context::SessionEnv::new(env, pgarbor_types::IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        f(&mut cx)
    }

    #[test]
    #[ignore = "needs EXE-27, EXE-33"]
    fn ctas_creates_table_and_reports_row_count() -> Result<()> {
        with_session(|cx| {
            let tag = run_sql(cx, "CREATE TABLE t_ctas AS SELECT 1 AS a UNION ALL SELECT 2")?.tag().to_string();
            assert_eq!(tag, "SELECT 2");
            assert_eq!(run_sql(cx, "SELECT a FROM t_ctas")?.row_count(), 2);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-27, EXE-33"]
    fn ctas_with_no_data_creates_empty_table() -> Result<()> {
        with_session(|cx| {
            let tag = run_sql(cx, "CREATE TABLE t_nodata AS SELECT 1 AS a WITH NO DATA")?.tag().to_string();
            assert_eq!(tag, "SELECT 0");
            assert_eq!(run_sql(cx, "SELECT a FROM t_nodata")?.row_count(), 0);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-27, EXE-33"]
    fn ctas_if_not_exists_skip_does_not_run_query() -> Result<()> {
        with_session(|cx| {
            run_sql(cx, "CREATE TABLE t_skip AS SELECT 1 AS a")?;
            let tag = run_sql(cx, "CREATE TABLE IF NOT EXISTS t_skip AS SELECT 1 AS a UNION ALL SELECT 2")?
                .tag()
                .to_string();
            assert_eq!(tag, "CREATE TABLE AS");
            assert_eq!(run_sql(cx, "SELECT a FROM t_skip")?.row_count(), 1);
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs EXE-02, EXE-27, EXE-33"]
    fn refresh_matview_replaces_rows() -> Result<()> {
        with_session(|cx| {
            run_sql(cx, "CREATE TABLE t_base AS SELECT 1 AS a")?;
            run_sql(cx, "CREATE MATERIALIZED VIEW mv_t AS SELECT a FROM t_base")?;
            run_sql(cx, "INSERT INTO t_base VALUES (2)")?;
            assert_eq!(run_sql(cx, "SELECT a FROM mv_t")?.row_count(), 1);
            let tag = run_sql(cx, "REFRESH MATERIALIZED VIEW mv_t")?.tag().to_string();
            assert_eq!(tag, "REFRESH MATERIALIZED VIEW");
            assert_eq!(run_sql(cx, "SELECT a FROM mv_t")?.row_count(), 2);
            Ok(())
        })
    }
}
