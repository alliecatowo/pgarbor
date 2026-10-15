//! WITH clauses: non-recursive, recursive, data-modifying CTEs (SQL-46).
//! PG references: `parse_cte.c` (`transformWithClause`, `analyzeCTE`, `checkWellFormedRecursion`),
//! `parse_clause.c` (`transformRangeTableRef` for CTE references), `analyze.c`.
//!
//! ## Visibility rules
//! * Names are visible to later CTEs of the same `WITH` and to the main query; with `WITH RECURSIVE`
//!   a CTE is visible to itself and (in order of appearance after dependency sorting) to all
//!   siblings; without RECURSIVE it is not visible to itself (a self reference then resolves to a
//!   table of that name, or fails with 42P01).
//! * Duplicate names in one WITH: 42712 `WITH query name "x" specified more than once`.
//! * Inner WITH shadows outer ones; CTE names shadow tables in FROM (`schema.x` is never a CTE).
//! * `CteFrame::entries` is pushed on `Binder::cte_frames` before binding the bodies so references
//!   resolve through [`find_cte`], which returns `RangeKind::Cte { cte_index, levels_up, .. }` data.
//!
//! ## Recursive CTEs
//! Shape required: `non_recursive_term UNION [ALL] recursive_term`. Errors (all 42P19 except where
//! noted): `recursive query "x" does not have the form non-recursive-term UNION [ALL] recursive-term`;
//! self reference in the non-recursive term -> `recursive reference to query "x" must not appear
//! within its non-recursive term` (42P19); in a subquery/outer join right side/EXCEPT/aggregate ->
//! the matching `recursive reference to query "x" must not appear within ...` messages
//! (`checkWellFormedRecursion`); more than one self reference -> `recursive reference to query "x"
//! must not appear more than once`; column types of the two terms must match after coercion
//! (42804 `recursive query "x" column 1 has type integer in non-recursive term but type numeric
//! overall` + HINT `Cast the output of the non-recursive term to the correct type.`).
//! The bound CTE has `recursive = true`; its body is `SetOp(Union)` with the non-recursive term
//! first. The self reference binds as `RangeKind::Cte { recursive_self: true }` with the columns
//! of the non-recursive term.
//!
//! ## Data-modifying CTEs
//! `WITH t AS (INSERT/UPDATE/DELETE ... RETURNING ...)` -> `CteBody::Modify(BoundStatement)`;
//! allowed only at the top level of the statement (inner WITH: 0A000 `WITH clause containing a
//! data-modifying statement must be at the top level`); without RETURNING the CTE has no columns
//! and cannot be referenced (`WITH query "x" does not have a RETURNING clause`, 0A000 as in PG's
//! `parse_relation.c`). `MATERIALIZED` / `NOT MATERIALIZED` hints are recorded; `SEARCH`/`CYCLE`
//! clauses are rejected by the parser layer (sqlparser 0.63 has no such fields on `Cte`).
//!
//! ## Implementation notes
//! * The binder works on sqlparser's AST without the `visitor` feature, so there is no generic
//!   AST walk. The checks run during binding, with state kept on the `CteEntry` while its body is
//!   analyzed (`in_progress`, `recursive`, `columns`, `ref_count`, `index == PENDING`):
//!   - non-recursive term: a reference while `in_progress && !recursive` -> 42P19 (`find_cte`);
//!   - the top level of a RECURSIVE item must be `UNION`, else a reference is the 42P19 form error;
//!   - a second reference -> `must not appear more than once` (`find_cte`);
//!   - the context of the one remaining reference is checked on the bound recursive term
//!     ([`check_rt_context`]): sublinks and nested WITH bodies are "within a subquery", derived
//!     tables keep their context, INTERSECT/EXCEPT operands and the nullable side of an outer join
//!     have their own messages, and aggregates in a level that reads the reference are 42P19.
//!     The position comes from the reference recorded by `find_cte`.
//! * RECURSIVE siblings: a reference to a sibling that is not analyzed yet makes `find_cte` report
//!   the dependency through the scratch key `cte.need`; `bind_with` then moves that sibling in front
//!   of the current item and binds again (a cycle is 0A000 `mutual recursion between WITH items is
//!   not implemented`). Bindings made by a failed attempt are rolled back with [`Mark`].
//! * `CteRef::levels_up` counts WITH frames (WITH-owning query levels), not query scopes.
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{Cte, CteAsMaterialized, LimitClause, Query, SetExpr, SetOperator, SetQuantifier, Statement, With};

use super::context::Binder;
use super::dispatch;
use super::select;
use super::setop;
use crate::diag;
use crate::ir::{
    BoundCte, BoundExpr, BoundQuery, BoundSelect, BoundStatement, CteBody, ExprKind, FromItem, JoinKind, OutputColumn,
    QueryBody, RangeEntry, RangeKind, SetOpKind, TargetEntry,
};

