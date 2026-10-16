//! WINDOW clause and `OVER (...)` specifications (SQL-22). PG references: `parse_clause.c`
//! (`transformWindowDefinitions`, `transformFrameOffset`, `findWindowClause`), `parse_agg.c`
//! (`transformWindowFuncCall`), `parse_expr.c`.
//!
//! * `bind_window_clause(b, named)` binds `WINDOW w AS (...), v AS (w ORDER BY ...)` into
//!   `QueryScope::windows` (named entries, in definition order). Duplicate names: 42P20 `window
//!   "w" is already defined`. References to later-defined names: 42704 `window "x" does not exist`.
//! * `bind_window_spec(b, over, call_pos)` is called for each `OVER` (`WindowType::WindowSpec(spec)` or
//!   `NamedWindow(ident)`) and returns an index into `QueryScope::windows`, de-duplicating
//!   structurally equal definitions (PG shares one `WindowClause` between identical OVER clauses).
//!   `OVER w` with no changes reuses window `w`. `OVER (w ORDER BY ...)` copies `w` and adds ordering:
//!   errors 42P20 `cannot override PARTITION BY clause of window "w"`, `cannot override ORDER BY
//!   clause of window "w"` (when w already has one), `cannot copy window "w" because it has a frame
//!   clause` HINT `Omit the parentheses in this OVER clause.`
//!
//! ## Specification content (`WindowSpec { window_name, partition_by, order_by, window_frame }`)
//! * `partition_by`: expressions bound with `ExprCtx::window_def(ExprClause::PartitionBy)`; need
//!   equality ops (42883 `could not identify an equality operator for type X`).
//! * `order_by`: bound like ORDER BY items but as `SortExpr`s (no target-list indirection) with
//!   `ExprCtx::window_def(ExprClause::WindowOrderBy)`.
//! * frame (`WindowFrame { units, start_bound, end_bound }`): units ROWS / RANGE / GROUPS; bounds
//!   `UNBOUNDED PRECEDING`, `n PRECEDING`, `CURRENT ROW`, `n FOLLOWING`, `UNBOUNDED FOLLOWING`.
//!   Rules (all 42P20 unless noted): frame start cannot be UNBOUNDED FOLLOWING (`frame start cannot be
//!   UNBOUNDED FOLLOWING`), frame end cannot be UNBOUNDED PRECEDING, frame starting from following row
//!   cannot end with current row / frame starting from current row cannot have preceding rows (the
//!   ordering rule `start <= end`), GROUPS and RANGE with offset require ORDER BY (`GROUPS mode
//!   requires an ORDER BY clause`, `RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY
//!   column`), offset expressions: ROWS/GROUPS -> `int8` (negative constant: 22013 `frame starting
//!   offset must not be negative`/`frame ending offset must not be negative`), RANGE -> the ordering
//!   column's offset type (`in_range` support: int types -> same type, date/timestamp(tz) ->
//!   `interval`, numeric -> numeric, float -> float8); no offset support: 42P20 `RANGE with offset
//!   PRECEDING/FOLLOWING is not supported for column type X`. Offsets must not contain variables,
//!   aggregates, windows (`ExprClause::WindowFrameRows/Range/Groups`). `EXCLUDE` clause supported
//!   (`FrameExclusion`); sqlparser 0.63 may not expose it -> 0A000.
//! * `frame_is_default` is set when no frame was written (`RANGE UNBOUNDED PRECEDING` to `CURRENT
//!   ROW` with ORDER BY, whole partition without).
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::resolve::coerce::can_coerce_type;
use pgarbor_types::{compare, oids, registry, Datum, TypeEnv, TypeId};
use sqlparser::ast::{
    Expr, NamedWindowDefinition, NamedWindowExpr, OrderByExpr, OrderBySort, WindowFrame, WindowFrameBound,
    WindowFrameUnits, WindowSpec, WindowType,
};

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

use super::coerce::{coerce_to_specific, resolve_unknown_output};
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::bind_expr;
use crate::diag::{self, ExprClause};
use crate::ir::{BoundExpr, FrameBound, FrameExclusion, FrameUnits, SortExpr, WindowDef};

/// Window-level error (SQLSTATE 42P20 unless given) with an optional position.
fn werr(state: SqlState, msg: impl Into<String>, pos: Option<u32>) -> PgError {
    diag::with_position(PgError::new(state, msg), pos)
}

