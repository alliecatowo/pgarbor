//! Definition and description functions (`pg_get_*def`, `pg_get_expr`, function text, `pg_describe_object`,
//! `obj_description`, ...). Lease CAT-46 (sonnet: many small functions, exact PG outputs). Sizes, sequence
//! info, index properties, keywords and option helpers live in `introspect.rs` (CAT-49). PG sources: `utils/adt/ruleutils.c`, `utils/adt/misc.c`,
//! `commands/comment.c` (`GetComment`), `catalog/objectaddress.c` (`pg_describe_object`,
//! `pg_identify_object*`), `utils/adt/dbsize.c`, `utils/adt/amutils.c`, `utils/adt/format_type.c`,
//! `utils/adt/regproc.c`, `commands/sequence.c`, `parser/keywords.c`.
//! Definition text comes from `crate::deparse` (CAT-26).
//!
//! | proc | behavior |
//! |---|---|
//! | `pg_get_expr(pg_node_tree, oid [, bool])` | first argument unchanged (see `sysfuncs/mod.rs`) |
//! | `pg_get_constraintdef(oid [, bool])` | `deparse::constraintdef`; unknown OID -> NULL |
//! | `pg_get_indexdef(oid)`, `pg_get_indexdef(oid, int, bool)` | `deparse::indexdef`; the 3-arg form with column number |
//! | `pg_get_viewdef(oid)`, `(oid, bool)`, `(oid, int)`, `(text)`, `(text, bool)` | `deparse::viewdef`; text forms resolve via `reg_lookup_impl(Class)`; non-view -> 42809? PG returns NULL for unknown OID and raises `"t" is not a view` for non-view relations: follow PG |
//! | `pg_get_ruledef(oid [, bool])` | for view rules: `CREATE RULE "_RETURN" AS ON SELECT TO t DO INSTEAD <viewdef>`-style text from `pg_rewrite` row (rule OID = view OID); unknown -> NULL |
//! | `pg_get_triggerdef(oid [, bool])` | no triggers: `XX000 could not find tuple for trigger <oid>` (PG wording, `errors::cache_lookup_failed`-style) |
//! | `pg_get_function_arguments(oid)`, `pg_get_function_identity_arguments(oid)`, `pg_get_function_result(oid)`, `pg_get_functiondef(oid)`, `pg_get_function_sqlbody(oid)`, `pg_get_function_arg_default(oid, int)` | `deparse::*`; builtin functions work too (`BuiltinProc`) |
//! | `pg_get_serial_sequence(text, text)` | table name resolved via `reg_lookup_impl(Class)`, column by name (42703 `column "x" of relation "t" does not exist`), result via `deparse::serial_sequence`; a table that does not exist -> 42P01 |
//! | `pg_get_userbyid(oid)` | role name, unknown -> `unknown (OID=<n>)` |
//! | `pg_get_partkeydef`, `pg_get_partition_constraintdef`, `pg_get_replica_identity_index(oid)` | NULL / NULL / the replica-identity index regclass or NULL |
//! | `pg_get_statisticsobjdef*` | NULL (no extended statistics) |
//! | `pg_describe_object(oid, oid, int)` | `describe::describe(ObjectAddress{class from classid, oid, sub_id})`; unknown -> NULL |
//! | `pg_identify_object(oid, oid, int)` | record `(type text, schema text, name text, identity text)` using `describe::type_description` and the PG identity text (`public.t`, `public.t.col`, `postgres`) |
//! | `pg_identify_object_as_address(oid, oid, int)` | record `(type, object_names text[], object_args text[])` |
//! | `pg_get_object_address(text, text[], text[])` | `Err(unimplemented)` |
//! | `obj_description(oid, name)`, `obj_description(oid)`, `col_description(oid, int)`, `shobj_description(oid, name)` | `snapshot.description(DescriptionKey{..})` with classoid from the catalog NAME (`pg_class` 1259, `pg_type` 1247, `pg_proc` 1255, `pg_namespace` 2615, `pg_constraint` 2606, `pg_database` 1262, `pg_authid` 1260); one-arg `obj_description` is deprecated and returns NULL; missing -> NULL |
use super::CatalogCallCtx;
use crate::deparse;
use crate::describe;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::class_oid;
use crate::model::{ArgMode, DescriptionKey, ObjClass, ObjectAddress, RelKind, ReplicaIdentity};
use crate::snapshot::ClassRef;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::array::Array;
use pgarbor_types::catalog_data::{self, BuiltinProc};
use pgarbor_types::context::RegKind;
use pgarbor_types::text::format::quote_identifier;
use pgarbor_types::{oids, Datum, TypeEnv};

