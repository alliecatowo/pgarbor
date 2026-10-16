//! User-defined functions and procedures (lease EXE-06): calls of `pg_proc` rows with OID >= 16384
//! from expressions, FROM items, `CALL` and `DO`.
//!
//! * [`call_user_function`] looks the function up in the statement's catalog snapshot, applies
//!   strictness (`proisstrict`: a NULL argument returns NULL / no rows without running the body), guards
//!   the recursion depth (54001 like PG's `max_stack_depth` check) and dispatches on `prolang`:
//!   `sql` (14) -> [`call_sql_function`], `plpgsql` (13647) -> `crate::plpgsql`, anything else 0A000.
//! * SQL functions (PG `functions.c`): the body is split into statements; each statement is bound just
//!   before it runs (so earlier DDL is visible), with `$n` and the parameter names (column names win
//!   over parameter names, PG `sql_fn_post_column_ref`) resolving to the arguments. The result is the
//!   first row of the last statement (all rows for SETOF), coerced to the declared result type with
//!   assignment casts (PG `check_sql_fn_retval`); a composite result is built from the row's columns.
//!   Errors carry the CONTEXT line `SQL function "f" statement N` unless the function is a simple
//!   single-expression `SELECT` that PG would inline.
use std::cell::Cell;
use std::rc::Rc;

use pgarbor_catalog::model::{ArgMode, FuncKind, FuncVolatility, Function};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::binder::plhook::{PlConflict, PlVarHook};
use pgarbor_sql::ir::{BoundExpr, BoundStatement, ExprKind, QueryBody};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::{oids, Datum, TypeId};

use crate::context::ExecContext;
use crate::spi;

/// First OID of user objects.
pub const FIRST_USER_OID: u32 = 16384;
/// `pg_language` OIDs.
pub const LANG_INTERNAL: Oid = 12;
pub const LANG_C: Oid = 13;
pub const LANG_SQL: Oid = 14;
pub const LANG_PLPGSQL: Oid = 13647;

/// Maximum nesting of user routine calls before 54001 (PG hits `max_stack_depth` = 2MB at a few
/// hundred PL/pgSQL frames; the exact depth is not observable).
const MAX_CALL_DEPTH: u32 = 160;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Decrements the call depth when a call frame ends.
pub struct DepthGuard;