/// Where a window specification sits in the query text. PG reports specification errors at the
/// specification's `(` (`WindowDef.location`, gram.y `window_specification`).
#[derive(Clone, Copy)]
enum SpecSite {
    /// Inline `OVER (...)` of the window call whose name starts at this position.
    Over(Option<u32>),
    /// `WINDOW name AS (...)`, with the position of `name`.
    Def(Option<u32>),
}

/// Source positions of one specification's parts.
#[derive(Default)]
struct SpecPos {
    /// The opening `(`.
    spec: Option<u32>,
    /// First token of the start bound (the frame keyword's operand: keyword or offset expression).
    frame_start: Option<u32>,
    /// First token of the end bound of a `BETWEEN` frame.
    frame_end: Option<u32>,
}

fn is_word(tok: Option<&Token>, kw: &str) -> bool {
    matches!(tok, Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(kw))
}

/// Index of the `(` that opens the specification of `site`.
fn spec_open_index(toks: &[(&Token, Option<u32>)], site: SpecSite) -> Option<usize> {
    match site {
        SpecSite::Over(call) => {
            let call = call?;
            let from = toks.iter().position(|(_, p)| *p == Some(call))?;
            // The call's own parentheses and FILTER come first; the first OVER outside them owns the spec.
            let mut depth = 0i32;
            for i in from..toks.len() {
                match toks[i].0 {
                    Token::LParen => depth += 1,
                    Token::RParen => depth -= 1,
                    Token::Word(_) if depth == 0 && is_word(Some(toks[i].0), "OVER") => {
                        return if matches!(toks.get(i + 1).map(|t| t.0), Some(Token::LParen)) { Some(i + 1) } else { None };
                    }
                    _ => {}
                }
            }
            None
        }
        SpecSite::Def(name) => {
            let name = name?;
            let name_idx = toks.iter().position(|(_, p)| *p == Some(name))?;
            (name_idx + 1..toks.len()).find(|&i| matches!(toks[i].0, Token::LParen))
        }
    }
}

/// Source positions of a specification's `(` and frame bounds. The AST keeps no spans for
/// `WindowSpec` or frame bounds, so the query text is tokenized again. Runs only to format errors.
fn spec_pos(b: &Binder, site: SpecSite) -> SpecPos {
    let dialect = PostgreSqlDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, b.src.text()).tokenize_with_location() else {
        return SpecPos::default();
    };
    let toks: Vec<(&Token, Option<u32>)> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .map(|t| (&t.token, b.src.pos_of_span(t.span)))
        .collect();
    let Some(open) = spec_open_index(&toks, site) else {
        return SpecPos::default();
    };
    let mut out = SpecPos { spec: toks[open].1, ..SpecPos::default() };
    // Frame keyword at depth 1 of the specification: the start bound follows it (after BETWEEN when
    // present); the end bound follows the AND at depth 1.
    let mut depth = 0i32;
    for i in open..toks.len() {
        match toks[i].0 {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Word(w) if depth == 1 && w.quote_style.is_none() && matches!(w.value.to_ascii_uppercase().as_str(), "ROWS" | "RANGE" | "GROUPS") => {
                let mut start = i + 1;
                if is_word(toks.get(start).map(|t| t.0), "BETWEEN") {
                    start += 1;
                    let mut d = 1i32;
                    for j in start..toks.len() {
                        match toks[j].0 {
                            Token::LParen => d += 1,
                            Token::RParen => d -= 1,
                            Token::Word(_) if d == 1 && is_word(Some(toks[j].0), "AND") => {
                                out.frame_end = toks.get(j + 1).and_then(|t| t.1);
                                break;
                            }
                            _ => {}
                        }
                        if d == 0 {
                            break;
                        }
                    }
                }
                out.frame_start = toks.get(start).and_then(|t| t.1);
                return out;
            }
            _ => {}
        }
        if depth == 0 && i > open {
            break;
        }
    }
    out
}

