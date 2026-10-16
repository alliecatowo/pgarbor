//! CREATE FUNCTION / PROCEDURE and CALL / DO (SQL-37). PG references: `functioncmds.c`
//! (`CreateFunction`, `compute_function_attributes`, `interpret_function_parameter_list`,
//! `ExecuteCallStmt`, `ExecuteDoStmt`), `gram.y` (`CreateFunctionStmt`, `func_args`, `createfunc_opt_item`).
//!
//! PGArbor stores function definitions in the catalog (v1) but cannot execute user-defined
//! function bodies yet (later work: SQL-language and PL/pgSQL). The binder still validates and
//! records them so ORMs/migration tools that create functions succeed and `\df`/`pg_proc` work.
//!
//! `bind_create_function(b, stmt)` (`Statement::CreateFunction(CreateFunction { or_replace, name, args,
//! return_type, function_body, behavior, called_on_null, parallel, language, determinism_specifier, ..
//! })` and `Statement::CreateProcedure { or_alter, name, params, body, .. }`) ->
//! `BoundStatement::CreateFunction(BoundCreateFunction { function: Function{oid: 0, ..}, or_replace })`:
//! * Parameters: mode (`IN` default, `OUT`, `INOUT`, `VARIADIC`), optional name, type, `DEFAULT expr`
//!   (bound with `ExprCtx::column_default()` and stored as canonical text); `RETURNS type`,
//!   `RETURNS TABLE(..)`, `RETURNS SETOF type`, `RETURNS void`; OUT parameters determine the return type
//!   (one OUT -> its type; several -> `record`). 42P13 `function result type must be X because of OUT
//!   parameters`, `only the last parameter can be variadic`, `input parameters after one with a default
//!   value must also have defaults`, 42P13 `procedures cannot have OUT arguments`? (PG 14+ allows OUT for
//!   procedures; INOUT too).
//! * Body: `AS 'text'` / `AS $$text$$` -> `body`; `LANGUAGE sql|plpgsql|c|internal`: `sql` (language OID
//!   14) and `plpgsql` (13647?? use `builtin::LANGUAGES` lookup by name) accepted; others 42704 `language
//!   "x" does not exist` (or 0A000 for `c`/`internal` user functions); `BEGIN ATOMIC ... END` bodies and
//!   `RETURN expr` -> `sql_body: Some(StoredExpr)` (deparsed) when binding succeeds.
//!   SQL-language bodies are syntax-checked lazily (`check_function_bodies` is `on` in PG: a SQL body is
//!   parsed/analyzed at creation; PGArbor v1 parses it with `parser::parse` and reports 42601 syntax errors,
//!   but does not bind it).
//! * Attributes: `IMMUTABLE|STABLE|VOLATILE` (default volatile), `[NOT] LEAKPROOF`, `STRICT|RETURNS NULL
//!   ON NULL INPUT|CALLED ON NULL INPUT`, `SECURITY DEFINER|INVOKER`, `PARALLEL UNSAFE|RESTRICTED|SAFE`,
//!   `COST n`, `ROWS n` (SRFs only), `SET guc = v` -> `config`, `WINDOW`, `TRANSFORM`: 0A000.
//! * `kind`: `FuncKind::Function` or `Procedure` (CREATE PROCEDURE; no RETURNS).
//! * Namespace and owner like tables. Dependencies: argument/return types.
//!
//! `bind_call(b, stmt)` (`Statement::Call(FunctionCall)`): resolve a PROCEDURE by name + argument types
//! (`resolve_function` with kind Procedure; 42883 `procedure p(integer) does not exist`; a function
//! called with CALL: 42809 `f(integer) is not a procedure` HINT `To call a function, use SELECT.`) ->
//! `BoundStatement::Call(BoundCall{procedure, name, args})`, args coerced; OUT args as NULL placeholders.
//! `bind_do(b, ..)`: `DO [LANGUAGE l] 'code'` -> `BoundStatement::Do(BoundDo{language, code})`
//! (`language` defaults to `plpgsql`; unknown language 42704).
//!
//! ## Implementation notes (SQL-37)
//! * Validation of the parameter list is a pure function ([`check_param_list`]) over normalized
//!   [`ParamSpec`]s, mirroring `interpret_function_parameter_list` (functioncmds.c). Defaults are bound
//!   afterwards in an empty table scope (so column references fail cleanly) and coerced with
//!   `coerce_to_specific(.., "DEFAULT")` like `coerce_to_specific_type`.
//! * `CREATE OR REPLACE` checks follow `ProcedureCreate` (pg_proc.c): routine kind, return type,
//!   input parameter names, removed defaults.
//! * `CALL` resolves against procedure candidates first (`resolve_function`); a matching non-procedure
//!   gives 42809, otherwise 42883 / 42725 are re-worded as `procedure ...`. OUT parameters are supplied
//!   by the caller as placeholders (positional calls): they are dropped for matching and re-inserted as
//!   NULL in the bound argument list.
use pgarbor_catalog::builtin::{self, BuiltinLanguage};
use pgarbor_catalog::model::{ArgMode, FuncArg, FuncKind, FuncVolatility, Function};
use pgarbor_catalog::{CatalogEnv, StoredExpr};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{CastContext, ProcKind};
use pgarbor_types::oids;
use pgarbor_types::registry::is_array_type;
use pgarbor_types::resolve::func::{resolve_function, signature_string, FuncCall, FuncCandidate, ResolvedFunc};
use pgarbor_types::TypeId;
use sqlparser::ast::{
    ArgMode as SqlArgMode, CreateFunction, CreateFunctionBody, DataType, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam, FunctionParallel,
    FunctionReturnType, FunctionSecurity, FunctionSetValue, Ident, ObjectName, ObjectNamePart,
    OperateFunctionArg, ProcedureParam, Statement, Value,
};

