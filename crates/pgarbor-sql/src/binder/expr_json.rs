//! SQL/JSON constructor syntax (PG `gram.y` `json_aggregate_func`, `JSON_OBJECT`, `JSON_SERIALIZE`;
//! `parse_expr.c` `transformJsonObjectConstructor`, `transformJsonAggConstructor`,
//! `transformJsonSerializeExpr`).
//!
//! sqlparser 0.63 parses these calls as ordinary function calls whose arguments are `key : value` /
//! `key VALUE value` (`FunctionArg::ExprNamed`) and whose `NULL ON NULL` / `ABSENT ON NULL` /
//! `RETURNING type` parts are `FunctionArgumentClause`s. `WITH UNIQUE KEYS` is turned into a name
//! marker by `parser::mark_json_unique_keys`.
//!
//! * `JSON_ARRAYAGG(v [ORDER BY ..] [ABSENT|NULL ON NULL] [RETURNING t])` is rewritten to
//!   `pg_catalog.json[b]_agg[_strict](v)` (`ABSENT ON NULL`, the default, is the `_strict` variant).
//! * `JSON_OBJECTAGG(k : v [ABSENT|NULL ON NULL] [WITH UNIQUE KEYS] [RETURNING t])` is rewritten to
//!   `pg_catalog.json[b]_object_agg[_unique][_strict](k, v)`.
//! * `JSON_OBJECT(k : v, ..)` with `NULL ON NULL` (the default) is rewritten to
//!   `pg_catalog.json[b]_build_object(k, v, ..)`; `ABSENT ON NULL` and `WITH UNIQUE KEYS` need runtime
//!   support PGArbor does not have yet and are 0A000 `SQL-10`.
//! * `JSON_SERIALIZE(v [RETURNING t])` casts `v` to json (unless it already is json or jsonb) and then to
//!   the string type `t` (default text); any other `t` is 42804.
//!
//! `RETURNING` types other than json and jsonb on the constructors are 0A000 `SQL-10`.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::{oids, TypeId};
use sqlparser::ast::{
    DataType, Function, FunctionArg, FunctionArgExpr, FunctionArgOperator, FunctionArgumentClause,
    FunctionArgumentList, FunctionArguments, Ident, JsonNullClause, ObjectName, ObjectNamePart,
};
use sqlparser::tokenizer::Span;

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::bind_arg;
use super::expr_func::bind_function_call;
use super::types;
use crate::diag;
use crate::ir::{BoundExpr, CoercionForm};
use crate::parser::JSON_UNIQUE_KEYS_MARKER;

/// `(lower-case name, WITH UNIQUE KEYS was written)` of an unqualified, unquoted SQL/JSON constructor
/// call name (or of the renamed form produced by the parser marker).
fn call_name(f: &Function) -> Option<(String, bool)> {
    let [ObjectNamePart::Identifier(id)] = f.name.0.as_slice() else {
        return None;
    };
    match id.quote_style {
        None => Some((id.value.to_ascii_lowercase(), false)),
        Some('"') => id.value.strip_suffix(JSON_UNIQUE_KEYS_MARKER).map(|n| (n.to_string(), true)),
        _ => None,
    }
}

/// Span of the first name part, so a rewritten call keeps the error position of the original.
fn name_span(f: &Function) -> Span {
    match f.name.0.first() {
        Some(ObjectNamePart::Identifier(id)) => id.span,
        _ => Span::empty(),
    }
}

/// `pg_catalog.<name>` carrying the span of the original function name.
fn catalog_name(f: &Function, name: &str) -> ObjectName {
    let span = name_span(f);
    ObjectName(vec![
        ObjectNamePart::Identifier(Ident::with_span(span, "pg_catalog")),
        ObjectNamePart::Identifier(Ident::with_span(span, name)),
    ])
}

fn argument_list(f: &Function) -> Option<&FunctionArgumentList> {
    match &f.args {
        FunctionArguments::List(list) => Some(list),
        _ => None,
    }
}

/// Split the clauses of a constructor call: `Some(true)` = `ABSENT ON NULL`, `Some(false)` =
/// `NULL ON NULL`; the `RETURNING` type; and the remaining clauses (`ORDER BY` ..).
fn split_clauses(
    list: &FunctionArgumentList,
) -> (Option<bool>, Option<&DataType>, Vec<FunctionArgumentClause>) {
    let mut absent = None;
    let mut returning = None;
    let mut rest = Vec::new();
    for c in &list.clauses {
        match c {
            FunctionArgumentClause::JsonNullClause(JsonNullClause::AbsentOnNull) => absent = Some(true),
            FunctionArgumentClause::JsonNullClause(JsonNullClause::NullOnNull) => absent = Some(false),
            FunctionArgumentClause::JsonReturningClause(r) => returning = Some(&r.data_type),
            other => rest.push(other.clone()),
        }
    }
    (absent, returning, rest)
}

