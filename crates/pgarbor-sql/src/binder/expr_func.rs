//! Function calls (SQL-12, sonnet). PG references: `parse_func.c` (`ParseFuncOrColumn`,
//! `func_get_detail`, `FuncNameAsType`, `make_fn_arguments`, `check_srf_call_placement`),
//! `parse_expr.c` (`transformFuncCall`, `transformSQLValueFunction`, `transformMinMaxExpr`,
//! `transformCoalesceExpr`, `transformXmlExpr` N/A), `gram.y` `func_expr_common_subexpr` (the
//! special-syntax functions), `parse_agg.c` for aggregate detection.
//!
//! ## `bind_function_call` (sqlparser `Expr::Function(Function)`)
//! `Function { name: ObjectName, args: FunctionArguments, filter, over: Option<WindowType>,
//! within_group, null_treatment, parameters, uses_odbc_syntax }`.
//! 1. Name: 1..3 parts (`Binder::qual`); `pg_catalog.f` allowed explicitly.
//! 2. **Special names** (unqualified or `pg_catalog.`-qualified, case-folded) handled without
//!    function lookup: `coalesce`, `greatest`, `least`, `nullif` (-> `expr_misc::bind_coalesce/
//!    bind_minmax/bind_nullif`), `current_date`, `current_time`, `current_timestamp`, `localtime`,
//!    `localtimestamp`, `current_role`, `current_user`, `session_user`, `user`, `current_catalog`,
//!    `current_schema` (with optional precision argument for the time ones) -> [`bind_sql_value_function`],
//!    `grouping` -> `expr_agg::bind_grouping`, `array` with a sub-select argument ->
//!    `expr_sublink::bind_array_subquery`, `row` -> `expr_misc::bind_row`.
//! 3. Argument forms: positional, named (`name => expr`, `name := expr`), `*` (only `count(*)` and
//!    other aggregates, else 42601), `VARIADIC expr` (sqlparser: `FunctionArgExpr` preceded by the
//!    `VARIADIC` keyword in `FunctionArgumentList`; check how 0.63 exposes it, otherwise 0A000),
//!    `DISTINCT`/`ALL` quantifier, `ORDER BY` inside the call, `ON OVERFLOW` (not PG).
//! 4. **Resolve** with `candidates = env.function_candidates(schema, name)?` (search_path order) and
//!    `pgarbor_types::resolve::func::resolve_function(env, &FuncCall{..}, &cands)`. Not found ->
//!    42883 `function foo(integer) does not exist` + HINT `No function matches the given name and
//!    argument types. You might need to add explicit type casts.` (from the types crate), position =
//!    function name. If the name matches no function but matches a TYPE name and there is one
//!    argument, treat as the cast `type(arg)` (`FuncNameAsType`: `int4(x)`, `text(x)`; display form
//!    `ExplicitCall`). If the call has the single-argument form `f(rel)` where `f` is a column of
//!    that relation (`col(t)` attribute-notation) -> field selection (rare; optional).
//! 5. **Kind dispatch** on `ResolvedFunc::kind`: `Aggregate` -> `expr_agg::bind_aggregate` (uses the
//!    already resolved function); `Window` -> requires OVER (else 42809 `window function X requires
//!    an OVER clause`); `Procedure` -> 42809 `X is a procedure` + HINT `To call a procedure, use
//!    CALL.`; `Function`: with OVER it is an error (42809 `OVER specified, but X is not a window
//!    function nor an aggregate function`); with FILTER/WITHIN GROUP/ORDER BY/DISTINCT inside:
//!    42809 `FILTER specified, but X is not an aggregate function` / `WITHIN GROUP is required for ...`
//!    / `DISTINCT specified, but X is not an aggregate function` / `ORDER BY specified, but X is not an
//!    aggregate function`.
//! 6. **Build** ([`build_call`]): coerce supplied args to `coerce_to` (Implicit; unknown literals
//!    convert), reorder by `arg_order` (named notation), append defaults (`ndefaults_used`: bind the
//!    default expression text `BuiltinProc::arg_defaults` — comma separated SQL — with
//!    `parser::parse_expr_text` + `bind_expr` in an empty scope; user functions: from the catalog
//!    `Function` defaults), collect the last `nvariadic` args into ONE `ArrayExpr` argument unless
//!    VARIADIC "any", set `FuncCallExpr { func, name, args, variadic, retset, volatility, strict,
//!    is_cast_call }`. Result `ty` = `ResolvedFunc::ret`.
//! 7. **Set-returning functions**: if `retset`: allowed in FROM (handled by `from`) and in the
//!    SELECT list when `ec.srf_ok` (sets `scope.has_target_srfs`); elsewhere 0A000 `set-returning
//!    functions are not allowed in WHERE` (`diag::srf_not_allowed`, clause from `ec.clause`); nested
//!    inside another function's arguments in the SELECT list is allowed (PG 10+ ProjectSet);
//!    inside CASE/COALESCE: 0A000 `set-returning functions are not allowed in CASE` (HINT
//!    `You might be able to move the set-returning function into a LATERAL FROM item.`).
//!
//! ## Special syntax
//! * `EXTRACT(field FROM source)` -> `extract(text, source)` with `field` a lower-case text constant
//!   (PG 14+: result `numeric`). `DateTimeField::{Year,Month,Day,Hour,Minute,Second,Epoch,Dow,Doy,
//!   Isodow,Isoyear,Week,Quarter,Century,Decade,Millennium,Microseconds,Milliseconds,Julian,Timezone,
//!   TimezoneHour,TimezoneMinute,Custom(Ident)}`; custom string field allowed (`'year'`).
//! * `SUBSTRING(s FROM a FOR b)` / `substring(s, a, b)` -> `substring(text,int4,int4)` ; `SUBSTRING(s
//!   FROM 'pat' [FOR 'esc'])` / `substring(s similar pat escape esc)` -> `substring(text,text[,text])`
//!   disambiguated by the type of the second argument; `POSITION(a IN b)` -> `position(b, a)`
//!   (arguments swapped, function name `position`); `OVERLAY(s PLACING r FROM a [FOR n])` ->
//!   `overlay`; `TRIM([BOTH|LEADING|TRAILING] [chars] FROM s)` -> `btrim`/`ltrim`/`rtrim` with
//!   `(s [, chars])`; `CEIL(x)`/`FLOOR(x)` plain `ceil`/`floor` (the `CEIL(x TO field)` and
//!   `DateTimeField` forms are not PG: 42601).
//! * `COLLATION FOR (expr)`, `IS NORMALIZED`, `XMLELEMENT`...: 0A000.
//!
//! ## Helpers used by other modules
//! [`call_function_by_name`] resolves `pg_catalog.<name>(args)` for internal rewrites (BETWEEN no,
//! AT TIME ZONE -> `timezone`, SIMILAR TO -> `similar_to_escape`, LIKE ESCAPE -> `like_escape`).
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::catalog_data::{self, CastContext, ProcKind, Volatility};
use pgarbor_types::oids;
use pgarbor_types::registry;
use pgarbor_types::resolve::func::{self, FuncCall, FuncCandidate, ResolvedFunc};
use pgarbor_types::{Datum, TypeId};
use sqlparser::ast::{
    CeilFloorKind, DateTimeField, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArguments, TrimWhereField, Value,
};