use super::coerce::{coerce_strict, coerce_to_specific};
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::{bind_arg, bind_expr};
use super::stored::{bind_ddl_expr, store_expr, table_expr_scope};
use super::types::resolve_data_type;
use crate::diag::{syntax_error_near, ExprClause};
use crate::ir::{BoundCall, BoundCreateFunction, BoundDo, BoundExpr, BoundStatement, CoercionForm};
use crate::parser::{parse, parse_expr_text};

/// `pg_language` OIDs (builtin::LANGUAGES).
const LANG_INTERNAL: Oid = 12;
const LANG_C: Oid = 13;
const LANG_SQL: Oid = 14;

/// SQLSTATE 42P13 `invalid_function_definition`.
const INVALID_FUNCTION_DEFINITION: SqlState = SqlState(*b"42P13");

/// CREATE FUNCTION / PROCEDURE.
pub fn bind_create_function(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::CreateFunction(cf) => create_function(b, cf),
        Statement::CreateProcedure { or_alter, name, params, language, .. } => {
            create_procedure(b, *or_alter, name, params.as_deref(), language.as_ref())
        }
        _ => Err(PgError::internal("bind_create_function: statement is not CREATE FUNCTION or PROCEDURE")),
    }
}

/// CALL.
pub fn bind_call(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::Call(func) = stmt else {
        return Err(PgError::internal("bind_call: statement is not CALL"));
    };
    bind_call_function(b, func)
}

/// DO (from `ExtraStatement::Do`).
pub fn bind_do(b: &mut Binder, language: Option<&Ident>, code: &str) -> Result<BoundStatement> {
    let name = match language {
        Some(id) => b.ident(id),
        None => "plpgsql".to_string(),
    };
    let lang = find_language(&name)?;
    // PG ExecuteDoStmt: a language without an inline handler cannot run DO blocks.
    if lang.inline_handler == 0 {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("language \"{name}\" does not support inline code execution"),
        ));
    }
    Ok(BoundStatement::Do(BoundDo { language: name, code: code.to_string() }))
}

// ---------------------------------------------------------------- small helpers

fn invalid_fn_def(msg: impl Into<String>) -> PgError {
    PgError::new(INVALID_FUNCTION_DEFINITION, msg)
}

/// 0A000 for syntax that PGArbor accepts from sqlparser but does not implement yet.
fn unsupported(what: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by PGArbor yet"))
}

fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    if pos.is_some() {
        e.position = pos;
    }
    e
}

/// `builtin::LANGUAGES` lookup by (folded) name: 42704 `language "x" does not exist`.
fn find_language(name: &str) -> Result<&'static BuiltinLanguage> {
    builtin::LANGUAGES
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| PgError::new(SqlState::UNDEFINED_OBJECT, format!("language \"{name}\" does not exist")))
}

/// Language for a user function: `c` and `internal` need object files / C symbols, not supported.
fn resolve_language(name: &str) -> Result<&'static BuiltinLanguage> {
    let lang = find_language(name)?;
    if lang.oid == LANG_C || lang.oid == LANG_INTERNAL {
        return Err(unsupported(&format!("{} language functions", lang.name)));
    }
    Ok(lang)
}

fn map_mode(m: Option<&SqlArgMode>) -> ArgMode {
    match m {
        None | Some(SqlArgMode::In) => ArgMode::In,
        Some(SqlArgMode::Out) => ArgMode::Out,
        Some(SqlArgMode::InOut) => ArgMode::InOut,
        Some(SqlArgMode::Variadic) => ArgMode::Variadic,
    }
}

/// Parameter type of an argument or a procedure parameter. PG prints an unknown argument type
/// unquoted (`type nosuch does not exist`), unlike RETURNS (quoted); `resolve_data_type` quotes.
fn arg_type(b: &mut Binder<'_>, dt: &DataType) -> Result<Oid> {
    match resolve_data_type(b, dt, None) {
        Ok(r) => Ok(r.id.oid),
        Err(e) if e.sqlstate == SqlState::UNDEFINED_OBJECT => {
            Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("type {dt} does not exist")))
        }
        Err(e) => Err(e),
    }
}

/// One declared parameter, normalized from `OperateFunctionArg` or `ProcedureParam`.
struct ParamSpec<'a> {
    /// Empty when unnamed.
    name: String,
    mode: ArgMode,
    type_oid: Oid,
    pos: Option<u32>,
    default: Option<&'a Expr>,
}