/// The `EXCLUDE` clause of the specification at `site` (removed from the token stream before
/// sqlparser runs, see `parser::strip_frame_exclusion`; read back from the query text here).
fn frame_exclusion(b: &Binder, site: SpecSite) -> FrameExclusion {
    if !b.src.text().to_ascii_lowercase().contains("exclude") {
        return FrameExclusion::NoOthers;
    }
    let dialect = PostgreSqlDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, b.src.text()).tokenize_with_location() else {
        return FrameExclusion::NoOthers;
    };
    let toks: Vec<(&Token, Option<u32>)> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .map(|t| (&t.token, b.src.pos_of_span(t.span)))
        .collect();
    let Some(open) = spec_open_index(&toks, site) else {
        return FrameExclusion::NoOthers;
    };
    let sig: Vec<&Token> = toks[open..].iter().map(|(t, _)| *t).filter(|t| !matches!(t, Token::Whitespace(_))).collect();
    let mut depth = 0i32;
    for k in 0..sig.len() {
        match sig[k] {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Token::Word(_) if depth == 1 && is_word(Some(sig[k]), "EXCLUDE") => {
                let w = |n: usize| sig.get(k + n).copied();
                return if is_word(w(1), "CURRENT") {
                    FrameExclusion::CurrentRow
                } else if is_word(w(1), "GROUP") {
                    FrameExclusion::Group
                } else if is_word(w(1), "TIES") {
                    FrameExclusion::Ties
                } else {
                    FrameExclusion::NoOthers
                };
            }
            _ => {}
        }
    }
    FrameExclusion::NoOthers
}

/// Specification-level error (42P20 unless given), placed at the specification's `(`.
fn spec_err(b: &Binder, site: SpecSite, state: SqlState, msg: impl Into<String>) -> PgError {
    werr(state, msg, spec_pos(b, site).spec)
}

/// Frame-shape error placed at the frame bound it concerns (gram.y reports at `@1`, `@2` or `@4`).
fn frame_err(b: &Binder, site: SpecSite, e: PgError) -> PgError {
    let at_start = e.message.starts_with("frame start cannot") || e.message.ends_with("cannot end with current row");
    let p = spec_pos(b, site);
    diag::with_position(e, if at_start { p.frame_start } else { p.frame_end })
}

/// Index of the named window `name` in the innermost scope (only definitions bound so far).
fn find_named_index(b: &Binder, name: &str) -> Result<Option<usize>> {
    Ok(b.scope()?.windows.iter().position(|w| w.name.as_deref() == Some(name)))
}

/// Bind the `WINDOW` clause definitions into the innermost scope.
///
/// PG `transformWindowDefinitions`: a definition may only reference names defined before it.
pub fn bind_window_clause(b: &mut Binder, named: &[NamedWindowDefinition]) -> Result<()> {
    for def in named {
        let NamedWindowDefinition(name_ident, expr) = def;
        let name_pos = b.pos_span(name_ident.span);
        let name = b.ident(name_ident);
        if find_named_index(b, &name)?.is_some() {
            let msg = format!("window \"{name}\" is already defined");
            return Err(spec_err(b, SpecSite::Def(name_pos), SqlState::WINDOWING_ERROR, msg));
        }
        let spec = match expr {
            NamedWindowExpr::WindowSpec(spec) => spec,
            // PG grammar: `WINDOW w AS (...)` only; a bare name is a syntax error.
            NamedWindowExpr::NamedWindow(ref_ident) => {
                return Err(diag::syntax_error_near(&ref_ident.value, b.pos_span(ref_ident.span)));
            }
        };
        let wd = resolve_spec(b, spec, Some(name), SpecSite::Def(name_pos))?;
        b.scope_mut()?.windows.push(wd);
    }
    Ok(())
}

/// Bind one `OVER` specification; returns its index in `QueryScope::windows`.
///
/// `OVER w` returns the index of the named definition. An inline `OVER (...)` is merged and
/// de-duplicated against existing definitions (structural equality, name ignored).
/// `call_pos` is the position of the window call (its function name), which locates the `OVER (`
/// for error positions.
pub fn bind_window_spec(b: &mut Binder, over: &WindowType, call_pos: Option<u32>) -> Result<usize> {
    match over {
        WindowType::NamedWindow(ident) => {
            let name = b.ident(ident);
            let pos = b.pos_span(ident.span);
            match find_named_index(b, &name)? {
                Some(idx) => Ok(idx),
                None => Err(werr(SqlState::UNDEFINED_OBJECT, format!("window \"{name}\" does not exist"), pos)),
            }
        }
        WindowType::WindowSpec(spec) => {
            let wd = resolve_spec(b, spec, None, SpecSite::Over(call_pos))?;
            let scope = b.scope_mut()?;
            if let Some(idx) = scope.windows.iter().position(|w| same_window(w, &wd)) {
                return Ok(idx);
            }
            let idx = scope.windows.len();
            scope.windows.push(wd);
            Ok(idx)
        }
    }
}