use super::context::{Binder, BoundArg, ExprCtx};
use super::coerce;
use super::expr::{bind_arg, bind_expr};
use super::expr_agg;
use super::expr_misc;
use super::expr_sublink;
use crate::diag::{self, ExprClause};
use crate::ir::{ArrayExpr, BoundExpr, CoercionForm, ExprKind, FuncCallExpr, SqlValueOp};

/// `pg_catalog` schema name (builtin procedures and the special-syntax rewrites).
const PG_CATALOG: &str = "pg_catalog";
/// First OID of user-defined objects; builtin procs are below it.
const FIRST_USER_OID: Oid = 16384;
/// `DEFAULT_SLOT` of `ResolvedFunc::arg_order` (a declared parameter filled from its default).
const DEFAULT_SLOT: usize = usize::MAX;
/// Maximum precision of `TIME` / `TIMESTAMP` (PG `MAX_TIME_PRECISION`, `MAX_TIMESTAMP_PRECISION`).
const MAX_TIME_PRECISION: i32 = 6;

/// Static facts about a callable (builtin `pg_proc` row or catalog function) needed to build nodes.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ProcInfo {
    pub oid: Oid,
    pub name: String,
    pub volatility: Volatility,
    pub strict: bool,
    pub retset: bool,
    pub ret: Oid,
    /// Defaults text for the trailing parameters (builtin: `arg_defaults`; user: stored exprs).
    pub arg_defaults: Vec<String>,
}

/// Look up the [`ProcInfo`] of a function OID: `catalog_data::builtin_proc` for OIDs below 16384,
/// else the catalog snapshot's `Function`. XX000 if unknown.
pub fn proc_info(b: &Binder, oid: Oid) -> Result<ProcInfo> {
    if oid < FIRST_USER_OID {
        let p = catalog_data::builtin_proc(oid)
            .ok_or_else(|| PgError::internal(format!("unknown builtin function oid {oid}")))?;
        return Ok(ProcInfo {
            oid,
            name: p.name.to_string(),
            volatility: p.volatility,
            strict: p.strict,
            retset: p.retset,
            ret: p.ret,
            arg_defaults: split_defaults(p.arg_defaults),
        });
    }
    let f = b
        .env
        .snapshot()
        .function(oid)
        .ok_or_else(|| PgError::internal(format!("unknown function oid {oid}")))?;
    let arg_defaults: Vec<String> = f
        .args
        .iter()
        .filter(|a| matches!(a.mode.as_char(), b'i' | b'b' | b'v'))
        .filter_map(|a| a.default.as_ref().map(|d| d.sql.clone()))
        .collect();
    Ok(ProcInfo {
        oid,
        name: f.name.clone(),
        volatility: f.volatility.to_types(),
        strict: f.strict,
        retset: f.retset,
        ret: f.ret,
        arg_defaults,
    })
}

/// Declared input parameter types of a resolved function (`proargtypes` / catalog in-args).
fn declared_arg_types(b: &Binder, oid: Oid) -> Result<Vec<Oid>> {
    if oid < FIRST_USER_OID {
        let p = catalog_data::builtin_proc(oid)
            .ok_or_else(|| PgError::internal(format!("unknown builtin function oid {oid}")))?;
        return Ok(p.arg_types.to_vec());
    }
    let f = b
        .env
        .snapshot()
        .function(oid)
        .ok_or_else(|| PgError::internal(format!("unknown function oid {oid}")))?;
    Ok(f.in_arg_types())
}