impl ParamSpec<'_> {
    /// IN, INOUT and VARIADIC parameters (PG `isinput`, the `proargtypes` members).
    fn is_input(&self) -> bool {
        matches!(self.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic)
    }
}

/// PG `interpret_function_parameter_list` checks that need only modes, names, types and defaults.
/// Returns `requiredResultType` (the type implied by OUT parameters, `None` when there are none).
/// `is_array` tells whether a type OID is an array type (VARIADIC parameters must be arrays).
fn check_param_list(params: &[ParamSpec<'_>], is_procedure: bool, is_array: &dyn Fn(Oid) -> bool) -> Result<Option<Oid>> {
    let mut out_count = 0usize;
    let mut var_count = 0usize;
    let mut required: Option<Oid> = None;
    let mut have_defaults = false;
    for (i, p) in params.iter().enumerate() {
        let input = p.is_input();
        if input && var_count > 0 {
            return Err(at(invalid_fn_def("VARIADIC parameter must be the last input parameter"), p.pos));
        }
        if !matches!(p.mode, ArgMode::In | ArgMode::Variadic) {
            if is_procedure {
                if var_count > 0 {
                    return Err(at(invalid_fn_def("VARIADIC parameter must be the last parameter"), p.pos));
                }
                // Procedures with output parameters always return RECORD.
                required = Some(oids::RECORD);
            } else if out_count == 0 {
                required = Some(p.type_oid);
            }
            out_count += 1;
        }
        if p.mode == ArgMode::Variadic {
            var_count += 1;
            let polymorphic_array = matches!(p.type_oid, oids::ANY | oids::ANYARRAY | oids::ANYCOMPATIBLEARRAY);
            if !polymorphic_array && !is_array(p.type_oid) {
                return Err(at(invalid_fn_def("VARIADIC parameter must be an array"), p.pos));
            }
        }
        if !p.name.is_empty() && params[..i].iter().any(|q| q.name == p.name) {
            return Err(at(invalid_fn_def(format!("parameter name \"{}\" used more than once", p.name)), p.pos));
        }
        match p.default {
            Some(_) => {
                if !input {
                    return Err(at(invalid_fn_def("only input parameters can have default values"), p.pos));
                }
                have_defaults = true;
            }
            None => {
                if input && have_defaults {
                    return Err(at(
                        invalid_fn_def("input parameters after one with a default value must also have defaults"),
                        p.pos,
                    ));
                }
                if is_procedure && have_defaults {
                    return Err(at(
                        invalid_fn_def("procedure OUT parameters cannot appear after one with a default value"),
                        p.pos,
                    ));
                }
            }
        }
    }
    if out_count > 1 {
        required = Some(oids::RECORD);
    }
    Ok(required)
}

/// Run `f` with an empty table scope pushed, so column references fail with the normal errors
/// instead of XX000 `no open query scope`. The scope is popped on every path.
fn with_empty_scope<T>(b: &mut Binder<'_>, f: impl FnOnce(&mut Binder<'_>) -> Result<T>) -> Result<T> {
    table_expr_scope(b, "", &[])?;
    let r = f(b);
    b.scopes.pop();
    r
}

/// Bind a `DEFAULT` of a CREATE FUNCTION parameter to canonical stored text.
fn bind_create_default(b: &mut Binder<'_>, e: &Expr, ty: Oid, pos: Option<u32>) -> Result<StoredExpr> {
    with_empty_scope(b, |b| {
        let bound = bind_expr(b, e, ExprCtx::column_default())?;
        let bound = coerce_to_specific(b, &BoundArg::new(bound, pos), TypeId::of(ty), "DEFAULT")?;
        let no_names = |_: i16| -> Option<String> { None };
        store_expr(b, &bound, None, &no_names)
    })
}

/// Bind a stored default of a catalog function (used when CALL omits a trailing argument).
fn bind_stored_default(b: &mut Binder<'_>, sd: &StoredExpr, ty: Oid) -> Result<BoundExpr> {
    with_empty_scope(b, |b| {
        let e = parse_expr_text(&sd.sql)?;
        let bound = bind_expr(b, &e, ExprCtx::column_default())?;
        coerce_strict(b, &BoundArg::new(bound, None), TypeId::of(ty), CastContext::Assignment, CoercionForm::Implicit)
    })
}

/// Text of an `AS` item: `'...'` or `$$...$$`. A second item (C link symbol) is 42P13 for SQL / PL/pgSQL.
fn as_string_body(b: &Binder<'_>, e: &Expr, has_link: bool, lang: &BuiltinLanguage) -> Result<String> {
    if has_link {
        return Err(invalid_fn_def(format!("only one AS item needed for language \"{}\"", lang.name)));
    }
    let text = match e {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) => s.clone(),
            Value::DollarQuotedString(d) => d.value.clone(),
            _ => return Err(syntax_error_near(&e.to_string(), b.pos(e))),
        },
        _ => return Err(syntax_error_near(&e.to_string(), b.pos(e))),
    };
    if lang.oid == LANG_SQL {
        // check_function_bodies: the SQL text must at least parse (binding is deferred).
        let _ = parse(&text)?;
    }
    Ok(text)
}