impl DepthGuard {
    pub fn enter() -> Result<DepthGuard> {
        let d = DEPTH.with(|c| c.get());
        if d >= MAX_CALL_DEPTH {
            return Err(PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
                "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
            ));
        }
        DEPTH.with(|c| c.set(d + 1));
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Result of a user routine call.
#[derive(Clone, Debug)]
pub enum CallOutput {
    Scalar(Datum),
    Set(Vec<Datum>),
}

impl CallOutput {
    pub fn into_scalar(self) -> Datum {
        match self {
            CallOutput::Scalar(d) => d,
            CallOutput::Set(mut v) => {
                if v.is_empty() {
                    Datum::Null
                } else {
                    v.swap_remove(0)
                }
            }
        }
    }
    pub fn into_set(self) -> Vec<Datum> {
        match self {
            CallOutput::Scalar(d) => vec![d],
            CallOutput::Set(v) => v,
        }
    }
}

/// The catalog row of a user function.
pub fn lookup(cx: &ExecContext, oid: Oid) -> Result<Function> {
    cx.se
        .env
        .snapshot()
        .function(oid)
        .cloned()
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for function {oid}")))
}

/// Call user function `oid` (module docs). `arg_types` are the static types of the evaluated
/// arguments (resolved polymorphic types), `result_type` the call's resolved result type.
pub fn call_user_function(
    cx: &mut ExecContext,
    oid: Oid,
    args: Vec<Datum>,
    arg_types: Vec<TypeId>,
    result_type: TypeId,
    want_set: bool,
) -> Result<CallOutput> {
    let f = lookup(cx, oid)?;
    if f.strict && args.iter().any(|a| a.is_null()) {
        return Ok(if want_set || f.retset { CallOutput::Set(Vec::new()) } else { CallOutput::Scalar(Datum::Null) });
    }
    let _guard = DepthGuard::enter()?;
    cx.check_interrupts()?;
    let config = apply_proconfig(cx, &f)?;
    let out = match f.language {
        LANG_SQL => call_sql_function(cx, &f, &args, &arg_types, result_type, want_set),
        LANG_PLPGSQL => crate::plpgsql::call_function(cx, &f, args, arg_types, result_type),
        LANG_INTERNAL | LANG_C => Err(PgError::unimplemented("EXE-06", "internal language functions")),
        _ => Err(PgError::unimplemented("EXE-06", format!("functions in language with OID {}", f.language))),
    };
    restore_proconfig(cx, config)?;
    out
}

/// `proconfig` (`SET x = y` on the function): applied for the duration of the call.
fn apply_proconfig(cx: &mut ExecContext, f: &Function) -> Result<Vec<(String, Option<String>)>> {
    let mut saved = Vec::new();
    for item in &f.config {
        let Some((name, value)) = item.split_once('=') else { continue };
        let old = cx.se.guc.get(name);
        cx.se.guc.set(name, value, true)?;
        spi::refresh_search_path(cx, name)?;
        saved.push((name.to_string(), old));
    }
    Ok(saved)
}

fn restore_proconfig(cx: &mut ExecContext, saved: Vec<(String, Option<String>)>) -> Result<()> {
    for (name, old) in saved.into_iter().rev() {
        if let Some(v) = old {
            cx.se.guc.set(&name, &v, true)?;
            spi::refresh_search_path(cx, &name)?;
        }
    }
    Ok(())
}

/// Names visible in a SQL function body: `$n`, the parameter names and `fname.param`.
struct SqlFnHook {
    fname: String,
    names: Vec<String>,
    types: Vec<TypeId>,
}

impl PlVarHook for SqlFnHook {
    fn resolve(&self, qualifier: &[String], name: &str, _pos: Option<u32>) -> Result<Option<BoundExpr>> {
        let matches_qual = match qualifier {
            [] => true,
            [q] => *q == self.fname,
            _ => false,
        };
        if !matches_qual {
            return Ok(None);
        }
        for (i, n) in self.names.iter().enumerate() {
            if !n.is_empty() && n == name {
                let ty = self.types.get(i).copied().unwrap_or(TypeId::UNKNOWN);
                return Ok(Some(BoundExpr::param(i as u32 + 1, ty)));
            }
        }
        Ok(None)
    }
    fn conflict(&self) -> PlConflict {
        PlConflict::UseColumn
    }
}

/// Input parameters (IN, INOUT, VARIADIC) of `f` in order with their names.
pub fn in_params(f: &Function) -> Vec<&pgarbor_catalog::model::FuncArg> {
    f.args.iter().filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic)).collect()
}

/// Output parameters (OUT, INOUT, TABLE).
pub fn out_params(f: &Function) -> Vec<&pgarbor_catalog::model::FuncArg> {
    f.args.iter().filter(|a| matches!(a.mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table)).collect()
}

/// Resolve a declared (possibly polymorphic) type from the actual input argument types.
pub fn resolve_declared_type(cx: &ExecContext, f: &Function, declared: Oid, actual_in: &[TypeId]) -> TypeId {
    if !pgarbor_types::registry::is_polymorphic(declared) {
        return TypeId::of(declared);
    }
    let decl_in: Vec<Oid> = f.in_arg_types();
    let actual: Vec<Oid> = actual_in.iter().map(|t| t.oid).collect();
    if decl_in.len() == actual.len() {
        if let Ok((_, t)) =
            pgarbor_types::resolve::polymorphic::enforce_generic_type_consistency(&cx.se.env, &actual, &decl_in, declared)
        {
            return TypeId::of(t);
        }
    }
    TypeId::of(declared)
}

/// The fields a composite result must have: named composite type fields, or the OUT parameters
/// (resolved). `None` for a scalar result.
pub fn result_fields(cx: &ExecContext, f: &Function, result_type: TypeId, arg_types: &[TypeId]) -> Option<Vec<(String, TypeId)>> {
    let outs = out_params(f);
    if outs.len() >= 2 || (result_type.oid == oids::RECORD && !outs.is_empty()) {
        return Some(
            outs.iter()
                .enumerate()
                .map(|(i, a)| {
                    let name = if a.name.is_empty() { format!("column{}", i + 1) } else { a.name.clone() };
                    (name, resolve_declared_type(cx, f, a.type_oid, arg_types))
                })
                .collect(),
        );
    }
    composite_fields(cx, result_type.oid)
}