/// One visible CTE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteEntry {
    pub name: String,
    /// Output columns once analyzed (after the column alias list); empty while the body is being
    /// analyzed for a not-yet-typed recursive reference (then `recursive_columns` is used).
    pub columns: Vec<OutputColumn>,
    /// Index in the owning query's `BoundQuery::ctes` / `BoundInsert::ctes`.
    pub index: u32,
    pub recursive: bool,
    /// Currently inside the body of this CTE (self reference allowed iff `recursive`).
    pub in_progress: bool,
    /// Data-modifying CTE without RETURNING: cannot be referenced.
    pub no_returning: bool,
    /// References seen so far (incremented by `find_cte`); copied into `BoundCte::ref_count`.
    pub ref_count: u32,
}

/// The CTEs of one WITH clause (one frame per `bind_with` call; innermost last on the Binder).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CteFrame {
    pub entries: Vec<CteEntry>,
}

/// Result of resolving a name against the CTE frames.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CteRef {
    pub cte_index: u32,
    /// How many WITH-owning query levels out the CTE lives (0 = the query being bound or its own WITH).
    pub levels_up: u32,
    pub recursive_self: bool,
    pub columns: Vec<OutputColumn>,
}

/// `CteEntry::index` of a RECURSIVE WITH item that has not been analyzed yet.
const PENDING: u32 = u32::MAX;
/// Scratch keys (per statement). Values are decimal numbers, or `-` for "none".
const KEY_DEF_POS: &str = "cte.def_pos";
const KEY_SELFREF_POS: &str = "cte.selfref_pos";
/// `"<frame>:<entry>"`: a sibling that the analysis needs first (set by `find_cte`).
const KEY_NEED: &str = "cte.need";

/// Context of a self reference inside a recursive term (PG `RecursionContext`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecCtx {
    Plain,
    NonRecursiveTerm,
    Subquery,
    OuterJoin,
    Intersect,
    Except,
}

fn recursion_message(ctx: RecCtx, name: &str) -> String {
    let tail = match ctx {
        RecCtx::Plain => "must not appear more than once",
        RecCtx::NonRecursiveTerm => "must not appear within its non-recursive term",
        RecCtx::Subquery => "must not appear within a subquery",
        RecCtx::OuterJoin => "must not appear within an outer join",
        RecCtx::Intersect => "must not appear within INTERSECT",
        RecCtx::Except => "must not appear within EXCEPT",
    };
    format!("recursive reference to query \"{name}\" {tail}")
}

/// Outcome of one attempt to analyze every item of a WITH clause.
enum Attempt {
    Fail(PgError),
    Need { current: usize, needed: usize },
}

/// Binder state to roll back when an attempt is abandoned.
struct Mark {
    scopes: usize,
    depth: u32,
    agg_depth: u32,
    frames: usize,
    notices: usize,
    deps: usize,
    expanding: usize,
}

fn take_mark(b: &Binder) -> Mark {
    Mark {
        scopes: b.scopes.len(),
        depth: b.depth,
        agg_depth: b.agg_depth,
        frames: b.cte_frames.len(),
        notices: b.notices.len(),
        deps: b.deps.len(),
        expanding: b.expanding_views.len(),
    }
}

fn rewind(b: &mut Binder, m: &Mark) {
    b.scopes.truncate(m.scopes);
    b.depth = m.depth;
    b.agg_depth = m.agg_depth;
    b.cte_frames.truncate(m.frames);
    b.notices.truncate(m.notices);
    b.deps.truncate(m.deps);
    b.expanding_views.truncate(m.expanding);
}

fn err_at(state: SqlState, msg: String, pos: Option<u32>) -> PgError {
    let e = PgError::new(state, msg);
    match pos {
        Some(p) => e.with_position(p),
        None => e,
    }
}

