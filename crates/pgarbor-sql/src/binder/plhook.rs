//! Procedural-language variable references (PG `p_pre_columnref_hook` / `p_post_columnref_hook`, used by
//! PL/pgSQL `plpgsql_pre_column_ref` / `plpgsql_post_column_ref` and SQL functions `sql_fn_post_column_ref`).
//!
//! A statement bound with [`bind_with_hook`] consults the hook for every column reference of THAT
//! statement (statements bound internally while expanding views or stored expressions use their own
//! source text and are not affected). The hook answers whether `qualifier.name` names a variable of the
//! calling routine and, if so, the expression standing for it (normally `$n` of the variable's type).
//!
//! Resolution order (`PlConflict`):
//! * `Error` (PL/pgSQL default `#variable_conflict error`): a name that is BOTH a column and a variable is
//!   42702 `column reference "x" is ambiguous` with DETAIL `It could refer to either a PL/pgSQL variable or
//!   a table column.`; a variable only -> the variable; a column only -> the column.
//! * `UseVariable`: the variable wins before columns are searched.
//! * `UseColumn` (PL/pgSQL `#variable_conflict use_column`, SQL functions): columns win; the variable is
//!   used only when no column matches.
use std::cell::RefCell;
use std::rc::Rc;

use pgarbor_common::{PgError, Result, SqlState};

use super::context::{BindContext, BindOutput};
use crate::ir::BoundExpr;
use crate::parser::RawStatement;
use crate::source::SourceMap;

/// Who wins when a name is both a column and a variable (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlConflict {
    Error,
    UseVariable,
    UseColumn,
}

/// Variable lookup supplied by the procedural-language executor.
pub trait PlVarHook {
    /// `qualifier.name` (qualifier possibly empty; all parts already folded) names a variable: the
    /// expression for it. `Ok(None)` = not a variable. Errors (e.g. a record variable that is not
    /// assigned yet, 55000) propagate.
    fn resolve(&self, qualifier: &[String], name: &str, pos: Option<u32>) -> Result<Option<BoundExpr>>;
    /// Conflict rule (module docs).
    fn conflict(&self) -> PlConflict;
}

struct Active {
    src: *const SourceMap,
    hook: Rc<dyn PlVarHook>,
}

thread_local! {
    static ACTIVE: RefCell<Vec<Active>> = const { RefCell::new(Vec::new()) };
}

/// Pops the hook frame even when binding fails.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            a.borrow_mut().pop();
        });
    }
}

/// Bind `stmt` (like [`super::bind`]) with `hook` resolving variable references (module docs).
pub fn bind_with_hook(stmt: &RawStatement, ctx: &BindContext, hook: Rc<dyn PlVarHook>) -> Result<BindOutput> {
    // `src` is only compared by address, never dereferenced.
    ACTIVE.with(|a| a.borrow_mut().push(Active { src: &*stmt.source as *const SourceMap, hook }));
    let _guard = Guard;
    super::dispatch::bind_statement(stmt, ctx)
}

/// Run `f` with the hook active for the statement whose source is `src`, if any.
fn with_hook<T>(src: &SourceMap, f: impl FnOnce(&dyn PlVarHook) -> Result<Option<T>>) -> Result<Option<T>> {
    let hook = ACTIVE.with(|a| {
        a.borrow().last().filter(|act| std::ptr::eq(act.src, src as *const SourceMap)).map(|act| Rc::clone(&act.hook))
    });
    match hook {
        Some(h) => f(&*h),
        None => Ok(None),
    }
}

/// Column-reference resolution with the hook (called by `expr::resolve_name`). `column` performs the
/// ordinary column lookup (`Ok(None)` = no such column).
pub fn resolve_with_hook(
    src: &SourceMap,
    qualifier: &[String],
    name: &str,
    pos: Option<u32>,
    column: impl FnOnce() -> Result<Option<BoundExpr>>,
) -> Result<Option<BoundExpr>> {
    let mut column = Some(column);
    let active = ACTIVE.with(|a| a.borrow().last().is_some_and(|act| std::ptr::eq(act.src, src as *const SourceMap)));
    if !active {
        return match column.take() {
            Some(c) => c(),
            None => Ok(None),
        };
    }
    let var = with_hook(src, |h| Ok(Some((h.conflict(), h.resolve(qualifier, name, pos)?))))?;
    let (conflict, var) = match var {
        Some(v) => v,
        None => (PlConflict::UseColumn, None),
    };
    if conflict == PlConflict::UseVariable {
        if let Some(v) = var {
            return Ok(Some(v));
        }
    }
    let col = match column.take() {
        Some(c) => c()?,
        None => None,
    };
    match (col, var) {
        (Some(_), Some(_)) if conflict == PlConflict::Error => {
            let full = if qualifier.is_empty() { name.to_string() } else { format!("{}.{}", qualifier.join("."), name) };
            let mut e = PgError::new(SqlState::AMBIGUOUS_COLUMN, format!("column reference \"{full}\" is ambiguous"))
                .with_detail("It could refer to either a PL/pgSQL variable or a table column.");
            e.position = pos;
            Err(e)
        }
        (Some(c), _) => Ok(Some(c)),
        (None, v) => Ok(v),
    }
}

/// True when a hook is active for the statement with source `src`.
pub fn hook_active(src: &SourceMap) -> bool {
    ACTIVE.with(|a| a.borrow().last().is_some_and(|act| std::ptr::eq(act.src, src as *const SourceMap)))
}
