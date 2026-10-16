//! Variables in SQL text (PG `plpgsql_pre/post_column_ref` + `plpgsql_param_ref`) and the evaluation
//! of expressions and statements through [`crate::spi`] with PG's error decoration
//! (`_SPI_error_callback`, `exec_eval_expr`).
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::binder::plhook::{PlConflict, PlVarHook};
use pgarbor_sql::ir::{BoundExpr, BoundStatement, DistinctSpec, QueryBody};
use pgarbor_sql::plpgsql::{PlSql, VarConflict};
use pgarbor_sql::RawStatement;
use pgarbor_types::{oids, Datum, TypeId};

use super::state::{field_of, not_assigned, resolve_parts, ErrSite, Interp, Scope, Var, VarRef};
use crate::context::ExecContext;
use crate::spi::{self, SpiResult};

thread_local! {
    static PARSE_CACHE: RefCell<HashMap<String, Rc<Vec<RawStatement>>>> = RefCell::new(HashMap::new());
}

/// Parse SQL text with a small per-thread cache (parsing is pure).
pub fn parse_cached(text: &str) -> Result<Rc<Vec<RawStatement>>> {
    if let Some(p) = PARSE_CACHE.with(|c| c.borrow().get(text).cloned()) {
        return Ok(p);
    }
    let parsed = Rc::new(spi::parse(text)?);
    PARSE_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(text.to_string(), Rc::clone(&parsed));
    });
    Ok(parsed)
}

/// Binder hook over the interpreter's variables (module docs).
pub struct VarHook {
    vars: Rc<RefCell<Vec<Var>>>,
    scopes: Rc<RefCell<Vec<Scope>>>,
    /// Number of `$n` parameters declared before the variable slots.
    base: u32,
    /// Values of the variable references made so far (slot `base + i + 1`).
    pub extra: RefCell<Vec<(TypeId, Datum)>>,
    conflict: PlConflict,
}

impl PlVarHook for VarHook {
    fn resolve(&self, qualifier: &[String], name: &str, _pos: Option<u32>) -> Result<Option<BoundExpr>> {
        let mut parts: Vec<String> = qualifier.to_vec();
        parts.push(name.to_string());
        let r = {
            let scopes = self.scopes.borrow();
            let vars = self.vars.borrow();
            resolve_parts(&scopes, &vars, &parts)
        };
        let Some(r) = r else { return Ok(None) };
        let (value, ty) = {
            let vars = self.vars.borrow();
            match &r {
                VarRef::Var(i) => {
                    let v = &vars[*i];
                    if v.is_record() && v.rec_fields.is_none() && v.value.is_null() {
                        return Err(not_assigned(&v.name));
                    }
                    (v.value.clone(), v.ty)
                }
                VarRef::Field(i, f) => field_of(&vars[*i], f)?,
            }
        };
        let mut extra = self.extra.borrow_mut();
        extra.push((ty, value));
        Ok(Some(BoundExpr::param(self.base + extra.len() as u32, ty)))
    }
    fn conflict(&self) -> PlConflict {
        self.conflict
    }
}

/// How an expression's errors are labeled.
#[derive(Clone, Copy)]
pub enum ExprKind<'a> {
    /// `PL/pgSQL expression "<text>"`.
    Expr,
    /// `PL/pgSQL assignment "<full>"`, with the expression starting `prefix_chars` into `full`.
    Assign { full: &'a PlSql },
}

/// The bound statement is a PG "simple expression" (no FROM, no clauses, no sub-queries): errors at
/// run time get no SPI context line.
fn is_simple(stmt: &BoundStatement) -> bool {
    let BoundStatement::Query(q) = stmt else { return false };
    if !q.ctes.is_empty() || !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() || !q.locking.is_empty() {
        return false;
    }
    let QueryBody::Select(s) = &q.body else { return false };
    s.from.is_empty()
        && s.where_.is_none()
        && s.having.is_none()
        && matches!(s.distinct, DistinctSpec::None)
        && !s.has_aggs
        && !s.has_windows
        && !s.has_target_srfs
        && !s.has_sublinks
        && s.group_by == Default::default()
}

impl Interp {
    pub fn make_hook(&self) -> Rc<VarHook> {
        let conflict = match self.conflict() {
            VarConflict::Error => PlConflict::Error,
            VarConflict::UseVariable => PlConflict::UseVariable,
            VarConflict::UseColumn => PlConflict::UseColumn,
        };
        Rc::new(VarHook {
            vars: Rc::clone(&self.vars),
            scopes: Rc::clone(&self.scopes),
            base: self.param_slots.len() as u32,
            extra: RefCell::new(Vec::new()),
            conflict,
        })
    }