/// Live (non-dropped) fields of a named composite type; `None` when `oid` is not composite.
pub fn composite_fields(cx: &ExecContext, oid: Oid) -> Option<Vec<(String, TypeId)>> {
    let info = cx.se.env.type_info(oid)?;
    if !info.is_composite() {
        return None;
    }
    let fields = cx.se.env.composite_fields(oid)?;
    Some(fields.iter().filter(|f| !f.dropped).map(|f| (f.name.clone(), f.type_id)).collect())
}

/// Coerce `value` of type `from` to `to` with PL assignment rules (assignment cast, else I/O
/// conversion, then domain constraints).
pub fn coerce_value(cx: &mut ExecContext, value: Datum, from: TypeId, to: TypeId) -> Result<Datum> {
    if value.is_null() {
        return check_domain(cx, value, to);
    }
    if from.oid == to.oid && (to.typmod < 0 || from.typmod == to.typmod) {
        return Ok(value);
    }
    if to.oid == oids::RECORD || to.oid == 0 || to.oid == oids::ANYELEMENT || from.oid == oids::UNKNOWN && to.oid == oids::TEXT {
        if let (Datum::Text(_), true) = (&value, from.oid == oids::UNKNOWN) {
            return Ok(value);
        }
        if to.oid == oids::RECORD {
            return Ok(value);
        }
    }
    let base_to = pgarbor_types::registry::base_type(&cx.se.env, to.oid);
    let target = if base_to != to.oid { TypeId { oid: base_to, typmod: to.typmod } } else { to };
    let cast = crate::eval_call::with_func_ctx(cx, &[from], target, |fc| {
        pgarbor_types::cast::apply::cast_datum(value.clone(), from, target, pgarbor_types::cast::CastContext::Assignment, fc)
    });
    let v = match cast {
        Ok(v) => v,
        Err(e) if e.sqlstate == SqlState::CANNOT_COERCE || e.sqlstate == SqlState::DATATYPE_MISMATCH || e.sqlstate == SqlState::UNDEFINED_FUNCTION => {
            // PL/pgSQL falls back to I/O conversion (COERCION_PLPGSQL).
            let io = cx.se.io_ctx();
            let text = pgarbor_types::io::output(&io, from, &value)?;
            pgarbor_types::io::input(&io, target, &text)?
        }
        Err(e) => return Err(e),
    };
    check_domain(cx, v, to)
}

/// Apply the constraints of domain `to` (no-op for other types).
pub fn check_domain(cx: &mut ExecContext, value: Datum, to: TypeId) -> Result<Datum> {
    let is_domain = cx.se.env.type_info(to.oid).is_some_and(|t| t.is_domain());
    if !is_domain {
        return Ok(value);
    }
    let base = pgarbor_types::registry::base_type(&cx.se.env, to.oid);
    let e = BoundExpr {
        kind: ExprKind::CoerceToDomain(Box::new(pgarbor_sql::ir::CoerceToDomain {
            arg: BoundExpr::constant(value, TypeId::of(base)),
            domain: to.oid,
            form: pgarbor_sql::ir::CoercionForm::Implicit,
        })),
        ty: to,
    };
    crate::eval::eval_standalone(cx, &e, &[])
}

/// Format a type for messages (`integer`, `character varying`).
pub fn type_name(cx: &ExecContext, ty: TypeId) -> String {
    pgarbor_types::names::display_name(&cx.se.env, ty.oid)
}

/// `name(argtypes)` as PG `format_procedure` prints it (schema-qualified when not visible).
pub fn signature(cx: &ExecContext, f: &Function) -> String {
    cx.se
        .env
        .reg_name(pgarbor_types::context::RegKind::Procedure, f.oid)
        .unwrap_or_else(|| format!("{}()", f.name))
}

