//! Query deparser: `BoundQuery` -> `pg_get_viewdef` text (SQL-44, sonnet).
//! Reference: `ruleutils.c` — `get_query_def`, `get_select_query_def`, `get_basic_select_query`,
//! `get_target_list`, `get_from_clause`, `get_from_clause_item`, `get_setop_query`,
//! `get_rule_orderby`, `get_rule_windowclause`, `get_with_clause`, `get_values_def`,
//! `get_name_for_var_field`, `get_variable`.
//!
//! The text is stored in the catalog as `ViewSpec.query` (`" SELECT ...;"` with the exact whitespace
//! of `pg_get_viewdef(oid, false)` and a trailing `;`) and returned verbatim by
//! `pg_get_viewdef`/`pg_views.definition`. **Re-binding** a view later parses this text again
//! (`from.rs` view expansion), so the output must also be valid SQL for our own parser.
//!
//! ## Layout (oracle, PG 18.6, non-pretty)
//! ```text
//!  SELECT t.a,
//!     (t.b || 'x'::text) AS bx,
//!     count(*) OVER () AS n,
//!     u.d
//!    FROM (scr_tmp.t
//!      JOIN scr_tmp.u ON ((t.a = u.a)))
//!   WHERE ((t.a > 1) AND (t.b ~~ 'z%'::text));
//! ```
//! * Line 1 is `" SELECT "` (leading space) followed by the first target; further targets on new lines
//!   indented 4 spaces, separated by `,` at the end of the previous line. `AS alias` is printed only when
//!   the output name differs from the column's own name (`t.a` has none, `(t.b || 'x'::text) AS bx`;
//!   a function result whose default name equals the alias still prints `AS "coalesce"` for quoted
//!   keywords).
//! * `FROM` on its own line indented 3 spaces: `"   FROM "`; further comma-separated items follow on new
//!   lines indented 4 spaces after a trailing comma (`FROM scr_tmp.t,\n    ( VALUES (1), (2)) s(x);`);
//!   explicit joins are wrapped: `(a\n     JOIN b ON (cond))`, `LEFT JOIN`, `RIGHT JOIN`, `FULL JOIN`,
//!   `CROSS JOIN`, `JOIN ... USING (id)`; sub-selects `( SELECT ... ) alias`; function scans `generate_series(1, 3) g(x)`.
//! * `WHERE`: `"  WHERE "` (2 spaces); `GROUP BY` `"  GROUP BY "`; `HAVING` `" HAVING "` (1 space);
//!   `ORDER BY` `"  ORDER BY "`; `OFFSET` `" OFFSET 1"` comes BEFORE `LIMIT` `" LIMIT 3"` (oracle prints `ORDER BY ..\n OFFSET 1\n LIMIT 3;`); `FETCH FIRST n ROWS WITH
//!   TIES`; window clause `WINDOW w AS (...)`; `DISTINCT` / `DISTINCT ON (..)` after SELECT; locking
//!   `FOR UPDATE`.
//! * Set operations: operands as `" SELECT ..."` blocks joined by `"UNION ALL"`/`"UNION"`/`"INTERSECT"`/
//!   `"EXCEPT"` lines; operand sub-queries that need parentheses (ORDER BY/LIMIT inside) are wrapped.
//! * CASE inside a view body spans lines:
//!   ```text
//!      SELECT t.a,
//!             CASE
//!                 WHEN (t.a > 1) THEN 'x'::text
//!                 ELSE 'y'::text
//!             END AS k,
//!   ```
//! * `VALUES (1, 'a'), (2, 'b')` bodies, `WITH x AS (...)` prefix (`WITH x AS (`...`)`), `WITH RECURSIVE`.
//! * Column qualification: variables print as `t.a` when `varprefix` is on — i.e. when the query has
//!   MORE than one range table entry OR it is nested inside another query (set-operation arm, sub-select);
//!   a single-table top-level query prints bare names (`SELECT b,\n    count(*) AS c ...`). Use
//!   `RangeColumns { qualify }` accordingly, maintaining the scope stack while recursing.
//! * Relation names are schema-qualified unless visible in the search_path of the deparsing session
//!   (`public.t` prints as `t` when `public` is in the path); aliases are printed when the alias differs from the relation name (`FROM t x`).
//! * Output aliases: names that are not valid unquoted identifiers are quoted (`AS "coalesce"`).
//!
//!
//! ## More oracle fixtures (PG 18.6, `pg_get_viewdef`, non-pretty)
//! ```text
//!  SELECT t.a,
//!     x.n
//!    FROM (scr_tmp.t
//!      LEFT JOIN ( SELECT u.a AS n
//!            FROM scr_tmp.u) x ON ((x.n = t.a))),
//!     scr_tmp.u uu
//!   WHERE (uu.d = 1)
//!   ORDER BY t.a DESC NULLS LAST
//!  OFFSET 1
//!  LIMIT 3;
//! ```
//! ```text
//!  SELECT t.a
//!    FROM (scr_tmp.t
//!      JOIN scr_tmp.u USING (a))
//!   WHERE ((t.a IN ( SELECT u_1.d
//!            FROM scr_tmp.u u_1)) AND (EXISTS ( SELECT 1
//!            FROM scr_tmp.u uu
//!           WHERE (uu.a = t.a))));
//! ```
//! (a sub-select that repeats a relation name gets PG's generated alias `u_1`; an explicit alias is kept)
//! ```text
//!  WITH q AS (
//!          SELECT 1 AS a
//!         )
//!  SELECT a
//!    FROM q;
//! ```
//! ```text
//!  SELECT s.x,
//!     ( SELECT count(*) AS count
//!            FROM scr_tmp.u) AS c,
//!     ARRAY( SELECT u.d
//!            FROM scr_tmp.u) AS ar,
//!     (ARRAY[1, 2])[1] AS e,
//!     (t.a IS DISTINCT FROM 1) AS d,
//!     (t.a = ANY (ARRAY[1, 2])) AS an,
//!         CASE t.a
//!             WHEN 1 THEN 'x'::text
//!             ELSE NULL::text
//!         END AS cs,
//!     COALESCE(t.b, 'q'::text) AS cb,
//!     NULLIF(t.a, 1) AS nl,
//!     GREATEST(t.a, 2) AS gr,
//!     (- t.a) AS neg,
//!     (t.b ~ similar_to_escape('x%'::text)) AS st,
//!     CURRENT_DATE AS cd,
//!     CURRENT_USER AS cu,
//!     count(*) OVER (PARTITION BY t.b ORDER BY t.a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS w,
//!     (t.c)::text AS ct,
//!     ((t.a > 1) IS TRUE) AS tr,
//!     ARRAY[t.a, 2] AS aa
//!    FROM scr_tmp.t,
//!     ( VALUES (1), (2)) s(x);
//! ```
//! Set operations (qualified columns inside arms):
//! ```text
//!  SELECT t.a,
//!         CASE
//!             WHEN (t.a > 1) THEN 'x'::text
//!             ELSE 'y'::text
//!         END AS k,
//!     COALESCE(t.b, 'q'::text) AS "coalesce"
//!    FROM scr_tmp.t
//!   WHERE ((t.a = ANY (ARRAY[1, 2, 3])) AND (t.c IS NOT NULL))
//! UNION ALL
//!  SELECT u.d AS a,
//!     'z'::text AS k,
//!     NULL::text AS "coalesce"
//!    FROM scr_tmp.u;
//! ```
//! `deparse_viewdef` returns the full text WITH the trailing `;`. `deparse_query_inline` is the form
//! used inside parentheses by sub-links and sub-selects (no leading space, no `;`, single line
//! separators as PG's `get_query_def` with `PRETTYFLAG` off and `startIndent` 0).
use std::collections::{HashMap, HashSet};

use pgarbor_catalog::model::{ObjClass, ObjectAddress as ObjAddr};
use pgarbor_common::{PgError, Result};
use pgarbor_types::text::format::quote_identifier;

use pgarbor_types::TypeId;