/// Scalar procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "pg_get_expr",
    "pg_get_constraintdef",
    "pg_get_indexdef",
    "pg_get_viewdef",
    "pg_get_ruledef",
    "pg_get_triggerdef",
    "pg_get_function_arguments",
    "pg_get_function_identity_arguments",
    "pg_get_function_result",
    "pg_get_functiondef",
    "pg_get_function_sqlbody",
    "pg_get_function_arg_default",
    "pg_get_serial_sequence",
    "pg_get_userbyid",
    "pg_get_partkeydef",
    "pg_get_partition_constraintdef",
    "pg_get_replica_identity_index",
    "pg_get_statisticsobjdef",
    "pg_get_statisticsobjdef_columns",
    "pg_get_statisticsobjdef_expressions",
    "pg_describe_object",
    "pg_identify_object",
    "pg_identify_object_as_address",
    "pg_get_object_address",
    "obj_description",
    "col_description",
    "shobj_description",
];

/// Set-returning procs among [`NAMES`].
// CONTRACT
pub const SET_NAMES: &[&str] = &[
];

/// Scalar dispatch on `proc.name` (module docs table).
// CONTRACT
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    if proc.name == "pg_get_expr" {
        // Expression columns hold canonical text already (see `sysfuncs/mod.rs`).
        return Ok(args.first().cloned().unwrap_or(Datum::Null));
    }
    // Every function here is strict in PostgreSQL; tolerate callers that did not short-circuit.
    if args.iter().any(Datum::is_null) {
        return Ok(Datum::Null);
    }
    match proc.name {
        "pg_get_constraintdef" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(opt_text(deparse::constraintdef(env, oid, pretty_arg(args, 1))?))
        }
        "pg_get_indexdef" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            if args.len() >= 3 {
                let col = arg_int(args, 1).unwrap_or(0).max(0) as usize;
                Ok(opt_text(deparse::indexdef(env, oid, Some(col), pretty_arg(args, 2))?))
            } else {
                Ok(opt_text(deparse::indexdef(env, oid, None, false)?))
            }
        }
        "pg_get_viewdef" => viewdef_call(env, args),
        "pg_get_ruledef" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(opt_text(ruledef(env, oid, pretty_arg(args, 1))?))
        }
        // PG 18 `pg_get_triggerdef_worker` returns NULL for an unknown trigger OID (missing_ok);
        // PGArbor has no triggers, so every OID is unknown.
        "pg_get_triggerdef" => Ok(Datum::Null),
        "pg_get_function_arguments" => func_text(args, |o| deparse::function_arguments(env, o)),
        "pg_get_function_identity_arguments" => func_text(args, |o| deparse::function_identity_arguments(env, o)),
        "pg_get_function_result" => func_text(args, |o| deparse::function_result(env, o)),
        "pg_get_functiondef" => func_text(args, |o| deparse::functiondef(env, o)),
        "pg_get_function_sqlbody" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(opt_text(env.snapshot().function(oid).and_then(|f| f.sql_body.as_ref()).map(|b| b.sql.clone())))
        }
        "pg_get_function_arg_default" => {
            let (Some(oid), Some(nth)) = (arg_oid(args, 0), arg_int(args, 1)) else { return Ok(Datum::Null) };
            Ok(opt_text(function_arg_default(env, oid, nth)))
        }
        "pg_get_serial_sequence" => serial_sequence_call(env, args),
        "pg_get_userbyid" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(Datum::Text(match env.snapshot().role_name(oid) {
                Some(n) => n.to_string(),
                None => format!("unknown (OID={oid})"),
            }))
        }
        "pg_get_partkeydef" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(opt_text(partkeydef(env, oid)?))
        }
        "pg_get_partition_constraintdef" => Ok(Datum::Null),
        "pg_get_replica_identity_index" => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            Ok(match replica_identity_index(env, oid) {
                Some(i) => Datum::Oid(i),
                None => Datum::Null,
            })
        }
        "pg_get_statisticsobjdef" | "pg_get_statisticsobjdef_columns" | "pg_get_statisticsobjdef_expressions" => {
            Ok(Datum::Null)
        }
        "pg_describe_object" => {
            let Some(addr) = address_args(args)? else { return Ok(Datum::Null) };
            Ok(opt_text(describe::describe(env, &addr)?))
        }
        "pg_identify_object" => {
            let Some(addr) = address_args(args)? else { return Ok(Datum::Null) };
            let ty = describe::type_description(env, &addr)?;
            let ident = match ty {
                Some(_) => identify(env, &addr)?,
                None => None,
            };
            Ok(match (ty, ident) {
                (Some(t), Some(i)) => Datum::Record(vec![
                    Datum::text(t),
                    opt_text(i.schema),
                    opt_text(i.name),
                    Datum::Text(i.identity),
                ]),
                _ => Datum::Record(vec![Datum::text(missing_type_word(addr.class)), Datum::Null, Datum::Null, Datum::Null]),
            })
        }
        "pg_identify_object_as_address" => {
            let Some(addr) = address_args(args)? else { return Ok(Datum::Null) };
            let ty = describe::type_description(env, &addr)?;
            let ident = match ty {
                Some(_) => identify(env, &addr)?,
                None => None,
            };
            Ok(match (ty, ident) {
                (Some(t), Some(i)) => Datum::Record(vec![Datum::text(t), text_array(i.names), text_array(i.args)]),
                _ => Datum::Record(vec![Datum::text(missing_type_word(addr.class)), Datum::Null, Datum::Null]),
            })
        }
        "pg_get_object_address" => Err(PgError::unimplemented("CAT-46", "pg_get_object_address")),
        "obj_description" => {
            if args.len() < 2 {
                // Deprecated one-argument form: searched every catalog; PGArbor returns NULL.
                return Ok(Datum::Null);
            }
            let (Some(oid), Some(cat)) = (arg_oid(args, 0), args[1].as_str()) else { return Ok(Datum::Null) };
            Ok(match catalog_classoid(cat) {
                Some(c) => description(env, oid, c, 0),
                None => Datum::Null,
            })
        }
        "col_description" => {
            let (Some(oid), Some(sub)) = (arg_oid(args, 0), arg_int(args, 1)) else { return Ok(Datum::Null) };
            Ok(description(env, oid, class_oid::PG_CLASS, sub))
        }
        "shobj_description" => {
            let (Some(oid), Some(cat)) = (arg_oid(args, 0), args.get(1).and_then(Datum::as_str)) else {
                return Ok(Datum::Null);
            };
            Ok(match catalog_classoid(cat) {
                Some(c) if matches!(c, class_oid::PG_DATABASE | class_oid::PG_AUTHID | class_oid::PG_TABLESPACE) => {
                    description(env, oid, c, 0)
                }
                _ => Datum::Null,
            })
        }
        other => Err(PgError::internal(format!("function {other} is not a definition function"))),
    }
}