/// True when the body is a single expression-only SELECT that PG inlines (no CONTEXT line).
fn inlinable(f: &Function, stmts: &[pgarbor_sql::RawStatement]) -> bool {
    if f.retset || f.security_definer || !f.config.is_empty() || stmts.len() != 1 {
        return false;
    }
    let text = stmts[0].text.trim_start().to_ascii_lowercase();
    if !text.starts_with("select") {
        return false;
    }
    !text.contains(" from ") && !text.contains(" where ") && !text.contains("(select")
}

/// Run a SQL-language function (module docs).
pub fn call_sql_function(
    cx: &mut ExecContext,
    f: &Function,
    args: &[Datum],
    arg_types: &[TypeId],
    result_type: TypeId,
    want_set: bool,
) -> Result<CallOutput> {
    if f.sql_body.is_some() && f.body.trim().is_empty() {
        return Err(PgError::unimplemented("EXE-06", "SQL-standard function bodies (BEGIN ATOMIC / RETURN)"));
    }
    let stmts = spi::parse(&f.body).map_err(|e| sql_fn_context(e, f, 0, None))?;
    let inline = inlinable(f, &stmts);
    let names: Vec<String> = in_params(f).iter().map(|a| a.name.clone()).collect();
    let hook: Rc<dyn PlVarHook> = Rc::new(SqlFnHook { fname: f.name.clone(), names, types: arg_types.to_vec() });
    let n = stmts.len();
    let mut last: Option<spi::SpiResult> = None;
    for (i, raw) in stmts.iter().enumerate() {
        let step = (|| -> Result<spi::SpiResult> {
            let bound = spi::bind(cx, raw, arg_types, Some(Rc::clone(&hook)))?;
            check_sql_statement(f, &bound.statement)?;
            let ptypes = spi::merged_types(arg_types, &bound.param_types);
            let plan = spi::plan(cx, &bound.statement, &ptypes)?;
            spi::execute_bound(cx, &bound.statement, plan.as_ref(), &ptypes, args)
        })();
        match step {
            Ok(r) => {
                if i + 1 == n {
                    last = Some(r);
                }
            }
            Err(e) => {
                return Err(if inline { e } else { sql_fn_context(e, f, i + 1, Some(&raw.text)) });
            }
        }
    }
    if result_type.oid == oids::VOID || (f.kind == FuncKind::Procedure && result_type.oid != oids::RECORD) {
        return Ok(if want_set || f.retset { CallOutput::Set(Vec::new()) } else { CallOutput::Scalar(Datum::Null) });
    }
    let last = last.ok_or_else(|| {
        PgError::new(SqlState(*b"42P13"), format!("return type mismatch in function declared to return {}", type_name(cx, result_type)))
            .with_detail("Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.")
    })?;
    if !last.returns_rows {
        return Err(PgError::new(
            SqlState(*b"42P13"),
            format!("return type mismatch in function declared to return {}", type_name(cx, result_type)),
        )
        .with_detail("Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING."));
    }
    let fields = result_fields(cx, f, result_type, arg_types);
    let col_types: Vec<TypeId> = last.columns.iter().map(|c| c.type_id).collect();
    let mut out = Vec::new();
    let take = if f.retset || want_set { last.rows.len() } else { last.rows.len().min(1) };
    for row in last.rows.into_iter().take(take) {
        out.push(shape_result_row(cx, f, row, &col_types, result_type, fields.as_deref(), inline)?);
    }
    if f.retset || want_set {
        Ok(CallOutput::Set(out))
    } else {
        Ok(CallOutput::Scalar(out.into_iter().next().unwrap_or(Datum::Null)))
    }
}

/// CALL of a SQL procedure: `result_type` is `record` when it has OUT / INOUT parameters (their
/// values come from the final statement's first row), else `void`.
pub fn call_sql_function_for_call(cx: &mut ExecContext, f: &Function, args: &[Datum], arg_types: &[TypeId], result_type: TypeId) -> Result<Datum> {
    Ok(call_sql_function(cx, f, args, arg_types, result_type, false)?.into_scalar())
}