/// `RETURN expr` / `AS RETURN expr`: bind in a scope of the named input parameters, coerce to the
/// return type, and keep the canonical deparsed text.
fn sql_return_body(b: &mut Binder<'_>, fname: &str, specs: &[ParamSpec<'_>], e: &Expr, ret: Oid) -> Result<StoredExpr> {
    let cols: Vec<(String, TypeId)> = specs
        .iter()
        .filter(|p| p.is_input() && !p.name.is_empty())
        .map(|p| (p.name.clone(), TypeId::of(p.type_oid)))
        .collect();
    table_expr_scope(b, fname, &cols)?;
    // The parameters are the scope's columns (attnum = 1-based position), so the stored text names them
    // by their declared names (PG `deparse` of the body shows parameter names).
    let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
    let column_names = move |attnum: i16| -> Option<String> {
        let idx = usize::try_from(attnum).ok()?.checked_sub(1)?;
        names.get(idx).cloned()
    };
    let r = bind_ddl_expr(b, e, ExprCtx::check(), Some(TypeId::of(ret)), None, &mut Vec::new(), &column_names)
        .map(|(_, stored)| stored);
    b.scopes.pop();
    r
}

/// `SET name = value, ...` -> `name=value` (`proconfig`).
fn set_config(b: &mut Binder<'_>, items: &[FunctionDefinitionSetParam]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let mut segs: Vec<String> = Vec::new();
        for part in item.name.0.iter() {
            match part {
                ObjectNamePart::Identifier(id) => segs.push(b.ident(id)),
                ObjectNamePart::Function(_) => return Err(unsupported("function syntax in a SET name")),
            }
        }
        let name = segs.join(".");
        let value = match &item.value {
            FunctionSetValue::Values(vals) => set_values_text(vals)?,
            FunctionSetValue::Default => return Err(unsupported("SET ... = DEFAULT in CREATE FUNCTION")),
            FunctionSetValue::FromCurrent => return Err(unsupported("SET ... FROM CURRENT in CREATE FUNCTION")),
        };
        out.push(format!("{name}={value}"));
    }
    Ok(out)
}

fn set_values_text(vals: &[Expr]) -> Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(vals.len());
    for v in vals {
        let s = match v {
            Expr::Identifier(id) => id.value.clone(),
            Expr::Value(vs) => match &vs.value {
                Value::SingleQuotedString(s) => s.clone(),
                Value::DollarQuotedString(d) => d.value.clone(),
                Value::Number(n, _) => n.to_string(),
                Value::Boolean(t) => t.to_string(),
                _ => return Err(unsupported("this SET value in CREATE FUNCTION")),
            },
            _ => return Err(unsupported("this SET value in CREATE FUNCTION")),
        };
        parts.push(s);
    }
    Ok(parts.join(", "))
}

/// PG `ProcedureCreate` checks for `CREATE OR REPLACE` of an existing routine with the same identity.
#[allow(clippy::too_many_arguments)]
fn check_replace(
    env: &CatalogEnv,
    old: &Function,
    kind: FuncKind,
    ret: Oid,
    retset: bool,
    specs: &[ParamSpec<'_>],
    in_types: &[Oid],
    n_defaults: usize,
    name: &str,
    is_procedure: bool,
) -> Result<()> {
    if old.kind != kind {
        let detail = match old.kind {
            FuncKind::Aggregate => format!("\"{name}\" is an aggregate function."),
            FuncKind::Procedure => format!("\"{name}\" is a procedure."),
            FuncKind::Window => format!("\"{name}\" is a window function."),
            FuncKind::Function => format!("\"{name}\" is a function."),
        };
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, "cannot change routine kind").with_detail(detail));
    }
    let drop_word = if is_procedure { "PROCEDURE" } else { "FUNCTION" };
    let sig = signature_string(env, name, in_types)?;
    let hint = format!("Use DROP {drop_word} {sig} first.");
    if old.ret != ret || old.retset != retset {
        return Err(invalid_fn_def("cannot change return type of existing function").with_hint(hint));
    }
    let old_in: Vec<&FuncArg> = old
        .args
        .iter()
        .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
        .collect();
    for (o, n) in old_in.iter().zip(specs.iter().filter(|p| p.is_input())) {
        if !o.name.is_empty() && o.name != n.name {
            return Err(invalid_fn_def(format!("cannot change name of input parameter \"{}\"", o.name))
                .with_hint(hint));
        }
    }
    if old.n_defaults() as usize > n_defaults {
        return Err(invalid_fn_def("cannot remove parameter defaults from existing function").with_hint(hint));
    }
    Ok(())
}

// ---------------------------------------------------------------- CREATE FUNCTION