/// Split the comma-separated default expression list of `pg_proc.proargdefaults` (commas inside
/// parentheses, brackets, string literals and quoted identifiers do not split).
fn split_defaults(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut in_ident = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            cur.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    cur.push('\'');
                    chars.next();
                } else {
                    in_str = false;
                }
            }
        } else if in_ident {
            cur.push(c);
            if c == '"' {
                in_ident = false;
            }
        } else {
            match c {
                '\'' => {
                    in_str = true;
                    cur.push(c);
                }
                '"' => {
                    in_ident = true;
                    cur.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Bind `Expr::Function` (module docs steps 1-7).
pub fn bind_function_call(b: &mut Binder, f: &Function, ec: ExprCtx) -> Result<BoundExpr> {
    let qn = b.qual(&f.name)?;
    let pos = qn.pos;
    if let Some(cat) = &qn.catalog {
        if *cat != b.ctx.session.database {
            let mut q = cat.clone();
            if let Some(s) = &qn.schema {
                q.push('.');
                q.push_str(s);
            }
            q.push('.');
            q.push_str(&qn.name);
            return Err(diag::cross_database(&q, pos));
        }
    }
    let name: String = qn.name.clone();
    let schema: Option<&str> = qn.schema.as_deref();
    let qdisp: String = qn.display();

    // Step 2: special names (no function lookup, no OVER/FILTER/WITHIN GROUP decoration).
    let special_schema = qn.schema.is_none() || qn.schema.as_deref() == Some(PG_CATALOG);
    if special_schema && f.over.is_none() && f.filter.is_none() && f.within_group.is_empty() {
        if let Some(e) = bind_special_function(b, &name, f, pos, ec)? {
            return Ok(e);
        }
    }

    let flags = call_flags(f, pos)?;
    let variadic_kw = variadic_call(f, pos)?;
    let cands = b
        .env
        .function_candidates(schema, &name)
        .map_err(|e| diag::with_position(e, pos))?;
    let is_agg_name = cands.iter().any(|c| c.kind == ProcKind::Aggregate);
    let star = is_star_call(f);

    // Step 3: bind the arguments. Arguments of an aggregate are bound as aggregate arguments
    // (`agg_depth` > 0 for nested-aggregate detection, no aggregates/windows inside).
    let (args, names): (Vec<BoundArg>, Vec<Option<String>>) = if star {
        (Vec::new(), Vec::new())
    } else {
        let saved_depth = b.agg_depth;
        if is_agg_name {
            b.agg_depth += 1;
        }
        let arg_ec = if is_agg_name { ec.inside_aggregate() } else { ec };
        let bound = bind_call_args(b, f, arg_ec, pos);
        b.agg_depth = saved_depth;
        bound?
    };

    // Step 4: resolve.
    let arg_oids: Vec<Oid> = args.iter().map(|a| a.ty().oid).collect();
    let resolved = match resolve_with(b, &cands, &name, arg_oids.clone(), names.clone(), variadic_kw, pos) {
        Ok(r) => r,
        Err(e) => {
            if e.sqlstate == SqlState::UNDEFINED_FUNCTION && args.len() == 1 && names.iter().all(|n| n.is_none()) {
                if let Some(cast) = function_style_cast(b, schema, &name, &args[0])? {
                    return Ok(cast);
                }
            }
            return Err(e);
        }
    };

    // Step 5: kind dispatch.
    if variadic_kw && matches!(resolved.kind, ProcKind::Aggregate | ProcKind::Window) {
        return Err(PgError::unimplemented("SQL-12", format!("VARIADIC call of aggregate or window function {qdisp}")));
    }
    match resolved.kind {
        ProcKind::Procedure => {
            let sig = func::signature_string(b.env, &name, &arg_oids)?;
            return Err(diag::with_position(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is a procedure"))
                    .with_hint("To call a procedure, use CALL."),
                pos,
            ));
        }
        ProcKind::Function => {
            if star {
                return Err(wrong_kind(format!("{qdisp}(*) specified, but {qdisp} is not an aggregate function"), pos));
            }
            if flags.distinct {
                return Err(wrong_kind(format!("DISTINCT specified, but {qdisp} is not an aggregate function"), pos));
            }
            if !f.within_group.is_empty() {
                return Err(wrong_kind(format!("WITHIN GROUP specified, but {qdisp} is not an aggregate function"), pos));
            }
            if flags.order_by {
                return Err(wrong_kind(format!("ORDER BY specified, but {qdisp} is not an aggregate function"), pos));
            }
            if f.filter.is_some() {
                return Err(wrong_kind(format!("FILTER specified, but {qdisp} is not an aggregate function"), pos));
            }
            if f.over.is_some() {
                return Err(wrong_kind(
                    format!("OVER specified, but {qdisp} is not a window function nor an aggregate function"),
                    pos,
                ));
            }
            check_srf_placement(b, resolved.retset, ec, pos)?;
            build_call(b, &resolved, &name, args, variadic_kw, false, pos, ec)
        }
        ProcKind::Aggregate => {
            if f.over.is_none() && !ec.aggs_ok {
                return Err(if b.agg_depth > 0 {
                    diag::nested_aggregate(pos)
                } else {
                    diag::agg_not_allowed(ec.clause, pos)
                });
            }
            match &f.over {
                Some(over) => {
                    if !ec.windows_ok {
                        return Err(diag::window_not_allowed(ec.clause, pos));
                    }
                    expr_agg::bind_window_call(b, f, &resolved, args, over, ec)
                }
                None => expr_agg::bind_aggregate(b, f, &resolved, args, ec),
            }
        }
        ProcKind::Window => {
            let over = match &f.over {
                Some(o) => o,
                None => return Err(wrong_kind(format!("window function {qdisp} requires an OVER clause"), pos)),
            };
            if !f.within_group.is_empty() {
                return Err(wrong_kind(format!("window function {qdisp} cannot have WITHIN GROUP"), pos));
            }
            if !ec.windows_ok {
                return Err(diag::window_not_allowed(ec.clause, pos));
            }
            expr_agg::bind_window_call(b, f, &resolved, args, over, ec)
        }
    }
}

/// Resolve and build a plain (non-aggregate) call from already bound arguments:
/// `resolve_function` + [`build_call`]. `arg_names[i]` is the named-notation name of argument i.
/// `variadic_kw` = call used `VARIADIC`. Aggregates/windows must not come through here.
pub fn resolve_and_build_call(
    b: &mut Binder,
    schema: Option<&str>,
    name: &str,
    args: Vec<BoundArg>,
    arg_names: Vec<Option<String>>,
    variadic_kw: bool,
    pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let cands = b
        .env
        .function_candidates(schema, name)
        .map_err(|e| diag::with_position(e, pos))?;
    let arg_oids: Vec<Oid> = args.iter().map(|a| a.ty().oid).collect();
    let resolved = resolve_with(b, &cands, name, arg_oids.clone(), arg_names, variadic_kw, pos)?;
    match resolved.kind {
        ProcKind::Function => {}
        ProcKind::Aggregate => return Err(diag::agg_not_allowed(ec.clause, pos)),
        ProcKind::Window => return Err(diag::window_not_allowed(ec.clause, pos)),
        ProcKind::Procedure => {
            let sig = func::signature_string(b.env, name, &arg_oids)?;
            return Err(diag::with_position(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is a procedure"))
                    .with_hint("To call a procedure, use CALL."),
                pos,
            ));
        }
    }
    check_srf_placement(b, resolved.retset, ec, pos)?;
    build_call(b, &resolved, name, args, variadic_kw, false, pos, ec)
}

/// Build the `FuncCall` node for an already resolved function (module docs step 6): coercions,
/// named-argument reordering, defaults, variadic packing, flags.
pub fn build_call(
    b: &mut Binder,
    resolved: &ResolvedFunc,
    name: &str,
    args: Vec<BoundArg>,
    variadic_kw: bool,
    is_cast_call: bool,
    pos: Option<u32>,
    ec: ExprCtx,
) -> Result<BoundExpr> {
    let _ = ec;
    let info = proc_info(b, resolved.oid)?;
    let declared = declared_arg_types(b, resolved.oid)?;
    let nparams = declared.len();
    let nargs = args.len();
    if resolved.coerce_to.len() != nargs {
        return Err(PgError::internal("binder: coercion list does not match the call arguments"));
    }

    // Coerce every supplied argument (call order) to the resolved input type.
    let mut coerced: Vec<Option<BoundExpr>> = Vec::with_capacity(nargs);
    for (j, a) in args.iter().enumerate() {
        let target = TypeId::of(resolved.coerce_to[j]);
        let e = coerce::coerce_strict(b, a, target, CastContext::Implicit, CoercionForm::Implicit)?;
        coerced.push(Some(e));
    }

    // VARIADIC "any" (concat, format, json_build_array, num_nulls, ...): PG passes the trailing
    // arguments through unpacked, so the call may carry more args than declared parameters. The
    // resolver expands `arg_order` to one entry per call argument in that case (identity order, no
    // packing, `nvariadic == 0`).
    if nargs > nparams && resolved.nvariadic == 0 {
        if resolved.arg_order.len() != nargs {
            return Err(PgError::internal("binder: more call arguments than declared parameters"));
        }
        let mut out: Vec<BoundExpr> = Vec::with_capacity(nargs);
        for j in 0..nargs {
            out.push(take_arg(&mut coerced, j)?);
        }
        return Ok(BoundExpr::new(
            ExprKind::FuncCall(Box::new(FuncCallExpr {
                func: resolved.oid,
                name: name.to_string(),
                args: out,
                variadic: variadic_kw,
                retset: resolved.retset,
                volatility: info.volatility,
                strict: info.strict,
                is_cast_call,
            })),
            resolved.ret,
        ));
    }

    // Place the arguments into declared parameter order.
    let mut slots: Vec<Option<BoundExpr>> = Vec::new();
    slots.resize_with(nparams, || None);
    let mut packed = false;
    if resolved.nvariadic > 0 {
        let fixed = nparams
            .checked_sub(1)
            .ok_or_else(|| PgError::internal("variadic call without a variadic parameter"))?;
        if fixed + resolved.nvariadic != nargs {
            return Err(PgError::internal("binder: variadic argument count mismatch"));
        }
        for p in 0..fixed {
            slots[p] = Some(take_arg(&mut coerced, p)?);
        }
        let mut elements: Vec<BoundExpr> = Vec::with_capacity(resolved.nvariadic);
        for j in fixed..nargs {
            elements.push(take_arg(&mut coerced, j)?);
        }
        let elem = resolved.coerce_to[fixed];
        let arr = registry::array_type_of(b.env, elem).ok_or_else(|| {
            diag::with_position(
                PgError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("could not find array type for data type {}", b.type_name(TypeId::of(elem))),
                ),
                pos,
            )
        })?;
        slots[nparams - 1] = Some(BoundExpr::new(
            ExprKind::Array(Box::new(ArrayExpr { elements, elem_type: elem, multidims: false })),
            TypeId::of(arr),
        ));
        packed = true;
    } else if resolved.arg_order.len() == nparams {
        for (p, idx) in resolved.arg_order.iter().enumerate() {
            if *idx == DEFAULT_SLOT {
                continue;
            }
            slots[p] = Some(take_arg(&mut coerced, *idx)?);
        }
    } else {
        if nargs > nparams {
            return Err(PgError::internal("binder: more call arguments than declared parameters"));
        }
        for p in 0..nargs {
            slots[p] = Some(take_arg(&mut coerced, p)?);
        }
    }

    // Append the defaults of the parameters that were not supplied.
    let nd = info.arg_defaults.len();
    if nd > nparams {
        return Err(PgError::internal(format!("binder: too many default expressions for {name}")));
    }
    let first_default = nparams - nd;
    for p in 0..nparams {
        if slots[p].is_some() {
            continue;
        }
        if p < first_default {
            return Err(PgError::internal(format!("binder: no value for argument {} of {name}", p + 1)));
        }
        let bound = bind_default(b, &info.arg_defaults[p - first_default])?;
        let v = coerce::coerce_strict(
            b,
            &BoundArg::new(bound, None),
            TypeId::of(declared[p]),
            CastContext::Implicit,
            CoercionForm::Implicit,
        )?;
        slots[p] = Some(v);
    }

    let mut out: Vec<BoundExpr> = Vec::with_capacity(nparams);
    for s in slots {
        match s {
            Some(e) => out.push(e),
            None => return Err(PgError::internal("binder: unfilled function argument")),
        }
    }
    Ok(BoundExpr::new(
        ExprKind::FuncCall(Box::new(FuncCallExpr {
            func: resolved.oid,
            name: name.to_string(),
            args: out,
            variadic: variadic_kw || packed,
            retset: resolved.retset,
            volatility: info.volatility,
            strict: info.strict,
            is_cast_call,
        })),
        resolved.ret,
    ))
}

/// `pg_catalog.<name>(args...)` for internal rewrites (args positional, Implicit coercions).
pub fn call_function_by_name(b: &mut Binder, name: &str, args: Vec<BoundArg>, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    pg_call(b, name, args, pos, ec)
}

/// `CURRENT_DATE` & co. `precision`: optional typmod argument (`CURRENT_TIMESTAMP(3)`; > 6 is a
/// WARNING `TIMESTAMP(7) precision reduced to maximum allowed, 6` and clamps). Result types:
/// current_date `date`, current_time `timetz`, current_timestamp `timestamptz`, localtime `time`,
/// localtimestamp `timestamp`, current_role/current_user/session_user/user `name`,
/// current_catalog `name`, current_schema `name` (typmod from precision for the time ones).
pub fn bind_sql_value_function(b: &mut Binder, op: SqlValueOp, precision: Option<i32>, pos: Option<u32>) -> Result<BoundExpr> {
    // (type OID, time kind name and whether WITH TIME ZONE applies) for the precision checks.
    let (oid, prec_kind): (Oid, Option<(&'static str, bool)>) = match op {
        SqlValueOp::CurrentDate => (oids::DATE, None),
        SqlValueOp::CurrentTime => (oids::TIMETZ, Some(("TIME", true))),
        SqlValueOp::CurrentTimestamp => (oids::TIMESTAMPTZ, Some(("TIMESTAMP", true))),
        SqlValueOp::LocalTime => (oids::TIME, Some(("TIME", false))),
        SqlValueOp::LocalTimestamp => (oids::TIMESTAMP, Some(("TIMESTAMP", false))),
        SqlValueOp::CurrentRole
        | SqlValueOp::CurrentUser
        | SqlValueOp::SessionUser
        | SqlValueOp::User
        | SqlValueOp::CurrentCatalog
        | SqlValueOp::CurrentSchema => (oids::NAME, None),
    };
    let typmod = match (precision, prec_kind) {
        (Some(p), Some((kind, istz))) => anytime_typmod(b, kind, istz, p, pos)?,
        _ => -1,
    };
    Ok(BoundExpr::new(ExprKind::SqlValue(op), TypeId::new(oid, typmod)))
}

/// `Extract`, `Substring`, `Position`, `Overlay`, `Trim`, `Ceil`, `Floor` forms (module docs
/// "Special syntax"). `e` is the whole `Expr`; only those variants are passed.
pub fn bind_special_syntax(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    let pos = b.pos(e);
    match e {
        Expr::Extract { field, expr, .. } => {
            let field_arg = BoundArg::new(
                BoundExpr::constant(Datum::text(extract_field_name(field)), TypeId::of(oids::UNKNOWN)),
                pos,
            );
            let src = bind_arg(b, expr, ec)?;
            pg_call(b, "extract", vec![field_arg, src], pos, ec)
        }
        Expr::Substring { expr, substring_from, substring_for, shorthand, .. } => {
            let mut args = vec![bind_arg(b, expr, ec)?];
            if let Some(from) = substring_from {
                args.push(bind_arg(b, from, ec)?);
            }
            if let Some(for_expr) = substring_for {
                args.push(bind_arg(b, for_expr, ec)?);
            }
            let name = if *shorthand { "substr" } else { "substring" };
            pg_call(b, name, args, pos, ec)
        }
        Expr::Position { expr, r#in } => {
            // PG: POSITION(needle IN haystack) -> position(haystack, needle).
            let haystack = bind_arg(b, r#in, ec)?;
            let needle = bind_arg(b, expr, ec)?;
            pg_call(b, "position", vec![haystack, needle], pos, ec)
        }
        Expr::Overlay { expr, overlay_what, overlay_from, overlay_for } => {
            let mut args = vec![
                bind_arg(b, expr, ec)?,
                bind_arg(b, overlay_what, ec)?,
                bind_arg(b, overlay_from, ec)?,
            ];
            if let Some(for_expr) = overlay_for {
                args.push(bind_arg(b, for_expr, ec)?);
            }
            pg_call(b, "overlay", args, pos, ec)
        }
        Expr::Trim { trim_where, trim_what, expr, trim_characters } => {
            let name = match trim_where {
                Some(TrimWhereField::Leading) => "ltrim",
                Some(TrimWhereField::Trailing) => "rtrim",
                _ => "btrim",
            };
            let mut args = vec![bind_arg(b, expr, ec)?];
            if let Some(what) = trim_what {
                args.push(bind_arg(b, what, ec)?);
            } else if let Some(chars) = trim_characters {
                match chars.as_slice() {
                    [] => {}
                    [one] => args.push(bind_arg(b, one, ec)?),
                    _ => return Err(diag::syntax("syntax error", pos)),
                }
            }
            pg_call(b, name, args, pos, ec)
        }
        Expr::Ceil { expr, field } => ceil_floor(b, "ceil", expr, field, pos, ec),
        Expr::Floor { expr, field } => ceil_floor(b, "floor", expr, field, pos, ec),
        _ => Err(PgError::internal("binder: bind_special_syntax called on a non-special expression")),
    }
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Flags of the aggregate-only decorations of one call (checked for non-aggregates).
struct CallFlags {
    distinct: bool,
    order_by: bool,
}

/// Is this argument the parser's `VARIADIC` marker (`crate::parser::VARIADIC_MARKER`)?
fn is_variadic_marker(a: &FunctionArg) -> bool {
    let id = match a {
        FunctionArg::Named { name, .. } => Some(name),
        FunctionArg::ExprNamed { name: Expr::Identifier(id), .. } => Some(id),
        _ => None,
    };
    id.is_some_and(|i| i.quote_style == Some('"') && i.value == crate::parser::VARIADIC_MARKER)
}

/// Does the call use the VARIADIC keyword (`f(a, VARIADIC arr)`)? PG grammar allows it only as the
/// last argument, so a marker anywhere else is a syntax error.
fn variadic_call(f: &Function, pos: Option<u32>) -> Result<bool> {
    let FunctionArguments::List(list) = &f.args else {
        return Ok(false);
    };
    let marks: Vec<usize> = list.args.iter().enumerate().filter(|(_, a)| is_variadic_marker(a)).map(|(i, _)| i).collect();
    match marks.as_slice() {
        [] => Ok(false),
        [i] if *i + 1 == list.args.len() => Ok(true),
        _ => Err(diag::syntax("syntax error", pos)),
    }
}

fn call_flags(f: &Function, pos: Option<u32>) -> Result<CallFlags> {
    let mut flags = CallFlags { distinct: false, order_by: false };
    if let FunctionArguments::List(list) = &f.args {
        flags.distinct = list.duplicate_treatment == Some(DuplicateTreatment::Distinct);
        for c in &list.clauses {
            match c {
                FunctionArgumentClause::OrderBy(_) => flags.order_by = true,
                FunctionArgumentClause::IgnoreOrRespectNulls(_) => {}
                _ => return Err(diag::syntax("syntax error", pos)),
            }
        }
    }
    Ok(flags)
}

/// `f(*)`: the single wildcard argument.
fn is_star_call(f: &Function) -> bool {
    match &f.args {
        FunctionArguments::List(list) => {
            list.duplicate_treatment.is_none()
                && list.clauses.is_empty()
                && matches!(list.args.as_slice(), [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)])
        }
        _ => false,
    }
}

/// Whether the call has any argument at all.
fn has_args(f: &Function) -> bool {
    match &f.args {
        FunctionArguments::None => false,
        FunctionArguments::List(list) => !list.args.is_empty(),
        FunctionArguments::Subquery(_) => true,
    }
}

/// The plain positional expressions of a call (for the special forms that take a fixed argument
/// list). Named arguments, `*`, DISTINCT and ORDER BY inside the call are syntax errors.
fn plain_exprs(f: &Function, pos: Option<u32>) -> Result<Vec<Expr>> {
    let list = match &f.args {
        FunctionArguments::None => return Ok(Vec::new()),
        FunctionArguments::Subquery(_) => return Err(diag::syntax("syntax error", pos)),
        FunctionArguments::List(list) => list,
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() || !f.within_group.is_empty() {
        return Err(diag::syntax("syntax error", pos));
    }
    let mut out: Vec<Expr> = Vec::with_capacity(list.args.len());
    for a in &list.args {
        match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => out.push(e.clone()),
            _ => return Err(diag::syntax("syntax error", pos)),
        }
    }
    Ok(out)
}

/// Special names handled without function lookup (module docs step 2). `None` when `name` is an
/// ordinary function name.
fn bind_special_function(b: &mut Binder, name: &str, f: &Function, pos: Option<u32>, ec: ExprCtx) -> Result<Option<BoundExpr>> {
    let e = match name {
        "coalesce" => {
            let exprs = plain_exprs(f, pos)?;
            expr_misc::bind_coalesce(b, &exprs, pos, ec)?
        }
        "greatest" | "least" => {
            let exprs = plain_exprs(f, pos)?;
            expr_misc::bind_minmax(b, name == "greatest", &exprs, pos, ec)?
        }
        "nullif" => {
            let exprs = plain_exprs(f, pos)?;
            expr_misc::bind_nullif(b, &exprs, pos, ec)?
        }
        "current_date" | "current_time" | "current_timestamp" | "localtime" | "localtimestamp" => {
            let op = match name {
                "current_date" => SqlValueOp::CurrentDate,
                "current_time" => SqlValueOp::CurrentTime,
                "current_timestamp" => SqlValueOp::CurrentTimestamp,
                "localtime" => SqlValueOp::LocalTime,
                _ => SqlValueOp::LocalTimestamp,
            };
            let precision = if name == "current_date" {
                if has_args(f) {
                    return Err(diag::syntax("syntax error", pos));
                }
                None
            } else {
                sql_value_precision(f, pos)?
            };
            bind_sql_value_function(b, op, precision, pos)?
        }
        "current_role" | "current_user" | "session_user" | "user" | "current_catalog" | "current_schema" => {
            let op = match name {
                "current_role" => SqlValueOp::CurrentRole,
                "current_user" => SqlValueOp::CurrentUser,
                "session_user" => SqlValueOp::SessionUser,
                "user" => SqlValueOp::User,
                "current_catalog" => SqlValueOp::CurrentCatalog,
                _ => SqlValueOp::CurrentSchema,
            };
            if has_args(f) {
                return Err(diag::syntax("syntax error", pos));
            }
            bind_sql_value_function(b, op, None, pos)?
        }
        "grouping" => expr_agg::bind_grouping(b, f, ec)?,
        "row" => {
            let exprs = plain_exprs(f, pos)?;
            expr_misc::bind_row(b, &exprs, ec)?
        }
        "array" => match &f.args {
            FunctionArguments::Subquery(q) => expr_sublink::bind_array_subquery(b, q, ec)?,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(e))
}

/// Precision argument of `CURRENT_TIME(n)` & co.: exactly one unsigned integer literal.
fn sql_value_precision(f: &Function, pos: Option<u32>) -> Result<Option<i32>> {
    let list = match &f.args {
        FunctionArguments::None => return Ok(None),
        FunctionArguments::List(list) => list,
        FunctionArguments::Subquery(_) => return Err(diag::syntax("syntax error", pos)),
    };
    let [arg] = list.args.as_slice() else {
        return Err(diag::syntax("syntax error", pos));
    };
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v))) = arg else {
        return Err(diag::syntax("syntax error", pos));
    };
    let Value::Number(text, _) = &v.value else {
        return Err(diag::syntax("syntax error", pos));
    };
    let n: i64 = text.parse().map_err(|_| diag::syntax("syntax error", pos))?;
    Ok(Some(n.clamp(i32::MIN as i64, i32::MAX as i64) as i32))
}

/// PG `anytime_typmod_check` / `anytimestamp_typmod_check` (date.c, timestamp.c): negative is an
/// error, above the maximum is a WARNING and clamps.
fn anytime_typmod(b: &mut Binder, kind: &str, istz: bool, typmod: i32, pos: Option<u32>) -> Result<i32> {
    let suffix = if istz { " WITH TIME ZONE" } else { "" };
    if typmod < 0 {
        return Err(diag::invalid_parameter_value(
            format!("{kind}({typmod}){suffix} precision must not be negative"),
            pos,
        ));
    }
    if typmod > MAX_TIME_PRECISION {
        b.notice(
            PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("{kind}({typmod}){suffix} precision reduced to maximum allowed, {MAX_TIME_PRECISION}"),
            )
            .with_severity(Severity::Warning),
        );
        return Ok(MAX_TIME_PRECISION);
    }
    Ok(typmod)
}

/// The `EXTRACT` field as PG's lower-case text constant.
fn extract_field_name(field: &DateTimeField) -> String {
    match field {
        DateTimeField::Custom(id) => id.value.to_lowercase(),
        DateTimeField::Week(_) => "week".to_string(),
        DateTimeField::Millenium | DateTimeField::Millennium => "millennium".to_string(),
        other => other.to_string().to_lowercase(),
    }
}

/// `CEIL(x)` / `FLOOR(x)` and the scale form `CEIL(x, s)` (which is not PG and fails to resolve).
fn ceil_floor(b: &mut Binder, name: &str, expr: &Expr, field: &CeilFloorKind, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    let mut args = vec![bind_arg(b, expr, ec)?];
    match field {
        CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) => {}
        CeilFloorKind::DateTimeField(_) => return Err(diag::syntax("syntax error", pos)),
        CeilFloorKind::Scale(v) => args.push(bind_arg(b, &Expr::Value(v.clone()), ec)?),
    }
    pg_call(b, name, args, pos, ec)
}