/// Is the `RETURNING` type jsonb (`true`), json or absent (`false`)? Other types are 0A000.
fn returning_is_jsonb(returning: Option<&DataType>, what: &str, pos: Option<u32>) -> Result<bool> {
    match returning {
        None | Some(DataType::JSON) => Ok(false),
        Some(DataType::JSONB) => Ok(true),
        Some(_) => Err(diag::with_position(
            PgError::unimplemented("SQL-10", format!("{what} with a RETURNING type other than json or jsonb")),
            pos,
        )),
    }
}

/// `f` with a new name and argument list (every other part of the call is kept).
fn rewritten(f: &Function, name: &str, args: Vec<FunctionArg>, list: &FunctionArgumentList, clauses: Vec<FunctionArgumentClause>) -> Function {
    let mut g = f.clone();
    g.name = catalog_name(f, name);
    g.args = FunctionArguments::List(FunctionArgumentList { duplicate_treatment: list.duplicate_treatment, args, clauses });
    g
}

fn is_key_value_operator(op: &FunctionArgOperator) -> bool {
    matches!(op, FunctionArgOperator::Colon | FunctionArgOperator::Value)
}

/// The `(key, value)` pair of one `key : value` / `key VALUE value` argument.
fn key_value(a: &FunctionArg) -> Option<(&sqlparser::ast::Expr, &sqlparser::ast::Expr)> {
    match a {
        FunctionArg::ExprNamed { name, arg: FunctionArgExpr::Expr(v), operator } if is_key_value_operator(operator) => {
            Some((name, v))
        }
        _ => None,
    }
}

fn unnamed(e: &sqlparser::ast::Expr) -> FunctionArg {
    FunctionArg::Unnamed(FunctionArgExpr::Expr(e.clone()))
}

/// If `f` is an SQL/JSON constructor call (module docs), bind it and return the result.
pub fn bind_json_constructor(b: &mut Binder, f: &Function, ec: ExprCtx) -> Result<Option<BoundExpr>> {
    let Some((name, unique)) = call_name(f) else {
        return Ok(None);
    };
    if f.over.is_some() && name != "json_arrayagg" && name != "json_objectagg" {
        return Ok(None);
    }
    let pos = b.src.pos_of_span(name_span(f));
    let rewrite = match name.as_str() {
        "json_arrayagg" => rewrite_arrayagg(f, pos)?,
        "json_objectagg" => rewrite_objectagg(f, unique, pos)?,
        "json_object" => rewrite_object(f, unique, pos)?,
        "json_serialize" => return bind_serialize(b, f, pos, ec).map(Some),
        "json_scalar" => rewrite_scalar(b, f)?,
        _ => None,
    };
    match rewrite {
        Some(g) => bind_function_call(b, &g, ec).map(Some),
        None => Ok(None),
    }
}

/// `JSON_SCALAR(v)` is `to_json(v)` (NULL in, NULL out). `JSON_SCALAR()` is a syntax error at the `)`.
fn rewrite_scalar(b: &Binder, f: &Function) -> Result<Option<Function>> {
    let span = name_span(f);
    let Some(list) = argument_list(f) else {
        return Ok(None);
    };
    if list.args.is_empty() && list.clauses.is_empty() {
        let close = sqlparser::tokenizer::Span::new(span.end, span.end);
        return Err(diag::syntax_error_near(")", b.src.pos_of_span(close)));
    }
    let [arg @ FunctionArg::Unnamed(FunctionArgExpr::Expr(_))] = list.args.as_slice() else {
        return Ok(None);
    };
    if !list.clauses.is_empty() || list.duplicate_treatment.is_some() {
        return Ok(None);
    }
    Ok(Some(rewritten(f, "to_json", vec![arg.clone()], list, Vec::new())))
}

fn rewrite_arrayagg(f: &Function, pos: Option<u32>) -> Result<Option<Function>> {
    let Some(list) = argument_list(f) else {
        return Err(diag::syntax("syntax error", pos));
    };
    let (absent, returning, rest) = split_clauses(list);
    let jsonb = returning_is_jsonb(returning, "JSON_ARRAYAGG", pos)?;
    let [arg @ FunctionArg::Unnamed(FunctionArgExpr::Expr(_))] = list.args.as_slice() else {
        return Err(diag::syntax("syntax error", pos));
    };
    // PG: JSON_ARRAYAGG defaults to ABSENT ON NULL.
    let strict = absent.unwrap_or(true);
    let name = format!("{}_agg{}", if jsonb { "jsonb" } else { "json" }, if strict { "_strict" } else { "" });
    Ok(Some(rewritten(f, &name, vec![arg.clone()], list, rest)))
}

