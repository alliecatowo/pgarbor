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
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::resolve::func::ResolvedFunc;
use sqlparser::ast::{Expr, Function};

use super::context::{Binder, BoundArg, ExprCtx};
use crate::ir::{BoundExpr, SqlValueOp};

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
#[allow(unused_variables)]
pub fn proc_info(b: &Binder, oid: Oid) -> Result<ProcInfo> {
    Err(PgError::unimplemented("SQL-12", "proc metadata lookup (expr_func::proc_info)"))
}

/// Bind `Expr::Function` (module docs steps 1-7).
#[allow(unused_variables)]
pub fn bind_function_call(b: &mut Binder, f: &Function, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-12", "function call binding (expr_func::bind_function_call)"))
}

/// Resolve and build a plain (non-aggregate) call from already bound arguments:
/// `resolve_function` + [`build_call`]. `arg_names[i]` is the named-notation name of argument i.
/// `variadic_kw` = call used `VARIADIC`. Aggregates/windows must not come through here.
#[allow(unused_variables)]
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
    Err(PgError::unimplemented("SQL-12", "function resolution (expr_func::resolve_and_build_call)"))
}

/// Build the `FuncCall` node for an already resolved function (module docs step 6): coercions,
/// named-argument reordering, defaults, variadic packing, flags.
#[allow(unused_variables)]
pub fn build_call(b: &mut Binder, resolved: &ResolvedFunc, name: &str, args: Vec<BoundArg>, variadic_kw: bool, is_cast_call: bool, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-12", "call node construction (expr_func::build_call)"))
}

/// `pg_catalog.<name>(args...)` for internal rewrites (args positional, Implicit coercions).
#[allow(unused_variables)]
pub fn call_function_by_name(b: &mut Binder, name: &str, args: Vec<BoundArg>, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-12", "internal function call (expr_func::call_function_by_name)"))
}

/// `CURRENT_DATE` & co. `precision`: optional typmod argument (`CURRENT_TIMESTAMP(3)`; > 6 is a
/// WARNING `TIMESTAMP(7) precision reduced to maximum allowed, 6` and clamps). Result types:
/// current_date `date`, current_time `timetz`, current_timestamp `timestamptz`, localtime `time`,
/// localtimestamp `timestamp`, current_role/current_user/session_user/user `name`,
/// current_catalog `name`, current_schema `name` (typmod from precision for the time ones).
#[allow(unused_variables)]
pub fn bind_sql_value_function(b: &mut Binder, op: SqlValueOp, precision: Option<i32>, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-12", "SQL value functions (expr_func::bind_sql_value_function)"))
}

/// `Extract`, `Substring`, `Position`, `Overlay`, `Trim`, `Ceil`, `Floor` forms (module docs
/// "Special syntax"). `e` is the whole `Expr`; only those variants are passed.
#[allow(unused_variables)]
pub fn bind_special_syntax(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-12", "special syntax functions (expr_func::bind_special_syntax)"))
}