/// Set-returning dispatch: this file has no set-returning procs ([`SET_NAMES`] is empty), so any
/// call is an internal error.
// CONTRACT
#[allow(unused_variables)]
pub fn call_set(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::internal(format!("function {} is not a set-returning definition function", proc.name)))
}

// ------------------------------------------------------------------ argument helpers

fn arg_oid(args: &[Datum], i: usize) -> Option<Oid> {
    match args.get(i)? {
        Datum::Oid(v) => Some(*v),
        Datum::Int4(v) => Some(*v as u32),
        Datum::Int8(v) => Some(*v as u32),
        _ => None,
    }
}

fn arg_int(args: &[Datum], i: usize) -> Option<i32> {
    match args.get(i)? {
        Datum::Int4(v) => Some(*v),
        Datum::Int2(v) => Some(i32::from(*v)),
        Datum::Int8(v) => Some(*v as i32),
        _ => None,
    }
}

fn pretty_arg(args: &[Datum], i: usize) -> bool {
    args.get(i).and_then(Datum::as_bool).unwrap_or(false)
}

fn opt_text(s: Option<String>) -> Datum {
    match s {
        Some(s) => Datum::Text(s),
        None => Datum::Null,
    }
}

fn text_array(items: Vec<String>) -> Datum {
    Datum::array(Array::from_vec(oids::TEXT, items.into_iter().map(Datum::Text).collect()))
}

fn func_text(args: &[Datum], f: impl FnOnce(Oid) -> Result<Option<String>>) -> Result<Datum> {
    match arg_oid(args, 0) {
        Some(oid) => Ok(opt_text(f(oid)?)),
        None => Ok(Datum::Null),
    }
}

fn qi(s: &str) -> Result<String> {
    quote_identifier(s)
}

fn qual(schema: &str, name: &str) -> Result<String> {
    Ok(format!("{}.{}", qi(schema)?, qi(name)?))
}

fn nsname(env: &CatalogEnv, ns: Oid) -> String {
    env.snapshot().namespace(ns).map(|n| n.name.clone()).unwrap_or_default()
}