fn create_function(b: &mut Binder<'_>, cf: &CreateFunction) -> Result<BoundStatement> {
    if cf.temporary {
        return Err(unsupported("CREATE TEMPORARY FUNCTION"));
    }
    if cf.if_not_exists {
        return Err(unsupported("CREATE FUNCTION IF NOT EXISTS"));
    }
    if cf.or_alter {
        return Err(unsupported("CREATE OR ALTER FUNCTION"));
    }
    if cf.using.is_some() {
        return Err(unsupported("CREATE FUNCTION ... USING"));
    }
    if cf.options.is_some() {
        return Err(unsupported("CREATE FUNCTION ... OPTIONS"));
    }
    if cf.remote_connection.is_some() {
        return Err(unsupported("CREATE FUNCTION ... REMOTE WITH CONNECTION"));
    }
    if cf.determinism_specifier.is_some() {
        return Err(unsupported("DETERMINISTIC / NOT DETERMINISTIC"));
    }
    let env: &CatalogEnv = b.env;
    let qn = b.qual(&cf.name)?;

    // Parameters (types resolved; the list checks run once all are known).
    let mut specs: Vec<ParamSpec<'_>> = Vec::new();
    for a in cf.args.as_deref().unwrap_or(&[]) {
        specs.push(operate_arg_spec(b, a)?);
    }
    let required = check_param_list(&specs, false, &|oid: Oid| is_array_type(env, oid))?;

    // LANGUAGE (RETURN bodies default to SQL).
    let inline_body = matches!(
        cf.function_body,
        Some(CreateFunctionBody::Return(_)) | Some(CreateFunctionBody::AsReturnExpr(_))
    );
    let lang_name = match &cf.language {
        Some(id) => b.ident(id),
        None if inline_body => "sql".to_string(),
        None => return Err(invalid_fn_def("no language specified")),
    };
    let lang = resolve_language(&lang_name)?;

    // Result type: RETURNS, else the OUT parameters' type.
    let declared_ret: Option<(Oid, bool)> = match &cf.return_type {
        None => None,
        Some(FunctionReturnType::DataType(dt)) => Some((resolve_data_type(b, dt, None)?.id.oid, false)),
        Some(FunctionReturnType::SetOf(dt)) => Some((resolve_data_type(b, dt, None)?.id.oid, true)),
    };
    let (ret, retset) = match (required, declared_ret) {
        (Some(req), Some((r, set))) => {
            if r != req {
                return Err(invalid_fn_def(format!(
                    "function result type must be {} because of OUT parameters",
                    b.type_name(TypeId::of(req))
                )));
            }
            (r, set)
        }
        (Some(req), None) => (req, false),
        (None, Some((r, set))) => (r, set),
        (None, None) => return Err(invalid_fn_def("function result type must be specified")),
    };

    // Body.
    let (body, sql_body) = match &cf.function_body {
        None => return Err(invalid_fn_def("no function body specified")),
        Some(CreateFunctionBody::AsBeforeOptions { body: e, link_symbol }) => {
            (as_string_body(b, e, link_symbol.is_some(), lang)?, None)
        }
        Some(CreateFunctionBody::AsAfterOptions(e)) => (as_string_body(b, e, false, lang)?, None),
        Some(CreateFunctionBody::Return(e)) | Some(CreateFunctionBody::AsReturnExpr(e)) => {
            if lang.oid != LANG_SQL {
                return Err(invalid_fn_def("inline SQL function body only valid for language SQL"));
            }
            let stored = sql_return_body(b, &qn.name, &specs, e, ret)?;
            (String::new(), Some(stored))
        }
        Some(CreateFunctionBody::AsBeginEnd(_)) => {
            return Err(PgError::unimplemented("SQL-37", "BEGIN ... END function bodies (ddl_function)"));
        }
        Some(CreateFunctionBody::AsReturnSelect(_)) => {
            return Err(PgError::unimplemented("SQL-37", "AS RETURN (SELECT ...) function bodies (ddl_function)"));
        }
    };

    // Attributes (compute_function_attributes).
    let volatility = match cf.behavior {
        None | Some(FunctionBehavior::Volatile) => FuncVolatility::Volatile,
        Some(FunctionBehavior::Immutable) => FuncVolatility::Immutable,
        Some(FunctionBehavior::Stable) => FuncVolatility::Stable,
    };
    let strict = matches!(
        cf.called_on_null,
        Some(FunctionCalledOnNull::Strict) | Some(FunctionCalledOnNull::ReturnsNullOnNullInput)
    );
    let parallel = match cf.parallel {
        None | Some(FunctionParallel::Unsafe) => b'u',
        Some(FunctionParallel::Restricted) => b'r',
        Some(FunctionParallel::Safe) => b's',
    };
    // SECURITY INVOKER is the default (security_definer = false).
    let security_definer = matches!(cf.security, Some(FunctionSecurity::Definer));
    let config = set_config(b, &cf.set_params)?;
    let rows = if retset { 1000.0f32 } else { 0.0f32 };

    // Parameter defaults and the catalog argument list.
    let mut args: Vec<FuncArg> = Vec::with_capacity(specs.len());
    for p in specs.iter() {
        let default = match p.default {
            Some(e) => {
                let pos = b.pos(e);
                Some(bind_create_default(b, e, p.type_oid, pos)?)
            }
            None => None,
        };
        args.push(FuncArg { name: p.name.clone(), type_oid: p.type_oid, mode: p.mode, default });
    }

    // Identity: (namespace, name, IN argument types).
    let ns = env.creation_namespace_for(qn.schema.as_deref(), &qn.name)?;
    let kind = FuncKind::Function;
    let in_types: Vec<Oid> = specs.iter().filter(|p| p.is_input()).map(|p| p.type_oid).collect();
    let n_defaults = specs.iter().filter(|p| p.default.is_some()).count();
    let existing = env
        .snapshot()
        .functions_in_namespace(ns, &qn.name)
        .into_iter()
        .find(|f| f.in_arg_types() == in_types);
    if let Some(old) = existing {
        if !cf.or_replace {
            return Err(PgError::new(
                SqlState::DUPLICATE_FUNCTION,
                format!("function \"{}\" already exists with same argument types", qn.name),
            ));
        }
        check_replace(env, old, kind, ret, retset, &specs, &in_types, n_defaults, &qn.name, false)?;
    }

    let function = Function {
        oid: 0,
        name: qn.name.clone(),
        namespace: ns,
        owner: env.current_user(),
        kind,
        language: lang.oid,
        args,
        ret,
        retset,
        volatility,
        strict,
        security_definer,
        leakproof: false,
        parallel,
        cost: 100.0,
        rows,
        body,
        sql_body,
        config,
        acl: None,
        depends_on: Vec::new(),
    };
    Ok(BoundStatement::CreateFunction(Box::new(BoundCreateFunction { function, or_replace: cf.or_replace })))
}

