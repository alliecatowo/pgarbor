//! Statement execution from inside the executor (PG `spi.c`): procedural code (SQL functions,
//! PL/pgSQL) runs SQL text through parse -> bind -> plan -> execute over the CURRENT [`ExecContext`]
//! (same transaction, same catalog environment, notices queued on the same session env).
//!
//! * Parameters: the nested statement gets its own `$n` values (`se.params` / `se.param_types` are
//!   swapped in for the duration of the statement and restored afterwards, also on error).
//! * Deferred foreign-key checks of an enclosing DML statement (`stmt.pending_fk`) are set aside while
//!   the nested statement runs, so the nested statement only checks its own rows.
//! * Plans are generic (`param_values: None`): constants are folded at plan time, parameter values are
//!   only seen at run time, which keeps PG's split between planning and execution errors.
//! * Session-level statements: `SET`/`RESET` go through the GUC table, `SHOW` returns one row;
//!   transaction control is rejected (callers decide the error text first); others are 0A000.
use std::rc::Rc;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::ColumnDesc;
use pgarbor_plan::{PlanContext, PlanOptions};
use pgarbor_sql::binder::plhook::{bind_with_hook, PlVarHook};
use pgarbor_sql::ir::{BoundStatement, CopyTarget, SetValue};
use pgarbor_sql::{BindContext, BindOutput, BindSession, RawStatement};
use pgarbor_types::{Datum, TypeId};

use crate::context::ExecContext;
use crate::execute::execute;
use crate::result::ExecResult;

/// Rows (or the command outcome) of one nested statement.
#[derive(Clone, Debug, Default)]
pub struct SpiResult {
    pub columns: Vec<ColumnDesc>,
    pub rows: Vec<Vec<Datum>>,
    pub tag: String,
    /// Rows processed (`SPI_processed`): rows returned, or rows affected by DML.
    pub processed: u64,
    /// The statement returns rows (query or DML with RETURNING).
    pub returns_rows: bool,
}

/// Parse SQL text into statements (42601 errors carry positions into `text`).
pub fn parse(text: &str) -> Result<Vec<RawStatement>> {
    pgarbor_sql::parse(text)
}

/// Bind one statement against the current environment. `declared` = types of `$1..$n`.
pub fn bind(cx: &ExecContext, raw: &RawStatement, declared: &[TypeId], hook: Option<Rc<dyn PlVarHook>>) -> Result<BindOutput> {
    let ctx = BindContext {
        env: &cx.se.env,
        session: BindSession { database: cx.se.services.database_name() },
        io_config: cx.se.io.clone(),
        statement_timestamp: cx.se.txn_ts,
        declared_params: declared.to_vec(),
    };
    match hook {
        Some(h) => bind_with_hook(raw, &ctx, h),
        None => pgarbor_sql::bind(raw, &ctx),
    }
}