use super::expr::deparse_expr;
use super::{ColumnNamer, DeparseCtx, DeparseOptions};
use crate::ir::{
    BoundExpr, BoundQuery, BoundSelect, BoundValues, CaseExpr, ColumnRef, CteBody, DistinctSpec,
    ExprKind, FrameBound, FrameExclusion, FrameUnits, FromItem, JoinItem, JoinKind, LockStrength,
    LockWait, OpRef, OuterRef, QueryBody, RangeEntry, RangeKind, SetOpBody, SetOpKind, WindowDef,
};

/// `PRETTYINDENT_STD` (ruleutils.c): indentation step of the pretty printer.
const PRETTYINDENT_STD: i32 = 8;
/// `PRETTYINDENT_JOIN`: extra indent of a join keyword.
const PRETTYINDENT_JOIN: i32 = 4;
/// `PRETTYINDENT_VAR`: extra indent of a wrapped list item.
const PRETTYINDENT_VAR: i32 = 4;
/// `PRETTYINDENT_LIMIT`: indentation wraps modulo this value once a level is this deep.
const PRETTYINDENT_LIMIT: i32 = 40;
/// First OID of user-created objects; lower OIDs are pinned built-ins that are never recorded as dependencies.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// Names visible at one query level (PG `deparse_namespace`): the unique refname and the column names of
/// every range-table entry. `outer` is the enclosing level, used for correlated (`levels_up > 0`) references.
/// A `transparent` level (a set-operation node, which has no range table of its own) forwards every
/// reference unchanged.
struct ScopeNamer<'a> {
    outer: Option<&'a dyn ColumnNamer>,
    /// Unique refname per range-table entry (index = `rt_index - 1`).
    names: Vec<String>,
    /// Column names per range-table entry (index = `attnum - 1`); dropped columns hold an empty string.
    cols: Vec<Vec<String>>,
    /// Qualify columns with the refname (PG `varprefix`).
    qualify: bool,
    transparent: bool,
}

impl ScopeNamer<'_> {
    /// Unqualified column name of a same-level reference (PG `get_variable` attname).
    fn attname(&self, c: &ColumnRef) -> Option<String> {
        if c.levels_up != 0 || c.attnum < 1 {
            return None;
        }
        let cols = self.cols.get((c.rt_index as usize).checked_sub(1)?)?;
        let name = cols.get(c.attnum as usize - 1)?;
        if name.is_empty() {
            None
        } else {
            Some(name.clone())
        }
    }
}

impl ColumnNamer for ScopeNamer<'_> {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        if self.transparent {
            return self.outer?.column(col);
        }
        if col.levels_up > 0 {
            let up = ColumnRef {
                levels_up: col.levels_up - 1,
                ..*col
            };
            return self.outer?.column(&up);
        }
        let idx = (col.rt_index as usize).checked_sub(1)?;
        let name = self.attname(col)?;
        let quoted = quote_identifier(&name).ok()?;
        if self.qualify {
            let alias = quote_identifier(self.names.get(idx)?).ok()?;
            Some(format!("{}.{}", alias, quoted))
        } else {
            Some(quoted)
        }
    }
    fn input_ref(&self, _index: u32) -> Option<String> {
        None
    }
    fn outer_ref(&self, _outer: &OuterRef) -> Option<String> {
        None
    }
}

/// Unique refnames of a range table (PG `set_rtable_names`): an alias already used by an enclosing level
/// or an earlier entry gets `_1`, `_2`, ... appended.
fn make_names(rt: &[RangeEntry], enclosing: &[String]) -> Vec<String> {
    let mut taken: HashMap<String, u32> = HashMap::new();
    for name in enclosing {
        taken.insert(name.clone(), 0);
    }
    let mut out = Vec::with_capacity(rt.len());
    for entry in rt {
        let base = entry.alias.clone();
        let name = if taken.contains_key(&base) {
            let candidate = loop {
                let counter = taken.entry(base.clone()).or_insert(0);
                *counter += 1;
                let c = format!("{}_{}", base, *counter);
                if !taken.contains_key(&c) {
                    break c;
                }
            };
            taken.insert(candidate.clone(), 0);
            candidate
        } else {
            taken.insert(base.clone(), 0);
            base
        };
        out.push(name);
    }
    out
}

/// Column names of a range-table entry by attnum (dropped columns become empty strings).
fn entry_cols(entry: &RangeEntry) -> Vec<String> {
    entry
        .columns
        .iter()
        .map(|c| {
            if c.dropped {
                String::new()
            } else {
                c.name.clone()
            }
        })
        .collect()
}

/// Plain set operation without ORDER BY / LIMIT / OFFSET / locking / WITH: PG `SetOperationStmt`.
fn plain_setop(q: &BoundQuery) -> Option<&SetOpBody> {
    if !q.ctes.is_empty()
        || !q.order_by.is_empty()
        || q.limit.is_some()
        || q.offset.is_some()
        || !q.locking.is_empty()
    {
        return None;
    }
    match &q.body {
        QueryBody::SetOp(s) => Some(s),
        _ => None,
    }
}

/// `ORDER BY` / `WITHIN GROUP` direction suffix (PG `get_rule_orderby`): the default operators print
/// nothing (ASC) or `DESC` (with `NULLS LAST` unless nulls first); any other operator prints `USING op`.
fn direction_text(descending: bool, nulls_first: bool, using: Option<&OpRef>) -> String {
    let is_lt = match using {
        None => !descending,
        Some(o) => o.name == "<" && !descending,
    };
    let is_gt = match using {
        None => descending,
        Some(o) => o.name == ">" && descending,
    };
    let mut s = String::new();
    if is_lt {
        if nulls_first {
            s.push_str(" NULLS FIRST");
        }
    } else if is_gt {
        s.push_str(" DESC");
        if !nulls_first {
            s.push_str(" NULLS LAST");
        }
    } else {
        let name = match using {
            Some(o) => o.name.as_str(),
            None => "<",
        };
        s.push_str(" USING ");
        s.push_str(name);
        s.push_str(if nulls_first {
            " NULLS FIRST"
        } else {
            " NULLS LAST"
        });
    }
    s
}

/// Printer state handed to a nested `deparse_query_inline` call through [`INLINE_FRAMES`]: `DeparseCtx`
/// cannot carry the layout, the enclosing names and the CTE scopes, so `Printer::expr` publishes them
/// for the duration of one expression deparse.
#[derive(Clone)]
struct InlineFrame {
    pretty: bool,
    level: i32,
    enclosing: Vec<String>,
    cte_stack: Vec<Vec<String>>,
}