/// Named/positional argument binding of a call (module docs step 3). WITHIN GROUP sort keys are
/// appended after the explicit arguments, as PG's `ParseFuncOrColumn` does (`fargs` = direct
/// arguments ++ WITHIN GROUP expressions).
fn bind_call_args(b: &mut Binder, f: &Function, ec: ExprCtx, pos: Option<u32>) -> Result<(Vec<BoundArg>, Vec<Option<String>>)> {
    let mut args: Vec<BoundArg> = Vec::new();
    let mut names: Vec<Option<String>> = Vec::new();
    match &f.args {
        FunctionArguments::None => {}
        FunctionArguments::Subquery(_) => return Err(diag::syntax("syntax error", pos)),
        FunctionArguments::List(list) => {
            let mut seen_named = false;
            for a in &list.args {
                if is_variadic_marker(a) {
                    // `VARIADIC expr` (the last argument): bound as a plain positional argument; the
                    // variadic flag itself comes from `variadic_call`.
                    let (FunctionArg::Named { arg: FunctionArgExpr::Expr(e), .. }
                    | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(e), .. }) = a
                    else {
                        return Err(diag::syntax("syntax error", pos));
                    };
                    args.push(bind_arg(b, e, ec)?);
                    names.push(None);
                    continue;
                }
                match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        if seen_named {
                            return Err(diag::syntax("positional argument cannot follow named argument", b.pos(e)));
                        }
                        let arg = bind_arg(b, e, ec)?;
                        args.push(arg);
                        names.push(None);
                    }
                    FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => {
                        seen_named = true;
                        let n = b.ident(name);
                        if names.iter().any(|x| x.as_deref() == Some(n.as_str())) {
                            return Err(diag::syntax(format!("argument name \"{n}\" used more than once"), pos));
                        }
                        let arg = bind_arg(b, e, ec)?;
                        args.push(arg);
                        names.push(Some(n));
                    }
                    FunctionArg::ExprNamed { name: Expr::Identifier(id), arg: FunctionArgExpr::Expr(e), .. } => {
                        seen_named = true;
                        let n = b.ident(id);
                        if names.iter().any(|x| x.as_deref() == Some(n.as_str())) {
                            return Err(diag::syntax(format!("argument name \"{n}\" used more than once"), pos));
                        }
                        let arg = bind_arg(b, e, ec)?;
                        args.push(arg);
                        names.push(Some(n));
                    }
                    _ => return Err(diag::syntax("syntax error", pos)),
                }
            }
        }
    }
    for ob in &f.within_group {
        let arg = bind_arg(b, &ob.expr, ec)?;
        args.push(arg);
        names.push(None);
    }
    Ok((args, names))
}