/// Turn one row of the final statement into the function's result value (PG `check_sql_fn_retval`).
fn shape_result_row(
    cx: &mut ExecContext,
    f: &Function,
    mut row: Vec<Datum>,
    col_types: &[TypeId],
    result_type: TypeId,
    fields: Option<&[(String, TypeId)]>,
    inline: bool,
) -> Result<Datum> {
    let mismatch = |cx: &ExecContext, actual: TypeId| {
        let e = PgError::new(
            SqlState(*b"42P13"),
            format!("return type mismatch in function declared to return {}", type_name(cx, result_type)),
        )
        .with_detail(format!("Actual return type is {}.", type_name(cx, actual)));
        if inline {
            e
        } else {
            e.with_context_line(format!("SQL function \"{}\" during startup", f.name))
        }
    };
    match fields {
        None => {
            let (v, t) = match (row.len(), col_types.first()) {
                (1, Some(t)) => (row.pop().unwrap_or(Datum::Null), *t),
                (_, Some(t)) => return Err(mismatch(cx, *t)),
                (_, None) => return Ok(Datum::Null),
            };
            if result_type.oid == oids::RECORD {
                return Ok(v);
            }
            if t.oid == result_type.oid {
                return Ok(v);
            }
            match coerce_value(cx, v, t, result_type) {
                Ok(v) => Ok(v),
                Err(e) if e.sqlstate == SqlState::INVALID_TEXT_REPRESENTATION => Err(mismatch(cx, t)),
                Err(e) => Err(e),
            }
        }
        Some(fields) => {
            // A single composite column of the right shape is the result itself.
            if row.len() == 1 && fields.len() != 1 {
                let t = col_types.first().copied().unwrap_or(TypeId::UNKNOWN);
                if t.oid == result_type.oid || t.oid == oids::RECORD || matches!(row[0], Datum::Record(_) | Datum::Null) {
                    return Ok(row.pop().unwrap_or(Datum::Null));
                }
            }
            if row.len() != fields.len() {
                let t = col_types.first().copied().unwrap_or(TypeId::UNKNOWN);
                let e = PgError::new(
                    SqlState(*b"42P13"),
                    format!("return type mismatch in function declared to return {}", type_name(cx, result_type)),
                )
                .with_detail(if row.len() < fields.len() {
                    "Final statement returns too few columns.".to_string()
                } else {
                    "Final statement returns too many columns.".to_string()
                });
                let _ = t;
                return Err(e);
            }
            let mut vals = Vec::with_capacity(fields.len());
            for ((v, (_, ft)), ct) in row.into_iter().zip(fields.iter()).zip(col_types.iter()) {
                vals.push(coerce_value(cx, v, *ct, *ft)?);
            }
            Ok(Datum::Record(vals))
        }
    }
}

/// Statements a SQL function may not contain (PG `init_execution_state` / `check_sql_fn_statements`).
fn check_sql_statement(f: &Function, stmt: &BoundStatement) -> Result<()> {
    if let BoundStatement::Transaction(_) = stmt {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{} is not allowed in an SQL function", stmt.tag()),
        ));
    }
    if f.volatility != FuncVolatility::Volatile {
        let modifies = matches!(stmt, BoundStatement::Insert(_) | BoundStatement::Update(_) | BoundStatement::Delete(_));
        let query_modifies = matches!(stmt, BoundStatement::Query(q) if matches!(q.body, QueryBody::Select(_)) && !q.ctes.is_empty() && q.ctes.iter().any(|c| matches!(c.body, pgarbor_sql::ir::CteBody::Modify(_))));
        if modifies || query_modifies {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("{} is not allowed in a non-volatile function", stmt.tag()),
            ));
        }
    }
    Ok(())
}

/// Add the `SQL function "f" statement N` CONTEXT line; a positioned error inside the body moves its
/// position to `internal_position`/`internal_query`.
fn sql_fn_context(mut e: PgError, f: &Function, stmt_no: usize, text: Option<&str>) -> PgError {
    if let Some(p) = e.position.take() {
        e.internal_position = Some(p);
        e.internal_query = Some(f.body.clone());
        let _ = text;
    }
    let line = if stmt_no == 0 { format!("SQL function \"{}\" during startup", f.name) } else { format!("SQL function \"{}\" statement {}", f.name, stmt_no) };
    e.with_context_line(line)
}