    /// Types and values of `$1..$n` (the input parameters).
    pub fn param_values(&self) -> (Vec<TypeId>, Vec<Datum>) {
        let vars = self.vars.borrow();
        let mut t = Vec::with_capacity(self.param_slots.len());
        let mut v = Vec::with_capacity(self.param_slots.len());
        for &i in &self.param_slots {
            t.push(vars[i].ty);
            v.push(vars[i].value.clone());
        }
        (t, v)
    }

    /// Bind (with variables), plan and execute one statement; `decorate` labels errors.
    /// Returns the result and whether the statement was a simple expression.
    fn run_bound_text(
        &mut self,
        cx: &mut ExecContext,
        text: &str,
        decorate: &dyn Fn(PgError, bool) -> PgError,
        check: &dyn Fn(&BoundStatement) -> Result<()>,
    ) -> Result<(SpiResult, bool)> {
        let raws = parse_cached(text).map_err(|e| decorate(e, true))?;
        if raws.len() != 1 {
            let e = PgError::new(SqlState::SYNTAX_ERROR, "cannot insert multiple commands into a prepared statement");
            return Err(decorate(e, true));
        }
        let hook = self.make_hook();
        let (ptypes, pvalues) = self.param_values();
        let dynhook: Rc<dyn PlVarHook> = hook.clone();
        let bound = spi::bind(cx, &raws[0], &ptypes, Some(dynhook)).map_err(|e| decorate(e, true))?;
        check(&bound.statement).map_err(|e| decorate(e, true))?;
        let simple = is_simple(&bound.statement);
        let mut types = spi::merged_types(&ptypes, &bound.param_types);
        types.truncate(ptypes.len().max(bound.param_types.len()));
        while types.len() < ptypes.len() {
            types.push(TypeId::UNKNOWN);
        }
        let mut values = pvalues;
        values.resize(types.len(), Datum::Null);
        for (t, v) in hook.extra.borrow().iter() {
            types.push(*t);
            values.push(v.clone());
        }
        let plan = spi::plan(cx, &bound.statement, &types).map_err(|e| decorate(e, true))?;
        let r = spi::execute_bound(cx, &bound.statement, plan.as_ref(), &types, &values).map_err(|e| decorate(e, false || !simple))?;
        Ok((r, simple))
    }