/// Overload resolution against an already listed candidate set.
fn resolve_with(
    b: &Binder,
    cands: &[FuncCandidate],
    name: &str,
    arg_types: Vec<Oid>,
    arg_names: Vec<Option<String>>,
    variadic_kw: bool,
    pos: Option<u32>,
) -> Result<ResolvedFunc> {
    let call = FuncCall { name: name.to_string(), arg_types, arg_names, variadic_keyword: variadic_kw };
    func::resolve_function(b.env, &call, cands).map_err(|e| diag::with_position(e, pos))
}

/// `pg_catalog.<name>(args)` through [`resolve_and_build_call`].
fn pg_call(b: &mut Binder, name: &str, args: Vec<BoundArg>, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    let n = args.len();
    resolve_and_build_call(b, Some(PG_CATALOG), name, args, vec![None; n], false, pos, ec)
}

/// PG `FuncNameAsType` cast call `type(arg)`: when the name is a type and a cast path exists in
/// the explicit context. `Ok(None)` when the name is not a type or no cast applies (the caller then
/// reports the original 42883).
fn function_style_cast(b: &mut Binder, schema: Option<&str>, name: &str, arg: &BoundArg) -> Result<Option<BoundExpr>> {
    let Some(oid) = b.env.lookup_type(schema, name)? else {
        return Ok(None);
    };
    coerce::try_coerce(b, arg, TypeId::of(oid), CastContext::Explicit, CoercionForm::ExplicitCall)
}