fn operate_arg_spec<'a>(b: &mut Binder<'_>, a: &'a OperateFunctionArg) -> Result<ParamSpec<'a>> {
    let name = match &a.name {
        Some(id) => b.ident(id),
        None => String::new(),
    };
    let type_oid = arg_type(b, &a.data_type)?;
    Ok(ParamSpec { name, mode: map_mode(a.mode.as_ref()), type_oid, pos: None, default: a.default_expr.as_ref() })
}

// ---------------------------------------------------------------- CREATE PROCEDURE

/// CREATE PROCEDURE: the parameter list and language are validated. The body is not bound: the
/// sqlparser AST carries `ConditionalStatements` (MsSql-style statements), not the dollar-quoted text
/// PostgreSQL stores in `prosrc`, so a procedure body cannot be recorded faithfully yet.
fn create_procedure(
    b: &mut Binder<'_>,
    or_alter: bool,
    name: &ObjectName,
    params: Option<&[ProcedureParam]>,
    language: Option<&Ident>,
) -> Result<BoundStatement> {
    if or_alter {
        return Err(unsupported("CREATE OR ALTER PROCEDURE"));
    }
    let env: &CatalogEnv = b.env;
    let _ = b.qual(name)?;
    let mut specs: Vec<ParamSpec<'_>> = Vec::new();
    for p in params.unwrap_or(&[]) {
        let pname = b.ident(&p.name);
        let type_oid = arg_type(b, &p.data_type)?;
        let pos = b.pos_span(p.name.span);
        specs.push(ParamSpec {
            name: pname,
            mode: map_mode(p.mode.as_ref()),
            type_oid,
            pos,
            default: p.default.as_ref(),
        });
    }
    check_param_list(&specs, true, &|oid: Oid| is_array_type(env, oid))?;
    let lang_name = match language {
        Some(id) => b.ident(id),
        None => return Err(invalid_fn_def("no language specified")),
    };
    resolve_language(&lang_name)?;
    Err(PgError::unimplemented(
        "SQL-37",
        "CREATE PROCEDURE body: sqlparser keeps ConditionalStatements, not the procedure body text",
    ))
}

// ---------------------------------------------------------------- CALL

/// Number of declared parameters that are OUT-only, when the call lists one value per declared
/// parameter (PG: CALL requires placeholders for OUT arguments).
fn out_placeholders(env: &CatalogEnv, c: &FuncCandidate, nargs: usize) -> Option<Vec<bool>> {
    let f = env.snapshot().function(c.oid)?;
    if f.args.len() != nargs || !f.args.iter().any(|a| a.mode == ArgMode::Out) {
        return None;
    }
    Some(f.args.iter().map(|a| a.mode == ArgMode::Out).collect())
}