/// Structural equality of two merged definitions (the name is ignored).
fn same_window(a: &WindowDef, b: &WindowDef) -> bool {
    a.partition_by == b.partition_by
        && a.order_by == b.order_by
        && a.units == b.units
        && a.start == b.start
        && a.end == b.end
        && a.exclusion == b.exclusion
        && a.frame_is_default == b.frame_is_default
}

fn is_offset_bound(bound: &WindowFrameBound) -> bool {
    matches!(bound, WindowFrameBound::Preceding(Some(_)) | WindowFrameBound::Following(Some(_)))
}

/// Frame shape rules that PG enforces in the grammar (`frame_extent` in gram.y), before any
/// analysis. `end` is `None` for the single-bound form (`ROWS 1 PRECEDING`, end = CURRENT ROW).
fn check_frame_shape(start: &WindowFrameBound, end: Option<&WindowFrameBound>) -> Result<()> {
    if matches!(start, WindowFrameBound::Following(None)) {
        return Err(werr(SqlState::WINDOWING_ERROR, "frame start cannot be UNBOUNDED FOLLOWING", None));
    }
    match end {
        None => {
            if matches!(start, WindowFrameBound::Following(Some(_))) {
                return Err(werr(
                    SqlState::WINDOWING_ERROR,
                    "frame starting from following row cannot end with current row",
                    None,
                ));
            }
        }
        Some(end) => {
            if matches!(end, WindowFrameBound::Preceding(None)) {
                return Err(werr(SqlState::WINDOWING_ERROR, "frame end cannot be UNBOUNDED PRECEDING", None));
            }
            let end_preceding = matches!(end, WindowFrameBound::Preceding(Some(_)));
            let end_current = matches!(end, WindowFrameBound::CurrentRow);
            if matches!(start, WindowFrameBound::CurrentRow) && end_preceding {
                return Err(werr(
                    SqlState::WINDOWING_ERROR,
                    "frame starting from current row cannot have preceding rows",
                    None,
                ));
            }
            if matches!(start, WindowFrameBound::Following(Some(_))) && (end_preceding || end_current) {
                return Err(werr(
                    SqlState::WINDOWING_ERROR,
                    "frame starting from following row cannot have preceding rows",
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn frame_units(units: &WindowFrameUnits) -> FrameUnits {
    match units {
        WindowFrameUnits::Rows => FrameUnits::Rows,
        WindowFrameUnits::Range => FrameUnits::Range,
        WindowFrameUnits::Groups => FrameUnits::Groups,
    }
}

/// Merge one window specification (PG `transformWindowDefinitions` for a single entry).
///
/// `name` is `Some` for `WINDOW name AS (...)`, `None` for an inline `OVER (...)`.
fn resolve_spec(b: &mut Binder, spec: &WindowSpec, name: Option<String>, site: SpecSite) -> Result<WindowDef> {
    let frame: Option<&WindowFrame> = spec.window_frame.as_ref();
    if let Some(f) = frame {
        if let Err(e) = check_frame_shape(&f.start_bound, f.end_bound.as_ref()) {
            return Err(frame_err(b, site, e));
        }
    }

    // Referenced window (`OVER (w ...)`, `WINDOW v AS (w ...)`): must be defined earlier.
    let refwc: Option<(String, WindowDef)> = match &spec.window_name {
        Some(ref_ident) => {
            let ref_name = b.ident(ref_ident);
            match find_named_index(b, &ref_name)? {
                Some(i) => {
                    let wd = b
                        .scope()?
                        .windows
                        .get(i)
                        .cloned()
                        .ok_or_else(|| PgError::internal("window index out of range"))?;
                    Some((ref_name, wd))
                }
                None => {
                    let msg = format!("window \"{ref_name}\" does not exist");
                    return Err(spec_err(b, site, SqlState::UNDEFINED_OBJECT, msg));
                }
            }
        }
        None => None,
    };

    // PARTITION BY and ORDER BY are bound before the copy rules are checked, as in PG.
    let mut own_order = Vec::with_capacity(spec.order_by.len());
    for item in &spec.order_by {
        own_order.push(bind_sort_item(b, item)?);
    }
    let mut own_partition = Vec::with_capacity(spec.partition_by.len());
    for e in &spec.partition_by {
        own_partition.push(bind_partition_item(b, e)?);
    }
    let has_own_partition = !spec.partition_by.is_empty();
    let has_own_order = !spec.order_by.is_empty();

    let (partition_by, order_by) = match &refwc {
        Some((ref_name, rdef)) => {
            if has_own_partition {
                let msg = format!("cannot override PARTITION BY clause of window \"{ref_name}\"");
                return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, msg));
            }
            if has_own_order && !rdef.order_by.is_empty() {
                let msg = format!("cannot override ORDER BY clause of window \"{ref_name}\"");
                return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, msg));
            }
            let order = if has_own_order { own_order } else { rdef.order_by.clone() };
            (rdef.partition_by.clone(), order)
        }
        None => (own_partition, own_order),
    };

    if let Some((ref_name, rdef)) = &refwc {
        if !rdef.frame_is_default {
            let msg = format!("cannot copy window \"{ref_name}\" because it has a frame clause");
            if name.is_some() || has_own_order || frame.is_some() {
                return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, msg));
            }
            return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, msg).with_hint("Omit the parentheses in this OVER clause."));
        }
    }

    let has_range_offset = match frame {
        Some(f) => {
            matches!(f.units, WindowFrameUnits::Range)
                && (is_offset_bound(&f.start_bound) || f.end_bound.as_ref().map_or(false, is_offset_bound))
        }
        None => false,
    };
    if has_range_offset && order_by.len() != 1 {
        let msg = "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column";
        return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, msg));
    }
    let is_groups = frame.map_or(false, |f| matches!(f.units, WindowFrameUnits::Groups));
    if is_groups && order_by.is_empty() {
        return Err(spec_err(b, site, SqlState::WINDOWING_ERROR, "GROUPS mode requires an ORDER BY clause"));
    }

    let (units, start, end, frame_is_default) = match frame {
        Some(f) => {
            let units = frame_units(&f.units);
            let order_ty = order_by.first().map(|s| s.expr.ty);
            let start = bind_frame_bound(b, &f.start_bound, units, order_ty, true)?;
            let end = match &f.end_bound {
                Some(e) => bind_frame_bound(b, e, units, order_ty, false)?,
                None => FrameBound::CurrentRow,
            };
            (units, start, end, false)
        }
        // PG FRAMEOPTION_DEFAULTS: RANGE UNBOUNDED PRECEDING to CURRENT ROW with ORDER BY; the
        // whole partition without ORDER BY.
        None => {
            let end = if order_by.is_empty() { FrameBound::UnboundedFollowing } else { FrameBound::CurrentRow };
            (FrameUnits::Range, FrameBound::UnboundedPreceding, end, true)
        }
    };

    let exclusion = if frame_is_default { FrameExclusion::NoOthers } else { frame_exclusion(b, site) };

    Ok(WindowDef {
        name,
        partition_by,
        order_by,
        units,
        start,
        end,
        exclusion,
        frame_is_default,
    })
}