thread_local! {
    static INLINE_FRAMES: std::cell::RefCell<Vec<InlineFrame>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Pops the frame pushed by `Printer::expr` when the expression deparse ends, including on error.
struct InlinePop;

impl Drop for InlinePop {
    fn drop(&mut self) {
        INLINE_FRAMES.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

/// Pretty-printer state for one deparse (PG `deparse_context` + the `StringInfo` buffer).
struct Printer<'e> {
    env: &'e pgarbor_catalog::CatalogEnv,
    opts: DeparseOptions,
    /// `PRETTYFLAG_INDENT`: newlines and indentation (view definitions) instead of single-line output.
    pretty: bool,
    /// PG `indentLevel`.
    level: i32,
    buf: String,
    /// CTE names of every enclosing query level, innermost last (`RangeKind::Cte::levels_up`).
    cte_stack: Vec<Vec<String>>,
    /// Enclosing refnames of the current query level (the `enclosing` list of `query_def`).
    cur_enc: Vec<String>,
}

impl<'e> Printer<'e> {
    fn new(
        env: &'e pgarbor_catalog::CatalogEnv,
        opts: DeparseOptions,
        pretty: bool,
    ) -> Printer<'e> {
        Printer {
            env,
            opts,
            pretty,
            level: 0,
            buf: String::new(),
            cte_stack: Vec::new(),
            cur_enc: Vec::new(),
        }
    }

    fn strip_trailing_spaces(&mut self) {
        while self.buf.ends_with(' ') {
            self.buf.pop();
        }
    }

    /// `appendContextKeyword` (ruleutils.c). Single-line mode appends `s` verbatim.
    fn keyword(&mut self, s: &str, before: i32, after: i32, plus: i32) {
        if !self.pretty {
            self.buf.push_str(s);
            return;
        }
        self.level += before;
        self.strip_trailing_spaces();
        self.buf.push('\n');
        let amount = if self.level < PRETTYINDENT_LIMIT {
            self.level.max(0) + plus
        } else {
            let wrapped =
                PRETTYINDENT_LIMIT + (self.level - PRETTYINDENT_LIMIT) / (PRETTYINDENT_STD / 2);
            wrapped % PRETTYINDENT_LIMIT + plus
        };
        for _ in 0..amount.max(0) {
            self.buf.push(' ');
        }
        self.buf.push_str(s);
        self.level += after;
        if self.level < 0 {
            self.level = 0;
        }
    }

    /// Expression text through the expression deparser (SQL-43) with this level's namer.
    fn expr(&self, scope: &dyn ColumnNamer, e: &BoundExpr) -> Result<String> {
        let frame = InlineFrame {
            pretty: self.pretty,
            level: self.level,
            enclosing: self.cur_enc.clone(),
            cte_stack: self.cte_stack.clone(),
        };
        INLINE_FRAMES.with(|f| f.borrow_mut().push(frame));
        let _pop = InlinePop;
        let ctx = DeparseCtx {
            env: self.env,
            catalog: self.env,
            namer: scope,
            opts: self.opts,
        };
        deparse_expr(&ctx, e)
    }

    /// `CASE` in a target list (PG `get_rule_expr`, `T_CaseExpr`, pretty mode): one keyword per line.
    /// Rendered here because the expression deparser has no indent level.
    fn case_text(&mut self, c: &CaseExpr, ty: TypeId, scope: &ScopeNamer<'_>) -> Result<String> {
        let saved_buf = std::mem::take(&mut self.buf);
        let saved_level = self.level;
        let result = self.case_body(c, ty, scope);
        let text = std::mem::replace(&mut self.buf, saved_buf);
        self.level = saved_level;
        result.map(|()| text)
    }

    fn case_body(&mut self, c: &CaseExpr, ty: TypeId, scope: &ScopeNamer<'_>) -> Result<()> {
        self.keyword("CASE", 0, PRETTYINDENT_VAR, 0);
        if let Some(arg) = &c.arg {
            self.buf.push(' ');
            let t = self.expr(scope, arg)?;
            self.buf.push_str(&t);
        }
        for arm in &c.arms {
            // Simple form: the condition is `CaseTestValue = value`; only the value is printed.
            let cond_text = match &arm.cond.kind {
                ExprKind::Op(o)
                    if c.arg.is_some()
                        && o.args.len() == 2
                        && matches!(o.args[0].kind, ExprKind::CaseTestValue) =>
                {
                    self.expr(scope, &o.args[1])?
                }
                _ => self.expr(scope, &arm.cond)?,
            };
            self.keyword("WHEN ", 0, 0, 0);
            self.buf.push_str(&cond_text);
            self.buf.push_str(" THEN ");
            let r = self.expr(scope, &arm.result)?;
            self.buf.push_str(&r);
        }
        self.keyword("ELSE ", 0, 0, 0);
        let default_text = match &c.default {
            Some(d) => self.expr(scope, d)?,
            None => self.expr(scope, &BoundExpr::null(ty))?,
        };
        self.buf.push_str(&default_text);
        self.keyword("END", -PRETTYINDENT_VAR, 0, 0);
        Ok(())
    }

    fn push_alias(&mut self, refname: &str) -> Result<()> {
        self.buf.push(' ');
        let q = quote_identifier(refname)?;
        self.buf.push_str(&q);
        Ok(())
    }

    fn push_col_list(&mut self, names: &[String]) -> Result<()> {
        self.buf.push('(');
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            let q = quote_identifier(n)?;
            self.buf.push_str(&q);
        }
        self.buf.push(')');
        Ok(())
    }

    /// `get_rule_sortgroupclause` for an expression used as a sort, group or partition key: columns print
    /// bare or qualified, function-like calls are parenthesized.
    fn sort_ref_expr(&self, scope: &ScopeNamer<'_>, e: &BoundExpr) -> Result<String> {
        if let ExprKind::Column(c) = &e.kind {
            if let Some(text) = scope.column(c) {
                return Ok(text);
            }
        }
        let text = self.expr(scope, e)?;
        let paren = matches!(
            e.kind,
            ExprKind::FuncCall(_) | ExprKind::Aggref(_) | ExprKind::WindowFunc(_)
        );
        Ok(if paren { format!("({})", text) } else { text })
    }

    /// Key expression of the target-list entry `idx` (sort / group / distinct-on reference).
    fn target_ref_text(
        &self,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        idx: usize,
    ) -> Result<String> {
        let tle = sel
            .target_list
            .get(idx)
            .ok_or_else(|| PgError::internal("query key refers to a missing target-list entry"))?;
        self.sort_ref_expr(scope, &tle.expr)
    }

    /// `get_query_def`: the text of one query block, starting at the current indent level. Restores the
    /// indent level on return (PG keeps it in a per-call context).
    fn query_def(
        &mut self,
        q: &BoundQuery,
        outer: Option<&dyn ColumnNamer>,
        enclosing: &[String],
        nested: bool,
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        let saved_level = self.level;
        let rt: &[RangeEntry] = match &q.body {
            QueryBody::Select(s) => &s.range_table[..],
            _ => &[],
        };
        let transparent = matches!(q.body, QueryBody::SetOp(_));
        let rt_size = match &q.body {
            QueryBody::Select(s) => s.range_table.len(),
            QueryBody::Values(_) => 1,
            QueryBody::SetOp(_) => 2,
        };
        let names = make_names(rt, enclosing);
        let cols: Vec<Vec<String>> = rt.iter().map(entry_cols).collect();
        let scope = ScopeNamer {
            outer,
            names,
            cols,
            qualify: nested || rt_size != 1,
            transparent,
        };
        let mut child_enc: Vec<String> = enclosing.to_vec();
        child_enc.extend(scope.names.iter().cloned());
        self.cte_stack
            .push(q.ctes.iter().map(|c| c.name.clone()).collect());
        let saved_enc = std::mem::replace(&mut self.cur_enc, child_enc.clone());
        let result = self.query_body(q, &scope, &child_enc, result_names, col_names_visible);
        self.cur_enc = saved_enc;
        self.cte_stack.pop();
        self.level = saved_level;
        result
    }

    fn query_body(
        &mut self,
        q: &BoundQuery,
        scope: &ScopeNamer<'_>,
        enc: &[String],
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        let scope_dyn: &dyn ColumnNamer = scope;
        self.with_clause(q, Some(scope_dyn), enc)?;
        match &q.body {
            QueryBody::Select(sel) => {
                self.basic_select(sel, scope, enc, result_names, col_names_visible)?;
                self.tail_clauses(q, Some(sel), scope)
            }
            QueryBody::Values(v) => {
                if self.pretty {
                    self.level += PRETTYINDENT_STD;
                    self.buf.push(' ');
                }
                self.values_rows(v, scope)?;
                self.tail_clauses(q, None, scope)
            }
            QueryBody::SetOp(s) => {
                self.setop_body(s, scope, enc, result_names, col_names_visible)?;
                self.tail_clauses(q, None, scope)
            }
        }
    }

    /// `get_with_clause`.
    fn with_clause(
        &mut self,
        q: &BoundQuery,
        outer: Option<&dyn ColumnNamer>,
        enc: &[String],
    ) -> Result<()> {
        if q.ctes.is_empty() {
            return Ok(());
        }
        if self.pretty {
            self.level += PRETTYINDENT_STD;
            self.buf.push(' ');
        }
        let recursive = q.ctes.iter().any(|c| c.recursive);
        let mut sep = if recursive {
            "WITH RECURSIVE "
        } else {
            "WITH "
        };
        for cte in &q.ctes {
            self.buf.push_str(sep);
            let qname = quote_identifier(&cte.name)?;
            self.buf.push_str(&qname);
            let needs_list = match &cte.body {
                CteBody::Query(bq) => {
                    !cte.columns.is_empty()
                        && (cte.columns.len() != bq.output.len()
                            || cte
                                .columns
                                .iter()
                                .zip(bq.output.iter())
                                .any(|(a, b)| a.name != b.name))
                }
                CteBody::Modify(_) => false,
            };
            if needs_list {
                let names: Vec<String> = cte.columns.iter().map(|c| c.name.clone()).collect();
                self.push_col_list(&names)?;
            }
            self.buf.push_str(" AS ");
            match cte.materialized {
                Some(true) => self.buf.push_str("MATERIALIZED "),
                Some(false) => self.buf.push_str("NOT MATERIALIZED "),
                None => {}
            }
            self.buf.push('(');
            if self.pretty {
                self.keyword("", 0, 0, 0);
            }
            match &cte.body {
                CteBody::Query(bq) => self.query_def(bq, outer, enc, true, None, true)?,
                CteBody::Modify(_) => {
                    return Err(PgError::internal(
                        "data-modifying WITH item cannot appear in a query definition",
                    ));
                }
            }
            if self.pretty {
                self.keyword("", 0, 0, 0);
            }
            self.buf.push(')');
            sep = ", ";
        }
        if self.pretty {
            self.level -= PRETTYINDENT_STD;
            self.keyword("", 0, 0, 0);
        } else {
            self.buf.push(' ');
        }
        Ok(())
    }

    /// `get_basic_select_query`.
    fn basic_select(
        &mut self,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        enc: &[String],
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        if self.pretty {
            self.level += PRETTYINDENT_STD;
            self.buf.push(' ');
        }
        self.buf.push_str("SELECT");
        match &sel.distinct {
            DistinctSpec::None => {}
            DistinctSpec::All => self.buf.push_str(" DISTINCT"),
            DistinctSpec::On(idxs) => {
                self.buf.push_str(" DISTINCT ON (");
                for (i, idx) in idxs.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let t = self.target_ref_text(sel, scope, *idx)?;
                    self.buf.push_str(&t);
                }
                self.buf.push(')');
            }
        }
        self.target_list(sel, scope, result_names, col_names_visible)?;
        self.from_clause(sel, scope, enc)?;
        if let Some(w) = &sel.where_ {
            self.keyword(" WHERE ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 1);
            let t = self.expr(scope, w)?;
            self.buf.push_str(&t);
        }
        self.group_clause(sel, scope)?;
        if let Some(h) = &sel.having {
            self.keyword(" HAVING ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 0);
            let t = self.expr(scope, h)?;
            self.buf.push_str(&t);
        }
        self.window_clause(sel, scope)
    }

    /// Target-list entry text and its unqualified attribute name (PG `get_target_list` helper).
    fn target_text(
        &mut self,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        e: &BoundExpr,
        col_names_visible: bool,
    ) -> Result<(String, Option<String>)> {
        if let ExprKind::Column(c) = &e.kind {
            if let Some(text) = scope.column(c) {
                return Ok((text, scope.attname(c)));
            }
        }
        let non_var_attname = if col_names_visible {
            None
        } else {
            Some("?column?".to_string())
        };
        if let (true, ExprKind::Case(c)) = (self.pretty, &e.kind) {
            let text = self.case_text(c, e.ty, scope)?;
            return Ok((text, non_var_attname));
        }
        let mut text = self.expr(scope, e)?;
        if let ExprKind::WindowFunc(w) = &e.kind {
            if !text.contains(" OVER ") {
                let def = sel.windows.get(w.window).ok_or_else(|| {
                    PgError::internal("window function refers to a missing window definition")
                })?;
                let spec = match &def.name {
                    Some(n) => quote_identifier(n)?,
                    None => self.window_spec(def, scope)?,
                };
                text.push_str(" OVER ");
                text.push_str(&spec);
            }
        }
        Ok((text, non_var_attname))
    }

    /// `get_target_list`: the projection, one entry per line in pretty mode.
    fn target_list(
        &mut self,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        let mut sep = " ";
        let mut colno = 0usize;
        for tle in sel.target_list.iter().filter(|t| !t.junk) {
            colno += 1;
            let (text, attname) = self.target_text(sel, scope, &tle.expr, col_names_visible)?;
            let colname = match result_names.and_then(|n| n.get(colno - 1)) {
                Some(n) => n.clone(),
                None => tle.name.clone(),
            };
            let mut target = text;
            if attname.as_deref() != Some(colname.as_str()) {
                target.push_str(" AS ");
                let q = quote_identifier(&colname)?;
                target.push_str(&q);
            }
            self.buf.push_str(sep);
            sep = ", ";
            if self.pretty && target.starts_with('\n') {
                self.strip_trailing_spaces();
                self.buf.push_str(&target);
            } else if self.pretty && colno > 1 {
                self.keyword("", -PRETTYINDENT_STD, PRETTYINDENT_STD, PRETTYINDENT_VAR);
                self.buf.push_str(&target);
            } else {
                self.buf.push_str(&target);
            }
        }
        Ok(())
    }

    /// `get_from_clause`.
    fn from_clause(
        &mut self,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        enc: &[String],
    ) -> Result<()> {
        if sel.from.is_empty() {
            return Ok(());
        }
        self.keyword(" FROM ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 2);
        for (i, item) in sel.from.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
                self.keyword("", -PRETTYINDENT_STD, PRETTYINDENT_STD, PRETTYINDENT_VAR);
            }
            self.from_item(item, sel, scope, enc)?;
        }
        Ok(())
    }

    /// `get_from_clause_item`.
    fn from_item(
        &mut self,
        item: &FromItem,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        enc: &[String],
    ) -> Result<()> {
        match item {
            FromItem::Range(idx) => self.range_item(*idx, sel, scope, enc),
            FromItem::Join(j) => self.join_item(j, sel, scope, enc),
        }
    }

    fn range_item(
        &mut self,
        idx: u32,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        enc: &[String],
    ) -> Result<()> {
        let i = (idx as usize)
            .checked_sub(1)
            .ok_or_else(|| PgError::internal("FROM item refers to range table entry 0"))?;
        let entry = sel
            .range_table
            .get(i)
            .ok_or_else(|| PgError::internal("FROM item refers to a missing range table entry"))?;
        let refname = scope
            .names
            .get(i)
            .cloned()
            .ok_or_else(|| PgError::internal("FROM item has no range table name"))?;
        if entry.lateral {
            self.buf.push_str("LATERAL ");
        }
        match &entry.kind {
            RangeKind::Relation {
                oid, name, only, ..
            } => {
                if *only {
                    self.buf.push_str("ONLY ");
                }
                let rel = if self.opts.qualify_non_visible {
                    self.env.qualified_class_name(*oid)?
                } else {
                    quote_identifier(name)?
                };
                self.buf.push_str(&rel);
                if &refname != name {
                    self.push_alias(&refname)?;
                }
            }
            RangeKind::Subquery { query } => {
                let outer_dyn: &dyn ColumnNamer = scope;
                self.buf.push('(');
                self.query_def(query, Some(outer_dyn), enc, true, None, true)?;
                self.buf.push(')');
                self.push_alias(&refname)?;
            }
            RangeKind::Function {
                funcs,
                with_ordinality,
            } => {
                if funcs.is_empty() {
                    return Err(PgError::internal("function FROM item without a function"));
                }
                if funcs.len() == 1 {
                    let t = self.expr(scope, &funcs[0].expr)?;
                    self.buf.push_str(&t);
                } else {
                    let all_unnest = funcs.iter().all(
                        |f| matches!(&f.expr.kind, ExprKind::FuncCall(fc) if fc.name == "unnest"),
                    );
                    if all_unnest {
                        self.buf.push_str("UNNEST(");
                        let mut first = true;
                        for f in funcs {
                            if let ExprKind::FuncCall(fc) = &f.expr.kind {
                                for a in &fc.args {
                                    if !first {
                                        self.buf.push_str(", ");
                                    }
                                    first = false;
                                    let t = self.expr(scope, a)?;
                                    self.buf.push_str(&t);
                                }
                            }
                        }
                        self.buf.push(')');
                    } else {
                        self.buf.push_str("ROWS FROM(");
                        for (n, f) in funcs.iter().enumerate() {
                            if n > 0 {
                                self.buf.push_str(", ");
                            }
                            let t = self.expr(scope, &f.expr)?;
                            self.buf.push_str(&t);
                        }
                        self.buf.push(')');
                    }
                }
                if *with_ordinality {
                    self.buf.push_str(" WITH ORDINALITY");
                }
                self.push_alias(&refname)?;
                let fname = match funcs.first().map(|f| &f.expr.kind) {
                    Some(ExprKind::FuncCall(fc)) => fc.name.clone(),
                    _ => String::new(),
                };
                let col_names: Vec<String> = funcs
                    .iter()
                    .flat_map(|f| f.columns.iter().map(|c| c.name.clone()))
                    .collect();
                let need_list = match col_names.len() {
                    0 => false,
                    1 => col_names[0] != refname && col_names[0] != fname,
                    _ => col_names[0] != fname,
                };
                if need_list {
                    self.push_col_list(&col_names)?;
                }
            }
            RangeKind::Values { values } => {
                self.buf.push_str(if self.pretty { "( " } else { "(" });
                self.values_rows(values, scope)?;
                self.buf.push(')');
                self.push_alias(&refname)?;
                let need_list = entry
                    .columns
                    .iter()
                    .enumerate()
                    .any(|(n, c)| c.name != format!("column{}", n + 1));
                if need_list {
                    let names: Vec<String> = entry.columns.iter().map(|c| c.name.clone()).collect();
                    self.push_col_list(&names)?;
                }
            }
            RangeKind::Cte {
                cte_index,
                levels_up,
                ..
            } => {
                let depth = self
                    .cte_stack
                    .len()
                    .checked_sub(1 + *levels_up as usize)
                    .ok_or_else(|| PgError::internal("CTE reference to a missing query level"))?;
                let cte_name = self.cte_stack[depth]
                    .get(*cte_index as usize)
                    .cloned()
                    .ok_or_else(|| PgError::internal("CTE reference to a missing WITH item"))?;
                let q = quote_identifier(&cte_name)?;
                self.buf.push_str(&q);
                if refname != cte_name {
                    self.push_alias(&refname)?;
                }
            }
        }
        Ok(())
    }

    fn join_item(
        &mut self,
        j: &JoinItem,
        sel: &BoundSelect,
        scope: &ScopeNamer<'_>,
        enc: &[String],
    ) -> Result<()> {
        self.buf.push('(');
        self.from_item(&j.left, sel, scope, enc)?;
        let keyword = match (j.kind, j.quals.is_some()) {
            (JoinKind::Inner, true) => " JOIN ",
            (JoinKind::Inner, false) => " CROSS JOIN ",
            (JoinKind::Left, _) => " LEFT JOIN ",
            (JoinKind::Right, _) => " RIGHT JOIN ",
            (JoinKind::Full, _) => " FULL JOIN ",
        };
        self.keyword(
            keyword,
            -PRETTYINDENT_STD,
            PRETTYINDENT_STD,
            PRETTYINDENT_JOIN,
        );
        self.from_item(&j.right, sel, scope, enc)?;
        if let Some(q) = &j.quals {
            self.buf.push_str(" ON ");
            self.buf.push('(');
            let t = self.expr(scope, q)?;
            self.buf.push_str(&t);
            self.buf.push(')');
        } else if j.kind != JoinKind::Inner {
            self.buf.push_str(" ON TRUE");
        }
        self.buf.push(')');
        Ok(())
    }

    /// `VALUES (..), (..)` rows (PG `get_values_def`).
    fn values_rows(&mut self, v: &BoundValues, scope: &ScopeNamer<'_>) -> Result<()> {
        self.buf.push_str("VALUES ");
        for (i, row) in v.rows.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.buf.push('(');
            for (j, cell) in row.iter().enumerate() {
                if j > 0 {
                    self.buf.push(',');
                }
                let t = self.expr(scope, cell)?;
                self.buf.push_str(&t);
            }
            self.buf.push(')');
        }
        Ok(())
    }

    /// `get_setop_query` for a set-operation node (PG `SetOperationStmt`).
    fn setop_body(
        &mut self,
        s: &SetOpBody,
        scope: &ScopeNamer<'_>,
        enc: &[String],
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        let need_paren_left =
            plain_setop(&s.left).map_or(false, |l| l.op != s.op || l.all != s.all);
        let mut subindent = 0;
        if need_paren_left {
            self.buf.push('(');
            subindent = PRETTYINDENT_STD;
            self.keyword("", subindent, 0, 0);
        }
        self.setop_node(&s.left, scope, enc, result_names, col_names_visible)?;
        if need_paren_left {
            self.keyword(") ", -subindent, 0, 0);
        } else if self.pretty {
            self.keyword("", 0, 0, 0);
        } else {
            self.buf.push(' ');
        }
        self.buf.push_str(match s.op {
            SetOpKind::Union => "UNION ",
            SetOpKind::Intersect => "INTERSECT ",
            SetOpKind::Except => "EXCEPT ",
        });
        if s.all {
            self.buf.push_str("ALL ");
        }
        let need_paren_right = plain_setop(&s.right).is_some();
        let mut subindent = 0;
        if need_paren_right {
            self.buf.push('(');
            subindent = PRETTYINDENT_STD;
        }
        self.keyword("", subindent, 0, 0);
        self.setop_node(&s.right, scope, enc, result_names, false)?;
        if self.pretty {
            self.level -= subindent;
        }
        if need_paren_right {
            self.keyword(")", 0, 0, 0);
        }
        Ok(())
    }

    /// One operand of a set operation: a nested plain set operation, or a leaf query (PG RangeTblRef).
    fn setop_node(
        &mut self,
        node: &BoundQuery,
        scope: &ScopeNamer<'_>,
        enc: &[String],
        result_names: Option<&[String]>,
        col_names_visible: bool,
    ) -> Result<()> {
        if let Some(inner) = plain_setop(node) {
            return self.setop_body(inner, scope, enc, result_names, col_names_visible);
        }
        let need_paren = !node.ctes.is_empty()
            || !node.order_by.is_empty()
            || node.limit.is_some()
            || node.offset.is_some()
            || !node.locking.is_empty()
            || matches!(node.body, QueryBody::SetOp(_));
        if need_paren {
            self.buf.push('(');
        }
        let outer_dyn: &dyn ColumnNamer = scope;
        self.query_def(
            node,
            Some(outer_dyn),
            enc,
            true,
            result_names,
            col_names_visible,
        )?;
        if need_paren {
            self.buf.push(')');
        }
        Ok(())
    }

    /// ORDER BY, OFFSET, LIMIT / FETCH and locking clauses after the query body.
    fn tail_clauses(
        &mut self,
        q: &BoundQuery,
        sel: Option<&BoundSelect>,
        scope: &ScopeNamer<'_>,
    ) -> Result<()> {
        if !q.order_by.is_empty() {
            self.keyword(" ORDER BY ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 1);
            for (i, k) in q.order_by.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                let text = match sel {
                    Some(s) => self.target_ref_text(s, scope, k.target_idx)?,
                    None => (k.target_idx + 1).to_string(),
                };
                self.buf.push_str(&text);
                self.buf.push_str(&direction_text(
                    k.descending,
                    k.nulls_first,
                    k.using_op.as_ref(),
                ));
            }
        }
        if let Some(off) = &q.offset {
            self.keyword(" OFFSET ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 0);
            let t = self.expr(scope, off)?;
            self.buf.push_str(&t);
        }
        if let Some(lim) = &q.limit {
            if q.limit_with_ties {
                self.keyword(" FETCH FIRST ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 0);
                let t = self.expr(scope, lim)?;
                self.buf.push('(');
                self.buf.push_str(&t);
                self.buf.push_str(") ROWS WITH TIES");
            } else {
                self.keyword(" LIMIT ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 0);
                if lim.is_null_const() {
                    self.buf.push_str("ALL");
                } else {
                    let t = self.expr(scope, lim)?;
                    self.buf.push_str(&t);
                }
            }
        }
        if let Some(s) = sel {
            for lc in &q.locking {
                let kw = match lc.strength {
                    LockStrength::Update => " FOR UPDATE",
                    LockStrength::NoKeyUpdate => " FOR NO KEY UPDATE",
                    LockStrength::Share => " FOR SHARE",
                    LockStrength::KeyShare => " FOR KEY SHARE",
                };
                let targets: Vec<u32> = if lc.of.is_empty() {
                    let mut all = Vec::new();
                    for (n, e) in s.range_table.iter().enumerate() {
                        if matches!(e.kind, RangeKind::Relation { .. }) {
                            all.push(n as u32 + 1);
                        }
                    }
                    all
                } else {
                    lc.of.clone()
                };
                for idx in targets {
                    self.keyword(kw, -PRETTYINDENT_STD, PRETTYINDENT_STD, 0);
                    let name =
                        scope
                            .names
                            .get((idx as usize).wrapping_sub(1))
                            .ok_or_else(|| {
                                PgError::internal(
                                    "locking clause refers to a missing range table entry",
                                )
                            })?;
                    self.buf.push_str(" OF ");
                    let qn = quote_identifier(name)?;
                    self.buf.push_str(&qn);
                    match lc.wait {
                        LockWait::Block => {}
                        LockWait::NoWait => self.buf.push_str(" NOWAIT"),
                        LockWait::SkipLocked => self.buf.push_str(" SKIP LOCKED"),
                    }
                }
            }
        }
        Ok(())
    }

    /// `GROUP BY` clause (grouping sets are printed as `GROUPING SETS`).
    fn group_clause(&mut self, sel: &BoundSelect, scope: &ScopeNamer<'_>) -> Result<()> {
        let gb = &sel.group_by;
        if gb.exprs.is_empty() && gb.sets.is_none() {
            return Ok(());
        }
        self.keyword(" GROUP BY ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 1);
        match &gb.sets {
            None => {
                for (i, idx) in gb.exprs.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let t = self.target_ref_text(sel, scope, *idx)?;
                    self.buf.push_str(&t);
                }
            }
            Some(sets) => {
                if sets.len() == 1 && sets[0].is_empty() {
                    self.buf.push_str("()");
                } else {
                    self.buf.push_str("GROUPING SETS (");
                    for (i, set) in sets.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.buf.push('(');
                        for (j, idx) in set.iter().enumerate() {
                            if j > 0 {
                                self.buf.push_str(", ");
                            }
                            let key = *gb.exprs.get(*idx).ok_or_else(|| {
                                PgError::internal("grouping set refers to a missing key")
                            })?;
                            let t = self.target_ref_text(sel, scope, key)?;
                            self.buf.push_str(&t);
                        }
                        self.buf.push(')');
                    }
                    self.buf.push(')');
                }
            }
        }
        Ok(())
    }

    /// `WINDOW name AS (...)` clause for the named window definitions.
    fn window_clause(&mut self, sel: &BoundSelect, scope: &ScopeNamer<'_>) -> Result<()> {
        let mut first = true;
        for w in &sel.windows {
            if let Some(name) = &w.name {
                if first {
                    self.keyword(" WINDOW ", -PRETTYINDENT_STD, PRETTYINDENT_STD, 1);
                    first = false;
                } else {
                    self.buf.push_str(", ");
                }
                let qn = quote_identifier(name)?;
                self.buf.push_str(&qn);
                self.buf.push_str(" AS ");
                let spec = self.window_spec(w, scope)?;
                self.buf.push_str(&spec);
            }
        }
        Ok(())
    }

    /// `get_rule_windowspec`: `(PARTITION BY .. ORDER BY .. frame)`.
    fn window_spec(&self, w: &WindowDef, scope: &ScopeNamer<'_>) -> Result<String> {
        let mut out = String::from("(");
        let mut needspace = false;
        if !w.partition_by.is_empty() {
            out.push_str("PARTITION BY ");
            for (i, e) in w.partition_by.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let t = self.sort_ref_expr(scope, e)?;
                out.push_str(&t);
            }
            needspace = true;
        }
        if !w.order_by.is_empty() {
            if needspace {
                out.push(' ');
            }
            out.push_str("ORDER BY ");
            for (i, s) in w.order_by.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let t = self.sort_ref_expr(scope, &s.expr)?;
                out.push_str(&t);
                out.push_str(&direction_text(
                    s.descending,
                    s.nulls_first,
                    s.using_op.as_ref(),
                ));
            }
            needspace = true;
        }
        if !w.frame_is_default {
            if needspace {
                out.push(' ');
            }
            let frame = self.frame_text(w, scope)?;
            out.push_str(&frame);
        }
        out.push(')');
        Ok(out)
    }

    /// `get_window_frame_options` for an explicit frame (always in BETWEEN form).
    fn frame_text(&self, w: &WindowDef, scope: &ScopeNamer<'_>) -> Result<String> {
        let mut s = String::new();
        s.push_str(match w.units {
            FrameUnits::Rows => "ROWS ",
            FrameUnits::Range => "RANGE ",
            FrameUnits::Groups => "GROUPS ",
        });
        s.push_str("BETWEEN ");
        s.push_str(&self.frame_bound(&w.start, scope)?);
        s.push_str("AND ");
        s.push_str(&self.frame_bound(&w.end, scope)?);
        match w.exclusion {
            FrameExclusion::NoOthers => {}
            FrameExclusion::CurrentRow => s.push_str("EXCLUDE CURRENT ROW "),
            FrameExclusion::Group => s.push_str("EXCLUDE GROUP "),
            FrameExclusion::Ties => s.push_str("EXCLUDE TIES "),
        }
        while s.ends_with(' ') {
            s.pop();
        }
        Ok(s)
    }

    fn frame_bound(&self, b: &FrameBound, scope: &ScopeNamer<'_>) -> Result<String> {
        Ok(match b {
            FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING ".to_string(),
            FrameBound::CurrentRow => "CURRENT ROW ".to_string(),
            FrameBound::Preceding(e) => format!("{} PRECEDING ", self.expr(scope, e)?),
            FrameBound::Following(e) => format!("{} FOLLOWING ", self.expr(scope, e)?),
            FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING ".to_string(),
        })
    }
}

