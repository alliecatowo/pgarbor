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
//!   SQL-language `AS` bodies are checked at creation (`check_function_bodies` is `on` in PG): the text is
//!   parsed with `parser::parse` (42601 syntax errors) and each query statement is bound with the input
//!   parameters in scope (`check_sql_body`). Non-query statements are not bound yet. The final statement
//!   must match a scalar declared result (`check_sql_result`: 42P13 `return type mismatch ...` with
//!   DETAIL `Final statement must return exactly one column.` / `Actual return type is X.` / the empty-body
//!   detail); the check is skipped for void, composite and polymorphic-argument functions.
//!   The check runs after the duplicate / `OR REPLACE` checks, as PG's validator does. `check_function_bodies
//!   = off` is not visible to the binder yet, so the check always runs (known gap).
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
use pgarbor_types::registry::{is_array_type, is_polymorphic};
use pgarbor_types::resolve::coerce::can_coerce_type;
use pgarbor_types::resolve::func::{resolve_function, signature_string, FuncCall, FuncCandidate, ResolvedFunc};
use pgarbor_types::TypeId;
use sqlparser::ast::{
    ArgMode as SqlArgMode, CreateFunction, CreateFunctionBody, DataType, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam, FunctionParallel,
    FunctionReturnType, FunctionSecurity, FunctionSetValue, Ident, ObjectName, ObjectNamePart,
    OperateFunctionArg, ProcedureParam, Statement, Value,
};

use super::coerce::{coerce_strict, coerce_to_specific};
use super::dispatch::CREATE_PROCEDURE_KEY;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::{bind_arg, bind_expr};
use super::params::ParamTable;
use super::select::{bind_query, QueryCtx};
use super::stored::{bind_ddl_expr, store_expr, table_expr_scope};
use super::types::resolve_data_type;
use crate::diag::{syntax_error_near, ExprClause};
use crate::ir::{BoundCall, BoundCreateFunction, BoundDo, BoundExpr, BoundStatement, CoercionForm};
use crate::parser::{parse, parse_expr_text, Parsed, RawStatement, UNNAMED_PARAM_MARKER};

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

/// PG `check_valid_polymorphic_signature` (ProcedureCreate): a polymorphic result type needs an input
/// of a family that can deduce it. 42P13 `cannot determine result data type` with the PG DETAIL text.
fn check_polymorphic_result(b: &Binder<'_>, ret: Oid, in_types: &[Oid]) -> Result<()> {
    let has = |list: &[Oid]| in_types.iter().any(|t| list.contains(t));
    let (ok, want) = match ret {
        oids::ANYRANGE | oids::ANYMULTIRANGE => (
            has(&[oids::ANYRANGE, oids::ANYMULTIRANGE]),
            "anyrange or anymultirange",
        ),
        oids::ANYCOMPATIBLERANGE | oids::ANYCOMPATIBLEMULTIRANGE => (
            has(&[oids::ANYCOMPATIBLERANGE, oids::ANYCOMPATIBLEMULTIRANGE]),
            "anycompatiblerange or anycompatiblemultirange",
        ),
        oids::ANYELEMENT | oids::ANYARRAY | oids::ANYNONARRAY | oids::ANYENUM => (
            has(&[
                oids::ANYELEMENT,
                oids::ANYARRAY,
                oids::ANYNONARRAY,
                oids::ANYENUM,
                oids::ANYRANGE,
                oids::ANYMULTIRANGE,
            ]),
            "anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange",
        ),
        oids::ANYCOMPATIBLE | oids::ANYCOMPATIBLEARRAY | oids::ANYCOMPATIBLENONARRAY => (
            has(&[
                oids::ANYCOMPATIBLE,
                oids::ANYCOMPATIBLEARRAY,
                oids::ANYCOMPATIBLENONARRAY,
                oids::ANYCOMPATIBLERANGE,
                oids::ANYCOMPATIBLEMULTIRANGE,
            ]),
            "anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange",
        ),
        _ => return Ok(()),
    };
    if ok {
        return Ok(());
    }
    Err(PgError::new(INVALID_FUNCTION_DEFINITION, "cannot determine result data type").with_detail(format!(
        "A result of type {} requires at least one input of type {want}.",
        b.type_name(TypeId::of(ret))
    )))
}

/// PG `ProcedureCreate` / `sql_validator`: a SQL-language function cannot take a pseudo-type argument,
/// except the polymorphic ones (42P13 `SQL functions cannot have arguments of type X`).
fn check_sql_arg_types(b: &Binder<'_>, specs: &[ParamSpec<'_>]) -> Result<()> {
    for p in specs.iter().filter(|p| p.is_input()) {
        if is_polymorphic(p.type_oid) {
            continue;
        }
        let typtype = pgarbor_types::registry::type_info(b.env, p.type_oid)?.typtype;
        if typtype == b'p' {
            return Err(PgError::new(
                INVALID_FUNCTION_DEFINITION,
                format!("SQL functions cannot have arguments of type {}", b.type_name(TypeId::of(p.type_oid))),
            ));
        }
    }
    Ok(())
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
    Ok(text)
}

/// One token of a PL/pgSQL body, as far as the FETCH check needs it. Comments are dropped; quoted strings
/// and quoted identifiers are reduced to their opening character.
#[derive(Debug, PartialEq)]
enum PlTok {
    /// Lower-cased unquoted word.
    Word(String),
    /// Any other character.
    Other(char),
}

fn is_pl_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Index of the closing `$` of a dollar-quote opening tag starting at `i` (`$$` or `$tag$`, the tag not
/// starting with a digit and not preceded by an identifier character).
fn dollar_tag_end(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'$') || (i > 0 && is_pl_ident_char(chars[i - 1])) {
        return None;
    }
    let mut j = i + 1;
    while j < chars.len() && is_pl_ident_char(chars[j]) {
        j += 1;
    }
    let digit_start = chars.get(i + 1).is_some_and(|d| d.is_ascii_digit());
    (chars.get(j) == Some(&'$') && !digit_start).then_some(j)
}

fn tok_is_word(tok: &PlTok, word: &str) -> bool {
    matches!(tok, PlTok::Word(w) if w.as_str() == word)
}

/// Tokens of a PL/pgSQL body, each with the char index of its first character.
fn plpgsql_tokens(text: &str) -> Vec<(usize, PlTok)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // Block comments nest (scan.l `xcdepth`).
            let mut depth = 1usize;
            i += 2;
            while depth > 0 && i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == '\'' || c == '"' {
            out.push((i, PlTok::Other(c)));
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if let Some(tag_end) = dollar_tag_end(&chars, i) {
            // Dollar-quoted string `$tag$ ... $tag$` (`$$ ... $$` when the tag is empty).
            let tag: Vec<char> = chars[i..=tag_end].to_vec();
            out.push((i, PlTok::Other('$')));
            let mut k = tag_end + 1;
            while k < chars.len() && !chars[k..].starts_with(&tag) {
                k += 1;
            }
            i = (k + tag.len()).min(chars.len());
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && is_pl_ident_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
            out.push((start, PlTok::Word(word)));
        } else {
            out.push((i, PlTok::Other(c)));
            i += 1;
        }
    }
    out
}

/// Whether the tokens after a FETCH keyword name a direction that can return several rows (pl_gram.y
/// `read_fetch_direction` / `complete_direction`: ALL, FORWARD|BACKWARD with anything but FROM|IN after
/// it, or a bare count expression). A bare word is not decided here: it may be a variable or a cursor.
fn fetch_direction_is_multi(after: &[(usize, PlTok)]) -> bool {
    let word_at = |i: usize| match after.get(i) {
        Some((_, PlTok::Word(w))) => Some(w.as_str()),
        _ => None,
    };
    match after.first() {
        Some((_, PlTok::Word(w))) => match w.as_str() {
            "all" => true,
            "forward" | "backward" => !matches!(word_at(1), Some("from" | "in")),
            _ => false,
        },
        Some((_, PlTok::Other(c))) => c.is_ascii_digit() || matches!(*c, '(' | '$' | '\'' | '-' | '+'),
        None => false,
    }
}

/// Char index (in `text`) of the first FETCH statement whose direction can return several rows: the
/// compile-time check of pl_gram.y `stmt_fetch` ("FETCH statement cannot return multiple rows").
/// Only FETCH at a statement start is considered (after `;`, `BEGIN`, `THEN`, `ELSE` or `LOOP`).
fn plpgsql_multi_row_fetch(text: &str) -> Option<usize> {
    let toks = plpgsql_tokens(text);
    for (k, (at, tok)) in toks.iter().enumerate() {
        if !tok_is_word(tok, "fetch") {
            continue;
        }
        let stmt_start = match k.checked_sub(1).map(|p| &toks[p].1) {
            None => true,
            Some(PlTok::Other(';')) => true,
            Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "then" | "else" | "loop"),
            Some(PlTok::Other(_)) => false,
        };
        if stmt_start && fetch_direction_is_multi(&toks[k + 1..]) {
            return Some(*at);
        }
    }
    None
}

