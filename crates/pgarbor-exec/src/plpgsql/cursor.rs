//! PL/pgSQL cursors: OPEN / FETCH / MOVE / CLOSE and FOR over a bound cursor (PG `exec_stmt_open`,
//! `exec_stmt_fetch`, `exec_stmt_close`, `exec_stmt_forc`) over [`crate::cursors`] portals.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_sql::plpgsql::{FetchDirection, PlQuery, PlSql, PlStmt, PlTarget};
use pgarbor_types::{oids, Datum, TypeId};

use super::state::{Flow, Interp, VarRef};
use crate::context::ExecContext;
use crate::cursors::{self, Direction, Portal};
use crate::spi::SpiResult;

fn pid(cx: &ExecContext) -> i32 {
    cx.se.services.backend_pid()
}

impl Interp {
    /// The cursor variable `name` (must be a refcursor variable).
    fn cursor_var(&self, name: &str) -> Result<usize> {
        match self.resolve(&[name.to_string()]) {
            Some(VarRef::Var(i)) => {
                let ty = self.vars.borrow()[i].ty;
                if ty.oid != oids::REFCURSOR {
                    return Err(PgError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("variable \"{name}\" must be of type cursor or refcursor"),
                    ));
                }
                Ok(i)
            }
            _ => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("\"{name}\" is not a known variable"))),
        }
    }

    /// Portal name held by cursor variable `idx` (`cursor variable "c" is null` when NULL).
    fn portal_name(&self, idx: usize) -> Result<String> {
        let vars = self.vars.borrow();
        match &vars[idx].value {
            Datum::Text(s) => Ok(s.clone()),
            Datum::Null => Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, format!("cursor variable \"{}\" is null", vars[idx].name))),
            other => Ok(format!("{other:?}")),
        }
    }

    /// Run a bound cursor's query with its arguments bound as variables.
    fn bound_cursor_rows(&mut self, cx: &mut ExecContext, idx: usize, args: Option<&PlSql>) -> Result<(SpiResult, Option<bool>, String)> {
        let decl = self.vars.borrow()[idx].cursor.clone().ok_or_else(|| PgError::internal("cursor variable without a query"))?;
        let cname = self.vars.borrow()[idx].name.clone();
        let given: Vec<String> = match args {
            Some(a) => split_args(&a.text),
            None => Vec::new(),
        };
        if decl.args.is_empty() && args.is_some() {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "arguments given for cursor without arguments"));
        }
        if !decl.args.is_empty() && args.is_none() {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "arguments required for cursor"));
        }
        // Positional or named (`name := value` / `name => value`) arguments.
        let mut values: Vec<Option<String>> = vec![None; decl.args.len()];
        let mut positional = 0usize;
        for g in &given {
            let named = g.find(":=").map(|p| (p, 2)).or_else(|| g.find("=>").map(|p| (p, 2)));
            match named {
                Some((p, l)) if g[..p].trim().chars().all(|c| c.is_alphanumeric() || c == '_') => {
                    let n = g[..p].trim().to_lowercase();
                    let k = decl.args.iter().position(|(an, _)| *an == n).ok_or_else(|| {
                        PgError::new(SqlState::SYNTAX_ERROR, format!("cursor \"{cname}\" has no argument named \"{n}\""))
                    })?;
                    values[k] = Some(g[p + l..].trim().to_string());
                }
                _ => {
                    if positional < values.len() {
                        values[positional] = Some(g.trim().to_string());
                    }
                    positional += 1;
                }
            }
        }
        if given.len() > decl.args.len() {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, format!("too many arguments for cursor \"{cname}\"")));
        }
        if values.iter().any(|v| v.is_none()) {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, format!("not enough arguments for cursor \"{cname}\"")));
        }
        let mut evaluated = Vec::new();
        for (k, v) in values.into_iter().enumerate() {
            let text = v.unwrap_or_default();
            let (val, t) = self.eval_expr(cx, &PlSql { text, offset: 0 }, None)?;
            evaluated.push((decl.args[k].0.clone(), decl.args[k].1, val, t));
        }
        // PG resolves the query when the cursor is declared: later declarations stay invisible.
        let hidden: Vec<(usize, Vec<(String, usize)>)> = {
            let mut scopes = self.scopes.borrow_mut();
            scopes
                .iter_mut()
                .enumerate()
                .map(|(i, sc)| {
                    let (keep, drop): (Vec<_>, Vec<_>) = sc.names.drain(..).partition(|(_, v)| *v < decl.visible_vars);
                    sc.names = keep;
                    (i, drop)
                })
                .collect()
        };
        self.push_scope(None);
        let r = (|| -> Result<SpiResult> {
            for (n, ty, val, t) in evaluated {
                let i = self.declare_var(cx, &n, ty, Datum::Null)?;
                self.assign_var(cx, i, val, t)?;
            }
            self.run_sql(cx, &decl.query.text, &|_| Ok(()))
        })();
        self.pop_scope();
        {
            let mut scopes = self.scopes.borrow_mut();
            for (i, drop) in hidden {
                if let Some(sc) = scopes.get_mut(i) {
                    sc.names.extend(drop);
                }
            }
        }
        Ok((r?, decl.scroll, decl.query.text.clone()))
    }

    pub fn exec_open(&mut self, cx: &mut ExecContext, cursor: &str, scroll: Option<bool>, query: Option<&PlQuery>, args: Option<&PlSql>) -> Result<Flow> {
        let idx = self.cursor_var(cursor)?;
        let txn = cx.txn.txn_id();
        // Portal name: the variable's value, else a generated one.
        let name = match self.var_value(idx) {
            Datum::Text(s) => s,
            _ => cursors::unnamed_name(pid(cx), txn),
        };
        if cursors::exists(pid(cx), txn, &name) {
            return Err(PgError::new(SqlState::from_code("42P03").unwrap_or(SqlState::DUPLICATE_OBJECT), format!("cursor \"{name}\" already in use")));
        }
        let (rows, scroll, text) = match query {
            Some(q) => {
                let text = match q {
                    PlQuery::Static(s) => s.text.clone(),
                    PlQuery::Execute { .. } => String::new(),
                };
                if let PlQuery::Static(s) = q {
                    check_scroll_for_update(scroll, &s.text)?;
                }
                (self.open_query(cx, q)?, scroll, text)
            }
            None => {
                let (r, s, t) = self.bound_cursor_rows(cx, idx, args)?;
                check_scroll_for_update(s, &t)?;
                (r, s, t)
            }
        };
        let mut portal = Portal::new(&name, rows.columns, rows.rows, txn);
        portal.no_scroll = !matches!(scroll, Some(true)) && (matches!(scroll, Some(false)) || !scrollable_text(&text));
        portal.scroll = matches!(scroll, Some(true));
        portal.statement = text;
        cursors::open(pid(cx), txn, portal, true)?;
        self.vars.borrow_mut()[idx].value = Datum::Text(name);
        Ok(Flow::Normal)
    }

    pub fn exec_fetch(&mut self, cx: &mut ExecContext, cursor: &str, direction: &FetchDirection, targets: &[PlTarget], is_move: bool) -> Result<Flow> {
        let idx = self.cursor_var(cursor)?;
        let name = self.portal_name(idx)?;
        let (dir, count) = self.direction(cx, direction)?;
        let txn = cx.txn.txn_id();
        let (rows, n, columns) = cursors::with_portal(pid(cx), txn, &name, |p| {
            let (rows, n) = p.fetch(dir, count, !is_move)?;
            Ok((rows, n, p.columns.clone()))
        })?;
        self.row_count = n;
        if is_move {
            self.set_found(n > 0);
            return Ok(Flow::Normal);
        }
        let types: Vec<TypeId> = columns.iter().map(|c| c.type_id).collect();
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        match rows.into_iter().next() {
            Some(row) => {
                self.assign_row(cx, targets, row, &types, &names)?;
                self.set_found(true);
            }
            None => {
                for t in targets {
                    if let Some(VarRef::Var(i)) = self.resolve(&t.name) {
                        let comp = {
                            let v = &self.vars.borrow()[i];
                            v.is_record() || v.rec_fields.is_some()
                        };
                        if comp {
                            self.vars.borrow_mut()[i].value = Datum::Null;
                            continue;
                        }
                    }
                    self.assign_target(cx, t, Datum::Null, TypeId::UNKNOWN)?;
                }
                self.set_found(false);
            }
        }
        Ok(Flow::Normal)
    }

    fn direction(&mut self, cx: &mut ExecContext, d: &FetchDirection) -> Result<(Direction, i64)> {
        let mut count = |me: &mut Interp, e: &PlSql| -> Result<i64> {
            let (v, t) = me.eval_expr(cx, e, None)?;
            let v = crate::plfunc::coerce_value(cx, v, t, TypeId::INT8)?;
            match v {
                Datum::Int8(i) => Ok(i),
                _ => Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "relative or absolute cursor position is null")),
            }
        };
        Ok(match d {
            FetchDirection::Next => (Direction::Forward, 1),
            FetchDirection::Prior => (Direction::Backward, 1),
            FetchDirection::First => (Direction::Absolute, 1),
            FetchDirection::Last => (Direction::Absolute, -1),
            FetchDirection::Absolute(e) => (Direction::Absolute, count(self, e)?),
            FetchDirection::Relative(e) => (Direction::Relative, count(self, e)?),
            FetchDirection::Forward(None) => (Direction::Forward, 1),
            FetchDirection::Forward(Some(e)) => (Direction::Forward, count(self, e)?),
            FetchDirection::ForwardAll => (Direction::Forward, cursors::ALL),
            FetchDirection::Backward(None) => (Direction::Backward, 1),
            FetchDirection::Backward(Some(e)) => (Direction::Backward, count(self, e)?),
            FetchDirection::BackwardAll => (Direction::Backward, cursors::ALL),
        })
    }

    pub fn exec_close(&mut self, cx: &mut ExecContext, cursor: &str) -> Result<Flow> {
        let idx = self.cursor_var(cursor)?;
        let name = self.portal_name(idx)?;
        if !cursors::close(pid(cx), cx.txn.txn_id(), &name) {
            return Err(cursors::missing(&name));
        }
        Ok(Flow::Normal)
    }

    pub fn exec_for_cursor(&mut self, cx: &mut ExecContext, label: &Option<String>, target: &str, cursor: &str, args: Option<&PlSql>, body: &[PlStmt]) -> Result<Flow> {
        let idx = self.cursor_var(cursor)?;
        let txn = cx.txn.txn_id();
        let name = match self.var_value(idx) {
            Datum::Text(s) => s,
            _ => cursors::unnamed_name(pid(cx), txn),
        };
        if cursors::exists(pid(cx), txn, &name) {
            return Err(PgError::new(SqlState::from_code("42P03").unwrap_or(SqlState::DUPLICATE_OBJECT), format!("cursor \"{name}\" already in use")));
        }
        let (rows, _, _) = self.bound_cursor_rows(cx, idx, args)?;
        // The loop variable is an implicitly declared record.
        self.push_scope(None);
        let r = (|| -> Result<Flow> {
            self.declare_var(cx, target, TypeId::of(oids::RECORD), Datum::Null)?;
            let t = PlTarget { name: vec![target.to_string()], subscripts: Vec::new(), text: target.to_string(), offset: 0 };
            self.loop_over_rows(cx, label, &[t], rows, body)
        })();
        self.pop_scope();
        r
    }
}