/// `pg_get_viewdef(oid)` text of `q` (module docs). `view_columns`: the view's column names (after the
/// `CREATE VIEW v(a,b)` list) used for `AS` aliases.
pub fn deparse_viewdef(
    env: &pgarbor_catalog::CatalogEnv,
    q: &BoundQuery,
    view_columns: &[String],
    opts: DeparseOptions,
) -> Result<String> {
    let mut p = Printer::new(env, opts, true);
    let names = if view_columns.is_empty() {
        None
    } else {
        Some(view_columns)
    };
    p.query_def(q, None, &[], false, names, true)?;
    p.buf.push(';');
    Ok(p.buf)
}

/// Inline form for nested queries (sub-links, derived tables, CTE bodies).
pub fn deparse_query_inline(ctx: &DeparseCtx, q: &BoundQuery) -> Result<String> {
    // Inside an expression deparse started by `Printer::expr`, reuse that printer's layout (sub-links
    // are pretty-printed at the caller's indent, PG `get_sublink_expr`, with `colNamesVisible` false).
    let frame = INLINE_FRAMES.with(|f| f.borrow().last().cloned());
    match frame {
        Some(f) => {
            let mut p = Printer::new(ctx.catalog, ctx.opts, f.pretty);
            p.level = f.level;
            p.cte_stack = f.cte_stack;
            p.query_def(q, Some(ctx.namer), &f.enclosing, true, None, false)?;
            Ok(p.buf)
        }
        None => {
            let mut p = Printer::new(ctx.catalog, ctx.opts, false);
            p.query_def(q, Some(ctx.namer), &[], true, None, true)?;
            Ok(p.buf)
        }
    }
}