/// Statement position of char `idx` of the body `text` of the `AS` item `e`. The body starts after the
/// opening quote or `$tag$`; a single-quoted body with any quote in it is not mapped (doubled quotes).
fn plpgsql_body_position(b: &Binder<'_>, e: &Expr, text: &str, idx: usize) -> Option<u32> {
    let prefix: u32 = match e {
        Expr::Value(v) => match &v.value {
            Value::DollarQuotedString(d) => 2 + d.tag.as_ref().map_or(0, |t| t.chars().count() as u32),
            Value::SingleQuotedString(_) if !text.contains('\'') => 1,
            _ => return None,
        },
        _ => return None,
    };
    let start = b.pos(e)? + prefix;
    Some(start + idx as u32)
}

/// Name of a top-level DECLARE item `name CONSTANT ...` (pl_gram.y `decl_statement`), if it is one.
fn constant_decl_name(decl: &[(usize, PlTok)]) -> Option<&str> {
    match decl {
        [(_, PlTok::Word(name)), (_, second), ..] if tok_is_word(second, "constant") => Some(name.as_str()),
        _ => None,
    }
}

/// Char index and name of the first assignment to a `CONSTANT` variable of the top-level DECLARE section:
/// PG `check_assignable` (pl_gram.y), raised while the body is compiled. Only a plain name at a statement
/// start (after `;`, `BEGIN`, `THEN`, `ELSE` or `LOOP`) followed by `:=` or `=` is an assignment, so a named
/// argument `f(c := 1)` is not matched. A body with a nested DECLARE is not checked, because a nested block
/// may redeclare the name (known gap).
fn plpgsql_constant_assignment(text: &str) -> Option<(usize, String)> {
    let toks = plpgsql_tokens(text);
    if !toks.first().is_some_and(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let begin = toks.iter().position(|(_, t)| tok_is_word(t, "begin"))?;
    if toks[begin + 1..].iter().any(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    // A duplicate declaration is an earlier compile error, so the body is not reached.
    if plpgsql_duplicate_declaration(text).is_some() {
        return None;
    }
    // Declarations up to BEGIN, split at `;` outside parentheses.
    let mut consts: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    let mut decl_start = 1;
    for k in 1..begin {
        match &toks[k].1 {
            PlTok::Other('(') => depth += 1,
            PlTok::Other(')') => depth -= 1,
            PlTok::Other(';') if depth == 0 => {
                let mut first = decl_start;
                while first < k && tok_is_word(&toks[first].1, "declare") {
                    first += 1;
                }
                consts.extend(constant_decl_name(&toks[first..k]));
                decl_start = k + 1;
            }
            _ => {}
        }
    }
    if consts.is_empty() {
        return None;
    }
    for k in begin + 1..toks.len() {
        let (at, tok) = &toks[k];
        let PlTok::Word(name) = tok else { continue };
        if !consts.contains(&name.as_str()) {
            continue;
        }
        let stmt_start = match k.checked_sub(1).map(|p| &toks[p].1) {
            Some(PlTok::Other(';')) => true,
            Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "then" | "else" | "loop"),
            _ => false,
        };
        let assigns = match toks.get(k + 1) {
            Some((_, PlTok::Other('='))) => true,
            Some((colon, PlTok::Other(':'))) => {
                matches!(toks.get(k + 2), Some((eq, PlTok::Other('='))) if *eq == colon + 1)
            }
            _ => false,
        };
        if stmt_start && assigns {
            return Some((*at, name.clone()));
        }
    }
    None
}

/// Char index (of the `NOT` keyword) and name of a declaration in `decl` (the tokens of one DECLARE item, without
/// its `;`) that is `NOT NULL` with no default: PG pl_gram.y `decl_statement` rejects it at compile time. The
/// `NOT NULL` of a declaration follows its type, so it is default-less when nothing follows it. A `NOT NULL`
/// after `=`, `:=` or `DEFAULT` belongs to the default expression. Aliases and cursors are skipped.
fn not_null_without_default(decl: &[(usize, PlTok)]) -> Option<(usize, String)> {
    let [(_, PlTok::Word(name)), rest @ ..] = decl else {
        return None;
    };
    if rest.iter().any(|(_, t)| tok_is_word(t, "alias") || tok_is_word(t, "cursor")) {
        return None;
    }
    for (j, (at, tok)) in rest.iter().enumerate() {
        if matches!(tok, PlTok::Other('=')) || tok_is_word(tok, "default") {
            return None;
        }
        if tok_is_word(tok, "not") && rest.get(j + 1).is_some_and(|(_, t)| tok_is_word(t, "null")) {
            return (j + 2 == rest.len()).then(|| (*at, name.clone()));
        }
    }
    None
}

/// First top-level DECLARE item (in order) that is `NOT NULL` without a default, see [`not_null_without_default`].
/// A DECLARE section with a leading label or a nested block is not checked (known gap).
fn plpgsql_not_null_without_default(text: &str) -> Option<(usize, String)> {
    let toks = plpgsql_tokens(text);
    if !toks.first().is_some_and(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let begin = toks.iter().position(|(_, t)| tok_is_word(t, "begin")).unwrap_or(toks.len());
    let mut item_start = 1;
    for k in 1..begin {
        if matches!(toks[k].1, PlTok::Other(';')) {
            if let Some(hit) = not_null_without_default(&toks[item_start..k]) {
                return Some(hit);
            }
            item_start = k + 1;
        }
    }
    None
}

/// Char index and length of the first top-level `UNION`, `INTERSECT` or `EXCEPT` in a PL/pgSQL expression. pl_gram.y
/// reads an expression with the `PLpgSQL_Expr` grammar (gram.y), a simple SELECT without set operations, so such a
/// keyword is a syntax error at that keyword. The expressions checked are the right side of an assignment
/// (`x := ...`, `x = ...`), `RETURN expr` (not `RETURN QUERY` / `RETURN NEXT`) and the condition of `IF`, `ELSIF`
/// or `WHILE`. A keyword inside parentheses (a subquery) or inside a CASE expression is not reported.
fn plpgsql_expr_set_op(text: &str) -> Option<(usize, usize)> {
    let toks = plpgsql_tokens(text);
    for (k, (_, tok)) in toks.iter().enumerate() {
        let stmt_start = match k.checked_sub(1).map(|p| &toks[p].1) {
            None => true,
            Some(PlTok::Other(';')) => true,
            Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "then" | "else" | "loop"),
            Some(PlTok::Other(_)) => false,
        };
        if !stmt_start {
            continue;
        }
        // First token of the expression, and a keyword that ends it besides `;`.
        let (first, stop) = match tok {
            PlTok::Word(w) if matches!(w.as_str(), "if" | "elsif") => (k + 1, Some("then")),
            PlTok::Word(w) if w.as_str() == "while" => (k + 1, Some("loop")),
            PlTok::Word(w) if w.as_str() == "return" => {
                if matches!(toks.get(k + 1), Some((_, PlTok::Word(n))) if matches!(n.as_str(), "query" | "next")) {
                    continue;
                }
                (k + 1, None)
            }
            PlTok::Word(_) => match (toks.get(k + 1), toks.get(k + 2)) {
                (Some((_, PlTok::Other('='))), _) => (k + 2, None),
                (Some((colon, PlTok::Other(':'))), Some((eq, PlTok::Other('=')))) if *eq == colon + 1 => (k + 3, None),
                _ => continue,
            },
            PlTok::Other(_) => continue,
        };
        let mut depth = 0i32;
        let mut case_depth = 0u32;
        for (at, t) in &toks[first..] {
            match t {
                PlTok::Other('(') => depth += 1,
                PlTok::Other(')') => depth -= 1,
                PlTok::Other(';') => break,
                PlTok::Word(w) if depth == 0 => match w.as_str() {
                    "case" => case_depth += 1,
                    "end" => case_depth = case_depth.saturating_sub(1),
                    "union" | "intersect" | "except" if case_depth == 0 => return Some((*at, w.chars().count())),
                    s if case_depth == 0 && Some(s) == stop => break,
                    _ => {}
                },
                _ => {}
            }
        }
    }
    None
}

/// Char index of the `;` when a PL/pgSQL body starts with `BEGIN` directly followed by `;` (pl_gram.y `pl_block`
/// expects a declaration or a statement after BEGIN, so the `;` is the syntax error). Labels are not stripped.
fn plpgsql_begin_semicolon(text: &str) -> Option<usize> {
    let toks = plpgsql_tokens(text);
    match (toks.first(), toks.get(1)) {
        (Some((_, first)), Some((at, PlTok::Other(';')))) if tok_is_word(first, "begin") => Some(*at),
        _ => None,
    }
}

/// Block labels of a top-level PL/pgSQL body (pl_gram.y `pl_block` -> `check_labels`): the leading `<< label >>` and
/// the label after the final `END`. Returns the char index and name of the END label, and the block's label, when
/// the END label differs from it (or the block has no label). A quoted label is not checked (known gap), and a
/// nested block is not checked.
fn plpgsql_end_label_mismatch(text: &str) -> Option<(usize, String, Option<String>)> {
    let toks = plpgsql_tokens(text);
    let start_label = match toks.as_slice() {
        [(_, PlTok::Other('<')), (_, PlTok::Other('<')), (_, PlTok::Word(l)), (_, PlTok::Other('>')), (_, PlTok::Other('>')), ..] => {
            Some(l.clone())
        }
        [(_, PlTok::Other('<')), (_, PlTok::Other('<')), ..] => return None,
        _ => None,
    };
    let mut end = toks.len();
    if matches!(toks.last(), Some((_, PlTok::Other(';')))) {
        end -= 1;
    }
    let [.., (_, PlTok::Word(end_kw)), (at, PlTok::Word(label))] = &toks[..end] else {
        return None;
    };
    if end_kw.as_str() != "end" || matches!(label.as_str(), "if" | "loop" | "case") {
        return None;
    }
    if start_label.as_deref() == Some(label.as_str()) {
        return None;
    }
    Some((*at, label.clone(), start_label))
}

/// Words that cannot be a block end label (PG pl_reserved_kwlist.h): after a subblock's END, one of these is not a label.
const PLPGSQL_RESERVED_WORDS: &[&str] = &[
    "all", "begin", "by", "case", "declare", "else", "end", "execute", "for", "foreach", "from", "if", "in", "into", "loop",
    "not", "null", "or", "strict", "then", "to", "using", "when", "while",
];

/// Label of the block whose first token (`BEGIN`, or `DECLARE` of its section) is `toks[k]`: a `<< label >>` just before it.
fn plpgsql_block_label_before(toks: &[(usize, PlTok)], k: usize) -> Option<String> {
    match k.checked_sub(5).and_then(|s| toks.get(s..k)) {
        Some([(_, PlTok::Other('<')), (_, PlTok::Other('<')), (_, PlTok::Word(l)), (_, PlTok::Other('>')), (_, PlTok::Other('>'))]) => {
            Some(l.clone())
        }
        _ => None,
    }
}

/// Char index and error of a subblock END whose end label is wrong (pl_gram.y `pl_block` -> `check_labels`, run when the
/// subblock's END is parsed). After the END of a subblock, a word that is not reserved is its end label: it must be the
/// subblock's own label, else PG reports it (`RETURN` after an unterminated subblock END is taken as a label). The
/// outermost block is checked by `plpgsql_end_label_mismatch`. Open IF, LOOP and CASE constructs are tracked on the
/// stack so an END that closes them (`END IF`, `END LOOP`, `END CASE`) or a CASE expression's END is not a block END.
fn plpgsql_nested_end_label(text: &str) -> Option<(usize, PgError)> {
    let toks = plpgsql_tokens(text);
    // One entry per open construct: `Some((label, outermost))` for a block, `None` for IF, LOOP and CASE.
    let mut open: Vec<Option<(Option<String>, bool)>> = Vec::new();
    let mut declare_label: Option<String> = None;
    for (k, (_, tok)) in toks.iter().enumerate() {
        let PlTok::Word(word) = tok else { continue };
        let prev = k.checked_sub(1).map(|p| &toks[p].1);
        let after_end = matches!(prev, Some(PlTok::Word(w)) if w.as_str() == "end");
        let stmt_start = match prev {
            None | Some(PlTok::Other(';')) => true,
            Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "then" | "else" | "loop"),
            Some(PlTok::Other(_)) => false,
        };
        match word.as_str() {
            "declare" => declare_label = plpgsql_block_label_before(&toks, k),
            "begin" => {
                let label = declare_label.take().or_else(|| plpgsql_block_label_before(&toks, k));
                let outermost = open.is_empty();
                open.push(Some((label, outermost)));
            }
            "loop" | "case" if !after_end => open.push(None),
            "if" if !after_end && stmt_start => open.push(None),
            "end" => {
                let popped = open.pop().flatten();
                let closes_statement = match toks.get(k + 1) {
                    Some((_, PlTok::Word(w))) => matches!(w.as_str(), "if" | "loop" | "case"),
                    _ => false,
                };
                if closes_statement {
                    continue;
                }
                let Some((label, false)) = popped else { continue };
                let Some((name_at, PlTok::Word(name))) = toks.get(k + 1) else { continue };
                if PLPGSQL_RESERVED_WORDS.contains(&name.as_str()) {
                    continue;
                }
                let msg = match &label {
                    Some(start) if start == name => continue,
                    Some(start) => format!("end label \"{name}\" differs from block's label \"{start}\""),
                    None => format!("end label \"{name}\" specified for unlabeled block"),
                };
                return Some((*name_at, PgError::new(SqlState::SYNTAX_ERROR, msg)));
            }
            _ => {}
        }
    }
    None
}

