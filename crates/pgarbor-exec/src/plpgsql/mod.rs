//! PL/pgSQL execution (lease EXE-06; PG `pl_exec.c`). The body is parsed by
//! `pgarbor_sql::plpgsql` (cached per body text) and interpreted statement by statement over the
//! caller's [`ExecContext`]: every SQL expression and statement runs through [`crate::spi`] with the
//! routine's variables bound as `$n` parameters by a binder hook ([`hook::VarHook`]).
//!
//! Module map: [`state`] (variables, scopes, name resolution, assignment), [`hook`] (binder hook and
//! expression / query evaluation with PG's error decoration), [`stmt`] (statement execution),
//! [`cursor`] (OPEN / FETCH / MOVE / CLOSE over materialized portals).
//!
//! Error CONTEXT lines follow `plpgsql_exec_error_callback`: `PL/pgSQL function f(integer) line 3 at
//! RAISE` for the innermost statement executing in each frame; `SQL statement "..."` /
//! `PL/pgSQL expression "..."` for SPI-run text (replaced by `internal_query` / `internal_position`
//! when the error has a position in that text). Notices raised while a statement runs get the same
//! lines.
pub mod cursor;
pub mod hook;
pub mod state;
pub mod stmt;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_catalog::model::{ArgMode, FuncKind, Function};
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::plpgsql::{parse_body, PlFunction};
use pgarbor_types::{oids, Datum, TypeId};

use crate::context::ExecContext;
use crate::plfunc::{self, CallOutput};

pub use state::Interp;

thread_local! {
    static PARSED: RefCell<HashMap<String, Rc<PlFunction>>> = RefCell::new(HashMap::new());
}

/// Parse (cached) a body. Syntax errors with a cursor position are moved into the statement text
/// that contains the body (PG `function_parse_error_transpose`), or reported as an internal position
/// in the body; other compile errors get the `compilation of PL/pgSQL function "f" near line N`
/// CONTEXT line.
pub fn compile(body: &str, fname: &str, query: Option<&str>) -> Result<Rc<PlFunction>> {
    if let Some(f) = PARSED.with(|p| p.borrow().get(body).cloned()) {
        return Ok(f);
    }
    match parse_body(body) {
        Ok(f) => {
            let f = Rc::new(f);
            PARSED.with(|p| {
                let mut m = p.borrow_mut();
                if m.len() > 512 {
                    m.clear();
                }
                m.insert(body.to_string(), Rc::clone(&f));
            });
            Ok(f)
        }
        Err(pe) => {
            let mut e = pe.error;
            e.position = None;
            match pe.offset {
                Some(off) => {
                    let cursor = body.get(..off.min(body.len())).map(|s| s.chars().count()).unwrap_or(0) + 1;
                    match query.and_then(|q| transpose_position(body, q, cursor)) {
                        Some(p) => e.position = Some(p as u32),
                        None => {
                            e.internal_position = Some(cursor as u32);
                            e.internal_query = Some(body.to_string());
                        }
                    }
                    Err(e)
                }
                None => {
                    let line = body.get(..pe.near.min(body.len())).map(|s| s.matches('\n').count() + 1).unwrap_or(1);
                    Err(e.with_context_line(format!("compilation of PL/pgSQL function \"{fname}\" near line {line}")))
                }
            }
        }
    }
}

/// PG `match_prosrc_to_query`: the 1-based character position in `query` of character `cursor` of
/// `prosrc`, when `prosrc` occurs exactly once in `query` as a dollar-quoted or single-quoted literal.
pub fn transpose_position(prosrc: &str, query: &str, cursor: usize) -> Option<usize> {
    let q = query.as_bytes();
    let p = prosrc.as_bytes();
    let mut matchpos: Option<usize> = None;
    if q.len() < p.len() {
        return None;
    }
    for cur in 0..(q.len() - p.len()) {
        if q[cur] == b'$' && q.get(cur + 1..cur + 1 + p.len()) == Some(p) && q.get(cur + 1 + p.len()) == Some(&b'$') {
            if matchpos.is_some() {
                return None;
            }
            let chars_before = query.get(..cur + 1).map(|s| s.chars().count())?;
            matchpos = Some(chars_before + cursor);
        } else if q[cur] == b'\'' {
            if let Some(newcp) = match_literal(prosrc, &query[cur + 1..], cursor) {
                if matchpos.is_some() {
                    return None;
                }
                let chars_before = query.get(..cur + 1).map(|s| s.chars().count())?;
                matchpos = Some(chars_before + newcp);
            }
        }
    }
    matchpos.filter(|m| *m > 0)
}

/// PG `match_prosrc_to_literal`.
fn match_literal(prosrc: &str, literal: &str, cursor: usize) -> Option<usize> {
    let lit = literal.as_bytes();
    let mut li = 0usize;
    let mut newcp = cursor as i64;
    let mut cursorpos = cursor as i64;
    for ch in prosrc.chars() {
        cursorpos -= 1;
        if lit.get(li) == Some(&b'\\') {
            li += 1;
            if cursorpos > 0 {
                newcp += 1;
            }
        } else if lit.get(li) == Some(&b'\'') {
            if lit.get(li + 1) != Some(&b'\'') {
                return None;
            }
            li += 1;
            if cursorpos > 0 {
                newcp += 1;
            }
        }
        let mut buf = [0u8; 4];
        let enc = ch.encode_utf8(&mut buf).as_bytes();
        if lit.get(li..li + enc.len()) != Some(enc) {
            return None;
        }
        li += enc.len();
    }
    if lit.get(li) == Some(&b'\'') && lit.get(li + 1) != Some(&b'\'') {
        return Some(newcp.max(0) as usize);
    }
    None
}