/// `"<what> is not implemented"` (0A000) at `pos`.
fn not_impl(what: &str, pos: Option<u32>) -> PgError {
    err_at(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not implemented"), pos)
}

fn scratch_get(b: &Binder, key: &'static str) -> Option<String> {
    b.scratch.get(key).cloned()
}

fn scratch_restore(b: &mut Binder, key: &'static str, saved: Option<String>) {
    match saved {
        Some(v) => {
            b.scratch.insert(key, v);
        }
        None => {
            b.scratch.remove(key);
        }
    }
}

fn scratch_set_num(b: &mut Binder, key: &'static str, n: Option<usize>) {
    let v = match n {
        Some(x) => x.to_string(),
        None => String::from("-"),
    };
    b.scratch.insert(key, v);
}

fn scratch_num(b: &Binder, key: &'static str) -> Option<usize> {
    b.scratch.get(key).and_then(|s| s.parse::<usize>().ok())
}

/// Entries of the innermost WITH frame.
fn top_entries<'b>(b: &'b mut Binder<'_>) -> Result<&'b mut Vec<CteEntry>> {
    match b.cte_frames.last_mut() {
        Some(f) => Ok(&mut f.entries),
        None => Err(PgError::internal("cte: no WITH frame is open")),
    }
}

/// Store the final entry of item `i` (non-recursive frames append in written order).
fn publish(b: &mut Binder, i: usize, entry: CteEntry) -> Result<()> {
    let entries = top_entries(b)?;
    if i < entries.len() {
        entries[i] = entry;
        return Ok(());
    }
    if i == entries.len() {
        entries.push(entry);
        return Ok(());
    }
    Err(PgError::internal("cte: WITH item published out of order"))
}

/// Mark item `i` of the innermost frame as being analyzed (or not), and set its columns.
fn set_state(b: &mut Binder, i: usize, in_progress: bool, recursive: bool, columns: Vec<OutputColumn>) -> Result<()> {
    let entries = top_entries(b)?;
    match entries.get_mut(i) {
        Some(e) => {
            e.in_progress = in_progress;
            e.recursive = recursive;
            e.columns = columns;
            e.ref_count = 0;
            Ok(())
        }
        None => Err(PgError::internal("cte: WITH item has no frame entry")),
    }
}

fn self_ref_count(b: &mut Binder, i: usize) -> Result<u32> {
    let entries = top_entries(b)?;
    match entries.get(i) {
        Some(e) => Ok(e.ref_count),
        None => Err(PgError::internal("cte: WITH item has no frame entry")),
    }
}

fn pending_entry(name: &str) -> CteEntry {
    CteEntry {
        name: name.to_string(),
        columns: Vec::new(),
        index: PENDING,
        recursive: false,
        in_progress: false,
        no_returning: false,
        ref_count: 0,
    }
}

/// Folded names of all items; duplicates are 42712 at the later item (PG `transformWithClause`).
fn bind_names(b: &mut Binder, with: &With) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(with.cte_tables.len());
    for cte in &with.cte_tables {
        names.push(b.ident(&cte.alias.name));
    }
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            if names[i] == names[j] {
                let pos = b.pos_span(with.cte_tables[j].alias.name.span);
                let msg = format!("WITH query name \"{}\" specified more than once", names[j]);
                return Err(err_at(SqlState::DUPLICATE_ALIAS, msg, pos));
            }
        }
    }
    Ok(names)
}

/// Column alias list of an item (`WITH x(a, b) AS ...`).
fn column_aliases(b: &mut Binder, cte: &Cte) -> Vec<String> {
    let mut out = Vec::with_capacity(cte.alias.columns.len());
    for c in &cte.alias.columns {
        out.push(b.ident(&c.name));
    }
    out
}

/// Apply the alias list to the output names. More aliases than columns: 42P10 (PG
/// `checkWellFormedRecursion` / `analyzeCTE`).
fn apply_aliases(cols: &mut [OutputColumn], aliases: &[String], name: &str, pos: Option<u32>) -> Result<()> {
    if aliases.len() > cols.len() {
        let msg = format!(
            "WITH query \"{}\" has {} columns available but {} columns specified",
            name,
            cols.len(),
            aliases.len()
        );
        return Err(err_at(SqlState::INVALID_COLUMN_REFERENCE, msg, pos));
    }
    for (col, alias) in cols.iter_mut().zip(aliases.iter()) {
        col.name = alias.clone();
    }
    Ok(())
}

fn output_columns(targets: &[TargetEntry]) -> Vec<OutputColumn> {
    targets
        .iter()
        .filter(|t| !t.junk)
        .map(|t| OutputColumn { name: t.name.clone(), ty: t.expr.ty, origin: t.origin })
        .collect()
}

/// A copy of the item's query whose body is `body` (the WITH clause is kept: both terms see it).
/// Query-level ORDER BY / LIMIT / OFFSET / FETCH / locks are not part of either term.
fn sub_query(template: &Query, body: &SetExpr) -> Query {
    let mut q = template.clone();
    q.body = Box::new(body.clone());
    q.order_by = None;
    q.limit_clause = None;
    q.fetch = None;
    q.locks = Vec::new();
    q.for_clause = None;
    q.settings = None;
    q.format_clause = None;
    q.pipe_operators = Vec::new();
    q
}

/// PG `checkWellFormedRecursion`: clauses of a recursive query that are not implemented (0A000).
fn check_recursive_clauses(q: &Query, pos: Option<u32>) -> Result<()> {
    if q.order_by.is_some() {
        return Err(not_impl("ORDER BY in a recursive query", pos));
    }
    let (has_offset, has_limit) = match &q.limit_clause {
        Some(LimitClause::LimitOffset { limit, offset, .. }) => (offset.is_some(), limit.is_some()),
        Some(_) => (true, true),
        None => (false, false),
    };
    if has_offset {
        return Err(not_impl("OFFSET in a recursive query", pos));
    }
    if has_limit || q.fetch.is_some() {
        return Err(not_impl("LIMIT in a recursive query", pos));
    }
    if !q.locks.is_empty() {
        return Err(not_impl("FOR UPDATE/SHARE in a recursive query", pos));
    }
    Ok(())
}