/// `SCROLL` cursors cannot use FOR UPDATE / FOR SHARE (0A000).
fn check_scroll_for_update(scroll: Option<bool>, text: &str) -> Result<()> {
    if scroll == Some(true) {
        let l = text.to_ascii_lowercase();
        for kw in ["for update", "for no key update", "for share", "for key share"] {
            if l.contains(kw) {
                return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("DECLARE SCROLL CURSOR ... {} is not supported", kw.to_ascii_uppercase()))
                    .with_detail("Scrollable cursors must be READ ONLY."));
            }
        }
    }
    Ok(())
}

/// Without SCROLL / NO SCROLL PG allows backward fetches when the plan supports backward scans
/// (scans, sorts, results; not joins, aggregation, DISTINCT, set operations or locking).
pub fn scrollable_text(text: &str) -> bool {
    let l = format!(" {} ", text.to_ascii_lowercase().replace(['\n', '\t', '(', ')', ','], " "));
    let blockers = [" join ", " group by ", " distinct ", " union ", " intersect ", " except ", " over ", " for update", " for share", " for no key", " for key share", "count", "sum ", "max ", "min ", "avg ", "array_agg", "string_agg"];
    if blockers.iter().any(|b| l.contains(b)) {
        return false;
    }
    // Several FROM items are a join too.
    if let Some(p) = l.find(" from ") {
        let rest = &l[p + 6..];
        let end = [" where ", " order ", " limit ", " offset "].iter().filter_map(|k| rest.find(k)).min().unwrap_or(rest.len());
        if text.to_ascii_lowercase().contains("generate_series") {
            return true;
        }
        let from = &rest[..end];
        let _ = from;
    }
    true
}

/// Split a comma-separated argument list at depth 0 (quotes respected).
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut in_str = false;
    for c in text.chars() {
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
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}