/// Accumulates dependency addresses in first-occurrence order without duplicates.
struct DepCollector {
    out: Vec<ObjAddr>,
    seen: HashSet<ObjAddr>,
}

impl DepCollector {
    fn add(&mut self, addr: ObjAddr) {
        if self.seen.insert(addr) {
            self.out.push(addr);
        }
    }
}

/// Names of the relations / columns / functions / types a query depends on, as catalog dependency
/// addresses (for `ViewSpec.query.depends_on`). Walks the range table (`RangeKind::Relation` OIDs,
/// recursing through sub-queries, CTEs and sub-links) and expressions (`stored::collect_dependencies`
/// semantics); columns are collected per range entry from the `Column` nodes. Order: first
/// occurrence, de-duplicated.
pub fn query_dependencies(q: &BoundQuery) -> Result<Vec<pgarbor_catalog::ObjectAddress>> {
    let mut d = DepCollector {
        out: Vec::new(),
        seen: HashSet::new(),
    };
    let mut stack: Vec<&[RangeEntry]> = Vec::new();
    collect_query(q, &mut stack, &mut d)?;
    Ok(d.out)
}

/// Visits a query block: its own range table is visible to the expressions and sub-queries below it.
fn collect_query<'q>(
    q: &'q BoundQuery,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    let pushed = match &q.body {
        QueryBody::Select(s) => {
            stack.push(&s.range_table[..]);
            true
        }
        _ => false,
    };
    let result = collect_query_parts(q, stack, d);
    if pushed {
        stack.pop();
    }
    result
}