/// Bind a `WITH` clause: pushes a `CteFrame` on `b.cte_frames` (the caller pops it with
/// [`pop_with`] after the main body is bound), analyzes every CTE in dependency order for
/// RECURSIVE, and returns the bound CTEs in written order. `top_level` is true when the owning
/// statement is the outermost one (data-modifying CTEs allowed). Uses `select::bind_query` for
/// query bodies and `dispatch::bind_inner_statement` for DML bodies.
pub fn bind_with(b: &mut Binder, with: &With, top_level: bool) -> Result<Vec<BoundCte>> {
    let names = bind_names(b, with)?;
    let n = with.cte_tables.len();
    let mut order: Vec<usize> = (0..n).collect();
    let max_restarts = n.saturating_mul(n).saturating_add(1);
    let mut restarts = 0usize;
    loop {
        let mark = take_mark(b);
        b.cte_frames.push(CteFrame::default());
        let frame = b.cte_frames.len() - 1;
        if with.recursive {
            if let Some(f) = b.cte_frames.last_mut() {
                for name in &names {
                    f.entries.push(pending_entry(name));
                }
            }
        }
        match analyze_all(b, with, top_level, &names, &order, frame) {
            Ok(bound) => return Ok(bound),
            Err(Attempt::Fail(e)) => {
                rewind(b, &mark);
                return Err(e);
            }
            Err(Attempt::Need { current, needed }) => {
                rewind(b, &mark);
                restarts += 1;
                if restarts > max_restarts {
                    let pos = b.pos_span(with.cte_tables[current].alias.name.span);
                    return Err(not_impl("mutual recursion between WITH items", pos));
                }
                if !move_before(&mut order, needed, current) {
                    return Err(PgError::internal("cte: cannot order WITH items"));
                }
            }
        }
    }
}

/// Move `item` (currently after `before`) in front of `before`.
fn move_before(order: &mut Vec<usize>, item: usize, before: usize) -> bool {
    let from = match order.iter().position(|&x| x == item) {
        Some(p) => p,
        None => return false,
    };
    let to = match order.iter().position(|&x| x == before) {
        Some(p) => p,
        None => return false,
    };
    if from <= to {
        return false;
    }
    let v = order.remove(from);
    order.insert(to, v);
    true
}

/// Consume the pending-sibling request if it is for the frame at index `frame`.
fn take_need(b: &mut Binder, frame: usize) -> Option<usize> {
    let raw = b.scratch.get(KEY_NEED)?.clone();
    let (f, e) = raw.split_once(':')?;
    if f.parse::<usize>().ok()? != frame {
        return None;
    }
    let needed = e.parse::<usize>().ok()?;
    b.scratch.remove(KEY_NEED);
    Some(needed)
}