/// Call a PL/pgSQL function (scalar or set-returning). `result_type` is the call's resolved type.
pub fn call_function(
    cx: &mut ExecContext,
    f: &Function,
    args: Vec<Datum>,
    arg_types: Vec<TypeId>,
    result_type: TypeId,
) -> Result<CallOutput> {
    let ast = compile(&f.body, &f.name, None)?;
    let sig = plfunc::signature(cx, f);
    let mut interp = Interp::new(sig, f.name.clone(), f.oid, ast.options);
    interp.retset = f.retset;
    interp.is_procedure = f.kind == FuncKind::Procedure;
    interp.ret_type = result_type;
    interp.atomic = true;
    if result_type.oid == oids::TRIGGER || result_type.oid == oids::EVENT_TRIGGER {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "trigger functions can only be called as triggers",
        )
        .with_context_line(format!("compilation of PL/pgSQL function \"{}\" near line 1", f.name)));
    }
    // Parameters: IN / INOUT / VARIADIC take the argument values; OUT parameters start NULL.
    let mut in_idx = 0usize;
    let mut out_vars = Vec::new();
    interp.push_scope(Some(f.name.clone()));
    for (pos, a) in f.args.iter().enumerate() {
        let is_in = matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic);
        let ty = if is_in {
            let t = arg_types.get(in_idx).copied().unwrap_or(TypeId::of(a.type_oid));
            if pgarbor_types::registry::is_polymorphic(a.type_oid) { t } else { TypeId::of(a.type_oid) }
        } else {
            plfunc::resolve_declared_type(cx, f, a.type_oid, &arg_types)
        };
        let value = if is_in { args.get(in_idx).cloned().unwrap_or(Datum::Null) } else { Datum::Null };
        if is_in {
            in_idx += 1;
        }
        let name = if a.name.is_empty() { format!("${}", pos + 1) } else { a.name.clone() };
        let idx = interp.declare_var(cx, &name, ty, value)?;
        if is_in {
            // `$n` aliases the n-th input parameter.
            interp.param_slots.push(idx);
            if !a.name.is_empty() {
                interp.add_alias(&format!("${in_idx}"), idx);
            }
        }
        if matches!(a.mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table) {
            out_vars.push(idx);
        }
    }
    interp.out_vars = out_vars;
    interp.declare_found(cx)?;
    let r = interp.run_block_top(cx, &ast.block);
    let r = r.and_then(|_| interp.finish(cx, f, &arg_types));
    interp.close_local_cursors();
    match r {
        Ok(v) => Ok(v),
        Err(e) => Err(e),
    }
}

/// Run a DO block (`inline_code_block`). `nonatomic`: transaction control is allowed.
pub fn run_inline(cx: &mut ExecContext, code: &str, nonatomic: bool) -> Result<()> {
    let query = cx.se.services.current_query();
    let ast = compile(code, "inline_code_block", query.as_deref())?;
    let _guard = plfunc::DepthGuard::enter()?;
    let mut interp = Interp::new("inline_code_block".to_string(), "inline_code_block".to_string(), 0, ast.options);
    interp.ret_type = TypeId::of(oids::VOID);
    interp.is_procedure = true;
    interp.is_inline = true;
    interp.atomic = !nonatomic;
    interp.push_scope(None);
    interp.declare_found(cx)?;
    let r = interp.run_block_top(cx, &ast.block);
    interp.close_local_cursors();
    r.map(|_| ())
}

/// Call a procedure (CALL): returns the values of the OUT / INOUT parameters (empty when none).
pub fn call_procedure(
    cx: &mut ExecContext,
    f: &Function,
    args: Vec<Datum>,
    arg_types: Vec<TypeId>,
    nonatomic: bool,
) -> Result<Vec<Datum>> {
    let ast = compile(&f.body, &f.name, None)?;
    let sig = plfunc::signature(cx, f);
    let mut interp = Interp::new(sig, f.name.clone(), f.oid, ast.options);
    interp.is_procedure = true;
    interp.ret_type = TypeId::of(oids::VOID);
    interp.atomic = !nonatomic;
    let mut in_idx = 0usize;
    let mut out_vars = Vec::new();
    interp.push_scope(Some(f.name.clone()));
    for (pos, a) in f.args.iter().enumerate() {
        // Procedures take every non-OUT argument; OUT arguments are passed as NULL placeholders.
        let is_in = matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic);
        let value = args.get(pos).cloned().unwrap_or(Datum::Null);
        let ty = match arg_types.get(pos) {
            Some(t) if pgarbor_types::registry::is_polymorphic(a.type_oid) => *t,
            _ => TypeId::of(a.type_oid),
        };
        let name = if a.name.is_empty() { format!("${}", pos + 1) } else { a.name.clone() };
        let idx = interp.declare_var(cx, &name, ty, if is_in { value } else { Datum::Null })?;
        if is_in {
            in_idx += 1;
            interp.param_slots.push(idx);
            if !a.name.is_empty() {
                interp.add_alias(&format!("${in_idx}"), idx);
            }
        }
        if matches!(a.mode, ArgMode::Out | ArgMode::InOut) {
            out_vars.push(idx);
        }
    }
    interp.out_vars = out_vars.clone();
    interp.declare_found(cx)?;
    let r = interp.run_block_top(cx, &ast.block);
    interp.close_local_cursors();
    r?;
    Ok(out_vars.iter().map(|&i| interp.var_value(i)).collect())
}