/// `(classid, objid, objsubid)` argument triple. Unknown class -> XX000 like `getObjectDescription`.
fn address_args(args: &[Datum]) -> Result<Option<ObjectAddress>> {
    let (Some(classid), Some(objid), Some(sub)) = (arg_oid(args, 0), arg_oid(args, 1), arg_int(args, 2)) else {
        return Ok(None);
    };
    let class = ObjClass::from_classid(classid)
        .ok_or_else(|| PgError::internal(format!("unsupported object class: {classid}")))?;
    Ok(Some(ObjectAddress { class, oid: objid, sub_id: sub }))
}

// ------------------------------------------------------------------ descriptions

fn catalog_classoid(name: &str) -> Option<Oid> {
    Some(match name {
        "pg_am" => class_oid::PG_AM,
        "pg_attrdef" => class_oid::PG_ATTRDEF,
        "pg_authid" => class_oid::PG_AUTHID,
        "pg_cast" => class_oid::PG_CAST,
        "pg_class" => class_oid::PG_CLASS,
        "pg_collation" => class_oid::PG_COLLATION,
        "pg_constraint" => class_oid::PG_CONSTRAINT,
        "pg_database" => class_oid::PG_DATABASE,
        "pg_extension" => class_oid::PG_EXTENSION,
        "pg_language" => class_oid::PG_LANGUAGE,
        "pg_namespace" => class_oid::PG_NAMESPACE,
        "pg_opclass" => class_oid::PG_OPCLASS,
        "pg_operator" => class_oid::PG_OPERATOR,
        "pg_opfamily" => class_oid::PG_OPFAMILY,
        "pg_proc" => class_oid::PG_PROC,
        "pg_rewrite" => class_oid::PG_REWRITE,
        "pg_tablespace" => class_oid::PG_TABLESPACE,
        "pg_trigger" => class_oid::PG_TRIGGER,
        "pg_type" => class_oid::PG_TYPE,
        _ => return None,
    })
}

fn description(env: &CatalogEnv, objoid: Oid, classoid: Oid, objsubid: i32) -> Datum {
    match env.snapshot().description(&DescriptionKey { objoid, classoid, objsubid }) {
        Some(s) => Datum::Text(s.to_string()),
        None => Datum::Null,
    }
}

// ------------------------------------------------------------------ names