/// PG `check_srf_call_placement` for a call that returns a set.
fn check_srf_placement(b: &mut Binder, retset: bool, ec: ExprCtx, pos: Option<u32>) -> Result<()> {
    if !retset {
        return Ok(());
    }
    if b.agg_depth > 0 {
        return Err(diag::with_position(
            PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "aggregate function calls cannot contain set-returning function calls",
            ),
            pos,
        ));
    }
    if ec.clause == ExprClause::FunctionInFrom {
        return Ok(());
    }
    if !ec.srf_ok {
        return Err(diag::srf_not_allowed(ec.clause, pos));
    }
    if let Ok(scope) = b.scope_mut() {
        scope.has_target_srfs = true;
    }
    Ok(())
}

/// 42809 wrong-kind error with the position of the call.
fn wrong_kind(msg: String, pos: Option<u32>) -> PgError {
    diag::with_position(PgError::new(SqlState::WRONG_OBJECT_TYPE, msg), pos)
}

/// Take the coerced argument at `i` (each call argument is used exactly once).
fn take_arg(v: &mut [Option<BoundExpr>], i: usize) -> Result<BoundExpr> {
    v.get_mut(i)
        .and_then(|s| s.take())
        .ok_or_else(|| PgError::internal("binder: function argument used twice or missing"))
}