fn analyze_all(
    b: &mut Binder,
    with: &With,
    top_level: bool,
    names: &[String],
    order: &[usize],
    frame: usize,
) -> std::result::Result<Vec<BoundCte>, Attempt> {
    let n = with.cte_tables.len();
    let mut slots: Vec<Option<BoundCte>> = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(None);
    }
    for &i in order {
        match analyze_one(b, with, top_level, names, i) {
            Ok(bc) => {
                if let Some(slot) = slots.get_mut(i) {
                    *slot = Some(bc);
                }
            }
            Err(e) => {
                return Err(match take_need(b, frame) {
                    Some(needed) => Attempt::Need { current: i, needed },
                    None => Attempt::Fail(e),
                });
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    for slot in slots {
        match slot {
            Some(bc) => out.push(bc),
            None => return Err(Attempt::Fail(PgError::internal("cte: WITH item was not analyzed"))),
        }
    }
    Ok(out)
}

fn analyze_one(b: &mut Binder, with: &With, top_level: bool, names: &[String], i: usize) -> Result<BoundCte> {
    let cte = &with.cte_tables[i];
    let name = names[i].clone();
    let def_pos = b.pos_span(cte.alias.name.span);
    let materialized = cte.materialized.as_ref().map(|m| matches!(m, CteAsMaterialized::Materialized));
    let recursive_with = with.recursive;
    match cte.query.body.as_ref() {
        SetExpr::Insert(st) | SetExpr::Update(st) | SetExpr::Delete(st) => {
            analyze_dml(b, cte, top_level, recursive_with, i, &name, def_pos, materialized, st)
        }
        _ => analyze_query(b, cte, recursive_with, i, &name, def_pos, materialized),
    }
}

/// Data-modifying item (PG `analyzeCTE`, `DeleteStmt`/`InsertStmt`/`UpdateStmt` bodies).
#[allow(clippy::too_many_arguments)]
fn analyze_dml(
    b: &mut Binder,
    cte: &Cte,
    top_level: bool,
    recursive_with: bool,
    i: usize,
    name: &str,
    def_pos: Option<u32>,
    materialized: Option<bool>,
    st: &Statement,
) -> Result<BoundCte> {
    if !top_level {
        let msg = "WITH clause containing a data-modifying statement must be at the top level".to_string();
        return Err(err_at(SqlState::FEATURE_NOT_SUPPORTED, msg, def_pos));
    }
    let aliases = column_aliases(b, cte);
    if recursive_with {
        // A data-modifying item cannot reference itself; siblings it names are still ordered first.
        set_state(b, i, true, false, Vec::new())?;
    }
    let bound = dispatch::bind_inner_statement(b, st, false)?;
    let mut columns = match &bound {
        BoundStatement::Insert(x) => output_columns(&x.returning),
        BoundStatement::Update(x) => output_columns(&x.returning),
        BoundStatement::Delete(x) => output_columns(&x.returning),
        _ => return Err(PgError::internal("cte: data-modifying WITH item did not bind to INSERT/UPDATE/DELETE")),
    };
    let no_returning = columns.is_empty();
    apply_aliases(&mut columns, &aliases, name, def_pos)?;
    let entry = CteEntry {
        name: name.to_string(),
        columns: columns.clone(),
        index: i as u32,
        recursive: false,
        in_progress: false,
        no_returning,
        ref_count: 0,
    };
    publish(b, i, entry)?;
    Ok(BoundCte {
        name: name.to_string(),
        columns,
        body: CteBody::Modify(Box::new(bound)),
        recursive: false,
        materialized,
        ref_count: 0,
    })
}

/// Publish a query item with its bound body.
fn finish_query(b: &mut Binder, i: usize, name: &str, bq: BoundQuery, recursive: bool, materialized: Option<bool>) -> Result<BoundCte> {
    let columns = bq.output.clone();
    let entry = CteEntry {
        name: name.to_string(),
        columns: columns.clone(),
        index: i as u32,
        recursive,
        in_progress: false,
        no_returning: false,
        ref_count: 0,
    };
    publish(b, i, entry)?;
    Ok(BoundCte {
        name: name.to_string(),
        columns,
        body: CteBody::Query(Box::new(bq)),
        recursive,
        materialized,
        ref_count: 0,
    })
}

/// Query-bodied item (PG `analyzeCTE`, `SelectStmt` bodies).
fn analyze_query(
    b: &mut Binder,
    cte: &Cte,
    recursive_with: bool,
    i: usize,
    name: &str,
    def_pos: Option<u32>,
    materialized: Option<bool>,
) -> Result<BoundCte> {
    let aliases = column_aliases(b, cte);
    if !recursive_with {
        let mut bq = select::bind_subselect(b, &cte.query, &[])?;
        apply_aliases(&mut bq.output, &aliases, name, def_pos)?;
        return finish_query(b, i, name, bq, false, materialized);
    }
    match cte.query.body.as_ref() {
        SetExpr::SetOperation { op: SetOperator::Union, set_quantifier, left, right } => analyze_recursive_union(
            b,
            cte,
            i,
            name,
            def_pos,
            materialized,
            &aliases,
            set_quantifier,
            left,
            right,
        ),
        _ => analyze_form(b, cte, i, name, def_pos, materialized, &aliases),
    }
}

/// RECURSIVE item whose top level is not `UNION`: any self reference is the 42P19 form error.
fn analyze_form(
    b: &mut Binder,
    cte: &Cte,
    i: usize,
    name: &str,
    def_pos: Option<u32>,
    materialized: Option<bool>,
    aliases: &[String],
) -> Result<BoundCte> {
    let saved = scratch_get(b, KEY_DEF_POS);
    scratch_set_num(b, KEY_DEF_POS, def_pos.map(|p| p as usize));
    set_state(b, i, true, true, Vec::new())?;
    let mut bq = select::bind_subselect(b, &cte.query, &[])?;
    scratch_restore(b, KEY_DEF_POS, saved);
    apply_aliases(&mut bq.output, aliases, name, def_pos)?;
    finish_query(b, i, name, bq, false, materialized)
}

/// RECURSIVE `non_recursive UNION [ALL] recursive` item (PG `checkWellFormedRecursion` +
/// `analyzeCTE` + `determineRecursiveColTypes`). An item whose recursive term never references it
/// is analyzed as an ordinary UNION.
#[allow(clippy::too_many_arguments)]
fn analyze_recursive_union(
    b: &mut Binder,
    cte: &Cte,
    i: usize,
    name: &str,
    def_pos: Option<u32>,
    materialized: Option<bool>,
    aliases: &[String],
    quantifier: &SetQuantifier,
    left: &SetExpr,
    right: &SetExpr,
) -> Result<BoundCte> {
    let all = match quantifier {
        SetQuantifier::All => true,
        SetQuantifier::None | SetQuantifier::Distinct => false,
        _ => return Err(not_impl("UNION BY NAME", def_pos)),
    };
    // Notices raised by both terms are dropped again if the item turns out not to be recursive.
    let notices_mark = b.notices.len();
    // Non-recursive term: a reference to the item here is the 42P19 error from `find_cte`.
    let lq = sub_query(&cte.query, left);
    set_state(b, i, true, false, Vec::new())?;
    let mut nr = select::bind_subselect(b, &lq, &[])?;
    apply_aliases(&mut nr.output, aliases, name, def_pos)?;

    // Recursive term: the self reference takes the columns of the non-recursive term.
    let saved = (scratch_get(b, KEY_DEF_POS), scratch_get(b, KEY_SELFREF_POS));
    scratch_set_num(b, KEY_DEF_POS, def_pos.map(|p| p as usize));
    scratch_set_num(b, KEY_SELFREF_POS, None);
    set_state(b, i, true, true, nr.output.clone())?;
    let rq = sub_query(&cte.query, right);
    let rt = select::bind_subselect(b, &rq, &[])?;
    let selfrefs = self_ref_count(b, i)?;
    let self_pos = scratch_num(b, KEY_SELFREF_POS).map(|p| p as u32);
    scratch_restore(b, KEY_DEF_POS, saved.0);
    scratch_restore(b, KEY_SELFREF_POS, saved.1);

    if selfrefs == 0 {
        // Not self-referencing after all: bind the whole body as an ordinary query.
        b.notices.truncate(notices_mark);
        set_state(b, i, true, false, Vec::new())?;
        let mut bq = select::bind_subselect(b, &cte.query, &[])?;
        apply_aliases(&mut bq.output, aliases, name, def_pos)?;
        return finish_query(b, i, name, bq, false, materialized);
    }

    check_recursive_clauses(&cte.query, def_pos)?;
    check_rt_context(&rt, name, self_pos)?;
    let bq = setop::bind_recursive_union(b, name, nr, rt, all)?;
    finish_query(b, i, name, bq, true, materialized)
}

/// Post-bind check of the bound recursive term for the contexts that are not visible while
/// binding (PG `checkWellFormedRecursionWalker`): INTERSECT / EXCEPT operands, the nullable side of
/// an outer join, sublinks and nested WITH bodies (subquery), and aggregates in a level that reads
/// the self reference. Derived tables keep the context of their FROM item, as in PG.
fn check_rt_context(rt: &BoundQuery, name: &str, pos: Option<u32>) -> Result<()> {
    walk_query(rt, RecCtx::Plain, name, pos)
}

fn walk_query(q: &BoundQuery, ctx: RecCtx, name: &str, pos: Option<u32>) -> Result<()> {
    // Nested WITH: each CTE body is walked as a subquery (PG sets SUBLINK for the WITH's items).
    for cte in &q.ctes {
        if let CteBody::Query(bq) = &cte.body {
            walk_query(bq, RecCtx::Subquery, name, pos)?;
        }
    }
    match &q.body {
        QueryBody::SetOp(s) => {
            let (lctx, rctx) = match s.op {
                SetOpKind::Except => (if s.all { RecCtx::Except } else { ctx }, RecCtx::Except),
                SetOpKind::Intersect => {
                    let c = if s.all { RecCtx::Intersect } else { ctx };
                    (c, c)
                }
                SetOpKind::Union => (ctx, ctx),
            };
            walk_query(&s.left, lctx, name, pos)?;
            walk_query(&s.right, rctx, name, pos)
        }
        QueryBody::Select(sel) => walk_select(sel, ctx, name, pos),
        QueryBody::Values(_) => Ok(()),
    }
}

fn walk_select(sel: &BoundSelect, ctx: RecCtx, name: &str, pos: Option<u32>) -> Result<()> {
    let reads_self = sel.range_table.iter().any(|r| matches!(r.kind, RangeKind::Cte { recursive_self: true, .. }));
    if sel.has_aggs && reads_self {
        let msg = "aggregate functions are not allowed in a recursive query's recursive term".to_string();
        return Err(err_at(SqlState::INVALID_RECURSION, msg, None));
    }
    for item in &sel.from {
        walk_from(item, ctx, &sel.range_table, name, pos)?;
    }
    // Sublinks in the expressions of this level are subqueries (PG `SubLink` case).
    let mut subs: Vec<&BoundQuery> = Vec::new();
    for t in &sel.target_list {
        collect_sublinks(&t.expr, &mut subs);
    }
    if let Some(w) = &sel.where_ {
        collect_sublinks(w, &mut subs);
    }
    if let Some(h) = &sel.having {
        collect_sublinks(h, &mut subs);
    }
    for item in &sel.from {
        collect_join_sublinks(item, &mut subs);
    }
    for sub in subs {
        walk_query(sub, RecCtx::Subquery, name, pos)?;
    }
    Ok(())
}

/// Subqueries of the sublink expressions inside one bound expression.
fn collect_sublinks<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundQuery>) {
    e.walk(&mut |node: &'a BoundExpr| {
        if let ExprKind::SubLink(sl) = &node.kind {
            out.push(&sl.subquery);
        }
        true
    });
}