fn collect_query_parts<'q>(
    q: &'q BoundQuery,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    match &q.body {
        QueryBody::Select(s) => collect_select(s, stack, d)?,
        QueryBody::Values(v) => collect_values(v, stack, d)?,
        QueryBody::SetOp(s) => {
            collect_query(&s.left, stack, d)?;
            collect_query(&s.right, stack, d)?;
        }
    }
    for cte in &q.ctes {
        match &cte.body {
            CteBody::Query(bq) => collect_query(bq, stack, d)?,
            CteBody::Modify(_) => {
                return Err(PgError::internal(
                    "data-modifying WITH item cannot appear in a query definition",
                ));
            }
        }
    }
    if let Some(l) = &q.limit {
        collect_expr(l, stack, d)?;
    }
    if let Some(o) = &q.offset {
        collect_expr(o, stack, d)?;
    }
    Ok(())
}

fn collect_select<'q>(
    s: &'q BoundSelect,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    for entry in &s.range_table {
        match &entry.kind {
            RangeKind::Relation { oid, .. } => {
                // Pinned (pre-16384) catalog relations are never recorded, as in PG's pg_depend.
                if *oid >= FIRST_NORMAL_OBJECT_ID {
                    d.add(ObjAddr::new(ObjClass::Relation, *oid));
                }
            }
            RangeKind::Subquery { query } => collect_query(query, stack, d)?,
            RangeKind::Function { funcs, .. } => {
                for f in funcs {
                    collect_expr(&f.expr, stack, d)?;
                }
            }
            RangeKind::Values { values } => collect_values(values, stack, d)?,
            RangeKind::Cte { .. } => {}
        }
    }
    for item in &s.from {
        collect_from(item, stack, d)?;
    }
    for t in &s.target_list {
        collect_expr(&t.expr, stack, d)?;
    }
    if let Some(w) = &s.where_ {
        collect_expr(w, stack, d)?;
    }
    if let Some(h) = &s.having {
        collect_expr(h, stack, d)?;
    }
    for w in &s.windows {
        for e in &w.partition_by {
            collect_expr(e, stack, d)?;
        }
        for o in &w.order_by {
            collect_expr(&o.expr, stack, d)?;
        }
        for b in [&w.start, &w.end] {
            if let FrameBound::Preceding(e) | FrameBound::Following(e) = b {
                collect_expr(e, stack, d)?;
            }
        }
    }
    Ok(())
}