/// Name (lower-cased) and char index of each top-level DECLARE item of `toks[..begin]`, in order. The first
/// token of an item is its name; items are split at `;` outside parentheses. Expects `toks[0]` to be DECLARE.
fn declared_names(toks: &[(usize, PlTok)], begin: usize) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut item_start = 1;
    for k in 1..begin {
        match &toks[k].1 {
            PlTok::Other('(') => depth += 1,
            PlTok::Other(')') => depth -= 1,
            PlTok::Other(';') if depth == 0 => {
                // pl_gram.y `decl_stmt` accepts useless extra `DECLARE` keywords before an item.
                let mut first = item_start;
                while first < k && tok_is_word(&toks[first].1, "declare") {
                    first += 1;
                }
                if let Some((at, PlTok::Word(name))) = toks.get(first).filter(|_| first < k) {
                    out.push((*at, name.as_str()));
                }
                item_start = k + 1;
            }
            _ => {}
        }
    }
    out
}

/// Char index (of the name) and name of the first top-level DECLARE item whose name an earlier item of the same
/// section already declared. PG pl_gram.y reports it while the declarations compile ("duplicate declaration").
/// Names are compared as unquoted identifiers (lower-cased); a nested block is not checked.
fn plpgsql_duplicate_declaration(text: &str) -> Option<(usize, String)> {
    let toks = plpgsql_tokens(text);
    if !toks.first().is_some_and(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let begin = toks.iter().position(|(_, t)| tok_is_word(t, "begin")).unwrap_or(toks.len());
    let mut seen: Vec<&str> = Vec::new();
    for (at, name) in declared_names(&toks, begin) {
        if seen.contains(&name) {
            return Some((at, name.to_string()));
        }
        seen.push(name);
    }
    None
}

/// Char index and error of the first `OPEN name` of the body on a cursor declared at top level with an explicit
/// query whose arguments do not match its declaration: PG pl_gram.y `read_cursor_args` reports "has arguments",
/// "has no arguments", "not enough arguments" and "too many arguments" (42601) while the OPEN parses. The index
/// is that of the token PG's cursor sits on (the token after the name, the `(`, the closing `)` or the surplus
/// `,`). Only unquoted names are handled.
fn plpgsql_cursor_args(text: &str) -> Option<(usize, PgError)> {
    let toks = plpgsql_tokens(text);
    if !toks.first().is_some_and(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let begin = toks.iter().position(|(_, t)| tok_is_word(t, "begin"))?;
    // (name, declared argument count) of each cursor declaration.
    let mut cursors: Vec<(&str, usize)> = Vec::new();
    let mut depth = 0i32;
    let mut item_start = 1;
    for k in 1..begin {
        match &toks[k].1 {
            PlTok::Other('(') => depth += 1,
            PlTok::Other(')') => depth -= 1,
            PlTok::Other(';') if depth == 0 => {
                let item = &toks[item_start..k];
                item_start = k + 1;
                let Some((_, PlTok::Word(name))) = item.first() else { continue };
                let Some(c) = item.iter().take(4).position(|(_, t)| tok_is_word(t, "cursor")) else { continue };
                if c == 0 {
                    continue;
                }
                let mut nargs = 0;
                if matches!(item.get(c + 1), Some((_, PlTok::Other('(')))) {
                    let mut d = 0i32;
                    nargs = 1;
                    for (_, t) in &item[c + 1..] {
                        match t {
                            PlTok::Other('(') => d += 1,
                            PlTok::Other(')') => {
                                d -= 1;
                                if d == 0 {
                                    break;
                                }
                            }
                            PlTok::Other(',') if d == 1 => nargs += 1,
                            _ => {}
                        }
                    }
                }
                cursors.push((name.as_str(), nargs));
            }
            _ => {}
        }
    }
    if cursors.is_empty() {
        return None;
    }
    let syntax = |msg: String| PgError::new(SqlState::SYNTAX_ERROR, msg);
    for k in begin + 1..toks.len() {
        if !tok_is_word(&toks[k].1, "open") || !plpgsql_stmt_start(&toks, k) {
            continue;
        }
        let Some((_, PlTok::Word(name))) = toks.get(k + 1) else { continue };
        let Some(&(_, nargs)) = cursors.iter().find(|(n, _)| *n == name.as_str()) else { continue };
        let Some((next_at, next)) = toks.get(k + 2) else { continue };
        let is_paren = matches!(next, PlTok::Other('('));
        if nargs == 0 {
            if is_paren {
                return Some((*next_at, syntax(format!("cursor \"{name}\" has no arguments"))));
            }
            continue;
        }
        if !is_paren {
            return Some((*next_at, syntax(format!("cursor \"{name}\" has arguments"))));
        }
        let mut d = 0i32;
        let mut commas = 0usize;
        for (at, t) in &toks[k + 2..] {
            match t {
                PlTok::Other('(') => d += 1,
                PlTok::Other(')') => {
                    d -= 1;
                    if d == 0 {
                        if commas + 1 < nargs {
                            return Some((*at, syntax(format!("not enough arguments for cursor \"{name}\""))));
                        }
                        break;
                    }
                }
                PlTok::Other(',') if d == 1 => {
                    commas += 1;
                    if commas == nargs {
                        return Some((*at, syntax(format!("too many arguments for cursor \"{name}\""))));
                    }
                }
                PlTok::Other(';') => break,
                _ => {}
            }
        }
    }
    None
}

/// Whether the token at `k` starts a statement: it follows `;`, `BEGIN`, `THEN`, `ELSE` or `LOOP`.
fn plpgsql_stmt_start(toks: &[(usize, PlTok)], k: usize) -> bool {
    match k.checked_sub(1).map(|p| &toks[p].1) {
        Some(PlTok::Other(';')) => true,
        Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "then" | "else" | "loop"),
        _ => false,
    }
}

/// Whether the tokens after `toks[k]` are the assignment operator `:=`. A bare `=` is not used: after THEN or
/// ELSE it may be a SQL comparison inside a CASE expression.
fn plpgsql_assign_op_after(toks: &[(usize, PlTok)], k: usize) -> bool {
    match (toks.get(k + 1), toks.get(k + 2)) {
        (Some((colon, PlTok::Other(':'))), Some((eq, PlTok::Other('=')))) => *eq == colon + 1,
        _ => false,
    }
}

/// Special variables of the PL/pgSQL namespace (FOUND, SQLSTATE, SQLERRM, NEW/OLD, TG_* and PG_* names); they are
/// never reported as undeclared, so the check stays on the ordinary variables.
fn plpgsql_builtin_var(name: &str) -> bool {
    matches!(name, "found" | "sqlstate" | "sqlerrm" | "new" | "old") || name.starts_with("pg_") || name.starts_with("tg_")
}

/// Char index and name of the first assignment `name := ...` (or `name = ...`) at a statement start of the body
/// whose name is not a known variable: PG pl_gram.y `plpgsql_parse_word` reports it while the body compiles
/// ("\"name\" is not a known variable"). Known variables are the parameters (`params`, lower-cased here), the
/// top-level DECLARE names, FOR/FOREACH loop variables and the special variables. A body with a labelled DECLARE
/// section or a nested DECLARE is not checked, because its block structure is not tracked (known gap).
fn plpgsql_undeclared_assignment(text: &str, params: &[&str]) -> Option<(usize, String)> {
    let toks = plpgsql_tokens(text);
    let begin = toks.iter().position(|(_, t)| tok_is_word(t, "begin"))?;
    if toks[begin + 1..].iter().any(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let declares = toks[..begin].iter().any(|(_, t)| tok_is_word(t, "declare"));
    if declares && !toks.first().is_some_and(|(_, t)| tok_is_word(t, "declare")) {
        return None;
    }
    let mut known: Vec<String> = params.iter().map(|p| p.to_lowercase()).collect();
    if declares {
        known.extend(declared_names(&toks, begin).into_iter().map(|(_, n)| n.to_string()));
    }
    for k in 0..toks.len() {
        if tok_is_word(&toks[k].1, "for") || tok_is_word(&toks[k].1, "foreach") {
            if let Some((_, PlTok::Word(w))) = toks.get(k + 1) {
                known.push(w.clone());
            }
        }
    }
    for k in begin + 1..toks.len() {
        let (at, tok) = &toks[k];
        let PlTok::Word(name) = tok else { continue };
        if !plpgsql_stmt_start(&toks, k) || !plpgsql_assign_op_after(&toks, k) {
            continue;
        }
        if known.iter().any(|n| n == name) || plpgsql_builtin_var(name) {
            continue;
        }
        return Some((*at, name.clone()));
    }
    None
}

/// Words that can start a PL/pgSQL statement: its own keywords and the SQL command keywords a statement may begin with.
const PLPGSQL_STMT_WORDS: &[&str] = &[
    "abort", "alter", "analyse", "analyze", "assert", "begin", "call", "case", "checkpoint", "close", "cluster",
    "comment", "commit", "continue", "copy", "create", "deallocate", "declare", "delete", "discard", "do", "drop",
    "else", "elseif", "elsif", "end", "exception", "exit", "execute", "explain", "fetch", "for", "foreach", "get", "grant", "if",
    "import", "insert", "listen", "load", "lock", "loop", "merge", "move", "notify", "null", "open", "perform",
    "prepare", "raise", "reassign", "refresh", "reindex", "release", "reset", "return", "revoke", "rollback",
    "savepoint", "security", "select", "set", "show", "start", "table", "truncate", "unlisten", "update", "vacuum",
    "values", "when", "while", "with",
];

/// Char index and error of the first statement of a PL/pgSQL body that the statement grammar rejects without the SQL
/// parser: a statement word that is neither a PL/pgSQL nor a SQL command keyword (`SELEC 1;`) is a syntax error at that
/// word. Only the words after `;`, `BEGIN` or `LOOP` are statement starts (a word after `THEN` or `ELSE` may be a CASE
/// expression), and DECLARE sections are skipped. The error's position is not set.
fn plpgsql_statement_syntax(text: &str) -> Option<(usize, PgError)> {
    let toks = plpgsql_tokens(text);
    let chars: Vec<char> = text.chars().collect();
    let mut in_declare = false;
    for (k, (at, tok)) in toks.iter().enumerate() {
        let PlTok::Word(word) = tok else { continue };
        match word.as_str() {
            "declare" => in_declare = true,
            "begin" => in_declare = false,
            _ => {}
        }
        if in_declare {
            continue;
        }
        if word.as_str() == "return" {
            if let Some(found) = return_expr_syntax(&chars, &toks, k) {
                return Some(found);
            }
            continue;
        }
        let stmt_start = match k.checked_sub(1).map(|p| &toks[p].1) {
            Some(PlTok::Other(';')) => true,
            Some(PlTok::Word(w)) => matches!(w.as_str(), "begin" | "loop"),
            _ => false,
        };
        if !stmt_start || PLPGSQL_STMT_WORDS.contains(&word.as_str()) {
            continue;
        }
        // A plain name followed by an assignment target part (`:=`, `=`, `.field`, `[subscript]`) is an assignment.
        let assigned = match toks.get(k + 1) {
            Some((_, PlTok::Other(c))) => matches!(*c, ':' | '=' | '.' | '['),
            _ => false,
        };
        if assigned {
            continue;
        }
        let token: String = chars.iter().skip(*at).take_while(|c| is_pl_ident_char(**c)).collect();
        return Some((*at, syntax_error_near(&token, None)));
    }
    None
}

/// Syntax error of the RETURN statement at `toks[k]`. PL/pgSQL reads the expression up to the `;` and parses it as
/// `SELECT <expr>`, so the SQL parser's 42601 error is reported. The char index is the body token at the error, or the
/// `;` when the expression ends early (`syntax error at end of input`). `RETURN QUERY` and `RETURN NEXT` are not
/// expressions, and an empty `RETURN;` has none.
fn return_expr_syntax(chars: &[char], toks: &[(usize, PlTok)], k: usize) -> Option<(usize, PgError)> {
    if matches!(toks.get(k + 1), Some((_, PlTok::Word(w))) if matches!(w.as_str(), "query" | "next")) {
        return None;
    }
    let after = toks[k].0 + "return".len();
    let semi = toks[k + 1..].iter().find(|(_, t)| matches!(t, PlTok::Other(';')))?.0;
    let expr: String = chars.iter().skip(after).take(semi.saturating_sub(after)).collect();
    if expr.trim().is_empty() {
        return None;
    }
    let err = parse(&format!("SELECT {expr}")).err()?;
    if err.sqlstate != SqlState::SYNTAX_ERROR {
        return None;
    }
    // "SELECT " is 7 characters and positions are 1-based, so the offset in the expression is `position - 8`.
    let idx = match err.position {
        Some(p) => (after + (p as usize).saturating_sub(8)).min(semi),
        None => semi,
    };
    Some((idx, err))
}

/// SQLSTATE 22005 `error_in_assignment`.
const ERROR_IN_ASSIGNMENT: SqlState = SqlState(*b"22005");

/// Char index of the `/*` that opens a block comment left unterminated in a PL/pgSQL body (PG scan.l). Block comments
/// nest, so an unmatched opener swallows the rest of the body. Quoted strings, dollar-quoted strings and `--` line
/// comments are skipped the way the scanner skips them, so an opener inside them is not a comment.
fn plpgsql_unterminated_comment(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let start = i;
            let mut depth = 1usize;
            i += 2;
            while depth > 0 && i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Some(start);
            }
        } else if c == '\'' || c == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if let Some(tag_end) = dollar_tag_end(&chars, i) {
            let tag: Vec<char> = chars[i..=tag_end].to_vec();
            let mut k = tag_end + 1;
            while k < chars.len() && !chars[k..].starts_with(&tag) {
                k += 1;
            }
            i = k + tag.len();
        } else {
            i += 1;
        }
    }
    None
}