/// One ORDER BY key of a window (PG `transformSortClause` with SQL-99 rules).
fn bind_sort_item(b: &mut Binder, item: &OrderByExpr) -> Result<SortExpr> {
    let pos = b.pos(&item.expr);
    let expr = bind_expr(b, &item.expr, ExprCtx::window_def(ExprClause::WindowOrderBy))?;
    let expr = if expr.ty.oid == oids::UNKNOWN { resolve_unknown_output(b, &BoundArg::new(expr, pos))? } else { expr };
    let (descending, default_nulls_first) = match &item.options.sort {
        Some(OrderBySort::Desc) => (true, true),
        Some(OrderBySort::Asc) | None => (false, false),
        // `ORDER BY x USING op` needs the operator lookup owned by the operator binder.
        Some(OrderBySort::Using(_)) => {
            return Err(PgError::unimplemented("SQL-22", "window ORDER BY USING <operator>"));
        }
    };
    let nulls_first = item.options.nulls_first.unwrap_or(default_nulls_first);
    let env: &dyn TypeEnv = b.env;
    if !compare::is_orderable(env, expr.ty)? {
        return Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not identify an ordering operator for type {}", b.type_name(expr.ty)),
        ));
    }
    Ok(SortExpr { expr, descending, nulls_first, using_op: None })
}