fn collect_join_sublinks<'a>(item: &'a FromItem, out: &mut Vec<&'a BoundQuery>) {
    if let FromItem::Join(j) = item {
        if let Some(q) = &j.quals {
            collect_sublinks(q, out);
        }
        collect_join_sublinks(&j.left, out);
        collect_join_sublinks(&j.right, out);
    }
}

fn walk_from(item: &FromItem, ctx: RecCtx, range: &[RangeEntry], name: &str, pos: Option<u32>) -> Result<()> {
    match item {
        FromItem::Range(rt) => {
            let idx = (*rt as usize).wrapping_sub(1);
            if let Some(entry) = range.get(idx) {
                match &entry.kind {
                    RangeKind::Cte { recursive_self: true, .. } => {
                        if ctx != RecCtx::Plain {
                            return Err(diag::invalid_recursion(recursion_message(ctx, name), pos));
                        }
                    }
                    RangeKind::Subquery { query } => walk_query(query, ctx, name, pos)?,
                    _ => {}
                }
            }
            Ok(())
        }
        FromItem::Join(j) => {
            let (lctx, rctx) = match &j.kind {
                JoinKind::Left => (ctx, RecCtx::OuterJoin),
                JoinKind::Right => (RecCtx::OuterJoin, ctx),
                JoinKind::Full => (RecCtx::OuterJoin, RecCtx::OuterJoin),
                JoinKind::Inner => (ctx, ctx),
            };
            walk_from(&j.left, lctx, range, name, pos)?;
            walk_from(&j.right, rctx, range, name, pos)
        }
    }
}