fn bind_call_function(b: &mut Binder<'_>, func: &sqlparser::ast::Function) -> Result<BoundStatement> {
    let env: &CatalogEnv = b.env;
    let qn = b.qual(&func.name)?;
    let name_pos = b.pos(&func.name);
    let list: &[FunctionArg] = match &func.args {
        FunctionArguments::List(l) => l.args.as_slice(),
        FunctionArguments::None => &[],
        FunctionArguments::Subquery(_) => return Err(unsupported("CALL with a subquery argument")),
    };

    let ctx = ExprCtx {
        clause: ExprClause::Other("CALL arguments"),
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: true,
        columns_ok: true,
    };
    let mut args: Vec<BoundArg> = Vec::with_capacity(list.len());
    let mut names: Vec<Option<String>> = Vec::with_capacity(list.len());
    for a in list {
        match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                args.push(bind_arg(b, e, ctx)?);
                names.push(None);
            }
            FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => {
                let n = b.ident(name);
                args.push(bind_arg(b, e, ctx)?);
                names.push(Some(n));
            }
            _ => return Err(unsupported("this CALL argument form")),
        }
    }
    let named = names.iter().any(|n| n.is_some());
    let arg_oids: Vec<Oid> = args.iter().map(|a| a.ty().oid).collect();
    let sig = signature_string(env, &qn.name, &arg_oids)?;
    let call = FuncCall {
        name: qn.name.clone(),
        arg_types: arg_oids.clone(),
        arg_names: names.clone(),
        variadic_keyword: false,
    };
    let all = env.function_candidates(qn.schema.as_deref(), &qn.name)?;
    let procs: Vec<FuncCandidate> = all.iter().filter(|c| matches!(c.kind, ProcKind::Procedure)).cloned().collect();

    // Procedures first; then procedures with OUT placeholders dropped (positional calls only).
    let no_drop = vec![false; args.len()];
    let mut plan: Option<(ResolvedFunc, Vec<bool>)> = None;
    let mut first_err: Option<PgError> = None;
    match resolve_function(env, &call, &procs) {
        Ok(r) => plan = Some((r, no_drop)),
        Err(e) => first_err = Some(e),
    }
    if plan.is_none() && !named {
        for c in procs.iter() {
            let Some(drop) = out_placeholders(env, c, args.len()) else {
                continue;
            };
            let kept_types: Vec<Oid> =
                arg_oids.iter().zip(drop.iter()).filter(|(_, d)| !**d).map(|(t, _)| *t).collect();
            let sc = FuncCall {
                name: qn.name.clone(),
                arg_types: kept_types.clone(),
                arg_names: vec![None; kept_types.len()],
                variadic_keyword: false,
            };
            if let Ok(r) = resolve_function(env, &sc, std::slice::from_ref(c)) {
                plan = Some((r, drop));
                break;
            }
        }
    }
    let (resolved, drop) = match plan {
        Some(p) => p,
        None => {
            // A function (not a procedure) matches: PG reports the kind first.
            if let Ok(r) = resolve_function(env, &call, &all) {
                if !matches!(r.kind, ProcKind::Procedure) {
                    return Err(at(
                        PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is not a procedure"))
                            .with_hint("To call a function, use SELECT."),
                        name_pos,
                    ));
                }
            }
            if let Some(e) = first_err {
                if e.sqlstate != SqlState::UNDEFINED_FUNCTION && e.sqlstate != SqlState::AMBIGUOUS_FUNCTION {
                    return Err(at(e, name_pos));
                }
                if e.sqlstate == SqlState::AMBIGUOUS_FUNCTION {
                    return Err(at(
                        PgError::new(SqlState::AMBIGUOUS_FUNCTION, format!("procedure {sig} is not unique"))
                            .with_hint("Could not choose a best candidate procedure. You might need to add explicit type casts."),
                        name_pos,
                    ));
                }
            }
            return Err(at(
                PgError::new(SqlState::UNDEFINED_FUNCTION, format!("procedure {sig} does not exist")).with_hint(
                    "No procedure matches the given name and argument types. You might need to add explicit type casts.",
                ),
                name_pos,
            ));
        }
    };
    if resolved.nvariadic > 0 {
        return Err(unsupported("CALL with a VARIADIC parameter"));
    }
    let target = env
        .snapshot()
        .function(resolved.oid)
        .ok_or_else(|| PgError::internal(format!("CALL target {} is not a catalog procedure", resolved.oid)))?;

    let kept: Vec<BoundArg> =
        args.into_iter().zip(drop.iter()).filter(|(_, d)| !**d).map(|(a, _)| a).collect();

    // Input-side arguments: supplied (coerced) or filled from the stored default.
    let in_decl: Vec<&FuncArg> = target
        .args
        .iter()
        .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
        .collect();
    let mut in_exprs: Vec<BoundExpr> = Vec::with_capacity(in_decl.len());
    for (p, decl) in in_decl.iter().enumerate() {
        let idx = resolved.arg_order.get(p).copied().unwrap_or(usize::MAX);
        if idx != usize::MAX && idx < kept.len() {
            let ty = resolved.coerce_to.get(idx).copied().unwrap_or(decl.type_oid);
            let expr = coerce_strict(b, &kept[idx], TypeId::of(ty), CastContext::Implicit, CoercionForm::Implicit)?;
            in_exprs.push(expr);
        } else {
            let sd = decl
                .default
                .as_ref()
                .ok_or_else(|| PgError::internal("CALL: missing argument without a default"))?;
            in_exprs.push(bind_stored_default(b, sd, decl.type_oid)?);
        }
    }

    // Full declared list: OUT positions become typed NULL placeholders.
    let mut ins = in_exprs.into_iter();
    let mut all_args: Vec<BoundExpr> = Vec::with_capacity(target.args.len());
    for a in target.args.iter() {
        match a.mode {
            ArgMode::Out | ArgMode::Table => all_args.push(BoundExpr::null(TypeId::of(a.type_oid))),
            _ => match ins.next() {
                Some(e) => all_args.push(e),
                None => return Err(PgError::internal("CALL: argument count mismatch")),
            },
        }
    }
    Ok(BoundStatement::Call(Box::new(BoundCall { procedure: resolved.oid, name: qn.name, args: all_args })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident as SqlIdent;

    fn spec<'a>(name: &str, mode: ArgMode, ty: Oid, default: Option<&'a Expr>) -> ParamSpec<'a> {
        ParamSpec { name: name.to_string(), mode, type_oid: ty, pos: None, default }
    }

    fn no_arrays(_: Oid) -> bool {
        false
    }

    fn marker() -> Expr {
        Expr::Identifier(SqlIdent::new("d"))
    }

    #[test]
    fn single_out_sets_required_type() {
        let r = check_param_list(&[spec("x", ArgMode::Out, oids::INT4, None)], false, &no_arrays);
        assert_eq!(r.unwrap(), Some(oids::INT4));
    }

    #[test]
    fn several_outs_require_record() {
        let ps = [spec("x", ArgMode::Out, oids::INT4, None), spec("y", ArgMode::Out, oids::TEXT, None)];
        assert_eq!(check_param_list(&ps, false, &no_arrays).unwrap(), Some(oids::RECORD));
    }

    #[test]
    fn no_out_gives_no_required_type() {
        let ps = [spec("a", ArgMode::In, oids::INT4, None)];
        assert_eq!(check_param_list(&ps, false, &no_arrays).unwrap(), None);
    }

    #[test]
    fn procedure_out_always_record() {
        let ps = [spec("x", ArgMode::Out, oids::INT4, None)];
        assert_eq!(check_param_list(&ps, true, &no_arrays).unwrap(), Some(oids::RECORD));
    }

    #[test]
    fn duplicate_parameter_name_is_42p13() {
        let ps = [spec("a", ArgMode::In, oids::INT4, None), spec("a", ArgMode::In, oids::INT4, None)];
        let e = check_param_list(&ps, false, &no_arrays).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P13");
        assert_eq!(e.message, "parameter name \"a\" used more than once");
    }

    #[test]
    fn input_after_default_needs_default() {
        let d = marker();
        let ps = [spec("a", ArgMode::In, oids::INT4, Some(&d)), spec("b", ArgMode::In, oids::INT4, None)];
        let e = check_param_list(&ps, false, &no_arrays).unwrap_err();
        assert_eq!(e.message, "input parameters after one with a default value must also have defaults");
    }

    #[test]
    fn out_parameter_cannot_have_default() {
        let d = marker();
        let ps = [spec("x", ArgMode::Out, oids::INT4, Some(&d))];
        let e = check_param_list(&ps, false, &no_arrays).unwrap_err();
        assert_eq!(e.message, "only input parameters can have default values");
    }

    #[test]
    fn procedure_out_after_default_rejected() {
        let d = marker();
        let ps = [spec("a", ArgMode::In, oids::INT4, Some(&d)), spec("b", ArgMode::Out, oids::INT4, None)];
        let e = check_param_list(&ps, true, &no_arrays).unwrap_err();
        assert_eq!(e.message, "procedure OUT parameters cannot appear after one with a default value");
    }

    #[test]
    fn variadic_must_be_last_input() {
        let ps = [spec("v", ArgMode::Variadic, oids::INT4_ARRAY, None), spec("a", ArgMode::In, oids::INT4, None)];
        let arr = |oid: Oid| oid == oids::INT4_ARRAY;
        let e = check_param_list(&ps, false, &arr).unwrap_err();
        assert_eq!(e.message, "VARIADIC parameter must be the last input parameter");
    }

    #[test]
    fn variadic_must_be_array() {
        let ps = [spec("v", ArgMode::Variadic, oids::INT4, None)];
        let e = check_param_list(&ps, false, &no_arrays).unwrap_err();
        assert_eq!(e.message, "VARIADIC parameter must be an array");
    }

    #[test]
    fn mode_mapping_defaults_to_in() {
        assert_eq!(map_mode(None), ArgMode::In);
        assert_eq!(map_mode(Some(&SqlArgMode::Out)), ArgMode::Out);
        assert_eq!(map_mode(Some(&SqlArgMode::InOut)), ArgMode::InOut);
    }

    #[test]
    fn language_lookup() {
        let sql = find_language("sql").unwrap();
        assert_eq!(sql.oid, LANG_SQL);
        assert_eq!(sql.inline_handler, 0);
        let plpgsql = find_language("plpgsql").unwrap();
        assert_ne!(plpgsql.inline_handler, 0);
        let e = find_language("nosuch").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42704");
        assert_eq!(e.message, "language \"nosuch\" does not exist");
        assert!(resolve_language("c").is_err());
    }
}
