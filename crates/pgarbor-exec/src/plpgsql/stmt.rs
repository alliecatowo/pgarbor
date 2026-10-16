//! Statement execution (PG `exec_stmt_*`).
use std::rc::Rc;

use pgarbor_catalog::model::Function;
use pgarbor_common::{PgError, Result, Severity, SqlState};
use pgarbor_sql::ir::BoundStatement;
use pgarbor_sql::plpgsql::{
    DiagItem, PlBlock, PlCondition, PlDecl, PlQuery, PlSql, PlStmt, PlStmtKind, PlTarget, PlType, RaiseLevel,
    RaiseOption, RaiseWhat,
};
use pgarbor_types::{oids, Datum, TypeId};

use super::state::{CursorDecl, ErrSite, Flow, Interp, VarRef};
use crate::context::ExecContext;
use crate::plfunc::{self, CallOutput};
use crate::spi::SpiResult;

fn sqlstate(code: &str) -> SqlState {
    SqlState::from_code(code).unwrap_or(SqlState::RAISE_EXCEPTION)
}

/// Does handler condition `c` catch `e`? (PG `exception_matches_conditions`: OTHERS catches everything
/// but QUERY_CANCELED and ASSERT_FAILURE; a category code (`XX000`) matches its class.)
fn condition_matches(c: &PlCondition, e: &PgError) -> bool {
    let code = e.sqlstate.code();
    match c {
        PlCondition::Others => code != "57014" && code != "P0004",
        PlCondition::SqlState(s) => {
            if s == code {
                return true;
            }
            s.ends_with("000") && s[..2] == code[..2]
        }
    }
}

impl Interp {
    /// Run the outermost block of a call; errors leaving the frame get this frame's CONTEXT line.
    pub fn run_block_top(&mut self, cx: &mut ExecContext, block: &PlBlock) -> Result<()> {
        self.notice_mark = cx.se.notices.borrow().len();
        let r = self.exec_block(cx, block);
        self.flush_notices(cx);
        match r {
            Ok(_) => Ok(()),
            Err(e) => Err(e.with_context_line(self.site_line())),
        }
    }

    pub fn exec_block(&mut self, cx: &mut ExecContext, block: &PlBlock) -> Result<Flow> {
        self.push_scope(block.label.clone());
        let r = self.exec_block_inner(cx, block);
        self.pop_scope();
        match r {
            Ok(Flow::Exit(Some(l))) if block.label.as_deref() == Some(l.as_str()) => Ok(Flow::Normal),
            other => other,
        }
    }

    fn exec_block_inner(&mut self, cx: &mut ExecContext, block: &PlBlock) -> Result<Flow> {
        // Declarations (defaults evaluated at every block entry).
        let saved_site = self.err_site.clone();
        self.err_site = ErrSite::Stmt { line: block.line, what: "statement block" };
        self.flush_notices(cx);
        for d in &block.decls {
            self.declare(cx, d, block.line)?;
        }
        self.flush_notices(cx);
        let Some(handlers) = &block.handlers else {
            let r = self.exec_stmts(cx, &block.body);
            if r.is_ok() {
                self.err_site = saved_site;
            }
            return r;
        };
        // EXCEPTION clause: run the body in a subtransaction.
        let token = cx.txn.subxact_begin()?;
        let saved_env = cx.se.env.clone();
        let body = self.exec_stmts(cx, &block.body);
        match body {
            Ok(flow) => {
                cx.txn.subxact_release(token)?;
                self.err_site = saved_site;
                Ok(flow)
            }
            Err(err) => {
                self.flush_notices(cx);
                cx.txn.subxact_rollback(token)?;
                cx.se.env = saved_env;
                cx.stmt.tables.borrow_mut().clear();
                cx.stmt.pending_fk.clear();
                let Some(h) = handlers.iter().find(|h| h.conditions.iter().any(|c| condition_matches(c, &err))) else {
                    return Err(err);
                };
                // The error is handled: its context belongs to this frame's error report.
                let mut handled = err;
                if handled.context.is_none() || !handled.context.as_deref().unwrap_or("").contains(&self.sig) {
                    let line = self.site_line();
                    handled = handled.with_context_line(line);
                }
                self.err_site = saved_site;
                self.push_scope(None);
                let sqlstate_v = self.declare_var(cx, "sqlstate", TypeId::TEXT, Datum::Text(handled.sqlstate.code().to_string()))?;
                let sqlerrm_v = self.declare_var(cx, "sqlerrm", TypeId::TEXT, Datum::Text(handled.message.clone()))?;
                let _ = (sqlstate_v, sqlerrm_v);
                self.handling.push(handled);
                let r = self.exec_stmts(cx, &h.body);
                self.handling.pop();
                self.pop_scope();
                r
            }
        }
    }