/// Remove the frame pushed by `bind_with` and compute each bound CTE's `ref_count`
/// (from `CteEntry::ref_count`; `materialized` hint semantics are the planner's).
///
/// The frame is dropped here. `ref_count` values in the `BoundCte`s returned by `bind_with` are
/// 0: references come from the main body, which is bound after `bind_with` returns, and this
/// function cannot reach the caller's vector. The caller must copy `CteEntry::ref_count` of
/// `b.cte_frames.last()` into `BoundCte::ref_count` before calling `pop_with`.
pub fn pop_with(b: &mut Binder) {
    b.cte_frames.pop();
}

/// Resolve an unqualified relation name against the visible CTEs (innermost frame first).
/// Returns `None` if no CTE has that name. A hit increments `CteEntry::ref_count` (the final count goes into `BoundCte::ref_count`). Errors: 0A000 for a
/// data-modifying CTE without RETURNING; 42P19-family for illegal recursive references (checked here
/// while the item's body is analyzed, see the module notes).
pub fn find_cte(b: &mut Binder, name: &str, pos: Option<u32>) -> Result<Option<CteRef>> {
    let nframes = b.cte_frames.len();
    let mut hit: Option<(usize, usize)> = None;
    for f in (0..nframes).rev() {
        if let Some(e) = b.cte_frames[f].entries.iter().position(|en| en.name == name) {
            hit = Some((f, e));
            break;
        }
    }
    let (f, e) = match hit {
        Some(x) => x,
        None => return Ok(None),
    };
    let levels_up = (nframes - 1 - f) as u32;
    let (in_progress, recursive, pending) = {
        let en = &b.cte_frames[f].entries[e];
        (en.in_progress, en.recursive, en.index == PENDING)
    };
    if pending && !in_progress {
        // A RECURSIVE sibling that is not analyzed yet: ask `bind_with` to analyze it first.
        b.scratch.insert(KEY_NEED, format!("{f}:{e}"));
        return Err(PgError::internal("cte: WITH item referenced before it is analyzed"));
    }
    if in_progress {
        if !recursive {
            return Err(diag::invalid_recursion(recursion_message(RecCtx::NonRecursiveTerm, name), pos));
        }
        if b.cte_frames[f].entries[e].columns.is_empty() {
            let def = scratch_num(b, KEY_DEF_POS).map(|p| p as u32);
            let msg = format!(
                "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
            );
            return Err(diag::invalid_recursion(msg, def));
        }
        // Context (subquery, outer join, EXCEPT/INTERSECT) is checked after binding, see `check_rt_context`.
        if b.cte_frames[f].entries[e].ref_count >= 1 {
            return Err(diag::invalid_recursion(recursion_message(RecCtx::Plain, name), pos));
        }
        scratch_set_num(b, KEY_SELFREF_POS, pos.map(|p| p as usize));
        let en = &mut b.cte_frames[f].entries[e];
        en.ref_count += 1;
        return Ok(Some(CteRef {
            cte_index: e as u32,
            levels_up,
            recursive_self: true,
            columns: en.columns.clone(),
        }));
    }
    let en = &mut b.cte_frames[f].entries[e];
    if en.no_returning {
        let msg = format!("WITH query \"{name}\" does not have a RETURNING clause");
        return Err(err_at(SqlState::FEATURE_NOT_SUPPORTED, msg, pos));
    }
    en.ref_count += 1;
    Ok(Some(CteRef {
        cte_index: e as u32,
        levels_up,
        recursive_self: false,
        columns: en.columns.clone(),
    }))
}

#[cfg(test)]
mod tests {
    // These tests bind whole statements, so they need the statement binder (SQL-04) and the query
    // binder (SQL-16) and run centrally once those are in place.
    use super::*;
    use crate::binder::context::{BindContext, BindOutput, BindSession};
    use crate::binder::dispatch::bind_statement;
    use crate::ir::{QueryBody, RangeKind};
    use pgarbor_catalog::CatalogEnv;
    use pgarbor_types::context::IoConfig;