/// PG `stringToQualifiedNameList`: dots outside double quotes separate, unquoted parts are folded to
/// lower case, quotes are stripped (`""` is a literal quote).
fn split_qualified_name(s: &str) -> Result<Vec<String>> {
    let bad = || PgError::new(SqlState::INVALID_NAME, "invalid name syntax");
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut parts = Vec::new();
    loop {
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len {
            return Err(bad());
        }
        let mut cur = String::new();
        if chars[i] == '"' {
            i += 1;
            let mut closed = false;
            while i < len {
                if chars[i] == '"' {
                    if i + 1 < len && chars[i + 1] == '"' {
                        cur.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            if !closed || cur.is_empty() {
                return Err(bad());
            }
        } else {
            while i < len && chars[i] != '.' && !chars[i].is_whitespace() {
                cur.push(chars[i].to_ascii_lowercase());
                i += 1;
            }
            if cur.is_empty() {
                return Err(bad());
            }
        }
        parts.push(cur);
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        if chars[i] == '.' {
            i += 1;
        } else {
            return Err(bad());
        }
    }
    Ok(parts)
}

/// Resolve a relation name given as text (`RangeVarGetRelid(.., missing_ok = false)`); also returns
/// the bare relation name for messages.
fn resolve_relation_text(env: &CatalogEnv, text: &str) -> Result<(Oid, String)> {
    let parts = split_qualified_name(text)?;
    let (schema, name): (Option<&str>, &str) = match parts.len() {
        1 => (None, parts[0].as_str()),
        2 => (Some(parts[0].as_str()), parts[1].as_str()),
        3 => {
            let db = env.snapshot().database(env.database()).map(|d| d.name.as_str()).unwrap_or("");
            if parts[0] != db {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cross-database references are not implemented: {}", parts.join(".")),
                ));
            }
            (Some(parts[1].as_str()), parts[2].as_str())
        }
        _ => {
            return Err(PgError::syntax(
                format!("improper qualified name (too many dotted names): {}", parts.join(".")),
                None,
            ))
        }
    };
    match env.reg_lookup_impl(RegKind::Class, text)? {
        Some(oid) => Ok((oid, name.to_string())),
        None => {
            if let Some(s) = schema {
                if !s.starts_with("pg_temp") && env.snapshot().namespace_by_name(s).is_none() {
                    return Err(errors::schema_does_not_exist(s));
                }
            }
            Err(errors::relation_does_not_exist(schema, name))
        }
    }
}

// ------------------------------------------------------------------ view / rule / serial

fn viewdef_call(env: &CatalogEnv, args: &[Datum]) -> Result<Datum> {
    let (rel, pretty) = match args.first() {
        Some(Datum::Text(name)) => (resolve_relation_text(env, name)?.0, pretty_arg(args, 1)),
        _ => {
            let Some(oid) = arg_oid(args, 0) else { return Ok(Datum::Null) };
            // (oid, int) is the wrap-column form, which PG prints pretty; (oid, bool) is explicit.
            let pretty = match args.get(1) {
                Some(Datum::Bool(b)) => *b,
                Some(Datum::Null) | None => false,
                Some(_) => true,
            };
            (oid, pretty)
        }
    };
    Ok(opt_text(viewdef_checked(env, rel, pretty)?))
}

/// PG returns NULL for unknown OIDs and for relations without a `_RETURN` rule.
fn viewdef_checked(env: &CatalogEnv, rel: Oid, pretty: bool) -> Result<Option<String>> {
    match env.snapshot().relation(rel) {
        Some(r) if matches!(r.kind, RelKind::View | RelKind::MatView) => deparse::viewdef(env, rel, pretty),
        _ => Ok(None),
    }
}

/// `pg_get_ruledef` for the `_RETURN` rule of a view (rule OID = view OID).
fn ruledef(env: &CatalogEnv, rule: Oid, pretty: bool) -> Result<Option<String>> {
    let snap = env.snapshot();
    let Some(r) = snap.relation(rule) else { return Ok(None) };
    if !matches!(r.kind, RelKind::View | RelKind::MatView) {
        return Ok(None);
    }
    let Some(def) = deparse::viewdef(env, rule, false)? else { return Ok(None) };
    let target = if pretty {
        env.qualified_class_name(rule)?
    } else {
        qual(&nsname(env, r.namespace), &r.name)?
    };
    Ok(Some(format!("CREATE RULE \"_RETURN\" AS\n    ON SELECT TO {target} DO INSTEAD {def}")))
}

fn serial_sequence_call(env: &CatalogEnv, args: &[Datum]) -> Result<Datum> {
    let (Some(table), Some(column)) = (args.first().and_then(Datum::as_str), args.get(1).and_then(Datum::as_str)) else {
        return Ok(Datum::Null);
    };
    let (rel_oid, rel_name) = resolve_relation_text(env, table)?;
    let snap = env.snapshot();
    // (has user column, is a system column name)
    let found = match snap.class(rel_oid) {
        Some(ClassRef::Relation(o)) => snap.relation(o).map(|r| {
            let user = r.column_by_name(column).is_some();
            let system = !user
                && r.kind.has_system_columns()
                && matches!(column, "ctid" | "xmin" | "xmax" | "cmin" | "cmax" | "tableoid");
            (user, system)
        }),
        _ => None,
    };
    match found {
        Some((true, _)) => Ok(opt_text(deparse::serial_sequence(env, rel_oid, column)?)),
        // System columns have a (negative) attnum and never own a sequence.
        Some((false, true)) => Ok(Datum::Null),
        _ => Err(errors::column_of_relation_does_not_exist(column, &rel_name)),
    }
}

// ------------------------------------------------------------------ functions

fn function_arg_default(env: &CatalogEnv, oid: Oid, nth: i32) -> Option<String> {
    if nth < 1 {
        return None;
    }
    let idx = (nth - 1) as usize;
    if let Some(f) = env.snapshot().function(oid) {
        let a = f.args.get(idx)?;
        if !matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic) {
            return None;
        }
        return a.default.as_ref().map(|d| d.sql.clone());
    }
    let p = catalog_data::builtin_proc(oid)?;
    let numargs = if p.all_arg_types.is_empty() { p.arg_types.len() } else { p.all_arg_types.len() };
    let is_input = |i: usize| match p.arg_modes.get(i) {
        None => true,
        Some(m) => matches!(*m, b'i' | b'b' | b'v'),
    };
    if idx >= numargs || !is_input(idx) || p.arg_defaults.is_empty() {
        return None;
    }
    let nth_input = (0..=idx).filter(|i| is_input(*i)).count() as i64;
    let defaults = split_top_level_commas(p.arg_defaults);
    let nth_default = nth_input - 1 - (i64::from(p.nargs) - i64::from(p.ndefaults));
    if nth_default < 0 {
        return None;
    }
    defaults.get(nth_default as usize).map(|s| s.to_string())
}

/// Split `a, 'x,y', f(1, 2)` at commas outside quotes and brackets.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(s[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(s[start..].trim());
    out
}

// ------------------------------------------------------------------ partitioning / replica identity