    /// One declaration (PG `exec_stmt_block` variable initialization).
    fn declare(&mut self, cx: &mut ExecContext, d: &PlDecl, block_line: u32) -> Result<()> {
        match d {
            PlDecl::Var { name, constant, ty, not_null, default, .. } => {
                let tid = self.resolve_type(cx, ty)?;
                let idx = self.declare_var(cx, name, tid, Datum::Null)?;
                {
                    let mut vars = self.vars.borrow_mut();
                    vars[idx].not_null = *not_null;
                }
                if let Some(def) = default {
                    let r = (|| -> Result<()> {
                        let (v, t) = self.eval_expr(cx, def, None)?;
                        // A NOT NULL default that evaluates to NULL.
                        self.assign_var(cx, idx, v, t)
                    })();
                    if let Err(e) = r {
                        self.err_site = ErrSite::Stmt { line: block_line, what: "statement block" };
                        self.err_site = ErrSite::Text(format!("line {block_line} during statement block local variable initialization"));
                        return Err(e);
                    }
                } else if tid.oid != oids::RECORD {
                    // Row variables start as a row of NULLs.
                    let fields = self.vars.borrow()[idx].rec_fields.clone();
                    if fields.is_some() {
                        self.vars.borrow_mut()[idx].value = Datum::Null;
                    }
                }
                self.vars.borrow_mut()[idx].constant = *constant;
                Ok(())
            }
            PlDecl::Alias { name, target, .. } => {
                let r = if target.len() == 1 && target[0].starts_with('$') {
                    let n: usize = target[0][1..].parse().unwrap_or(0);
                    self.param_slots.get(n.wrapping_sub(1)).map(|i| VarRef::Var(*i))
                } else {
                    self.resolve(target)
                };
                match r {
                    Some(VarRef::Var(i)) => {
                        self.add_alias(name, i);
                        Ok(())
                    }
                    _ => Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("variable \"{}\" does not exist", target.join(".")))),
                }
            }
            PlDecl::Cursor { name, scroll, args, query, .. } => {
                let mut cargs = Vec::new();
                for (an, at) in args {
                    cargs.push((an.clone(), self.resolve_type(cx, at)?));
                }
                let idx = self.declare_var(cx, name, TypeId::of(oids::REFCURSOR), Datum::Null)?;
                self.vars.borrow_mut()[idx].cursor = Some(Rc::new(CursorDecl { scroll: *scroll, args: cargs, query: query.clone() }));
                // A bound cursor's portal is named after the variable unless assigned otherwise.
                self.vars.borrow_mut()[idx].value = Datum::Text(name.clone());
                Ok(())
            }
        }
    }

    /// Resolve a declared type (`%TYPE`, `%ROWTYPE`, names).
    pub fn resolve_type(&mut self, cx: &mut ExecContext, ty: &PlType) -> Result<TypeId> {
        match ty {
            PlType::Named(sql) => resolve_type_name(cx, &sql.text),
            PlType::VarType { name, array, .. } => {
                let base = if let Some(VarRef::Var(i)) = self.resolve(name) {
                    self.vars.borrow()[i].ty
                } else if let Some(VarRef::Field(i, f)) = self.resolve(name) {
                    self.field_value(i, &f)?.1
                } else if name.len() >= 2 {
                    // table.column%TYPE
                    let (col, rel) = name.split_last().ok_or_else(|| PgError::internal("empty %TYPE name"))?;
                    let rel_text = rel.join(".");
                    let rel_oid = resolve_relation(cx, &rel_text)?;
                    let fields = plfunc::composite_fields(cx, rel_oid).unwrap_or_default();
                    match fields.iter().find(|(n, _)| n == col) {
                        Some((_, t)) => *t,
                        None => {
                            return Err(PgError::new(
                                SqlState::UNDEFINED_COLUMN,
                                format!("column \"{}\" of relation \"{}\" does not exist", col, rel_text),
                            ))
                        }
                    }
                } else {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, format!("variable \"{}\" does not exist", name.join("."))));
                };
                if *array {
                    let arr = pgarbor_types::registry::array_type_of(&cx.se.env, base.oid).unwrap_or(0);
                    return Ok(TypeId::of(arr));
                }
                Ok(base)
            }
            PlType::RowType { name, .. } => {
                let text = name.join(".");
                let oid = resolve_relation(cx, &text)?;
                Ok(TypeId::of(oid))
            }
        }
    }

    pub fn exec_stmts(&mut self, cx: &mut ExecContext, stmts: &[PlStmt]) -> Result<Flow> {
        for s in stmts {
            let f = self.exec_stmt(cx, s)?;
            if f != Flow::Normal {
                return Ok(f);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, cx: &mut ExecContext, s: &PlStmt) -> Result<Flow> {
        cx.check_interrupts()?;
        let saved = self.err_site.clone();
        self.flush_notices(cx);
        self.err_site = ErrSite::Stmt { line: s.line, what: s.type_name() };
        let r = self.exec_stmt_inner(cx, s);
        self.flush_notices(cx);
        let f = r?;
        self.err_site = saved;
        Ok(f)
    }

    fn exec_stmt_inner(&mut self, cx: &mut ExecContext, s: &PlStmt) -> Result<Flow> {
        match &s.kind {
            PlStmtKind::Block(b) => self.exec_block(cx, b),
            PlStmtKind::Assign { target, expr, full } => {
                let (v, t) = self.eval_expr(cx, expr, Some(full))?;
                self.check_constant(target)?;
                self.assign_target(cx, target, v, t)?;
                Ok(Flow::Normal)
            }
            PlStmtKind::If { arms, else_body } => {
                for (cond, body) in arms {
                    if self.eval_cond(cx, cond)? {
                        self.flush_notices(cx);
                        return self.exec_stmts(cx, body);
                    }
                }
                self.flush_notices(cx);
                match else_body {
                    Some(b) => self.exec_stmts(cx, b),
                    None => Ok(Flow::Normal),
                }
            }
            PlStmtKind::Case { expr, arms, else_body } => self.exec_case(cx, expr.as_ref(), arms, else_body.as_deref()),
            PlStmtKind::Loop { label, body } => {
                loop {
                    match self.exec_stmts(cx, body)? {
                        Flow::Normal => {}
                        f => match self.loop_flow(f, label) {
                            LoopCtl::Break(out) => return Ok(out),
                            LoopCtl::Next => {}
                        },
                    }
                }
            }
            PlStmtKind::While { label, cond, body } => {
                loop {
                    if !self.eval_cond(cx, cond)? {
                        return Ok(Flow::Normal);
                    }
                    self.flush_notices(cx);
                    match self.exec_stmts(cx, body)? {
                        Flow::Normal => {}
                        f => match self.loop_flow(f, label) {
                            LoopCtl::Break(out) => return Ok(out),
                            LoopCtl::Next => {}
                        },
                    }
                }
            }
            PlStmtKind::ForInt { label, var, reverse, lower, upper, step, body } => {
                self.exec_fori(cx, label, var, *reverse, lower, upper, step.as_ref(), body)
            }
            PlStmtKind::ForQuery { label, targets, query, body } => {
                let rows = self.open_query(cx, query)?;
                self.flush_notices(cx);
                self.loop_over_rows(cx, label, targets, rows, body)
            }
            PlStmtKind::ForCursor { label, target, cursor, args, body } => {
                self.exec_for_cursor(cx, label, target, cursor, args.as_ref(), body)
            }
            PlStmtKind::Foreach { label, targets, slice, array, body } => self.exec_foreach(cx, label, targets, *slice, array, body),
            PlStmtKind::Exit { is_continue, label, cond } => {
                if let Some(c) = cond {
                    if !self.eval_cond(cx, c)? {
                        return Ok(Flow::Normal);
                    }
                }
                Ok(if *is_continue { Flow::Continue(label.clone()) } else { Flow::Exit(label.clone()) })
            }
            PlStmtKind::Return { expr } => self.exec_return(cx, expr.as_ref()),
            PlStmtKind::ReturnNext { expr } => self.exec_return_next(cx, expr.as_ref()),
            PlStmtKind::ReturnQuery { query } => {
                if !self.retset {
                    return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot use RETURN QUERY in a non-SETOF function"));
                }
                let rows = self.open_query(cx, query)?;
                self.append_query_rows(cx, rows)?;
                Ok(Flow::Normal)
            }
            PlStmtKind::Raise { level, what, options } => self.exec_raise(cx, *level, what, options),
            PlStmtKind::Assert { cond, message } => {
                let enabled = cx.se.guc.get("plpgsql.check_asserts").map(|v| v != "off" && v != "false").unwrap_or(true);
                if !enabled {
                    return Ok(Flow::Normal);
                }
                let (v, t) = self.eval_expr(cx, cond, None)?;
                let v = if t.oid == oids::BOOL { v } else { plfunc::coerce_value(cx, v, t, TypeId::BOOL)? };
                if !matches!(v, Datum::Bool(true)) {
                    let msg = match message {
                        Some(m) => {
                            let (mv, mt) = self.eval_expr(cx, m, None)?;
                            if mv.is_null() {
                                "assertion failed".to_string()
                            } else {
                                self.to_text(cx, &mv, mt)?
                            }
                        }
                        None => "assertion failed".to_string(),
                    };
                    return Err(PgError::new(sqlstate("P0004"), msg));
                }
                Ok(Flow::Normal)
            }
            PlStmtKind::Perform { query } => {
                let text = format!("SELECT {}", query.text);
                let r = self.run_sql(cx, &text, &|_| Ok(()))?;
                self.row_count = r.processed;
                self.set_found(r.processed > 0);
                Ok(Flow::Normal)
            }
            PlStmtKind::ExecSql { sql, into, first_word } => self.exec_sql(cx, sql, into.as_ref(), first_word),
            PlStmtKind::DynExecute { query, into, using } => self.exec_dynexecute(cx, query, into.as_ref(), using),
            PlStmtKind::GetDiag { stacked, items } => self.exec_getdiag(cx, *stacked, items),
            PlStmtKind::Open { cursor, scroll, query, args } => self.exec_open(cx, cursor, *scroll, query.as_ref(), args.as_ref()),
            PlStmtKind::Fetch { cursor, direction, targets, is_move } => self.exec_fetch(cx, cursor, direction, targets, *is_move),
            PlStmtKind::Close { cursor } => self.exec_close(cx, cursor),
            PlStmtKind::Null => Ok(Flow::Normal),
            PlStmtKind::Commit { chain } => self.exec_txn_control(cx, true, *chain),
            PlStmtKind::Rollback { chain } => self.exec_txn_control(cx, false, *chain),
        }
    }

    fn check_constant(&self, target: &PlTarget) -> Result<()> {
        if let Some(VarRef::Var(i)) = self.resolve(&target.name) {
            let vars = self.vars.borrow();
            if vars[i].constant {
                return Err(PgError::new(SqlState::from_code("22005").unwrap_or(SqlState::SYNTAX_ERROR), format!("variable \"{}\" is declared CONSTANT", vars[i].name)));
            }
        }
        Ok(())
    }

    /// What a non-normal flow means for the loop labeled `label`.
    fn loop_flow(&self, f: Flow, label: &Option<String>) -> LoopCtl {
        match f {
            Flow::Exit(None) => LoopCtl::Break(Flow::Normal),
            Flow::Exit(Some(l)) if label.as_deref() == Some(l.as_str()) => LoopCtl::Break(Flow::Normal),
            Flow::Continue(None) => LoopCtl::Next,
            Flow::Continue(Some(l)) if label.as_deref() == Some(l.as_str()) => LoopCtl::Next,
            other => LoopCtl::Break(other),
        }
    }

    fn exec_case(&mut self, cx: &mut ExecContext, expr: Option<&PlSql>, arms: &[(PlSql, Vec<PlStmt>)], else_body: Option<&[PlStmt]>) -> Result<Flow> {
        match expr {
            Some(e) => {
                let (v, t) = self.eval_expr(cx, e, None)?;
                // `WHEN a, b` compares with `=`: evaluate `$v = ANY(ARRAY[a, b])` through a temporary variable.
                self.push_scope(None);
                let tmp = self.declare_var(cx, "\u{0}case", t, v)?;
                let _ = tmp;
                let mut chosen = None;
                for (i, (when, _)) in arms.iter().enumerate() {
                    let cond = PlSql { text: format!("\"\u{0}case\" IN ({})", when.text), offset: when.offset };
                    if self.eval_cond(cx, &cond)? {
                        chosen = Some(i);
                        break;
                    }
                }
                self.pop_scope();
                self.flush_notices(cx);
                match chosen {
                    Some(i) => self.exec_stmts(cx, &arms[i].1),
                    None => match else_body {
                        Some(b) => self.exec_stmts(cx, b),
                        None => Err(PgError::new(sqlstate("20000"), "case not found")
                            .with_hint("CASE statement is missing ELSE part.")),
                    },
                }
            }
            None => {
                for (when, body) in arms {
                    if self.eval_cond(cx, when)? {
                        self.flush_notices(cx);
                        return self.exec_stmts(cx, body);
                    }
                }
                self.flush_notices(cx);
                match else_body {
                    Some(b) => self.exec_stmts(cx, b),
                    None => Err(PgError::new(sqlstate("20000"), "case not found").with_hint("CASE statement is missing ELSE part.")),
                }
            }
        }
    }

    fn exec_fori(
        &mut self,
        cx: &mut ExecContext,
        label: &Option<String>,
        var: &str,
        reverse: bool,
        lower: &PlSql,
        upper: &PlSql,
        step: Option<&PlSql>,
        body: &[PlStmt],
    ) -> Result<Flow> {
        let (lv, lt) = self.eval_expr(cx, lower, None)?;
        let lv = plfunc::coerce_value(cx, lv, lt, TypeId::INT4)?;
        let lo = match lv {
            Datum::Int4(i) => i as i64,
            _ => return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "lower bound of FOR loop cannot be null")),
        };
        let (uv, ut) = self.eval_expr(cx, upper, None)?;
        let uv = plfunc::coerce_value(cx, uv, ut, TypeId::INT4)?;
        let hi = match uv {
            Datum::Int4(i) => i as i64,
            _ => return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "upper bound of FOR loop cannot be null")),
        };
        let by = match step {
            Some(s) => {
                let (sv, st) = self.eval_expr(cx, s, None)?;
                match plfunc::coerce_value(cx, sv, st, TypeId::INT4)? {
                    Datum::Int4(i) if i > 0 => i as i64,
                    Datum::Int4(_) => {
                        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "BY value of FOR loop must be greater than zero"))
                    }
                    _ => return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "BY value of FOR loop cannot be null")),
                }
            }
            None => 1,
        };
        self.flush_notices(cx);
        self.push_scope(label.clone());
        let idx = self.declare_var(cx, var, TypeId::INT4, Datum::Null)?;
        let mut i = lo;
        let mut found = false;
        let result = loop {
            if (!reverse && i > hi) || (reverse && i < hi) {
                break Ok(Flow::Normal);
            }
            found = true;
            self.vars.borrow_mut()[idx].value = Datum::Int4(i as i32);
            match self.exec_stmts(cx, body) {
                Err(e) => break Err(e),
                Ok(Flow::Normal) => {}
                Ok(f) => match self.loop_flow(f, label) {
                    LoopCtl::Break(out) => break Ok(out),
                    LoopCtl::Next => {}
                },
            }
            if reverse {
                i -= by;
            } else {
                i += by;
            }
            if i > i32::MAX as i64 || i < i32::MIN as i64 {
                break Ok(Flow::Normal);
            }
        };
        self.pop_scope();
        let r = result?;
        self.set_found(found);
        Ok(r)
    }

    /// Run a loop body over materialized rows (FOR over query).
    pub fn loop_over_rows(&mut self, cx: &mut ExecContext, label: &Option<String>, targets: &[PlTarget], rows: SpiResult, body: &[PlStmt]) -> Result<Flow> {
        let types: Vec<TypeId> = rows.columns.iter().map(|c| c.type_id).collect();
        let names: Vec<String> = rows.columns.iter().map(|c| c.name.clone()).collect();
        let mut found = false;
        self.push_scope(label.clone());
        let mut out = Ok(Flow::Normal);
        for row in rows.rows {
            found = true;
            if let Err(e) = self.assign_row(cx, targets, row, &types, &names) {
                out = Err(e);
                break;
            }
            match self.exec_stmts(cx, body) {
                Err(e) => {
                    out = Err(e);
                    break;
                }
                Ok(Flow::Normal) => {}
                Ok(f) => match self.loop_flow(f, label) {
                    LoopCtl::Break(o) => {
                        out = Ok(o);
                        break;
                    }
                    LoopCtl::Next => {}
                },
            }
        }
        self.pop_scope();
        let r = out?;
        self.set_found(found);
        Ok(r)
    }

    fn exec_foreach(&mut self, cx: &mut ExecContext, label: &Option<String>, targets: &[PlTarget], slice: u32, array: &PlSql, body: &[PlStmt]) -> Result<Flow> {
        let (v, t) = self.eval_expr(cx, array, None)?;
        let arr = match v {
            Datum::Null => return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "FOREACH expression must not be null")),
            Datum::Array(a) => a,
            _ => {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("FOREACH expression must yield an array, not type {}", plfunc::type_name(cx, t)),
                ))
            }
        };
        let ndims = arr.dims.len() as u32;
        if slice > ndims {
            return Err(PgError::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                format!("slice dimension ({slice}) is out of the valid range 0..{ndims}"),
            ));
        }
        let elem = TypeId::of(arr.elem);
        // Target type checks (PG: slice needs an array variable, no slice needs a non-array one).
        if targets.len() == 1 {
            if let Some(VarRef::Var(i)) = self.resolve(&targets[0].name) {
                let vt = self.vars.borrow()[i].ty;
                let is_arr = pgarbor_types::registry::element_type_of(&cx.se.env, pgarbor_types::registry::base_type(&cx.se.env, vt.oid)).is_some_and(|e| e != 0);
                if slice > 0 && !is_arr {
                    return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "FOREACH ... SLICE loop variable must be of an array type"));
                }
                if slice == 0 && is_arr && vt.oid != oids::TEXT {
                    return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "FOREACH loop variable must not be of an array type"));
                }
            }
        }
        self.flush_notices(cx);
        let items: Vec<(Datum, TypeId)> = if slice == 0 {
            arr.items.iter().map(|d| (d.clone(), elem)).collect()
        } else {
            // Each slice spans the last `slice` dimensions.
            let inner_dims: Vec<(i32, i32)> = arr.dims[(ndims - slice) as usize..].iter().map(|(n, _)| (*n, 1)).collect();
            let per: usize = inner_dims.iter().map(|(n, _)| *n as usize).product::<usize>().max(1);
            let arr_ty = TypeId::of(pgarbor_types::registry::array_type_of(&cx.se.env, arr.elem).unwrap_or(0));
            arr.items
                .chunks(per)
                .map(|c| {
                    (
                        Datum::Array(Box::new(pgarbor_types::array::Array { elem: arr.elem, dims: inner_dims.clone(), items: c.to_vec() })),
                        arr_ty,
                    )
                })
                .collect()
        };
        let mut out = Ok(Flow::Normal);
        let mut found = false;
        for (v, vt) in items {
            found = true;
            let r = if targets.len() == 1 {
                self.assign_target(cx, &targets[0], v, vt)
            } else {
                match v {
                    Datum::Record(vals) => {
                        let types: Vec<TypeId> = vals.iter().map(|d| TypeId::of(super::state::datum_type(d))).collect();
                        let names: Vec<String> = (0..vals.len()).map(|i| format!("f{}", i + 1)).collect();
                        self.assign_row(cx, targets, vals, &types, &names)
                    }
                    other => self.assign_target(cx, &targets[0], other, vt),
                }
            };
            if let Err(e) = r {
                out = Err(e);
                break;
            }
            match self.exec_stmts(cx, body) {
                Err(e) => {
                    out = Err(e);
                    break;
                }
                Ok(Flow::Normal) => {}
                Ok(f) => match self.loop_flow(f, label) {
                    LoopCtl::Break(o) => {
                        out = Ok(o);
                        break;
                    }
                    LoopCtl::Next => {}
                },
            }
        }
        let r = out?;
        self.set_found(found);
        Ok(r)
    }

    /// Run the query of FOR / RETURN QUERY / OPEN.
    pub fn open_query(&mut self, cx: &mut ExecContext, q: &PlQuery) -> Result<SpiResult> {
        match q {
            PlQuery::Static(sql) => self.run_sql(cx, &sql.text, &|stmt| {
                if !matches!(stmt, BoundStatement::Query(_)) && !stmt_returns_rows(stmt) {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, "cannot open a non-SELECT query as cursor"));
                }
                Ok(())
            }),
            PlQuery::Execute { query, using } => {
                let text = self.eval_query_string(cx, query)?;
                let args = self.eval_using(cx, using)?;
                self.run_dynamic(cx, &text, &args, &|_| Ok(()))
            }
        }
    }

    /// EXECUTE's query string (NULL -> 22004).
    pub fn eval_query_string(&mut self, cx: &mut ExecContext, e: &PlSql) -> Result<String> {
        let (v, t) = self.eval_expr(cx, e, None)?;
        if v.is_null() {
            return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "query string argument of EXECUTE is null"));
        }
        self.to_text(cx, &v, t)
    }

    pub fn eval_using(&mut self, cx: &mut ExecContext, using: &[PlSql]) -> Result<Vec<(Datum, TypeId)>> {
        let mut out = Vec::with_capacity(using.len());
        for u in using {
            let (v, t) = self.eval_expr(cx, u, None)?;
            // Unknown literals in USING are text (PG passes them as text parameters).
            let t = if t.oid == oids::UNKNOWN { TypeId::TEXT } else { t };
            out.push((v, t));
        }
        Ok(out)
    }

    /// Text output of a value.
    pub fn to_text(&self, cx: &ExecContext, v: &Datum, t: TypeId) -> Result<String> {
        let io = cx.se.io_ctx();
        if let (Datum::Record(vals), true) = (v, t.oid == oids::RECORD) {
            let types: Vec<TypeId> = vals.iter().map(|d| TypeId::of(super::state::datum_type(d))).collect();
            return pgarbor_types::record::output_record(&io, &types, vals);
        }
        pgarbor_types::io::output(&io, t, v)
    }

    /// Assign one result row to INTO / loop targets (PG `exec_move_row`).
    pub fn assign_row(&mut self, cx: &mut ExecContext, targets: &[PlTarget], row: Vec<Datum>, types: &[TypeId], names: &[String]) -> Result<()> {
        if targets.len() == 1 {
            let t = &targets[0];
            let r = self.resolve(&t.name).ok_or_else(|| super::state::not_a_variable(&t.text))?;
            if let VarRef::Var(i) = &r {
                let (is_rec, fields) = {
                    let vars = self.vars.borrow();
                    (vars[*i].is_record(), vars[*i].rec_fields.clone())
                };
                if is_rec || fields.is_some() {
                    // Single composite column assigned to a row/record target: expand it.
                    if row.len() == 1 && (types.first().map(|t| t.oid == oids::RECORD || plfunc::composite_fields(cx, t.oid).is_some()).unwrap_or(false)) {
                        let mut row = row;
                        let v = row.pop().unwrap_or(Datum::Null);
                        let ty = types[0];
                        return self.assign_var(cx, *i, v, ty);
                    }
                    if is_rec {
                        let rec_fields: Vec<(String, TypeId)> = names.iter().cloned().zip(types.iter().copied()).collect();
                        let mut vars = self.vars.borrow_mut();
                        vars[*i].value = Datum::Record(row);
                        vars[*i].rec_fields = Some(rec_fields);
                        return Ok(());
                    }
                    let fields = fields.unwrap_or_default();
                    let mut vals = Vec::with_capacity(fields.len());
                    let mut it = row.into_iter();
                    for (k, (_, ft)) in fields.iter().enumerate() {
                        let v = it.next().unwrap_or(Datum::Null);
                        let st = types.get(k).copied().unwrap_or(*ft);
                        vals.push(plfunc::coerce_value(cx, v, st, *ft)?);
                    }
                    self.vars.borrow_mut()[*i].value = Datum::Record(vals);
                    return Ok(());
                }
            }
            let v = row.into_iter().next().unwrap_or(Datum::Null);
            let ty = types.first().copied().unwrap_or(TypeId::UNKNOWN);
            return self.assign_target(cx, t, v, ty);
        }
        let mut it = row.into_iter();
        for (k, t) in targets.iter().enumerate() {
            let v = it.next().unwrap_or(Datum::Null);
            let ty = types.get(k).copied().unwrap_or(TypeId::UNKNOWN);
            self.assign_target(cx, t, v, ty)?;
        }
        Ok(())
    }

    /// Set INTO targets to NULLs (no row).
    fn assign_nulls(&mut self, cx: &mut ExecContext, targets: &[PlTarget]) -> Result<()> {
        if targets.len() == 1 {
            if let Some(VarRef::Var(i)) = self.resolve(&targets[0].name) {
                let is_comp = {
                    let vars = self.vars.borrow();
                    vars[i].is_record() || vars[i].rec_fields.is_some()
                };
                if is_comp {
                    self.vars.borrow_mut()[i].value = Datum::Null;
                    return Ok(());
                }
            }
        }
        for t in targets {
            self.assign_target(cx, t, Datum::Null, TypeId::UNKNOWN)?;
        }
        Ok(())
    }

    /// INTO handling shared by SQL statements and EXECUTE.
    fn store_into(&mut self, cx: &mut ExecContext, r: SpiResult, strict: bool, targets: &[PlTarget], is_dml: bool) -> Result<()> {
        let types: Vec<TypeId> = r.columns.iter().map(|c| c.type_id).collect();
        let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        let n = r.rows.len();
        if n == 0 {
            if strict {
                return Err(PgError::new(sqlstate("P0002"), "query returned no rows"));
            }
            return self.assign_nulls(cx, targets);
        }
        if n > 1 && (strict || is_dml) {
            return Err(PgError::new(sqlstate("P0003"), "query returned more than one row")
                .with_hint("Make sure the query returns a single row, or use LIMIT 1."));
        }
        let row = r.rows.into_iter().next().unwrap_or_default();
        self.assign_row(cx, targets, row, &types, &names)
    }

    fn exec_sql(&mut self, cx: &mut ExecContext, sql: &PlSql, into: Option<&(bool, Vec<PlTarget>)>, first_word: &str) -> Result<Flow> {
        if first_word == "call" {
            return self.exec_call_stmt(cx, sql);
        }
        let r = self.run_sql(cx, &sql.text, &|stmt| {
            if let BoundStatement::Copy(c) = stmt {
                if matches!(c.endpoint, pgarbor_sql::ir::CopyEndpoint::Stdin | pgarbor_sql::ir::CopyEndpoint::Stdout) {
                    return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot COPY to/from client in PL/pgSQL"));
                }
            }
            Ok(())
        })?;
        let is_dml = matches!(first_word, "insert" | "update" | "delete" | "merge");
        let sets_found = is_dml || matches!(first_word, "select" | "with" | "values" | "table");
        self.row_count = r.processed;
        match into {
            Some((strict, targets)) => {
                if !r.returns_rows {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, "INTO used with a command that cannot return data"));
                }
                let any = !r.rows.is_empty();
                self.store_into(cx, r, *strict, targets, is_dml)?;
                self.set_found(any);
            }
            None => {
                if r.returns_rows && !r.tag.starts_with("SHOW") {
                    let is_select = r.tag.starts_with("SELECT");
                    let mut e = PgError::new(SqlState::SYNTAX_ERROR, "query has no destination for result data");
                    if is_select {
                        e = e.with_hint("If you want to discard the results of a SELECT, use PERFORM instead.");
                    }
                    return Err(e);
                }
                if sets_found {
                    self.set_found(r.processed > 0);
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_dynexecute(&mut self, cx: &mut ExecContext, query: &PlSql, into: Option<&(bool, Vec<PlTarget>)>, using: &[PlSql]) -> Result<Flow> {
        let text = self.eval_query_string(cx, query)?;
        let args = self.eval_using(cx, using)?;
        let r = self.run_dynamic(cx, &text, &args, &|stmt| {
            if let BoundStatement::Transaction(_) = stmt {
                return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "EXECUTE of transaction commands is not implemented"));
            }
            Ok(())
        })?;
        self.row_count = r.processed;
        if let Some((strict, targets)) = into {
            if !r.returns_rows {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "INTO used with a command that cannot return data"));
            }
            let dml = !r.tag.starts_with("SELECT");
            self.store_into(cx, r, *strict, targets, dml && false)?;
        }
        Ok(Flow::Normal)
    }

    fn exec_return(&mut self, cx: &mut ExecContext, expr: Option<&PlSql>) -> Result<Flow> {
        if self.retset {
            if expr.is_some() {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "RETURN cannot have a parameter in function returning set"));
            }
            self.returned = true;
            return Ok(Flow::Return);
        }
        match expr {
            None => {
                self.returned = true;
                Ok(Flow::Return)
            }
            Some(e) => {
                if self.is_procedure {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, "RETURN cannot have a parameter in a procedure"));
                }
                let (v, t) = self.eval_expr(cx, e, None)?;
                self.retval = Some((v, t));
                self.returned = true;
                Ok(Flow::Return)
            }
        }
    }

    fn exec_return_next(&mut self, cx: &mut ExecContext, expr: Option<&PlSql>) -> Result<Flow> {
        if !self.retset {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "cannot use RETURN NEXT in a non-SETOF function"));
        }
        match expr {
            None => {
                if self.out_vars.is_empty() {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, "RETURN NEXT must have a parameter"));
                }
                let row = self.out_row();
                self.ret_rows.push(row);
            }
            Some(e) => {
                let (v, t) = self.eval_expr(cx, e, None)?;
                let v = self.coerce_result(cx, v, t)?;
                self.ret_rows.push(v);
            }
        }
        Ok(Flow::Normal)
    }

    /// The OUT parameters as the result value (one OUT -> its value, several -> a record).
    fn out_row(&self) -> Datum {
        if self.out_vars.len() == 1 {
            return self.var_value(self.out_vars[0]);
        }
        Datum::Record(self.out_vars.iter().map(|&i| self.var_value(i)).collect())
    }

    /// Coerce a RETURN / RETURN NEXT value to the function's result type.
    fn coerce_result(&mut self, cx: &mut ExecContext, v: Datum, t: TypeId) -> Result<Datum> {
        let rt = self.ret_type;
        if rt.oid == oids::VOID {
            return Ok(Datum::Null);
        }
        if let Some(fields) = plfunc::composite_fields(cx, rt.oid) {
            return match v {
                Datum::Null => Ok(Datum::Null),
                Datum::Record(vals) => {
                    let src = plfunc::composite_fields(cx, t.oid).or_else(|| super::state::record_fields_of(&Datum::Record(vals.clone()), t));
                    if vals.len() != fields.len() {
                        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "returned record type does not match expected record type")
                            .with_detail(format!("Number of returned columns ({}) does not match expected column count ({}).", vals.len(), fields.len())));
                    }
                    let mut out = Vec::with_capacity(fields.len());
                    for (k, (val, (_, ft))) in vals.into_iter().zip(fields.iter()).enumerate() {
                        let st = src.as_ref().and_then(|s| s.get(k)).map(|s| s.1).unwrap_or(*ft);
                        out.push(plfunc::coerce_value(cx, val, st, *ft)?);
                    }
                    Ok(Datum::Record(out))
                }
                _ => Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot return non-composite value from function returning composite type")),
            };
        }
        if rt.oid == oids::RECORD {
            return Ok(v);
        }
        plfunc::coerce_value(cx, v, t, rt)
    }

    fn append_query_rows(&mut self, cx: &mut ExecContext, r: SpiResult) -> Result<()> {
        let types: Vec<TypeId> = r.columns.iter().map(|c| c.type_id).collect();
        self.row_count = r.processed;
        self.set_found(r.processed > 0);
        let expect: Option<Vec<(String, TypeId)>> = if self.out_vars.len() > 1 {
            Some(self.out_vars.iter().map(|&i| {
                let v = self.var(i);
                v.map(|v| (v.name, v.ty)).unwrap_or_else(|| (String::new(), TypeId::UNKNOWN))
            }).collect())
        } else {
            plfunc::composite_fields(cx, self.ret_type.oid)
        };
        for row in r.rows {
            match &expect {
                Some(fields) => {
                    if row.len() != fields.len() {
                        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "structure of query does not match function result type")
                            .with_detail(format!("Number of returned columns ({}) does not match expected column count ({}).", row.len(), fields.len())));
                    }
                    let mut out = Vec::with_capacity(fields.len());
                    for (k, (v, (_, ft))) in row.into_iter().zip(fields.iter()).enumerate() {
                        let st = types[k];
                        if st.oid != ft.oid && !pgarbor_types::cast::lookup::is_binary_coercible(&cx.se.env, st.oid, ft.oid).unwrap_or(false) {
                            return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "structure of query does not match function result type")
                                .with_detail(format!("Returned type {} does not match expected type {} in column \"{}\" (position {}).", plfunc::type_name(cx, st), plfunc::type_name(cx, *ft), fields[k].0, k + 1)));
                        }
                        out.push(v);
                    }
                    self.ret_rows.push(Datum::Record(out));
                }
                None => {
                    if row.len() != 1 {
                        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "structure of query does not match function result type")
                            .with_detail(format!("Number of returned columns ({}) does not match expected column count (1).", row.len())));
                    }
                    let st = types[0];
                    let rt = self.ret_type;
                    if st.oid != rt.oid && rt.oid != oids::RECORD && !pgarbor_types::cast::lookup::is_binary_coercible(&cx.se.env, st.oid, rt.oid).unwrap_or(false) {
                        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "structure of query does not match function result type")
                            .with_detail(format!("Returned type {} does not match expected type {} in column \"{}\" (position 1).", plfunc::type_name(cx, st), plfunc::type_name(cx, rt), r.columns[0].name)));
                    }
                    let mut row = row;
                    self.ret_rows.push(row.pop().unwrap_or(Datum::Null));
                }
            }
        }
        Ok(())
    }

    fn exec_raise(&mut self, cx: &mut ExecContext, level: RaiseLevel, what: &RaiseWhat, options: &[(RaiseOption, PlSql)]) -> Result<Flow> {
        if matches!(what, RaiseWhat::Nothing) && options.is_empty() {
            return match self.handling.last() {
                Some(e) => {
                    let mut e = e.clone();
                    // The re-thrown error keeps its original context.
                    e.context = None;
                    Err(e)
                }
                None => Err(PgError::new(
                    sqlstate("0Z002"),
                    "RAISE without parameters cannot be used outside an exception handler",
                )),
            };
        }
        let mut code: Option<String> = None;
        let mut condname: Option<String> = None;
        let mut message: Option<String> = None;
        match what {
            RaiseWhat::Nothing => {}
            RaiseWhat::Format { format, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let (v, t) = self.eval_expr(cx, a, None)?;
                    vals.push(if v.is_null() { "<NULL>".to_string() } else { self.to_text(cx, &v, t)? });
                }
                let mut out = String::new();
                let mut it = vals.into_iter();
                let mut chars = format.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '%' {
                        if chars.peek() == Some(&'%') {
                            chars.next();
                            out.push('%');
                        } else {
                            out.push_str(&it.next().unwrap_or_default());
                        }
                    } else {
                        out.push(c);
                    }
                }
                message = Some(out);
            }
            RaiseWhat::Condition { name, sqlstate } => {
                code = Some(sqlstate.clone());
                condname = Some(name.clone());
            }
            RaiseWhat::SqlState(s) => {
                code = Some(s.clone());
                condname = Some(s.clone());
            }
        }
        let mut detail = None;
        let mut hint = None;
        let mut column = None;
        let mut constraint = None;
        let mut datatype = None;
        let mut table = None;
        let mut schema = None;
        let dup = |name: &str| PgError::new(SqlState::SYNTAX_ERROR, format!("RAISE option already specified: {name}"));
        for (opt, e) in options {
            let (v, t) = self.eval_expr(cx, e, None)?;
            if v.is_null() {
                return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "RAISE statement option cannot be null"));
            }
            let s = self.to_text(cx, &v, t)?;
            match opt {
                RaiseOption::Errcode => {
                    if code.is_some() {
                        return Err(dup("ERRCODE"));
                    }
                    let c = if s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) {
                        s.clone()
                    } else {
                        match pgarbor_sql::plpgsql::errcodes::CONDITIONS.iter().find(|(n, _)| *n == s) {
                            Some((_, c)) => (*c).to_string(),
                            None => {
                                return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("unrecognized exception condition \"{s}\"")))
                            }
                        }
                    };
                    code = Some(c);
                    condname = Some(s);
                }
                RaiseOption::Message => {
                    if message.is_some() {
                        return Err(dup("MESSAGE"));
                    }
                    message = Some(s);
                }
                RaiseOption::Detail => {
                    if detail.is_some() {
                        return Err(dup("DETAIL"));
                    }
                    detail = Some(s);
                }
                RaiseOption::Hint => {
                    if hint.is_some() {
                        return Err(dup("HINT"));
                    }
                    hint = Some(s);
                }
                RaiseOption::Column => {
                    if column.is_some() {
                        return Err(dup("COLUMN"));
                    }
                    column = Some(s);
                }
                RaiseOption::Constraint => {
                    if constraint.is_some() {
                        return Err(dup("CONSTRAINT"));
                    }
                    constraint = Some(s);
                }
                RaiseOption::Datatype => {
                    if datatype.is_some() {
                        return Err(dup("DATATYPE"));
                    }
                    datatype = Some(s);
                }
                RaiseOption::Table => {
                    if table.is_some() {
                        return Err(dup("TABLE"));
                    }
                    table = Some(s);
                }
                RaiseOption::Schema => {
                    if schema.is_some() {
                        return Err(dup("SCHEMA"));
                    }
                    schema = Some(s);
                }
            }
        }
        let is_error = level == RaiseLevel::Exception;
        let code = match code {
            Some(c) => c,
            None if is_error => "P0001".to_string(),
            None if level == RaiseLevel::Warning => "01000".to_string(),
            None => "00000".to_string(),
        };
        let message = match message {
            Some(m) => m,
            None => condname.unwrap_or_else(|| code.clone()),
        };
        let mut e = PgError::new(sqlstate(&code), message);
        e.detail = detail;
        e.hint = hint;
        e.column = column;
        e.constraint = constraint;
        e.datatype = datatype;
        e.table = table;
        e.schema = schema;
        if is_error {
            return Err(e);
        }
        let sev = match level {
            RaiseLevel::Warning => Severity::Warning,
            RaiseLevel::Notice => Severity::Notice,
            RaiseLevel::Info => Severity::Info,
            RaiseLevel::Log => Severity::Log,
            _ => Severity::Debug,
        };
        cx.notice(e.with_severity(sev));
        Ok(Flow::Normal)
    }

    fn exec_getdiag(&mut self, cx: &mut ExecContext, stacked: bool, items: &[(PlTarget, DiagItem)]) -> Result<Flow> {
        if stacked && self.handling.is_empty() {
            return Err(PgError::new(sqlstate("0Z002"), "GET STACKED DIAGNOSTICS cannot be used outside an exception handler"));
        }
        for (target, item) in items {
            let (v, t): (Datum, TypeId) = match item {
                DiagItem::RowCount => (Datum::Int8(self.row_count as i64), TypeId::INT8),
                DiagItem::PgRoutineOid => (Datum::Oid(self.fn_oid), TypeId::OID),
                DiagItem::PgContext => (Datum::Text(self.site_line()), TypeId::TEXT),
                _ => {
                    let e = self.handling.last().cloned().unwrap_or_else(|| PgError::new(SqlState::SUCCESSFUL_COMPLETION, ""));
                    let s = match item {
                        DiagItem::ReturnedSqlstate => e.sqlstate.code().to_string(),
                        DiagItem::MessageText => e.message.clone(),
                        DiagItem::PgExceptionDetail => e.detail.clone().unwrap_or_default(),
                        DiagItem::PgExceptionHint => e.hint.clone().unwrap_or_default(),
                        DiagItem::PgExceptionContext => e.context.clone().unwrap_or_default(),
                        DiagItem::ColumnName => e.column.clone().unwrap_or_default(),
                        DiagItem::ConstraintName => e.constraint.clone().unwrap_or_default(),
                        DiagItem::PgDatatypeName => e.datatype.clone().unwrap_or_default(),
                        DiagItem::TableName => e.table.clone().unwrap_or_default(),
                        DiagItem::SchemaName => e.schema.clone().unwrap_or_default(),
                        _ => String::new(),
                    };
                    (Datum::Text(s), TypeId::TEXT)
                }
            };
            self.assign_target(cx, target, v, t)?;
        }
        Ok(Flow::Normal)
    }

    /// CALL inside PL/pgSQL: INOUT / OUT arguments that are variables receive the results.
    fn exec_call_stmt(&mut self, cx: &mut ExecContext, sql: &PlSql) -> Result<Flow> {
        let nonatomic = !self.atomic;
        let r = self.run_sql(cx, &sql.text, &|_| Ok(()))?;
        let _ = nonatomic;
        if r.returns_rows {
            // Map the output row back to the variables named as OUT / INOUT arguments.
            if let Some(row) = r.rows.into_iter().next() {
                let targets = call_output_targets(&sql.text);
                let types: Vec<TypeId> = r.columns.iter().map(|c| c.type_id).collect();
                for (k, v) in row.into_iter().enumerate() {
                    if let Some(Some(t)) = targets.get(k) {
                        let target = PlTarget { name: t.clone(), subscripts: Vec::new(), text: t.join("."), offset: 0 };
                        if self.resolve(&target.name).is_some() {
                            self.assign_target(cx, &target, v, types[k])?;
                        }
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_txn_control(&mut self, cx: &mut ExecContext, commit: bool, chain: bool) -> Result<Flow> {
        if self.atomic {
            return Err(PgError::new(sqlstate("2D000"), "invalid transaction termination"));
        }
        crate::utility::procedure_txn_control(cx, commit, chain)?;
        Ok(Flow::Normal)
    }

    /// End of the call: produce the result value (PG `plpgsql_exec_function` exit).
    pub fn finish(&mut self, cx: &mut ExecContext, f: &Function, arg_types: &[TypeId]) -> Result<CallOutput> {
        if self.retset {
            let rows = std::mem::take(&mut self.ret_rows);
            return Ok(CallOutput::Set(rows));
        }
        if self.is_procedure {
            return Ok(CallOutput::Scalar(Datum::Null));
        }
        if self.ret_type.oid == oids::VOID {
            // PL/pgSQL returns a non-NULL void value (void_out prints '').
            return Ok(CallOutput::Scalar(Datum::Text(String::new())));
        }
        if !self.out_vars.is_empty() {
            return Ok(CallOutput::Scalar(self.out_row()));
        }
        if !self.returned {
            self.err_site = ErrSite::None;
            return Err(PgError::new(sqlstate("2F005"), "control reached end of function without RETURN")
                .with_context_line(self.site_line()));
        }
        let (v, t) = self.retval.take().unwrap_or((Datum::Null, TypeId::UNKNOWN));
        let _ = (f, arg_types);
        match self.coerce_result(cx, v, t) {
            Ok(v) => Ok(CallOutput::Scalar(v)),
            Err(e) => Err(e.with_context_line(format!("PL/pgSQL function {} while casting return value to function's return type", self.sig))),
        }
    }
}

enum LoopCtl {
    Break(Flow),
    Next,
}

fn stmt_returns_rows(stmt: &BoundStatement) -> bool {
    stmt.result_columns().is_some_and(|c| !c.is_empty())
}

/// The plain variable names passed as arguments of `CALL p(a, b, ...)` (None for other expressions).
fn call_output_targets(text: &str) -> Vec<Option<Vec<String>>> {
    let Some(open) = text.find('(') else { return Vec::new() };
    let Some(close) = text.rfind(')') else { return Vec::new() };
    if close <= open {
        return Vec::new();
    }
    let inner = &text[open + 1..close];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut in_str = false;
    for c in inner.chars() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' | '[' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 && !in_str => {
                out.push(cur.clone());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .map(|a| {
            let a = a.trim();
            let a = match a.find("=>") {
                Some(p) => a[p + 2..].trim(),
                None => a,
            };
            let ok = !a.is_empty() && a.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '"'));
            if ok {
                Some(a.split('.').map(|p| if p.starts_with('"') { p.trim_matches('"').to_string() } else { p.to_lowercase() }).collect())
            } else {
                None
            }
        })
        .collect()
}

/// Resolve a SQL type name through the binder (`integer`, `varchar(10)`, `t%rowtype` handled elsewhere).
pub fn resolve_type_name(cx: &mut ExecContext, text: &str) -> Result<TypeId> {
    let lower = text.trim().to_ascii_lowercase();
    if lower == "record" {
        return Ok(TypeId::of(oids::RECORD));
    }
    if lower == "refcursor" {
        return Ok(TypeId::of(oids::REFCURSOR));
    }
    // Bind `NULL::<type>` to reuse the binder's type-name resolution.
    let sql = format!("SELECT NULL::{text}");
    let raws = super::hook::parse_cached(&sql)?;
    let raw = raws.first().ok_or_else(|| PgError::internal("type name parse produced no statement"))?;
    let out = crate::spi::bind(cx, raw, &[], None).map_err(|mut e| {
        if e.position.is_some() {
            e.position = None;
        }
        e
    })?;
    match out.statement {
        BoundStatement::Query(q) => q.output.first().map(|c| c.ty).ok_or_else(|| PgError::internal("type name resolution produced no column")),
        _ => Err(PgError::internal("type name resolution produced no query")),
    }
}

/// Resolve a relation name (for `%ROWTYPE` / `table.col%TYPE`) to its row type OID.
fn resolve_relation(cx: &mut ExecContext, text: &str) -> Result<pgarbor_common::Oid> {
    let ty = resolve_type_name(cx, text).map_err(|_| {
        PgError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{text}\" does not exist"))
    })?;
    Ok(ty.oid)
}