/// One PARTITION BY key of a window (PG `transformGroupClause` with SQL-99 rules).
fn bind_partition_item(b: &mut Binder, e: &Expr) -> Result<BoundExpr> {
    let pos = b.pos(e);
    let expr = bind_expr(b, e, ExprCtx::window_def(ExprClause::PartitionBy))?;
    let expr = if expr.ty.oid == oids::UNKNOWN { resolve_unknown_output(b, &BoundArg::new(expr, pos))? } else { expr };
    let env: &dyn TypeEnv = b.env;
    if !has_equality(env, expr.ty.oid) {
        return Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not identify an equality operator for type {}", b.type_name(expr.ty)),
        ));
    }
    Ok(expr)
}

/// Whether values of the type have an equality operator. json and xml have none; arrays and
/// composites inherit the restriction from their elements / fields.
fn has_equality(env: &dyn TypeEnv, oid: Oid) -> bool {
    let base = registry::base_type(env, oid);
    if base == oids::JSON || base == oids::XML {
        return false;
    }
    if let Some(info) = env.type_info(base) {
        if info.is_array() {
            return has_equality(env, info.elem);
        }
    }
    if let Some(fields) = env.composite_fields(base) {
        return fields.iter().filter(|f| !f.dropped).all(|f| has_equality(env, f.type_id.oid));
    }
    true
}

/// One frame bound (`start` or `end`); offsets are bound and typed here.
fn bind_frame_bound(
    b: &mut Binder,
    bound: &WindowFrameBound,
    units: FrameUnits,
    order_ty: Option<TypeId>,
    is_start: bool,
) -> Result<FrameBound> {
    Ok(match bound {
        WindowFrameBound::CurrentRow => FrameBound::CurrentRow,
        WindowFrameBound::Preceding(None) => FrameBound::UnboundedPreceding,
        WindowFrameBound::Following(None) => FrameBound::UnboundedFollowing,
        WindowFrameBound::Preceding(Some(e)) => {
            FrameBound::Preceding(bind_frame_offset(b, e, units, order_ty, is_start)?)
        }
        WindowFrameBound::Following(Some(e)) => {
            FrameBound::Following(bind_frame_offset(b, e, units, order_ty, is_start)?)
        }
    })
}

/// `transformFrameOffset`: ROWS/GROUPS offsets coerce to int8; RANGE offsets to the in_range
/// offset type chosen from the ordering column.
fn bind_frame_offset(
    b: &mut Binder,
    e: &Expr,
    units: FrameUnits,
    order_ty: Option<TypeId>,
    is_start: bool,
) -> Result<BoundExpr> {
    let (clause, what) = match units {
        FrameUnits::Rows => (ExprClause::WindowFrameRows, "ROWS"),
        FrameUnits::Range => (ExprClause::WindowFrameRange, "RANGE"),
        FrameUnits::Groups => (ExprClause::WindowFrameGroups, "GROUPS"),
    };
    // Offsets are evaluated without a row: no column references, aggregates or windows.
    let ec = ExprCtx { columns_ok: false, ..ExprCtx::window_def(clause).inside_aggregate() };
    let pos = b.pos(e);
    let arg = bind_expr(b, e, ec)?;
    match units {
        FrameUnits::Rows | FrameUnits::Groups => {
            let arg = coerce_to_specific(b, &BoundArg::new(arg, pos), TypeId::INT8, what)?;
            check_const_offset(&arg, is_start)?;
            Ok(arg)
        }
        FrameUnits::Range => {
            let col = order_ty.ok_or_else(|| PgError::internal("window RANGE offset without ORDER BY"))?;
            let target = select_range_offset_type(b, col, &arg, pos)?;
            coerce_to_specific(b, &BoundArg::new(arg, pos), target, what)
        }
    }
}