/// Bind one default expression text in an empty scope (no column references are possible).
fn bind_default(b: &mut Binder, text: &str) -> Result<BoundExpr> {
    let expr = crate::parser::parse_expr_text(text)?;
    let saved = std::mem::take(&mut b.scopes);
    let bound = bind_expr(b, &expr, ExprCtx::column_default());
    b.scopes = saved;
    bound
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_split_at_top_level_only() {
        assert!(split_defaults("").is_empty());
        assert_eq!(split_defaults("0, 0, 0, 0, 0, 0, 0.0"), vec!["0", "0", "0", "0", "0", "0", "0.0"]);
        assert_eq!(split_defaults("'{}'::jsonb, false"), vec!["'{}'::jsonb", "false"]);
        assert_eq!(split_defaults("'a,b'::text, ARRAY[1,2]"), vec!["'a,b'::text", "ARRAY[1,2]"]);
        assert_eq!(split_defaults("'it''s, ok'::text"), vec!["'it''s, ok'::text"]);
        assert_eq!(split_defaults("true, 'use_json_null'::text"), vec!["true", "'use_json_null'::text"]);
    }

    #[test]
    fn extract_fields_are_lower_case_text() {
        assert_eq!(extract_field_name(&DateTimeField::Year), "year");
        assert_eq!(extract_field_name(&DateTimeField::Epoch), "epoch");
        assert_eq!(extract_field_name(&DateTimeField::Millenium), "millennium");
        assert_eq!(extract_field_name(&DateTimeField::Custom(sqlparser::ast::Ident::new("Quarter"))), "quarter");
    }

    #[test]
    fn take_arg_is_single_use() {
        let mut v: Vec<Option<BoundExpr>> = vec![Some(BoundExpr::bool_const(true))];
        assert!(take_arg(&mut v, 0).is_ok());
        assert!(take_arg(&mut v, 0).is_err());
        assert!(take_arg(&mut v, 5).is_err());
    }
}