/// PL/pgSQL compile of a body (`check_plpgsql_compile`), after the lexical check of PG scan.l: an unterminated block
/// comment is raised when the scanner reaches it, so only the text before it is compiled first, and the comment's
/// `unterminated /* comment` 42601 error follows if none of those checks fail.
fn check_plpgsql_body(
    b: &Binder<'_>,
    e: &Expr,
    text: &str,
    lang: &BuiltinLanguage,
    params: &[ParamSpec<'_>],
) -> Result<()> {
    if lang.name != "plpgsql" {
        return Ok(());
    }
    let Some(open) = plpgsql_unterminated_comment(text) else {
        return check_plpgsql_compile(b, e, text, lang, params);
    };
    let before: String = text.chars().take(open).collect();
    check_plpgsql_compile(b, e, &before, lang, params)?;
    let near: String = text.chars().skip(open).collect();
    Err(PgError::syntax(
        format!("unterminated /* comment at or near \"{near}\""),
        plpgsql_body_position(b, e, text, open),
    ))
}

/// PL/pgSQL compile-time checks made without a PL/pgSQL parser (the PL/pgSQL interpreter is not
/// implemented, so only the duplicate declaration rule, the NOT NULL default rule, the undeclared assignment rule,
/// the statement syntax and RETURN expression rules, the FETCH direction rule, the CONSTANT assignment rule and the
/// top-level set-operation rule are enforced; other
/// bodies are stored unchecked). The earliest error in the body is reported, as PG compiles in order.
fn check_plpgsql_compile(
    b: &Binder<'_>,
    e: &Expr,
    text: &str,
    lang: &BuiltinLanguage,
    params: &[ParamSpec<'_>],
) -> Result<()> {
    if lang.name != "plpgsql" {
        return Ok(());
    }
    // pl_gram.y `pl_block`: a `;` right after the opening BEGIN is a syntax error. No earlier token can fail, so
    // this is the first body error whenever it is present.
    if let Some(idx) = plpgsql_begin_semicolon(text) {
        return Err(syntax_error_near(";", plpgsql_body_position(b, e, text, idx)));
    }
    // Declarations compile before the body, so the NOT NULL and duplicate rules are checked first, in order.
    let not_null = plpgsql_not_null_without_default(text);
    if let Some((idx, name)) = plpgsql_duplicate_declaration(text).filter(|(d, _)| not_null.as_ref().map_or(true, |(n, _)| d < n)) {
        let mut err = PgError::new(SqlState::SYNTAX_ERROR, format!("duplicate declaration at or near \"{name}\""));
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    if let Some((idx, name)) = not_null {
        let msg = format!("variable \"{name}\" must have a default value, since it's declared NOT NULL");
        let mut err = PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, msg);
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    let fetch = plpgsql_multi_row_fetch(text);
    let before_fetch = |idx: usize| fetch.map_or(true, |f| idx < f);
    // An undeclared assignment is raised when its statement compiles, so it is the earliest body error when it
    // comes first (the other checks below are filtered against it).
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    let undeclared = plpgsql_undeclared_assignment(text, &names);
    let before_undeclared = |idx: usize| undeclared.as_ref().map_or(true, |(u, _)| idx < *u);
    // A statement syntax error, or a wrong subblock END label, is raised when its statement is parsed, so the checks
    // after it are not reported.
    let syntax = match (plpgsql_statement_syntax(text), plpgsql_nested_end_label(text)) {
        (Some(s), Some(n)) => Some(if n.0 < s.0 { n } else { s }),
        (s, n) => s.or(n),
    };
    let syntax = match (syntax, plpgsql_cursor_args(text)) {
        (Some(s), Some(c)) => Some(if c.0 < s.0 { c } else { s }),
        (s, c) => s.or(c),
    };
    let before_syntax = |idx: usize| syntax.as_ref().map_or(true, |(s, _)| idx < *s);
    let constant = plpgsql_constant_assignment(text)
        .filter(|(idx, _)| before_fetch(*idx) && before_undeclared(*idx) && before_syntax(*idx));
    // A top-level set operation is a syntax error at compile time, in statement order with the other checks.
    let set_op = plpgsql_expr_set_op(text).filter(|&(idx, _)| {
        before_fetch(idx)
            && before_undeclared(idx)
            && before_syntax(idx)
            && constant.as_ref().map_or(true, |(c, _)| idx < *c)
    });
    if let Some((idx, len)) = set_op {
        let token: String = text.chars().skip(idx).take(len).collect();
        return Err(syntax_error_near(&token, plpgsql_body_position(b, e, text, idx)));
    }
    if let Some((idx, name)) = undeclared.filter(|(u, _)| {
        before_fetch(*u)
            && before_syntax(*u)
            && constant.as_ref().map_or(true, |(c, _)| u < c)
            && set_op.map_or(true, |(s, _)| *u < s)
    }) {
        let mut err = PgError::new(SqlState::SYNTAX_ERROR, format!("\"{name}\" is not a known variable"));
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    if let Some((idx, name)) = constant {
        let mut err = PgError::new(ERROR_IN_ASSIGNMENT, format!("variable \"{name}\" is declared CONSTANT"));
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    if let Some(idx) = fetch.filter(|f| before_syntax(*f)) {
        let mut err = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "FETCH statement cannot return multiple rows");
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    if let Some((idx, mut err)) = syntax {
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    // check_labels runs when the outermost block's END is parsed, after every statement of the body.
    if let Some((idx, label, block)) = plpgsql_end_label_mismatch(text) {
        let msg = match block {
            Some(start) => format!("end label \"{label}\" differs from block's label \"{start}\""),
            None => format!("end label \"{label}\" specified for unlabeled block"),
        };
        let mut err = PgError::new(SqlState::SYNTAX_ERROR, msg);
        err.position = plpgsql_body_position(b, e, text, idx);
        return Err(err);
    }
    Ok(())
}

/// `check_function_bodies` for a SQL-language `AS` body (PG `prosqlvalidator` -> `check_sql_fn_statements`
/// and `check_sql_fn_retval`). Errors are repositioned from the body text to the statement text
/// (single-line bodies; the literal's first character is `e`'s position, plus the opening delimiter).
fn check_sql_body(
    b: &mut Binder<'_>,
    fname: &str,
    specs: &[ParamSpec<'_>],
    e: &Expr,
    text: &str,
    lang: &BuiltinLanguage,
    ret: Oid,
) -> Result<()> {
    if lang.oid != LANG_SQL {
        return Ok(());
    }
    let prefix: u32 = match e {
        Expr::Value(v) => match &v.value {
            Value::DollarQuotedString(d) => 2 + d.tag.as_ref().map_or(0, |t| t.chars().count() as u32),
            _ => 1,
        },
        _ => 1,
    };
    let body_start = b.pos(e).map(|q| q + prefix);
    check_sql_statements(b, fname, specs, text, ret).map_err(|mut err| {
        err.position = match (err.position, body_start) {
            (Some(p), Some(start)) if p >= 1 && !text.contains('\n') => Some(start + (p - 1)),
            _ => None,
        };
        err
    })
}

/// Parse the body text and bind its query statements in a scope of the input parameters (by name and
/// as `$n`). The outer statement's dependencies and parameter table are restored afterwards.
fn check_sql_statements(
    b: &mut Binder<'_>,
    fname: &str,
    specs: &[ParamSpec<'_>],
    text: &str,
    ret: Oid,
) -> Result<()> {
    let stmts = parse(text)?;
    // PG `haspolyarg`: with a polymorphic input the body is only parsed, never analyzed or checked.
    if specs.iter().any(|p| p.is_input() && is_polymorphic(p.type_oid)) {
        return Ok(());
    }
    let cols: Vec<(String, TypeId)> = specs
        .iter()
        .filter(|p| p.is_input() && !p.name.is_empty())
        .map(|p| (p.name.clone(), TypeId::of(p.type_oid)))
        .collect();
    let in_types: Vec<TypeId> = specs.iter().filter(|p| p.is_input()).map(|p| TypeId::of(p.type_oid)).collect();
    let saved_params = std::mem::replace(&mut b.params, ParamTable::new(&in_types));
    let deps_mark = b.deps.len();
    let r = match table_expr_scope(b, fname, &cols) {
        Ok(()) => {
            let r = bind_body_statements(b, &stmts);
            b.scopes.pop();
            r
        }
        Err(e) => Err(e),
    };
    b.params = saved_params;
    b.deps.truncate(deps_mark);
    check_sql_result(b, ret, &r?)
}

/// The last statement of a SQL function body, as far as the binder checks it (PG looks only at the
/// last statement: `check_sql_fn_retval`).
enum BodyTail {
    /// No statement at all (`AS ''`).
    Empty,
    /// A query; the output column type OIDs in order.
    Query(Vec<Oid>),
    /// Any other statement kind (not checked yet).
    Other,
}

/// Bind each query statement of a SQL function body and report the shape of the last statement.
/// Other statement kinds are not bound yet.
fn bind_body_statements(b: &mut Binder<'_>, stmts: &[RawStatement]) -> Result<BodyTail> {
    let mut tail = BodyTail::Empty;
    for raw in stmts {
        tail = BodyTail::Other;
        if let Parsed::Ast(stmt) = &raw.parsed {
            if let Statement::Query(q) = &**stmt {
                let ctx = QueryCtx { column_aliases: Vec::new(), top_level: true, keep_unknown_outputs: false };
                let bound = bind_query(b, q, &ctx)?;
                tail = BodyTail::Query(bound.output.iter().map(|c| c.ty.oid).collect());
            } else if matches!(&**stmt, Statement::Call(_)) {
                // PG `init_execution_state`: a nested CALL of a procedure with OUT/INOUT arguments is
                // rejected while the SQL body is validated.
                if let BoundStatement::Call(call) = bind_call(b, stmt)? {
                    let has_out = b.env.snapshot().function(call.procedure).is_some_and(|f| {
                        f.args.iter().any(|a| matches!(a.mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
                    });
                    if has_out {
                        return Err(PgError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "calling procedures with output arguments is not supported in SQL functions",
                        ));
                    }
                }
            }
        }
    }
    Ok(tail)
}

/// PG `check_sql_fn_retval` for a scalar declared result (base, domain, enum, range, multirange):
/// the final statement must be a query returning exactly one column that is assignment-coercible
/// to `ret`. Void, composite, pseudo-type and other statement results are not checked yet.
fn check_sql_result(b: &Binder<'_>, ret: Oid, tail: &BodyTail) -> Result<()> {
    if ret == oids::VOID {
        return Ok(());
    }
    let typtype = pgarbor_types::registry::type_info(b.env, ret)?.typtype;
    if !matches!(typtype, b'b' | b'd' | b'e' | b'r' | b'm') {
        return Ok(());
    }
    let mismatch = |detail: String| {
        PgError::new(
            INVALID_FUNCTION_DEFINITION,
            format!("return type mismatch in function declared to return {}", b.type_name(TypeId::of(ret))),
        )
        .with_detail(detail)
    };
    let cols = match tail {
        BodyTail::Other => return Ok(()),
        BodyTail::Empty => {
            return Err(mismatch(
                "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.".to_string(),
            ))
        }
        BodyTail::Query(cols) => cols,
    };
    if cols.len() != 1 {
        return Err(mismatch("Final statement must return exactly one column.".to_string()));
    }
    let actual = cols[0];
    if !can_coerce_type(b.env, &[actual], &[ret], CastContext::Assignment)? {
        return Err(mismatch(format!("Actual return type is {}.", b.type_name(TypeId::of(actual)))));
    }
    Ok(())
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

/// `COST`, `ROWS` and `[NOT] LEAKPROOF` of a CREATE FUNCTION, carried by the parser as marker `SET`
/// parameters (`parser::FN_ATTR_COST` and friends).
#[derive(Default)]
struct FnAttrs {
    cost: Option<f32>,
    rows: Option<f32>,
    leakproof: Option<bool>,
}

/// Split the attribute markers off `items`: returns the genuine `SET` parameters and the attributes.
/// A repeated attribute is 42601 `conflicting or redundant options`.
fn take_attr_markers(items: &[FunctionDefinitionSetParam]) -> Result<(Vec<FunctionDefinitionSetParam>, FnAttrs)> {
    let mut rest = Vec::with_capacity(items.len());
    let mut attrs = FnAttrs::default();
    for item in items {
        let marker = match item.name.0.as_slice() {
            [ObjectNamePart::Identifier(id)] => Some(id.value.as_str()),
            _ => None,
        };
        let number = match &item.value {
            FunctionSetValue::Values(vals) => match vals.as_slice() {
                [Expr::Value(vs)] => match &vs.value {
                    Value::Number(n, _) => n.parse::<f64>().ok(),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        let conflict = || PgError::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options");
        match (marker, number) {
            (Some(crate::parser::FN_ATTR_COST), Some(n)) => {
                if attrs.cost.replace(n as f32).is_some() {
                    return Err(conflict());
                }
            }
            (Some(crate::parser::FN_ATTR_ROWS), Some(n)) => {
                if attrs.rows.replace(n as f32).is_some() {
                    return Err(conflict());
                }
            }
            (Some(crate::parser::FN_ATTR_LEAKPROOF), Some(n)) => {
                if attrs.leakproof.replace(n != 0.0).is_some() {
                    return Err(conflict());
                }
            }
            _ => rest.push(item.clone()),
        }
    }
    Ok((rest, attrs))
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
        // PG `GUC_NO_RESET` parameters (guc.c set_config_option, GUC_ACTION_SAVE): a function cannot SET them.
        if matches!(name.to_ascii_lowercase().as_str(), "transaction_isolation" | "transaction_read_only" | "transaction_deferrable") {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("parameter \"{}\" cannot be set locally in functions", name.to_ascii_lowercase()),
            ));
        }
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
    // CREATE PROCEDURE (`CREATE_PROCEDURE_KEY`) takes the function grammar without RETURNS (PG gram.y
    // `CreateProcedureStmt`); its result is VOID, or RECORD with OUT parameters.
    let is_procedure = b.scratch.contains_key(CREATE_PROCEDURE_KEY);
    if is_procedure && cf.return_type.is_some() {
        return Err(syntax_error_near("RETURNS", None));
    }
    let env: &CatalogEnv = b.env;
    let qn = b.qual(&cf.name)?;

    // Parameters (types resolved; the list checks run once all are known).
    let mut specs: Vec<ParamSpec<'_>> = Vec::new();
    for a in cf.args.as_deref().unwrap_or(&[]) {
        specs.push(operate_arg_spec(b, a)?);
    }
    let required = check_param_list(&specs, is_procedure, &|oid: Oid| is_array_type(env, oid))?;

    // LANGUAGE (RETURN bodies default to SQL).
    let inline_body = matches!(
        cf.function_body,
        Some(CreateFunctionBody::Return(_)) | Some(CreateFunctionBody::AsReturnExpr(_))
    );
    if is_procedure && inline_body {
        return Err(syntax_error_near("RETURN", None));
    }
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
        (None, None) if is_procedure => (oids::VOID, false),
        (None, None) => return Err(invalid_fn_def("function result type must be specified")),
    };

    // Body. A SQL-language `AS` body is validated after the name/replace checks (PG runs the validator
    // after `ProcedureCreate`), so a duplicate function still reports 42723 first.
    let mut sql_check: Option<(&Expr, String)> = None;
    let (body, sql_body) = match &cf.function_body {
        None => return Err(invalid_fn_def("no function body specified")),
        Some(CreateFunctionBody::AsBeforeOptions { body: e, link_symbol }) => {
            let text = as_string_body(b, e, link_symbol.is_some(), lang)?;
            sql_check = Some((e, text.clone()));
            (text, None)
        }
        Some(CreateFunctionBody::AsAfterOptions(e)) => {
            let text = as_string_body(b, e, false, lang)?;
            sql_check = Some((e, text.clone()));
            (text, None)
        }
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

    // Attributes (compute_function_attributes). A procedure takes none of the volatility, null-input or
    // parallel attributes (PG `compute_common_attribute`).
    let (set_params, attrs) = take_attr_markers(&cf.set_params)?;
    if is_procedure
        && (cf.behavior.is_some()
            || cf.called_on_null.is_some()
            || cf.parallel.is_some()
            || attrs.cost.is_some()
            || attrs.rows.is_some()
            || attrs.leakproof.is_some())
    {
        return Err(invalid_fn_def("invalid attribute in procedure definition"));
    }
    if attrs.cost.is_some_and(|c| c <= 0.0) {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "COST must be positive"));
    }
    if attrs.rows.is_some_and(|r| r <= 0.0) {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "ROWS must be positive"));
    }
    if attrs.leakproof == Some(true) {
        return Err(PgError::unimplemented("SQL-37", "CREATE FUNCTION ... LEAKPROOF"));
    }
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
    let config = set_config(b, &set_params)?;
    // PG ProcedureCreate inputs (functioncmds.c `CreateFunction`): ROWS needs a set-returning function.
    if attrs.rows.is_some() && !retset {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "ROWS is not applicable when function does not return a set",
        ));
    }
    let rows = match attrs.rows {
        Some(r) => r,
        None if retset => 1000.0f32,
        None => 0.0f32,
    };
    let cost = attrs.cost.unwrap_or(100.0f32);

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
    let kind = if is_procedure { FuncKind::Procedure } else { FuncKind::Function };
    let in_types: Vec<Oid> = specs.iter().filter(|p| p.is_input()).map(|p| p.type_oid).collect();
    let n_defaults = specs.iter().filter(|p| p.default.is_some()).count();
    // PG ProcedureCreate: a polymorphic result must be deducible from the inputs (before the duplicate check).
    check_polymorphic_result(b, ret, &in_types)?;
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
        check_replace(env, old, kind, ret, retset, &specs, &in_types, n_defaults, &qn.name, is_procedure)?;
    }
    // PG sql_validator: pseudo-type arguments are rejected before the body is checked.
    if lang.oid == LANG_SQL {
        check_sql_arg_types(b, &specs)?;
    }
    if let Some((e, text)) = &sql_check {
        check_sql_body(b, &qn.name, &specs, e, text, lang, ret)?;
        check_plpgsql_body(b, e, text, lang, &specs)?;
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
        cost,
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
    // An unnamed CREATE PROCEDURE parameter carries the parser's marker name (`parser::UNNAMED_PARAM_MARKER`).
    let name = match &a.name {
        Some(id) if id.value != UNNAMED_PARAM_MARKER => b.ident(id),
        _ => String::new(),
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
        // An unnamed parameter carries the parser's marker name (`parser::UNNAMED_PARAM_MARKER`).
        let pname = if p.name.value == UNNAMED_PARAM_MARKER { String::new() } else { b.ident(&p.name) };
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
/// Which call arguments are OUT-parameter placeholders of candidate `c` (PG 14+: CALL passes every
/// OUT argument). Positional arguments take the declared positions; a named argument is a
/// placeholder when it names an OUT parameter. `None` when `c` has no OUT parameter or the call does
/// not supply each of them exactly once.
fn out_placeholders(env: &CatalogEnv, c: &FuncCandidate, names: &[Option<String>]) -> Option<Vec<bool>> {
    let f = env.snapshot().function(c.oid)?;
    let nout = f.args.iter().filter(|a| a.mode == ArgMode::Out).count();
    if nout == 0 {
        return None;
    }
    let mut drop = Vec::with_capacity(names.len());
    for (i, n) in names.iter().enumerate() {
        let is_out = match n {
            None => f.args.get(i).is_some_and(|a| a.mode == ArgMode::Out),
            Some(n) => f.args.iter().any(|a| a.mode == ArgMode::Out && a.name == *n),
        };
        drop.push(is_out);
    }
    if drop.iter().filter(|d| **d).count() != nout {
        return None;
    }
    Some(drop)
}

/// True when procedure candidate `c` has OUT parameters.
fn has_out_params(env: &CatalogEnv, c: &FuncCandidate) -> bool {
    env.snapshot().function(c.oid).is_some_and(|f| f.args.iter().any(|a| a.mode == ArgMode::Out))
}

fn bind_call_function(b: &mut Binder<'_>, func: &sqlparser::ast::Function) -> Result<BoundStatement> {
    let env: &CatalogEnv = b.env;
    let qn = b.qual(&func.name)?;
    let name_pos = b.pos(&func.name);
    let list: &[FunctionArg] = match &func.args {
        FunctionArguments::List(l) => l.args.as_slice(),
        FunctionArguments::None => {
            // `CALL p` without parentheses: the grammar needs the argument list.
            let len = b.src.text().chars().count() as u32;
            return Err(PgError::syntax("syntax error at end of input", Some(len + 1)));
        }
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
            FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. }
            | FunctionArg::ExprNamed { name: Expr::Identifier(name), arg: FunctionArgExpr::Expr(e), .. } => {
                let n = b.ident(name);
                args.push(bind_arg(b, e, ctx)?);
                names.push(Some(n));
            }
            _ => return Err(unsupported("this CALL argument form")),
        }
    }
    let named = names.iter().any(|n| n.is_some());
    let arg_oids: Vec<Oid> = args.iter().map(|a| a.ty().oid).collect();
    // PG `func_signature_string` keeps the written qualifier: `procedure s1.p(integer) does not exist`.
    let sig = if named {
        // PG `func_signature_string`: named arguments print as `name => type`.
        let parts: Vec<String> = names
            .iter()
            .zip(arg_oids.iter())
            .map(|(n, t)| {
                let tn = pgarbor_types::names::display_name(env, *t);
                match n {
                    Some(n) => format!("{n} => {tn}"),
                    None => tn,
                }
            })
            .collect();
        format!("{}({})", qn.display(), parts.join(", "))
    } else {
        signature_string(env, &qn.display(), &arg_oids)?
    };
    let call = FuncCall {
        name: qn.name.clone(),
        arg_types: arg_oids.clone(),
        arg_names: names.clone(),
        variadic_keyword: false,
    };
    let all = env.function_candidates(qn.schema.as_deref(), &qn.name)?;
    let all_procs: Vec<FuncCandidate> = all.iter().filter(|c| matches!(c.kind, ProcKind::Procedure)).cloned().collect();
    // Procedures with OUT parameters only match calls that pass their OUT arguments (second pass).
    let procs: Vec<FuncCandidate> = all_procs.iter().filter(|c| !has_out_params(env, c)).cloned().collect();

    // Procedures first; then procedures with OUT placeholders dropped (positional calls only).
    let no_drop = vec![false; args.len()];
    let mut plan: Option<(ResolvedFunc, Vec<bool>)> = None;
    let mut first_err: Option<PgError> = None;
    match resolve_function(env, &call, &procs) {
        Ok(r) => plan = Some((r, no_drop)),
        Err(e) => first_err = Some(e),
    }
    if plan.is_none() {
        for c in all_procs.iter() {
            let Some(drop) = out_placeholders(env, c, &names) else {
                continue;
            };
            let kept_types: Vec<Oid> =
                arg_oids.iter().zip(drop.iter()).filter(|(_, d)| !**d).map(|(t, _)| *t).collect();
            let kept_names: Vec<Option<String>> =
                names.iter().zip(drop.iter()).filter(|(_, d)| !**d).map(|(n, _)| n.clone()).collect();
            let sc = FuncCall {
                name: qn.name.clone(),
                arg_types: kept_types.clone(),
                arg_names: kept_names,
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
    fn plpgsql_statement_and_return_syntax() {
        let misspelled = "\nBEGIN\n  SELEC 1;\n  RETURN 1;\nEND;\n";
        let (idx, err) = plpgsql_statement_syntax(misspelled).unwrap();
        assert_eq!(idx, misspelled.find("SELEC").unwrap());
        assert_eq!(err.message, "syntax error at or near \"SELEC\"");

        let dangling = "\nBEGIN\n  RETURN 1 +;\nEND;\n";
        let (idx, err) = plpgsql_statement_syntax(dangling).unwrap();
        assert_eq!(idx, dangling.find(';').unwrap());
        assert_eq!(err.message, "syntax error at end of input");

        let ok = "\nDECLARE\n  n int := 0;\nBEGIN\n  n := n + 1;\n  PERFORM f(n);\n  RETURN QUERY SELECT 1;\n  \
                  RETURN NEXT;\n  CASE n WHEN 1 THEN n := 2; ELSE n := 3; END CASE;\n  RETURN CASE WHEN n > 1 THEN 1 ELSE 0 END;\nEND;\n";
        assert!(plpgsql_statement_syntax(ok).is_none());
    }

    #[test]
    fn plpgsql_fetch_multi_row_directions() {
        let at_fetch = |t: &str| t.find("FETCH");
        let multi = "BEGIN FETCH FORWARD 2 FROM c INTO r; END";
        assert_eq!(plpgsql_multi_row_fetch(multi), at_fetch(multi));
        let all = "BEGIN FETCH ALL FROM c INTO r; END";
        assert_eq!(plpgsql_multi_row_fetch(all), at_fetch(all));
        let count = "BEGIN FETCH 2 FROM c INTO r; END";
        assert_eq!(plpgsql_multi_row_fetch(count), at_fetch(count));
        for single in [
            "BEGIN FETCH FORWARD FROM c INTO r; END",
            "BEGIN FETCH BACKWARD FROM c INTO r; END",
            "BEGIN FETCH RELATIVE n FROM c INTO r; END",
            "BEGIN FETCH NEXT FROM c INTO r; END",
            "BEGIN -- FETCH ALL FROM c\n MOVE ALL FROM c; END",
            "BEGIN RETURN 'fetch all'; END",
        ] {
            assert_eq!(plpgsql_multi_row_fetch(single), None, "{single}");
        }
    }

    #[test]
    fn plpgsql_not_null_needs_default() {
        let text = "\nDECLARE n int NOT NULL;\nBEGIN RETURN 1; END ";
        assert_eq!(plpgsql_not_null_without_default(text), Some((text.find("NOT").unwrap(), "n".to_string())));
        for ok in [
            "DECLARE n int NOT NULL DEFAULT NULL; BEGIN RETURN 1; END",
            "DECLARE n int NOT NULL := 1; BEGIN RETURN 1; END",
            "DECLARE b boolean := x IS NOT NULL; BEGIN RETURN 1; END",
            "DECLARE c CURSOR FOR SELECT 1 WHERE x IS NOT NULL; BEGIN RETURN 1; END",
            "DECLARE n int; BEGIN n := 1; IF n IS NOT NULL THEN RETURN 1; END IF; END",
        ] {
            assert_eq!(plpgsql_not_null_without_default(ok), None, "{ok}");
        }
    }

    #[test]
    fn mode_mapping_defaults_to_in() {
        assert_eq!(map_mode(None), ArgMode::In);
        assert_eq!(map_mode(Some(&SqlArgMode::Out)), ArgMode::Out);
        assert_eq!(map_mode(Some(&SqlArgMode::InOut)), ArgMode::InOut);
    }

    #[test]
    fn constant_assignment_is_found_at_statement_start() {
        let text = "\nDECLARE c CONSTANT int := 5;\nBEGIN c := 6; RETURN c; END ";
        assert_eq!(plpgsql_constant_assignment(text), Some((text.find("c := 6").unwrap(), "c".to_string())));
        assert!(plpgsql_constant_assignment("DECLARE c CONSTANT int := 5; BEGIN c = 6; END").is_some());
        for single in [
            "DECLARE c CONSTANT int := 5; BEGIN RETURN c * 2; END",
            "DECLARE c CONSTANT int := 5; BEGIN f(c := 6); END",
            "DECLARE x int := 5; BEGIN x := 6; END",
            "BEGIN c := 6; END",
            "DECLARE c CONSTANT int := 5; BEGIN -- c := 6\n RETURN c; END",
            "DECLARE c CONSTANT int := 5; DECLARE c int; BEGIN c := 6; END",
        ] {
            assert_eq!(plpgsql_constant_assignment(single), None, "{single}");
        }
    }

    #[test]
    fn plpgsql_top_level_set_operation_in_expression() {
        let assign = "DECLARE v int; BEGIN v := 1 UNION SELECT 2; RETURN v; END";
        assert_eq!(plpgsql_expr_set_op(assign), Some((assign.find("UNION").unwrap(), 5)));
        let cond = "BEGIN IF id > 0 FROM t WHERE id = 1 UNION SELECT id FROM t THEN NULL; END IF; END";
        assert_eq!(plpgsql_expr_set_op(cond), Some((cond.find("UNION").unwrap(), 5)));
        for ok in [
            "BEGIN RETURN QUERY SELECT 1 UNION SELECT 2; END",
            "BEGIN SELECT 1 UNION SELECT 2 INTO x; END",
            "BEGIN v := (SELECT 1 UNION SELECT 2); END",
            "BEGIN IF CASE WHEN a THEN 1 ELSE 2 END = 1 THEN NULL; END IF; END",
        ] {
            assert_eq!(plpgsql_expr_set_op(ok), None, "{ok}");
        }
    }

    #[test]
    fn plpgsql_undeclared_assignment_is_found() {
        let text = "BEGIN zzz := 1; RETURN 1; END";
        assert_eq!(plpgsql_undeclared_assignment(text, &[]), Some((text.find("zzz").unwrap(), "zzz".to_string())));
        for ok in [
            "BEGIN RETURN CASE WHEN a THEN b = c ELSE false END; END",
            "DECLARE x int; BEGIN x := 1; RETURN x; END",
            "BEGIN p := 1; RETURN p; END",
            "BEGIN PERFORM f(c := 1); RETURN 1; END",
            "BEGIN rec.f := 1; RETURN 1; END",
            "BEGIN FOUND := true; RETURN 1; END",
            "BEGIN FOR i IN 1..3 LOOP i := i; END LOOP; RETURN 1; END",
            "BEGIN -- zzz := 1\n RETURN 1; END",
            "DECLARE x int; BEGIN BEGIN DECLARE y int; END; zzz := 1; RETURN 1; END",
        ] {
            assert_eq!(plpgsql_undeclared_assignment(ok, &["p"]), None, "{ok}");
        }
    }

    #[test]
    fn plpgsql_duplicate_declaration_is_found() {
        let text = "\nDECLARE x int; x int;\nBEGIN RETURN 1; END ";
        let second = text.rfind('x').unwrap();
        assert_eq!(plpgsql_duplicate_declaration(text), Some((second, "x".to_string())));
        for ok in [
            "DECLARE x int; y int; BEGIN RETURN 1; END",
            "DECLARE x int; BEGIN DECLARE x int; RETURN 1; END",
            "BEGIN RETURN 1; END",
        ] {
            assert_eq!(plpgsql_duplicate_declaration(ok), None, "{ok}");
        }
    }

    #[test]
    fn plpgsql_cursor_args_are_checked() {
        let msg = |t: &str| plpgsql_cursor_args(t).map(|(_, e)| e.message);
        let decl = "DECLARE c CURSOR (g int, h int) FOR SELECT 1; d CURSOR FOR SELECT 2; BEGIN ";
        assert_eq!(msg(&format!("{decl}OPEN c; END")), Some("cursor \"c\" has arguments".to_string()));
        assert_eq!(msg(&format!("{decl}OPEN c(1); END")), Some("not enough arguments for cursor \"c\"".to_string()));
        assert_eq!(msg(&format!("{decl}OPEN c(1, 2, 3); END")), Some("too many arguments for cursor \"c\"".to_string()));
        assert_eq!(msg(&format!("{decl}OPEN d(1); END")), Some("cursor \"d\" has no arguments".to_string()));
        assert_eq!(msg(&format!("{decl}OPEN c(1, 2); OPEN d; END")), None);
    }

    #[test]
    fn plpgsql_begin_semicolon_is_found() {
        let text = "\nBEGIN;\n  RETURN 1;\nEND ";
        assert_eq!(plpgsql_begin_semicolon(text), Some(text.find(';').unwrap()));
        for ok in ["BEGIN RETURN 1; END", "DECLARE x int; BEGIN; END", "BEGIN -- ;\n RETURN 1; END"] {
            assert_eq!(plpgsql_begin_semicolon(ok), None, "{ok}");
        }
    }

    #[test]
    fn plpgsql_unterminated_nested_comment_is_found() {
        // Block comments nest, so the inner `*/` only closes the nested comment and the outer one stays open.
        let text = "\n/* unterminated /* nested */\nBEGIN RETURN 1; END ";
        assert_eq!(plpgsql_unterminated_comment(text), Some(1));
        assert_eq!(plpgsql_unterminated_comment("BEGIN /* a /* b */ c */ RETURN 1; END"), None);
        assert_eq!(plpgsql_unterminated_comment("BEGIN RETURN '/*' ; END"), None);
        assert_eq!(plpgsql_unterminated_comment("BEGIN -- /*\n RETURN 1; END"), None);
        assert_eq!(plpgsql_unterminated_comment("BEGIN RETURN $$/*$$; END"), None);
    }

    #[test]
    fn plpgsql_end_label_mismatch_is_found() {
        let text = "\n<< a >>\nBEGIN\n  RETURN 1;\nEND b ";
        assert_eq!(plpgsql_end_label_mismatch(text), Some((text.rfind('b').unwrap(), "b".to_string(), Some("a".to_string()))));
        let unlabeled = "\nBEGIN RETURN 1; END b ";
        assert_eq!(plpgsql_end_label_mismatch(unlabeled), Some((unlabeled.rfind('b').unwrap(), "b".to_string(), None)));
        for ok in ["<< a >>\nBEGIN RETURN 1; END a", "<< a >>\nBEGIN RETURN 1; END a;", "BEGIN RETURN 1; END;", "BEGIN RETURN 1; END"] {
            assert_eq!(plpgsql_end_label_mismatch(ok), None, "{ok}");
        }
    }

    #[test]
    fn plpgsql_nested_end_label_is_found() {
        let text = "BEGIN BEGIN RETURN 1; END RETURN 0; END";
        let (idx, err) = plpgsql_nested_end_label(text).unwrap();
        assert_eq!(idx, text.find("RETURN 0").unwrap());
        assert_eq!(err.message, "end label \"return\" specified for unlabeled block");
        let labeled = "BEGIN << a >> BEGIN RETURN 1; END b; END";
        let (idx, err) = plpgsql_nested_end_label(labeled).unwrap();
        assert_eq!(idx, labeled.find('b').unwrap());
        assert_eq!(err.message, "end label \"b\" differs from block's label \"a\"");
        for ok in [
            "BEGIN BEGIN RETURN 1; END; RETURN 0; END",
            "BEGIN << a >> BEGIN RETURN 1; END a; END",
            "BEGIN << a >> DECLARE x int := 1; BEGIN RETURN x; END a; END",
            "BEGIN IF true THEN BEGIN RETURN 1; END; END IF; RETURN 0; END",
            "BEGIN RETURN CASE WHEN true THEN 1 END; END",
            "BEGIN BEGIN RETURN CASE WHEN true THEN 1 ELSE 2 END; END; END",
            "BEGIN BEGIN RETURN 1; END; END",
        ] {
            assert_eq!(plpgsql_nested_end_label(ok).map(|(i, _)| i), None, "{ok}");
        }
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