fn collect_from<'q>(
    item: &'q FromItem,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    match item {
        FromItem::Range(_) => Ok(()),
        FromItem::Join(j) => {
            collect_from(&j.left, stack, d)?;
            collect_from(&j.right, stack, d)?;
            if let Some(q) = &j.quals {
                collect_expr(q, stack, d)?;
            }
            Ok(())
        }
    }
}

fn collect_values<'q>(
    v: &'q BoundValues,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    for row in &v.rows {
        for cell in row {
            collect_expr(cell, stack, d)?;
        }
    }
    Ok(())
}

/// Dependencies of one expression tree: columns of relations, user functions, aggregates, window
/// functions and operators; sub-links are entered through their sub-query.
fn collect_expr<'q>(
    e: &'q BoundExpr,
    stack: &mut Vec<&'q [RangeEntry]>,
    d: &mut DepCollector,
) -> Result<()> {
    let mut failure: Option<PgError> = None;
    e.walk(&mut |node: &'q BoundExpr| {
        if failure.is_some() {
            return false;
        }
        if node.ty.oid >= FIRST_NORMAL_OBJECT_ID {
            d.add(ObjAddr::new(ObjClass::Type, node.ty.oid));
        }
        match &node.kind {
            ExprKind::Column(c) => {
                if let Err(err) = add_column_dep(c, stack, d) {
                    failure = Some(err);
                    return false;
                }
            }
            ExprKind::FuncCall(f) => {
                if f.func >= FIRST_NORMAL_OBJECT_ID {
                    d.add(ObjAddr::new(ObjClass::Proc, f.func));
                }
            }
            ExprKind::Aggref(a) => {
                if a.agg >= FIRST_NORMAL_OBJECT_ID {
                    d.add(ObjAddr::new(ObjClass::Proc, a.agg));
                }
            }
            ExprKind::WindowFunc(w) => {
                if w.func >= FIRST_NORMAL_OBJECT_ID {
                    d.add(ObjAddr::new(ObjClass::Proc, w.func));
                }
            }
            ExprKind::Op(o) => {
                if o.op.op >= FIRST_NORMAL_OBJECT_ID {
                    d.add(ObjAddr::new(ObjClass::Operator, o.op.op));
                }
            }
            ExprKind::SubLink(sl) => {
                if let Err(err) = collect_query(&sl.subquery, stack, d) {
                    failure = Some(err);
                    return false;
                }
            }
            _ => {}
        }
        true
    });
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Records the relation column behind a `Column` reference (system and whole-row references add nothing;
/// the relation itself is recorded from the range table).
fn add_column_dep(c: &ColumnRef, stack: &[&[RangeEntry]], d: &mut DepCollector) -> Result<()> {
    if c.attnum < 1 {
        return Ok(());
    }
    let level = stack
        .len()
        .checked_sub(1 + c.levels_up as usize)
        .ok_or_else(|| PgError::internal("column reference to a missing query level"))?;
    let idx = (c.rt_index as usize)
        .checked_sub(1)
        .ok_or_else(|| PgError::internal("column reference to range table entry 0"))?;
    if let Some(entry) = stack[level].get(idx) {
        if let RangeKind::Relation { oid, .. } = &entry.kind {
            if *oid >= FIRST_NORMAL_OBJECT_ID {
                d.add(ObjAddr {
                    class: ObjClass::Relation,
                    oid: *oid,
                    sub_id: c.attnum as i32,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deparse::{DeparseCtx, TableColumns};
    use crate::ir::{
        AccessNeeded, BoundCte, FuncCallExpr, OutputColumn, RangeColumn, SortKey, TargetEntry,
    };
    use pgarbor_catalog::model::RelKind;
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::{Datum, TypeId};

    fn rel_entry(oid: u32, name: &str, cols: &[&str]) -> RangeEntry {
        RangeEntry {
            kind: RangeKind::Relation {
                oid,
                schema: "pg_catalog".to_string(),
                name: name.to_string(),
                rel_kind: RelKind::Table,
                only: false,
                pseudo: None,
            },
            alias: name.to_string(),
            columns: cols
                .iter()
                .map(|c| RangeColumn {
                    name: c.to_string(),
                    ty: TypeId::TEXT,
                    dropped: false,
                })
                .collect(),
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }
    }

    fn target(expr: BoundExpr, name: &str) -> TargetEntry {
        TargetEntry {
            expr,
            name: name.to_string(),
            junk: false,
            origin: None,
        }
    }

    fn query_of(sel: BoundSelect, output: &[&str]) -> BoundQuery {
        BoundQuery {
            ctes: Vec::new(),
            body: QueryBody::Select(Box::new(sel)),
            output: output
                .iter()
                .map(|n| OutputColumn {
                    name: n.to_string(),
                    ty: TypeId::TEXT,
                    origin: None,
                })
                .collect(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        }
    }

    #[test]
    fn unique_refnames() {
        let rt = vec![rel_entry(1259, "u", &[]), rel_entry(1259, "u", &[])];
        assert_eq!(
            make_names(&rt, &[]),
            vec!["u".to_string(), "u_1".to_string()]
        );
        assert_eq!(
            make_names(&rt[..1], &["u".to_string()]),
            vec!["u_1".to_string()]
        );
    }

    #[test]
    fn direction_defaults() {
        assert_eq!(direction_text(false, false, None), "");
        assert_eq!(direction_text(true, false, None), " DESC NULLS LAST");
        assert_eq!(direction_text(false, true, None), " NULLS FIRST");
    }

    #[test]
    fn viewdef_single_table_bare_names() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut sel = BoundSelect::empty();
        sel.range_table
            .push(rel_entry(1259, "pg_class", &["relname"]));
        sel.from.push(FromItem::Range(1));
        sel.target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::TEXT), "relname"));
        let q = query_of(sel, &["relname"]);
        let text = deparse_viewdef(
            &env,
            &q,
            &["relname".to_string()],
            DeparseOptions::default(),
        )?;
        assert_eq!(text, " SELECT relname\n   FROM pg_class;");
        Ok(())
    }

    #[test]
    fn inline_query_is_single_line_and_qualified() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut sel = BoundSelect::empty();
        sel.range_table
            .push(rel_entry(1259, "pg_class", &["relname"]));
        sel.from.push(FromItem::Range(1));
        sel.target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::TEXT), "relname"));
        let q = query_of(sel, &["relname"]);
        let namer = TableColumns::default();
        let ctx = DeparseCtx {
            env: &env,
            catalog: &env,
            namer: &namer,
            opts: DeparseOptions::default(),
        };
        assert_eq!(
            deparse_query_inline(&ctx, &q)?,
            "SELECT pg_class.relname FROM pg_class"
        );
        Ok(())
    }

    #[test]
    fn dependencies_relation_columns_and_function_once() -> Result<()> {
        let mut sel = BoundSelect::empty();
        sel.range_table.push(rel_entry(16390, "t", &["a", "b"]));
        sel.from.push(FromItem::Range(1));
        let arg = BoundExpr::column(0, 1, 2, TypeId::TEXT);
        let call = BoundExpr::new(
            ExprKind::FuncCall(Box::new(FuncCallExpr {
                func: 16400,
                name: "f".to_string(),
                args: vec![arg.clone(), arg],
                variadic: false,
                retset: false,
                volatility: Volatility::Immutable,
                strict: false,
                is_cast_call: false,
            })),
            TypeId::TEXT,
        );
        sel.target_list.push(target(call, "f"));
        let q = query_of(sel, &["f"]);
        let deps = query_dependencies(&q)?;
        let rel = ObjAddr::new(ObjClass::Relation, 16390);
        let col = ObjAddr {
            class: ObjClass::Relation,
            oid: 16390,
            sub_id: 2,
        };
        let func = ObjAddr::new(ObjClass::Proc, 16400);
        assert_eq!(deps.iter().filter(|d| **d == func).count(), 1);
        assert!(deps.contains(&rel));
        assert!(deps.contains(&col));
        assert_eq!(deps.len(), 3);
        Ok(())
    }

    #[test]
    fn viewdef_union_all_arms_are_qualified() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut left = BoundSelect::empty();
        left.range_table
            .push(rel_entry(1259, "pg_class", &["relname"]));
        left.from.push(FromItem::Range(1));
        left.target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::TEXT), "relname"));
        let mut right = BoundSelect::empty();
        right
            .range_table
            .push(rel_entry(1247, "pg_type", &["typname"]));
        right.from.push(FromItem::Range(1));
        right
            .target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::TEXT), "relname"));
        let body = QueryBody::SetOp(SetOpBody {
            op: SetOpKind::Union,
            all: true,
            left: Box::new(query_of(left, &["relname"])),
            right: Box::new(query_of(right, &["relname"])),
            types: vec![TypeId::TEXT],
        });
        let q = BoundQuery {
            ctes: Vec::new(),
            body,
            output: vec![OutputColumn {
                name: "relname".to_string(),
                ty: TypeId::TEXT,
                origin: None,
            }],
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        };
        let text = deparse_viewdef(
            &env,
            &q,
            &["relname".to_string()],
            DeparseOptions::default(),
        )?;
        assert_eq!(
            text,
            " SELECT pg_class.relname\n   FROM pg_class\nUNION ALL\n SELECT pg_type.typname AS relname\n   FROM pg_type;"
        );
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-43"]
    fn viewdef_order_offset_limit() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut sel = BoundSelect::empty();
        sel.range_table
            .push(rel_entry(1259, "pg_class", &["relname"]));
        sel.from.push(FromItem::Range(1));
        sel.target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::TEXT), "relname"));
        let mut q = query_of(sel, &["relname"]);
        q.order_by.push(SortKey {
            target_idx: 0,
            descending: true,
            nulls_first: false,
            using_op: None,
        });
        q.offset = Some(BoundExpr::constant(Datum::Int4(1), TypeId::INT4));
        q.limit = Some(BoundExpr::constant(Datum::Int4(3), TypeId::INT4));
        let text = deparse_viewdef(
            &env,
            &q,
            &["relname".to_string()],
            DeparseOptions::default(),
        )?;
        assert_eq!(text, " SELECT relname\n   FROM pg_class\n  ORDER BY relname DESC NULLS LAST\n OFFSET 1\n LIMIT 3;");
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-43"]
    fn viewdef_with_clause_layout() -> Result<()> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let mut body = BoundSelect::empty();
        body.target_list.push(target(
            BoundExpr::constant(Datum::Int4(1), TypeId::INT4),
            "a",
        ));
        let cte_query = query_of(body, &["a"]);
        let cte = BoundCte {
            name: "q".to_string(),
            columns: vec![OutputColumn {
                name: "a".to_string(),
                ty: TypeId::INT4,
                origin: None,
            }],
            body: CteBody::Query(Box::new(cte_query)),
            recursive: false,
            materialized: None,
            ref_count: 1,
        };
        let mut sel = BoundSelect::empty();
        sel.range_table.push(RangeEntry {
            kind: RangeKind::Cte {
                cte_index: 0,
                levels_up: 0,
                recursive_self: false,
            },
            alias: "q".to_string(),
            columns: vec![RangeColumn {
                name: "a".to_string(),
                ty: TypeId::INT4,
                dropped: false,
            }],
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        });
        sel.from.push(FromItem::Range(1));
        sel.target_list
            .push(target(BoundExpr::column(0, 1, 1, TypeId::INT4), "a"));
        let mut q = query_of(sel, &["a"]);
        q.ctes.push(cte);
        let text = deparse_viewdef(&env, &q, &["a".to_string()], DeparseOptions::default())?;
        assert_eq!(
            text,
            " WITH q AS (\n         SELECT 1 AS a\n        )\n SELECT a\n   FROM q;"
        );
        Ok(())
    }
}