/// Constant ROWS/GROUPS offsets that PG rejects at run time (`nodeWindowAgg.c`), checked now.
fn check_const_offset(expr: &BoundExpr, is_start: bool) -> Result<()> {
    let which = if is_start { "starting" } else { "ending" };
    match expr.as_const() {
        Some(Datum::Null) => Err(PgError::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            format!("frame {which} offset must not be null"),
        )),
        Some(Datum::Int8(v)) if *v < 0 => Err(PgError::new(
            SqlState(*b"22013"),
            format!("frame {which} offset must not be negative"),
        )),
        _ => Ok(()),
    }
}

/// Offset types with an in_range support function for a sort column of type `col`
/// (`pg_amproc` BTINRANGE_PROC rows, PG 18). Empty when RANGE offsets are unsupported.
fn range_offset_candidates(col: Oid) -> &'static [Oid] {
    match col {
        oids::INT8 => &[oids::INT8],
        oids::INT4 | oids::INT2 => &[oids::INT8, oids::INT4, oids::INT2],
        oids::FLOAT4 | oids::FLOAT8 => &[oids::FLOAT8],
        oids::NUMERIC => &[oids::NUMERIC],
        oids::DATE | oids::TIME | oids::TIMETZ | oids::TIMESTAMP | oids::TIMESTAMPTZ | oids::INTERVAL => {
            &[oids::INTERVAL]
        }
        _ => &[],
    }
}

fn select_range_offset_type(b: &Binder, col: TypeId, arg: &BoundExpr, pos: Option<u32>) -> Result<TypeId> {
    // The in_range lookup uses the operator class input type: varchar sorts with text_ops.
    let col_oid = if col.oid == oids::VARCHAR { oids::TEXT } else { col.oid };
    let cands = range_offset_candidates(col_oid);
    let node = arg.ty.oid;
    let env: &dyn TypeEnv = b.env;
    let mut implicit_ok = Vec::with_capacity(cands.len());
    for &cand in cands {
        let ok = node == oids::UNKNOWN || can_coerce_type(env, &[node], &[cand], CastContext::Implicit)?;
        implicit_ok.push(ok);
    }
    let selected = select_offset_type(
        col_oid,
        node,
        cands,
        &implicit_ok,
        &b.type_name(TypeId::of(col_oid)),
        &b.type_name(arg.ty),
        pos,
    )?;
    Ok(TypeId::of(selected))
}