/// True for `name(...)` where the call spans the whole text (`looks_like_function` in ruleutils.c).
fn looks_like_function(expr: &str) -> bool {
    let Some(open) = expr.find('(') else { return false };
    let name = &expr[..open];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || !expr.ends_with(')') {
        return false;
    }
    let mut depth = 0i32;
    for (i, c) in expr.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i + 1 != expr.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn partkeydef(env: &CatalogEnv, rel: Oid) -> Result<Option<String>> {
    let Some(r) = env.snapshot().relation(rel) else { return Ok(None) };
    let Some(pk) = r.partition_key.as_ref() else { return Ok(None) };
    let strat = match pk.strategy {
        b'h' => "HASH",
        b'l' => "LIST",
        b'r' => "RANGE",
        other => return Err(PgError::internal(format!("unexpected partition strategy: {other}"))),
    };
    let mut exprs = pk.exprs.iter();
    let mut parts = Vec::with_capacity(pk.columns.len());
    for &attnum in &pk.columns {
        if attnum != 0 {
            let col = r
                .column(attnum)
                .ok_or_else(|| PgError::internal(format!("cache lookup failed for attribute {attnum} of relation {rel}")))?;
            parts.push(qi(&col.name)?);
        } else {
            let e = exprs.next().ok_or_else(|| PgError::internal("too few entries in partexprs list"))?;
            if looks_like_function(&e.sql) {
                parts.push(e.sql.clone());
            } else {
                parts.push(format!("({})", e.sql));
            }
        }
    }
    Ok(Some(format!("{strat} ({})", parts.join(", "))))
}

/// `RelationGetReplicaIndex`: the primary key for DEFAULT identity, the flagged index for USING INDEX.
fn replica_identity_index(env: &CatalogEnv, rel: Oid) -> Option<Oid> {
    let snap = env.snapshot();
    let r = snap.relation(rel)?;
    let indexes = snap.indexes_of(rel);
    match r.replica_identity {
        ReplicaIdentity::Default => indexes.iter().find(|i| i.primary && i.valid).map(|i| i.oid),
        ReplicaIdentity::Index => indexes.iter().find(|i| i.replident).map(|i| i.oid),
        ReplicaIdentity::Nothing | ReplicaIdentity::Full => None,
    }
}

// ------------------------------------------------------------------ pg_identify_object

/// Identity columns of an existing object.
struct Ident {
    schema: Option<String>,
    name: Option<String>,
    identity: String,
    names: Vec<String>,
    args: Vec<String>,
}

/// `getObjectTypeDescription` for an object that does not exist.
fn missing_type_word(class: ObjClass) -> &'static str {
    match class {
        ObjClass::Relation => "relation",
        ObjClass::Type => "type",
        ObjClass::Namespace => "schema",
        ObjClass::Constraint => "constraint",
        ObjClass::AttrDefault => "default value",
        ObjClass::Proc => "routine",
        ObjClass::Role => "role",
        ObjClass::Database => "database",
        ObjClass::Collation => "collation",
        ObjClass::Operator => "operator",
        ObjClass::Cast => "cast",
        ObjClass::Language => "language",
        ObjClass::AccessMethod => "access method",
        ObjClass::Extension => "extension",
        ObjClass::Rewrite => "rule",
        ObjClass::Trigger => "trigger",
        ObjClass::Tablespace => "tablespace",
        ObjClass::OpClass => "operator class",
        ObjClass::OpFamily => "operator family",
        ObjClass::AmOp => "operator of access method",
        ObjClass::AmProc => "function of access method",
        ObjClass::Conversion => "conversion",
    }
}

/// `format_type_be_qualified`: catalog types print their SQL name, others `schema.name`.
fn type_identity(env: &CatalogEnv, oid: Oid) -> Result<String> {
    let Some(info) = env.type_info(oid) else { return Ok(oid.to_string()) };
    if info.is_array() {
        return Ok(format!("{}[]", type_identity(env, info.elem)?));
    }
    if info.namespace == crate::builtin::PG_CATALOG_NAMESPACE {
        return pgarbor_types::names::format_type(env, oid, -1);
    }
    qual(&nsname(env, info.namespace), &info.name)
}

fn simple_ident(schema: Option<String>, name: &str) -> Result<Ident> {
    Ok(Ident {
        schema,
        name: Some(name.to_string()),
        identity: qi(name)?,
        names: vec![name.to_string()],
        args: Vec::new(),
    })
}