    fn bind_sql(env: &CatalogEnv, sql: &str) -> Result<BindOutput> {
        let stmt = crate::parser::parse_one(sql)?;
        let ctx = BindContext {
            env,
            session: BindSession { database: String::from("pgarbor") },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        bind_statement(&stmt, &ctx)
    }

    fn query_of(out: &BindOutput) -> Option<&crate::ir::BoundQuery> {
        match &out.statement {
            crate::ir::BoundStatement::Query(q) => Some(q.as_ref()),
            _ => None,
        }
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn simple_cte_is_visible_to_main_query() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let out = bind_sql(&env, "with x as (select 1 a) select * from x").expect("bind");
        let q = query_of(&out).expect("query");
        assert_eq!(q.ctes.len(), 1);
        assert_eq!(q.ctes[0].columns.len(), 1);
        assert_eq!(q.ctes[0].columns[0].name, "a");
        assert_eq!(q.ctes[0].columns[0].ty.oid, pgarbor_types::oids::INT4);
        assert!(!q.ctes[0].recursive);
        // Set by the caller from the frame before `pop_with` (see the `pop_with` docs).
        assert_eq!(q.ctes[0].ref_count, 1);
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn duplicate_name_is_42712() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let err = bind_sql(&env, "with x as (select 1), x as (select 2) select 1").expect_err("dup");
        assert_eq!(err.sqlstate, SqlState::DUPLICATE_ALIAS);
        assert_eq!(err.message, "WITH query name \"x\" specified more than once");
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn recursive_cte_has_union_body() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let out = bind_sql(
            &env,
            "with recursive r(n) as (select 1 union all select n+1 from r where n < 5) select * from r",
        )
        .expect("bind");
        let q = query_of(&out).expect("query");
        assert!(q.ctes[0].recursive);
        match &q.ctes[0].body {
            CteBody::Query(bq) => match &bq.body {
                QueryBody::SetOp(s) => {
                    assert_eq!(s.op, SetOpKind::Union);
                    assert!(s.all);
                }
                other => panic!("expected SetOp body, got {other:?}"),
            },
            other => panic!("expected query body, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn self_reference_in_non_recursive_term_is_42p19() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let err = bind_sql(
            &env,
            "with recursive r as (select * from r union all select 1) select * from r",
        )
        .expect_err("42P19");
        assert_eq!(err.sqlstate, SqlState::INVALID_RECURSION);
        assert_eq!(
            err.message,
            "recursive reference to query \"r\" must not appear within its non-recursive term"
        );
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn column_alias_count_mismatch_is_42p10() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let err = bind_sql(&env, "with x(a, b) as (select 1) select * from x").expect_err("42P10");
        assert_eq!(err.sqlstate, SqlState::INVALID_COLUMN_REFERENCE);
        assert_eq!(err.message, "WITH query \"x\" has 1 columns available but 2 columns specified");
    }

    #[test]
    #[ignore = "needs SQL-16 and a catalog table t (create it with testing::run and a CreateTableSpec)"]
    fn data_modifying_cte_binds_as_modify() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let out = bind_sql(&env, "with d as (delete from t returning *) select * from d").expect("bind");
        let q = query_of(&out).expect("query");
        assert!(matches!(q.ctes[0].body, CteBody::Modify(_)));
    }

    #[test]
    #[ignore = "needs SQL-16"]
    fn nested_data_modifying_cte_is_0a000() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap");
        let err = bind_sql(&env, "select * from (with d as (delete from t returning *) select * from d) s")
            .expect_err("0A000");
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn move_before_orders_dependency_first() {
        let mut order = vec![0, 1, 2];
        assert!(move_before(&mut order, 2, 0));
        assert_eq!(order, vec![2, 0, 1]);
        // Now 2 sits before 0; an item already in front of `before` is not moved.
        assert!(!move_before(&mut order, 2, 0));
        assert_eq!(order, vec![2, 0, 1]);
        // 0 is after 2, so it can move in front of 2.
        assert!(move_before(&mut order, 0, 2));
        assert_eq!(order, vec![0, 2, 1]);
    }

    #[test]
    fn recursion_messages_match_pg() {
        assert_eq!(
            recursion_message(RecCtx::NonRecursiveTerm, "r"),
            "recursive reference to query \"r\" must not appear within its non-recursive term"
        );
        assert_eq!(
            recursion_message(RecCtx::Plain, "r"),
            "recursive reference to query \"r\" must not appear more than once"
        );
        assert_eq!(
            recursion_message(RecCtx::Except, "r"),
            "recursive reference to query \"r\" must not appear within EXCEPT"
        );
    }

    #[test]
    fn range_kind_variant_is_matchable() {
        let k = RangeKind::Cte { cte_index: 0, levels_up: 0, recursive_self: true };
        assert!(matches!(k, RangeKind::Cte { recursive_self: true, .. }));
    }
}