/// PG `transformFrameOffset` (RANGE branch) selection among the in_range offset candidates.
/// `implicit_ok[i]` says whether the offset type coerces implicitly to `cands[i]`.
fn select_offset_type(
    col: Oid,
    node: Oid,
    cands: &[Oid],
    implicit_ok: &[bool],
    col_name: &str,
    node_name: &str,
    pos: Option<u32>,
) -> Result<Oid> {
    if cands.is_empty() {
        return Err(werr(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("RANGE with offset PRECEDING/FOLLOWING is not supported for column type {col_name}"),
            pos,
        ));
    }
    let preferred = if node == oids::UNKNOWN { col } else { node };
    let mut nmatches = 0usize;
    let mut selected: Option<Oid> = None;
    for (&cand, &ok) in cands.iter().zip(implicit_ok.iter()) {
        if !ok {
            continue;
        }
        nmatches += 1;
        // PG keeps the exact match once found, otherwise the last matching candidate.
        if selected != Some(preferred) {
            selected = Some(cand);
        }
    }
    let Some(sel) = selected else {
        return Err(werr(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!(
                "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {col_name} and offset type {node_name}"
            ),
            pos,
        )
        .with_hint("Cast the offset value to an appropriate type."));
    };
    if nmatches != 1 && sel != preferred {
        return Err(werr(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!(
                "RANGE with offset PRECEDING/FOLLOWING has multiple interpretations for column type {col_name} and offset type {node_name}"
            ),
            pos,
        )
        .with_hint("Cast the offset value to the exact intended type."));
    }
    Ok(sel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident;

    fn offset_expr() -> Option<Box<Expr>> {
        Some(Box::new(Expr::Identifier(Ident::new("n"))))
    }

    fn message_of(r: Result<()>) -> String {
        match r {
            Ok(()) => String::from("<ok>"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn frame_start_unbounded_following_is_rejected() {
        let r = check_frame_shape(&WindowFrameBound::Following(None), Some(&WindowFrameBound::CurrentRow));
        assert_eq!(message_of(r), "frame start cannot be UNBOUNDED FOLLOWING");
    }

    #[test]
    fn frame_end_unbounded_preceding_is_rejected() {
        let r = check_frame_shape(&WindowFrameBound::Preceding(None), Some(&WindowFrameBound::Preceding(None)));
        assert_eq!(message_of(r), "frame end cannot be UNBOUNDED PRECEDING");
    }

    #[test]
    fn following_start_with_current_row_end_is_rejected() {
        let r = check_frame_shape(&WindowFrameBound::Following(offset_expr()), Some(&WindowFrameBound::CurrentRow));
        assert_eq!(message_of(r), "frame starting from following row cannot have preceding rows");
        let single = check_frame_shape(&WindowFrameBound::Following(offset_expr()), None);
        assert_eq!(message_of(single), "frame starting from following row cannot end with current row");
    }

    #[test]
    fn current_start_with_preceding_end_is_rejected() {
        let r = check_frame_shape(&WindowFrameBound::CurrentRow, Some(&WindowFrameBound::Preceding(offset_expr())));
        assert_eq!(message_of(r), "frame starting from current row cannot have preceding rows");
    }

    #[test]
    fn valid_frames_are_accepted() {
        let ok = check_frame_shape(&WindowFrameBound::Preceding(offset_expr()), None);
        assert_eq!(message_of(ok), "<ok>");
        let ok2 = check_frame_shape(&WindowFrameBound::Preceding(None), Some(&WindowFrameBound::Following(None)));
        assert_eq!(message_of(ok2), "<ok>");
    }

    #[test]
    fn range_offset_candidates_follow_in_range_support() {
        let int4 = range_offset_candidates(oids::INT4);
        assert!(int4.contains(&oids::INT8) && int4.contains(&oids::INT4) && int4.contains(&oids::INT2));
        assert_eq!(range_offset_candidates(oids::TIMESTAMPTZ), &[oids::INTERVAL][..]);
        assert_eq!(range_offset_candidates(oids::FLOAT4), &[oids::FLOAT8][..]);
        assert!(range_offset_candidates(oids::TEXT).is_empty());
    }

    #[test]
    fn range_offset_unsupported_column_type_error_text() {
        let r = select_offset_type(oids::TEXT, oids::INT4, &[], &[], "text", "integer", None);
        match r {
            Err(e) => {
                assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
                assert_eq!(e.message, "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text");
            }
            Ok(_) => panic!("text ordering must not accept RANGE offsets"),
        }
    }

    #[test]
    fn range_offset_offset_type_mismatch_error_text() {
        let cands = range_offset_candidates(oids::DATE);
        let r = select_offset_type(oids::DATE, oids::INT4, cands, &[false], "date", "integer", None);
        match r {
            Err(e) => {
                assert_eq!(
                    e.message,
                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type date and offset type integer"
                );
                assert_eq!(e.hint.as_deref(), Some("Cast the offset value to an appropriate type."));
            }
            Ok(_) => panic!("integer offset must not match a date column"),
        }
    }

    #[test]
    fn range_offset_type_selection() {
        let cands = range_offset_candidates(oids::INT4);
        // Unknown literal: the column type is preferred.
        assert_eq!(
            select_offset_type(oids::INT4, oids::UNKNOWN, cands, &[true, true, true], "integer", "unknown", None)
                .ok(),
            Some(oids::INT4)
        );
        // int2 offset on an int4 column: exact match wins.
        assert_eq!(
            select_offset_type(oids::INT4, oids::INT2, cands, &[true, true, true], "integer", "smallint", None).ok(),
            Some(oids::INT2)
        );
        // int8 offset on an int4 column: only int8 matches implicitly.
        assert_eq!(
            select_offset_type(oids::INT4, oids::INT8, cands, &[true, false, false], "integer", "bigint", None).ok(),
            Some(oids::INT8)
        );
    }

    #[test]
    fn const_offset_checks() {
        let neg = BoundExpr::constant(Datum::Int8(-1), TypeId::INT8);
        match check_const_offset(&neg, true) {
            Err(e) => {
                assert_eq!(e.sqlstate, SqlState(*b"22013"));
                assert_eq!(e.message, "frame starting offset must not be negative");
            }
            Ok(()) => panic!("negative ROWS offset must be rejected"),
        }
        let ok = BoundExpr::constant(Datum::Int8(0), TypeId::INT8);
        assert!(check_const_offset(&ok, false).is_ok());
    }
}