    /// Evaluate a PL/pgSQL expression (`SELECT <expr>`): one column, at most one row (NULL when none).
    pub fn eval_expr(&mut self, cx: &mut ExecContext, e: &PlSql, assign: Option<&PlSql>) -> Result<(Datum, TypeId)> {
        let kind = match assign {
            Some(full) => ExprKind::Assign { full },
            None => ExprKind::Expr,
        };
        let text = format!("SELECT {}", e.text);
        let (query_text, prefix_chars): (String, i64) = match kind {
            ExprKind::Expr => (e.text.clone(), 0),
            ExprKind::Assign { full } => {
                let pre = e.offset.saturating_sub(full.offset);
                let chars = full.text.get(..pre.min(full.text.len())).map(|s| s.chars().count()).unwrap_or(0);
                (full.text.clone(), chars as i64)
            }
        };
        let label = match kind {
            ExprKind::Expr => format!("PL/pgSQL expression \"{}\"", query_text),
            ExprKind::Assign { .. } => format!("PL/pgSQL assignment \"{}\"", query_text),
        };
        let qt = query_text.clone();
        let decorate = move |mut err: PgError, add: bool| -> PgError {
            if let Some(p) = err.position.take() {
                let ip = p as i64 - 7 + prefix_chars;
                err.internal_position = Some(ip.max(1) as u32);
                err.internal_query = Some(qt.clone());
                return err;
            }
            if add {
                err.with_context_line(label.clone())
            } else {
                err
            }
        };
        let mark = cx.se.notices.borrow().len();
        let res = self.run_bound_text(cx, &text, &decorate, &|stmt| {
            if !matches!(stmt, BoundStatement::Query(_)) {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "query is not a SELECT"));
            }
            Ok(())
        });
        let (r, simple) = match res {
            Ok(x) => x,
            Err(e) => return Err(e),
        };
        if !simple {
            let label2 = match kind {
                ExprKind::Expr => format!("PL/pgSQL expression \"{}\"", query_text),
                ExprKind::Assign { .. } => format!("PL/pgSQL assignment \"{}\"", query_text),
            };
            annotate_notices(cx, mark, &label2);
        }
        if r.columns.len() != 1 {
            let n = r.columns.len();
            return Err(match kind {
                ExprKind::Assign { full } => PgError::new(SqlState::SYNTAX_ERROR, format!("assignment source returned {n} column{}", if n == 1 { "" } else { "s" }))
                    .with_context_line(format!("PL/pgSQL assignment \"{}\"", full.text)),
                ExprKind::Expr => PgError::new(SqlState::SYNTAX_ERROR, format!("query returned {n} column{}", if n == 1 { "" } else { "s" }))
                    .with_context_line(format!("query: {}", query_text)),
            });
        }
        let ty = r.columns[0].type_id;
        match r.rows.len() {
            0 => Ok((Datum::Null, ty)),
            1 => {
                let mut rows = r.rows;
                let v = rows.pop().and_then(|mut row| row.pop()).unwrap_or(Datum::Null);
                Ok((v, ty))
            }
            _ => Err(PgError::new(SqlState::CARDINALITY_VIOLATION, "query returned more than one row")
                .with_context_line(format!("query: {}", query_text))),
        }
    }

    /// Evaluate a boolean condition (NULL = false).
    pub fn eval_cond(&mut self, cx: &mut ExecContext, e: &PlSql) -> Result<bool> {
        let (v, t) = self.eval_expr(cx, e, None)?;
        let v = if t.oid == oids::BOOL { v } else { crate::plfunc::coerce_value(cx, v, t, TypeId::BOOL)? };
        Ok(matches!(v, Datum::Bool(true)))
    }

    /// Run a static SQL statement of the body (variables bound); errors and notices get the
    /// `SQL statement "<text>"` line (or `internal_query` when positioned).
    pub fn run_sql(&mut self, cx: &mut ExecContext, text: &str, check: &dyn Fn(&BoundStatement) -> Result<()>) -> Result<SpiResult> {
        let qt = text.to_string();
        let decorate = move |mut err: PgError, _add: bool| -> PgError {
            if let Some(p) = err.position.take() {
                err.internal_position = Some(p);
                err.internal_query = Some(qt.clone());
                return err;
            }
            err.with_context_line(format!("SQL statement \"{}\"", qt))
        };
        let mark = cx.se.notices.borrow().len();
        let r = self.run_bound_text(cx, text, &decorate, check);
        annotate_notices(cx, mark, &format!("SQL statement \"{}\"", text));
        Ok(r?.0)
    }

    /// Run dynamic SQL (EXECUTE): no variables, `$n` = the USING values.
    pub fn run_dynamic(&mut self, cx: &mut ExecContext, text: &str, using: &[(Datum, TypeId)], check: &dyn Fn(&BoundStatement) -> Result<()>) -> Result<SpiResult> {
        let decorate = |mut err: PgError| -> PgError {
            if let Some(p) = err.position.take() {
                err.internal_position = Some(p);
                err.internal_query = Some(text.to_string());
                return err;
            }
            err.with_context_line(format!("SQL statement \"{}\"", text))
        };
        let mark = cx.se.notices.borrow().len();
        let types: Vec<TypeId> = using.iter().map(|(_, t)| *t).collect();
        let values: Vec<Datum> = using.iter().map(|(v, _)| v.clone()).collect();
        let r = (|| -> Result<SpiResult> {
            let raws = parse_cached(text).map_err(decorate)?;
            if raws.len() > 1 {
                return Err(decorate(PgError::new(SqlState::SYNTAX_ERROR, "cannot insert multiple commands into a prepared statement")));
            }
            let Some(raw) = raws.first() else {
                return Ok(SpiResult::default());
            };
            let bound = spi::bind(cx, raw, &types, None).map_err(decorate)?;
            check(&bound.statement).map_err(decorate)?;
            let ptypes = spi::merged_types(&types, &bound.param_types);
            let mut vals = values.clone();
            vals.resize(ptypes.len().max(vals.len()), Datum::Null);
            let plan = spi::plan(cx, &bound.statement, &ptypes).map_err(decorate)?;
            spi::execute_bound(cx, &bound.statement, plan.as_ref(), &ptypes, &vals).map_err(decorate)
        })();
        annotate_notices(cx, mark, &format!("SQL statement \"{}\"", text));
        r
    }

    /// The CONTEXT line of this frame for the current error site.
    pub fn site_line(&self) -> String {
        match &self.err_site {
            ErrSite::Stmt { line, what } => format!("PL/pgSQL function {} line {} at {}", self.sig, line, what),
            ErrSite::Text(t) => format!("PL/pgSQL function {} {}", self.sig, t),
            ErrSite::None => format!("PL/pgSQL function {}", self.sig),
        }
    }

    /// Attribute notices raised since the last flush to the current site.
    pub fn flush_notices(&mut self, cx: &ExecContext) {
        let line = self.site_line();
        let len = cx.se.notices.borrow().len();
        if self.notice_mark < len {
            annotate_notices(cx, self.notice_mark, &line);
        }
        self.notice_mark = len;
    }
}

/// Append a CONTEXT line to the notices queued since index `from`.
pub fn annotate_notices(cx: &ExecContext, from: usize, line: &str) {
    let mut n = cx.se.notices.borrow_mut();
    let len = n.len();
    for item in n.iter_mut().take(len).skip(from) {
        let old = std::mem::replace(item, PgError::notice(""));
        *item = old.with_context_line(line.to_string());
    }
}