fn identify(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<Ident>> {
    let snap = env.snapshot();
    match addr.class {
        ObjClass::Relation => {
            let Some((ns, name, _)) = snap.class_identity(addr.oid) else { return Ok(None) };
            let schema = nsname(env, ns);
            let name = name.to_string();
            let rel = qual(&schema, &name)?;
            if addr.sub_id == 0 {
                return Ok(Some(Ident {
                    schema: Some(schema.clone()),
                    name: Some(name.clone()),
                    identity: rel,
                    names: vec![schema, name],
                    args: Vec::new(),
                }));
            }
            let col = i16::try_from(addr.sub_id)
                .ok()
                .and_then(|n| snap.relation(addr.oid).and_then(|r| r.column(n)))
                .filter(|c| !c.dropped);
            let Some(col) = col else { return Ok(None) };
            Ok(Some(Ident {
                schema: Some(schema.clone()),
                name: Some(name.clone()),
                identity: format!("{rel}.{}", qi(&col.name)?),
                names: vec![schema, name, col.name.clone()],
                args: Vec::new(),
            }))
        }
        ObjClass::Type => {
            let Some(info) = env.type_info(addr.oid) else { return Ok(None) };
            let ident = type_identity(env, addr.oid)?;
            Ok(Some(Ident {
                schema: Some(nsname(env, info.namespace)),
                name: Some(info.name.to_string()),
                identity: ident.clone(),
                names: vec![ident],
                args: Vec::new(),
            }))
        }
        ObjClass::Namespace => match snap.namespace(addr.oid) {
            Some(n) => Ok(Some(simple_ident(None, &n.name)?)),
            None => Ok(None),
        },
        ObjClass::Role => match snap.role_name(addr.oid) {
            Some(n) => Ok(Some(simple_ident(None, n)?)),
            None => Ok(None),
        },
        ObjClass::Database => match snap.database(addr.oid) {
            Some(d) => Ok(Some(simple_ident(None, &d.name)?)),
            None => Ok(None),
        },
        ObjClass::Constraint => {
            let Some(c) = snap.constraint(addr.oid) else { return Ok(None) };
            let schema = nsname(env, c.namespace);
            let (owner_schema, owner_name) = if c.table != 0 {
                match snap.class_identity(c.table) {
                    Some((ns, n, _)) => (nsname(env, ns), n.to_string()),
                    None => return Ok(None),
                }
            } else {
                match env.type_info(c.domain) {
                    Some(t) => (nsname(env, t.namespace), t.name.to_string()),
                    None => return Ok(None),
                }
            };
            Ok(Some(Ident {
                schema: Some(schema),
                name: Some(c.name.clone()),
                identity: format!("{} on {}", qi(&c.name)?, qual(&owner_schema, &owner_name)?),
                names: vec![owner_schema, owner_name, c.name.clone()],
                args: Vec::new(),
            }))
        }
        ObjClass::AttrDefault => {
            for r in snap.user_relations() {
                for c in &r.columns {
                    if c.dropped || c.default.as_ref().map(|d| d.oid) != Some(addr.oid) {
                        continue;
                    }
                    let schema = nsname(env, r.namespace);
                    return Ok(Some(Ident {
                        schema: None,
                        name: None,
                        identity: format!("for {}.{}", qual(&schema, &r.name)?, qi(&c.name)?),
                        names: vec![schema, r.name.clone(), c.name.clone()],
                        args: Vec::new(),
                    }));
                }
            }
            Ok(None)
        }
        ObjClass::Proc => {
            let (ns, name, arg_types) = if let Some(f) = snap.function(addr.oid) {
                (f.namespace, f.name.clone(), f.in_arg_types())
            } else if let Some(p) = catalog_data::builtin_proc(addr.oid) {
                (p.namespace, p.name.to_string(), p.arg_types.to_vec())
            } else {
                return Ok(None);
            };
            let schema = nsname(env, ns);
            let mut args = Vec::with_capacity(arg_types.len());
            for t in arg_types {
                args.push(type_identity(env, t)?);
            }
            Ok(Some(Ident {
                schema: Some(schema.clone()),
                name: None,
                identity: format!("{}({})", qual(&schema, &name)?, args.join(",")),
                names: vec![schema, name],
                args,
            }))
        }
        ObjClass::Rewrite => {
            let Some(r) = snap.relation(addr.oid) else { return Ok(None) };
            if !matches!(r.kind, RelKind::View | RelKind::MatView) {
                return Ok(None);
            }
            let schema = nsname(env, r.namespace);
            Ok(Some(Ident {
                schema: None,
                name: Some("_RETURN".to_string()),
                identity: format!("\"_RETURN\" on {}", qual(&schema, &r.name)?),
                names: vec![schema, r.name.clone(), "_RETURN".to_string()],
                args: Vec::new(),
            }))
        }
        // The catalog snapshot keeps no rows for these classes.
        ObjClass::Collation
        | ObjClass::Operator
        | ObjClass::Cast
        | ObjClass::Language
        | ObjClass::AccessMethod
        | ObjClass::Extension
        | ObjClass::Trigger
        | ObjClass::Tablespace
        | ObjClass::OpClass
        | ObjClass::OpFamily
        | ObjClass::AmOp
        | ObjClass::AmProc
        | ObjClass::Conversion => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    fn run(name: &str, args: &[Datum]) -> Result<Datum> {
        let (_kv, env) = testing::bootstrapped()?;
        let proc = catalog_data::procs_named(name)
            .next()
            .ok_or_else(|| PgError::internal(format!("no builtin proc {name}")))?;
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = CatalogCallCtx { env: &env, session: &session, io_config: &io };
        call(&ctx, proc, args)
    }

    #[test]
    fn get_expr_returns_first_argument() {
        let d = run("pg_get_expr", &[Datum::text("(a > 0)"), Datum::Oid(1)]).unwrap();
        assert_eq!(d, Datum::text("(a > 0)"));
        assert_eq!(run("pg_get_expr", &[Datum::Null, Datum::Oid(1)]).unwrap(), Datum::Null);
    }

    #[test]
    fn userbyid() {
        assert_eq!(run("pg_get_userbyid", &[Datum::Oid(10)]).unwrap(), Datum::text("postgres"));
        assert_eq!(run("pg_get_userbyid", &[Datum::Oid(999999)]).unwrap(), Datum::text("unknown (OID=999999)"));
        assert_eq!(run("pg_get_userbyid", &[Datum::Null]).unwrap(), Datum::Null);
    }

    #[test]
    fn stubs_return_null() {
        assert_eq!(run("pg_get_triggerdef", &[Datum::Oid(12345)]).unwrap(), Datum::Null);
        assert_eq!(run("pg_get_partition_constraintdef", &[Datum::Oid(1259)]).unwrap(), Datum::Null);
        assert_eq!(run("pg_get_statisticsobjdef", &[Datum::Oid(1)]).unwrap(), Datum::Null);
    }

    #[test]
    fn unknown_catalog_name_gives_null_description() {
        let d = run("obj_description", &[Datum::Oid(1), Datum::text("no_such_catalog")]).unwrap();
        assert_eq!(d, Datum::Null);
        let d = run("col_description", &[Datum::Oid(999999), Datum::Int4(1)]).unwrap();
        assert_eq!(d, Datum::Null);
        let d = run("shobj_description", &[Datum::Oid(1), Datum::text("pg_class")]).unwrap();
        assert_eq!(d, Datum::Null);
    }

    #[test]
    fn unsupported_class_is_internal_error() {
        let e = run("pg_describe_object", &[Datum::Oid(1), Datum::Oid(1), Datum::Int4(0)]).unwrap_err();
        assert_eq!(e.message, "unsupported object class: 1");
    }

    #[test]
    fn qualified_name_splitting() {
        assert_eq!(split_qualified_name("t").unwrap(), vec!["t"]);
        assert_eq!(split_qualified_name("Public.T").unwrap(), vec!["public", "t"]);
        assert_eq!(split_qualified_name("\"Mixed\".\"a.b\"").unwrap(), vec!["Mixed", "a.b"]);
        assert_eq!(split_qualified_name("\"q\"\"x\"").unwrap(), vec!["q\"x"]);
        assert!(split_qualified_name("a..b").is_err());
        assert!(split_qualified_name("\"open").is_err());
        assert!(split_qualified_name("").is_err());
    }

    #[test]
    fn comma_splitting() {
        assert_eq!(split_top_level_commas("1, 'a,b', f(2, 3)"), vec!["1", "'a,b'", "f(2, 3)"]);
        assert_eq!(split_top_level_commas("x"), vec!["x"]);
    }

    #[test]
    fn function_shape_detection() {
        assert!(looks_like_function("lower(b)"));
        assert!(looks_like_function("f(g(1), 2)"));
        assert!(!looks_like_function("(a + 1)"));
        assert!(!looks_like_function("f(1) + g(2)"));
        assert!(!looks_like_function("a"));
    }

    #[test]
    fn arg_default_edge_cases() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let p = catalog_data::procs_named("pg_get_userbyid").next().unwrap();
        assert_eq!(function_arg_default(&env, p.oid, 1), None);
        assert_eq!(function_arg_default(&env, p.oid, 0), None);
        assert_eq!(function_arg_default(&env, 0xFFFF_FF00, 1), None);
    }

    #[test]
    fn missing_type_words() {
        assert_eq!(missing_type_word(ObjClass::Relation), "relation");
        assert_eq!(missing_type_word(ObjClass::Namespace), "schema");
    }
}