fn rewrite_objectagg(f: &Function, unique: bool, pos: Option<u32>) -> Result<Option<Function>> {
    let Some(list) = argument_list(f) else {
        return Err(diag::syntax("syntax error", pos));
    };
    let (absent, returning, rest) = split_clauses(list);
    let jsonb = returning_is_jsonb(returning, "JSON_OBJECTAGG", pos)?;
    let [one] = list.args.as_slice() else {
        return Err(diag::syntax("syntax error", pos));
    };
    let Some((k, v)) = key_value(one) else {
        return Err(diag::syntax("syntax error", pos));
    };
    // PG: JSON_OBJECTAGG defaults to NULL ON NULL.
    let strict = absent.unwrap_or(false);
    let name = format!(
        "{}_object_agg{}{}",
        if jsonb { "jsonb" } else { "json" },
        if unique { "_unique" } else { "" },
        if strict { "_strict" } else { "" }
    );
    Ok(Some(rewritten(f, &name, vec![unnamed(k), unnamed(v)], list, rest)))
}

fn rewrite_object(f: &Function, unique: bool, pos: Option<u32>) -> Result<Option<Function>> {
    let empty = FunctionArgumentList { duplicate_treatment: None, args: Vec::new(), clauses: Vec::new() };
    let list = match &f.args {
        FunctionArguments::List(list) => list,
        FunctionArguments::None => &empty,
        FunctionArguments::Subquery(_) => return Ok(None),
    };
    // `json_object(text[])` and `json_object(text[], text[])` are ordinary functions; the SQL/JSON
    // constructor is recognized by its `key : value` arguments, its clauses, or having no arguments.
    let constructor = unique
        || list.args.is_empty()
        || !list.clauses.is_empty()
        || list.args.iter().any(|a| key_value(a).is_some());
    if !constructor {
        return Ok(None);
    }
    let (absent, returning, rest) = split_clauses(list);
    if !rest.is_empty() || list.duplicate_treatment.is_some() {
        return Err(diag::syntax("syntax error", pos));
    }
    let jsonb = returning_is_jsonb(returning, "JSON_OBJECT", pos)?;
    let mut args: Vec<FunctionArg> = Vec::with_capacity(list.args.len() * 2);
    for a in &list.args {
        let Some((k, v)) = key_value(a) else {
            return Err(diag::syntax("syntax error", pos));
        };
        args.push(unnamed(k));
        args.push(unnamed(v));
    }
    if absent == Some(true) || unique {
        return Err(diag::with_position(
            PgError::unimplemented("SQL-10", "JSON_OBJECT with ABSENT ON NULL or WITH UNIQUE KEYS"),
            pos,
        ));
    }
    let name = if jsonb { "jsonb_build_object" } else { "json_build_object" };
    Ok(Some(rewritten(f, name, args, list, Vec::new())))
}

/// `JSON_SERIALIZE(expr [RETURNING type])` (module docs).
fn bind_serialize(b: &mut Binder, f: &Function, pos: Option<u32>, ec: ExprCtx) -> Result<BoundExpr> {
    let Some(list) = argument_list(f) else {
        return Err(diag::syntax("syntax error", pos));
    };
    let (absent, returning, rest) = split_clauses(list);
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(e))] = list.args.as_slice() else {
        return Err(diag::syntax("syntax error", pos));
    };
    if absent.is_some() || !rest.is_empty() || list.duplicate_treatment.is_some() {
        return Err(diag::syntax("syntax error", pos));
    }
    let arg = bind_arg(b, e, ec)?;
    let ret: TypeId = match returning {
        None => TypeId::of(oids::TEXT),
        Some(dt) => types::resolve_data_type(b, dt, pos)?.id,
    };
    if ret.oid == oids::BYTEA {
        return Err(diag::with_position(PgError::unimplemented("SQL-10", "JSON_SERIALIZE returning bytea"), pos));
    }
    if !matches!(ret.oid, oids::TEXT | oids::VARCHAR | oids::BPCHAR | oids::NAME | oids::CHAR) {
        return Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("cannot use type {} in RETURNING clause of JSON_SERIALIZE()", b.type_name(ret)),
        )
        .with_hint("Try returning a string type or bytea."));
    }
    let as_json = if matches!(arg.ty().oid, oids::JSON | oids::JSONB) {
        arg
    } else {
        let j = coerce::coerce_strict(b, &arg, TypeId::of(oids::JSON), CastContext::Explicit, CoercionForm::Explicit)?;
        BoundArg::new(j, arg.pos)
    };
    coerce::coerce_strict(b, &as_json, ret, CastContext::Explicit, CoercionForm::Explicit)
}