/// Planner options from the session GUCs (`enable_*`).
pub fn plan_options(cx: &ExecContext) -> PlanOptions {
    let g = cx.se.guc;
    let on = |name: &str| g.get(name).map(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1")).unwrap_or(true);
    PlanOptions {
        enable_seqscan: on("enable_seqscan"),
        enable_indexscan: on("enable_indexscan"),
        enable_hashjoin: on("enable_hashjoin"),
        enable_mergejoin: on("enable_mergejoin"),
        enable_nestloop: on("enable_nestloop"),
        enable_hashagg: on("enable_hashagg"),
        enable_sort: on("enable_sort"),
        enable_material: on("enable_material"),
        ..PlanOptions::default()
    }
}

/// Plan `stmt` when it reads data (plannable statements and `COPY (query) TO`).
pub fn plan(cx: &ExecContext, stmt: &BoundStatement, param_types: &[TypeId]) -> Result<Option<pgarbor_plan::Plan>> {
    let copy_query: Option<BoundStatement> = match stmt {
        BoundStatement::Copy(copy) => match &copy.target {
            CopyTarget::Query(q) => Some(BoundStatement::Query(q.clone())),
            CopyTarget::Table { .. } => None,
        },
        _ => None,
    };
    let to_plan: &BoundStatement = match &copy_query {
        Some(q) => q,
        None if stmt.is_plannable() => stmt,
        None => return Ok(None),
    };
    let env = cx.se.env.clone();
    let ctx = PlanContext {
        env: &env,
        param_types: param_types.to_vec(),
        param_values: None,
        options: plan_options(cx),
        table_rows: Default::default(),
    };
    Ok(Some(pgarbor_plan::plan(to_plan, &ctx)?))
}

/// Execute a bound (and, when plannable, planned) statement with `$n` = `values` (module docs).
pub fn execute_bound(
    cx: &mut ExecContext,
    stmt: &BoundStatement,
    plan: Option<&pgarbor_plan::Plan>,
    types: &[TypeId],
    values: &[Datum],
) -> Result<SpiResult> {
    if let Some(r) = session_statement(cx, stmt)? {
        return Ok(r);
    }
    let saved_params = std::mem::replace(&mut cx.se.params, values.to_vec());
    let saved_types = std::mem::replace(&mut cx.se.param_types, types.to_vec());
    let saved_fk = std::mem::take(&mut cx.stmt.pending_fk);
    let out = execute(stmt, plan, cx);
    cx.se.params = saved_params;
    cx.se.param_types = saved_types;
    let mut nested_fk = std::mem::replace(&mut cx.stmt.pending_fk, saved_fk);
    if !nested_fk.is_empty() {
        // Checks the nested statement left behind (it failed before running them): drop them.
        nested_fk.clear();
    }
    Ok(match out? {
        ExecResult::Empty => SpiResult::default(),
        ExecResult::Command { tag } => {
            let processed = affected(&tag);
            SpiResult { tag, processed, ..SpiResult::default() }
        }
        ExecResult::Rows { desc, rows, tag } => {
            let processed = if tag.starts_with("SELECT") || tag.is_empty() { rows.len() as u64 } else { affected(&tag) };
            SpiResult { columns: desc, rows, tag, processed, returns_rows: true }
        }
    })
}

/// The trailing row count of a command tag (`INSERT 0 3` -> 3), 0 when there is none.
pub fn affected(tag: &str) -> u64 {
    tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Parse, bind, plan and execute one statement text with explicit `$n` parameters (no variables).
pub fn run_text(cx: &mut ExecContext, text: &str, types: &[TypeId], values: &[Datum]) -> Result<Vec<SpiResult>> {
    let stmts = parse(text)?;
    let mut out = Vec::with_capacity(stmts.len());
    for raw in &stmts {
        let bound = bind(cx, raw, types, None)?;
        let ptypes = merged_types(types, &bound.param_types);
        let plan = plan(cx, &bound.statement, &ptypes)?;
        out.push(execute_bound(cx, &bound.statement, plan.as_ref(), &ptypes, values)?);
    }
    Ok(out)
}

/// Declared types extended by the binder's final parameter types.
pub fn merged_types(declared: &[TypeId], bound: &[TypeId]) -> Vec<TypeId> {
    let mut out: Vec<TypeId> = bound.to_vec();
    for (i, t) in declared.iter().enumerate() {
        if i < out.len() {
            if out[i].oid == 0 {
                out[i] = *t;
            }
        } else {
            out.push(*t);
        }
    }
    out
}

/// Transaction-control statements are not executed here; `None` for anything else.
pub fn is_transaction_control(stmt: &BoundStatement) -> bool {
    matches!(stmt, BoundStatement::Transaction(_))
}

/// SET / RESET / SHOW run against the GUC table; other session statements are 0A000.
fn session_statement(cx: &mut ExecContext, stmt: &BoundStatement) -> Result<Option<SpiResult>> {
    use BoundStatement as S;
    match stmt {
        S::Set(s) => {
            let local = matches!(s.scope, pgarbor_sql::ir::SetScope::Local);
            match &s.value {
                SetValue::Values(v) => {
                    cx.se.guc.set(&s.name, &v.join(", "), local)?;
                }
                SetValue::Default => {
                    return Err(PgError::unimplemented("EXE-06", "SET ... TO DEFAULT inside a function"));
                }
                SetValue::FromCurrent => {}
            }
            refresh_search_path(cx, &s.name)?;
            Ok(Some(SpiResult { tag: "SET".into(), ..SpiResult::default() }))
        }
        S::Show(Some(name)) => {
            let value = cx.se.guc.get(name).ok_or_else(|| {
                PgError::new(SqlState::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\""))
            })?;
            Ok(Some(SpiResult {
                columns: vec![ColumnDesc { name: name.clone(), type_id: TypeId::TEXT, table_oid: 0, column_attnum: 0 }],
                rows: vec![vec![Datum::Text(value)]],
                tag: "SHOW".into(),
                processed: 1,
                returns_rows: true,
            }))
        }
        S::Transaction(_) => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "unsupported transaction command in PL/pgSQL")),
        S::Reset(_)
        | S::Show(None)
        | S::SetRole { .. }
        | S::SetSessionAuthorization { .. }
        | S::Prepare(_)
        | S::Execute(_)
        | S::Deallocate(_)
        | S::Discard(_)
        | S::Listen(_)
        | S::Unlisten(_) => Err(PgError::unimplemented("EXE-06", format!("{} inside a function", stmt.tag()))),
        S::Notify { channel, payload } => {
            cx.se.services.notify(channel, payload.as_deref().unwrap_or(""))?;
            Ok(Some(SpiResult { tag: "NOTIFY".into(), ..SpiResult::default() }))
        }
        _ => Ok(None),
    }
}

/// After `SET search_path` the environment's search path must follow (later statements of the
/// same routine resolve names with it).
pub fn refresh_search_path(cx: &mut ExecContext, name: &str) -> Result<()> {
    if !name.eq_ignore_ascii_case("search_path") {
        return Ok(());
    }
    let Some(value) = cx.se.guc.get("search_path") else { return Ok(()) };
    let path = pgarbor_catalog::search_path::SearchPath::parse(&value)?;
    let env = pgarbor_catalog::CatalogEnv::new(cx.se.env.snapshot().clone(), &path, cx.se.env.current_user())?
        .with_session(cx.se.env.session_user(), cx.se.env.database());
    cx.se.env = env;
    cx.stmt.tables.borrow_mut().clear();
    Ok(())
}
