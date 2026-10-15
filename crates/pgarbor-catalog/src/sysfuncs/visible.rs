//! Name-resolution functions: visibility predicates, `to_reg*`, `current_schema(s)`. Lease CAT-45.
//! PG sources: `catalog/namespace.c` (`*IsVisible`, `fetch_search_path`), `utils/adt/regproc.c`
//! (`to_regclass`, `to_regproc`, `to_regprocedure`, `to_regoper`, `to_regoperator`, `to_regnamespace`,
//! `to_regrole`, `to_regcollation`, `regclass(text)`), `utils/adt/misc.c` (`current_schema`,
//! `current_schemas`, `pg_my_temp_schema`).
//!
//! | proc (args) | behavior |
//! |---|---|
//! | `pg_table_is_visible(oid)` | `CatalogEnv::relation_is_visible`; unknown OID -> NULL |
//! | `pg_type_is_visible(oid)` | `type_is_visible`; unknown -> NULL |
//! | `pg_function_is_visible(oid)` | `function_is_visible`; unknown -> NULL |
//! | `pg_operator_is_visible(oid)`, `pg_opclass_is_visible`, `pg_opfamily_is_visible`, `pg_collation_is_visible`, `pg_conversion_is_visible`, `pg_statistics_obj_is_visible`, `pg_ts_config_is_visible`, `pg_ts_dict_is_visible`, `pg_ts_parser_is_visible`, `pg_ts_template_is_visible` | operator/opclass/opfamily/collation: visible iff the object's namespace is in `path().effective` and no earlier same-name object (builtins live in pg_catalog, which is searched first/implicitly); unknown OID -> NULL; conversions, statistics objects and text-search objects do not exist -> NULL |
//! | `to_regclass(text)` | `reg_lookup_impl(RegKind::Class, name)` -> `Datum::Oid` or NULL when missing; NEVER raises 42P01; syntax errors in the name (unterminated quotes, too many dots) are also swallowed to NULL like PG (`to_reg*` catch errors) |
//! | `to_regproc(text)`, `to_regprocedure(text)`, `to_regoper`, `to_regoperator`, `to_regnamespace`, `to_regrole`, `to_regcollation` | same with the matching `RegKind`; `to_regproc` of an ambiguous name -> raises 42725 `more than one function named "f"` (PG raises here) |
//! | `regclass(text)` | the text->regclass conversion (`'t'::regclass` semantics): `reg_lookup_impl(Class, name)` -> OID, missing -> 42P01 `relation "t" does not exist` (`errors::relation_does_not_exist`) |
//! | `current_schema()` | the first existing schema of `path().explicit` that is the creation target: name of `creation_namespace`, NULL if none |
//! | `current_schemas(bool)` | `name[]` of `path().explicit` names (false) or `effective` (true, includes implicit `pg_catalog`; oracle default: `{pg_catalog,public}` for true, `{public}` for false) |
//! | `pg_my_temp_schema()` | `session.temp_namespace()` or 0 |
//! | `pg_is_other_temp_schema(oid)` | false (no temp schemas of other sessions are visible) |
use super::CatalogCallCtx;
use crate::builtin::{OPCLASSES, OPFAMILIES, PG_CATALOG_NAMESPACE, TextRows, builtin_collation};
use crate::errors;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::BuiltinProc;
use pgarbor_types::catalog_data::{builtin_operator, builtin_proc};
use pgarbor_types::context::{RegKind, TypeEnv};
use pgarbor_types::oids;
use pgarbor_types::{Array, Datum};

/// Procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "pg_table_is_visible",
    "pg_type_is_visible",
    "pg_function_is_visible",
    "pg_operator_is_visible",
    "pg_opclass_is_visible",
    "pg_opfamily_is_visible",
    "pg_collation_is_visible",
    "pg_conversion_is_visible",
    "pg_statistics_obj_is_visible",
    "pg_ts_config_is_visible",
    "pg_ts_dict_is_visible",
    "pg_ts_parser_is_visible",
    "pg_ts_template_is_visible",
    "to_regclass",
    "to_regproc",
    "to_regprocedure",
    "to_regoper",
    "to_regoperator",
    "to_regnamespace",
    "to_regrole",
    "to_regcollation",
    "regclass",
    "current_schema",
    "current_schemas",
    "pg_my_temp_schema",
    "pg_is_other_temp_schema",
];

/// Dispatch on `proc.name` (module docs table).
// CONTRACT
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    match proc.name {
        "pg_table_is_visible" => relation_is_visible_fn(ctx, args),
        "pg_type_is_visible" => type_is_visible_fn(ctx, args),
        "pg_function_is_visible" => function_is_visible_fn(ctx, args),
        "pg_operator_is_visible" => operator_is_visible_fn(ctx, args),
        "pg_opclass_is_visible" => text_row_is_visible_fn(ctx, args, &OPCLASSES),
        "pg_opfamily_is_visible" => text_row_is_visible_fn(ctx, args, &OPFAMILIES),
        "pg_collation_is_visible" => collation_is_visible_fn(ctx, args),
        // PGArbor has no conversions, statistics objects or text-search objects.
        "pg_conversion_is_visible"
        | "pg_statistics_obj_is_visible"
        | "pg_ts_config_is_visible"
        | "pg_ts_dict_is_visible"
        | "pg_ts_parser_is_visible"
        | "pg_ts_template_is_visible" => Ok(Datum::Null),
        "to_regclass" => soft_reg(ctx, RegKind::Class, args),
        "to_regproc" => soft_reg(ctx, RegKind::Proc, args),
        "to_regprocedure" => soft_reg(ctx, RegKind::Procedure, args),
        "to_regoper" => soft_reg(ctx, RegKind::Oper, args),
        "to_regoperator" => soft_reg(ctx, RegKind::Operator, args),
        "to_regnamespace" => soft_reg(ctx, RegKind::Namespace, args),
        "to_regrole" => soft_reg(ctx, RegKind::Role, args),
        "to_regcollation" => soft_reg(ctx, RegKind::Collation, args),
        "regclass" => regclass_input(ctx, args),
        "current_schema" => current_schema(ctx),
        "current_schemas" => current_schemas(ctx, args),
        "pg_my_temp_schema" => Ok(Datum::Oid(ctx.session.temp_namespace().unwrap_or(0))),
        "pg_is_other_temp_schema" => match oid_arg(args, 0)? {
            None => Ok(Datum::Null),
            Some(_) => Ok(Datum::Bool(false)),
        },
        other => Err(PgError::internal(format!("function {other} is not a visibility or name-resolution function"))),
    }
}

// ---------------------------------------------------------------- argument helpers

/// Oid argument at `i`; NULL (or a missing argument) -> None.
fn oid_arg(args: &[Datum], i: usize) -> Result<Option<Oid>> {
    match args.get(i) {
        None | Some(Datum::Null) => Ok(None),
        Some(Datum::Oid(o)) => Ok(Some(*o)),
        Some(_) => Err(PgError::internal("catalog function expected an oid argument")),
    }
}

/// Text argument at `i`; NULL (or a missing argument) -> None.
fn text_arg(args: &[Datum], i: usize) -> Result<Option<&str>> {
    match args.get(i) {
        None | Some(Datum::Null) => Ok(None),
        Some(Datum::Text(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(PgError::internal("catalog function expected a text argument")),
    }
}

/// Boolean argument at `i`; NULL (or a missing argument) -> None.
fn bool_arg(args: &[Datum], i: usize) -> Result<Option<bool>> {
    match args.get(i) {
        None | Some(Datum::Null) => Ok(None),
        Some(Datum::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(PgError::internal("catalog function expected a boolean argument")),
    }
}

// ---------------------------------------------------------------- visibility predicates

/// PG `pg_table_is_visible`: any pg_class OID; unknown OID -> NULL.
fn relation_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    if ctx.env.snapshot().class(oid).is_none() {
        return Ok(Datum::Null);
    }
    Ok(Datum::Bool(ctx.env.relation_is_visible(oid)?))
}

/// PG `pg_type_is_visible`: unknown type OID -> NULL.
fn type_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    if ctx.env.type_info(oid).is_none() {
        return Ok(Datum::Null);
    }
    Ok(Datum::Bool(ctx.env.type_is_visible(oid)?))
}

/// PG `pg_function_is_visible`: unknown function OID -> NULL.
fn function_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    let known = builtin_proc(oid).is_some() || ctx.env.snapshot().function(oid).is_some();
    if !known {
        return Ok(Datum::Null);
    }
    Ok(Datum::Bool(ctx.env.function_is_visible(oid)?))
}

/// PG `pg_operator_is_visible`. Builtin operators live in pg_catalog and PGArbor has no user
/// operators that could shadow them, so visibility is "pg_catalog is in the effective path".
fn operator_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    if builtin_operator(oid).is_none() {
        return Ok(Datum::Null);
    }
    Ok(Datum::Bool(path_visible(&ctx.env.path().effective, PG_CATALOG_NAMESPACE, |_| false)))
}

/// `pg_opclass_is_visible` / `pg_opfamily_is_visible` over the builtin text-row tables (both keep
/// the OID in column 0 and the namespace in column 3). Builtin rows only; no shadowing objects.
fn text_row_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum], table: &TextRows) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    let Some(row) = text_row(table, oid) else {
        return Ok(Datum::Null);
    };
    let ns = row
        .get(3)
        .copied()
        .flatten()
        .and_then(|s| s.parse::<Oid>().ok())
        .ok_or_else(|| PgError::internal("malformed builtin catalog row"))?;
    Ok(Datum::Bool(path_visible(&ctx.env.path().effective, ns, |_| false)))
}

/// `pg_collation_is_visible`: builtin collations only (PGArbor has no CREATE COLLATION objects).
fn collation_is_visible_fn(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(oid) = oid_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    let Some(coll) = builtin_collation(oid) else {
        return Ok(Datum::Null);
    };
    Ok(Datum::Bool(path_visible(&ctx.env.path().effective, coll.namespace, |_| false)))
}

/// PG visibility rule over an effective search order: the object's namespace must be on the path
/// and no EARLIER namespace on the path may hold a same-name object (`shadowed_in`).
fn path_visible(path: &[Oid], ns: Oid, shadowed_in: impl Fn(Oid) -> bool) -> bool {
    match path.iter().position(|n| *n == ns) {
        None => false,
        Some(pos) => !path[..pos].iter().any(|n| shadowed_in(*n)),
    }
}

/// Row of a builtin text-row table whose column 0 is `oid`.
fn text_row(table: &TextRows, oid: Oid) -> Option<&'static [Option<&'static str>]> {
    table.rows.iter().copied().find(|row| row.first().copied().flatten().and_then(|s| s.parse::<Oid>().ok()) == Some(oid))
}

// ---------------------------------------------------------------- to_reg* and regclass

/// `to_reg*` family: NULL when the name does not resolve; syntax and lookup errors become NULL like
/// PG's soft-error input functions. Ambiguity (42725) and internal/unimplemented errors propagate.
fn soft_reg(ctx: &CatalogCallCtx, kind: RegKind, args: &[Datum]) -> Result<Datum> {
    let Some(name) = text_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    match ctx.env.reg_lookup_impl(kind, name) {
        Ok(Some(oid)) => Ok(Datum::Oid(oid)),
        Ok(None) => Ok(Datum::Null),
        Err(e) if keeps_error(&e) => Err(e),
        Err(_) => Ok(Datum::Null),
    }
}

/// Which `reg_lookup_impl` errors a `to_reg*` call re-raises. Only name-resolution/syntax classes
/// (42xxx, 3F000) are swallowed; 42725 (ambiguous name) and every other class propagate.
fn keeps_error(e: &PgError) -> bool {
    if e.sqlstate == SqlState::AMBIGUOUS_FUNCTION {
        return true;
    }
    let code = e.sqlstate.code();
    !(code.starts_with("42") || code.starts_with("3F"))
}

/// `regclass(text)`: relation name input, 42P01 when the relation does not exist.
fn regclass_input(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(name) = text_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    match ctx.env.reg_lookup_impl(RegKind::Class, name)? {
        Some(oid) => Ok(Datum::Oid(oid)),
        None => {
            let parts = split_qualified_name(name)?;
            Err(missing_relation_error(&parts))
        }
    }
}

/// 42P01 `relation "s.t" does not exist` built from the parsed (unquoted) name parts, as PG's
/// `NameListToString` prints them.
fn missing_relation_error(parts: &[String]) -> PgError {
    match parts {
        [name] => errors::relation_does_not_exist(None, name.as_str()),
        [schema, name] => errors::relation_does_not_exist(Some(schema.as_str()), name.as_str()),
        _ => errors::relation_does_not_exist(None, &parts.join(".")),
    }
}

/// PG `stringToQualifiedNameList` rules: dots outside double quotes separate parts; unquoted parts
/// are down-cased (ASCII); `"..."` keeps case with `""` as an escaped quote; surrounding whitespace is
/// ignored; anything else is 42602 `invalid name syntax`. Parts are truncated to 63 bytes
/// (NAMEDATALEN - 1). The number of parts is NOT limited here (the reg lookup rejects > 2).
fn split_qualified_name(text: &str) -> Result<Vec<String>> {
    let b = text.as_bytes();
    let mut parts = Vec::new();
    let mut i = 0usize;
    loop {
        while i < b.len() && is_name_space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            return Err(invalid_name());
        }
        let mut ident: Vec<u8> = Vec::new();
        if b[i] == b'"' {
            i += 1;
            loop {
                if i >= b.len() {
                    return Err(invalid_name());
                }
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        ident.push(b'"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                ident.push(b[i]);
                i += 1;
            }
            if ident.is_empty() {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "zero-length delimited identifier"));
            }
        } else {
            let start = i;
            while i < b.len() && is_ident_byte(b[i]) {
                i += 1;
            }
            if i == start {
                return Err(invalid_name());
            }
            ident.extend(b[start..i].iter().map(|c| c.to_ascii_lowercase()));
        }
        let s = String::from_utf8(ident).map_err(|_| invalid_name())?;
        parts.push(truncate_identifier(s));
        while i < b.len() && is_name_space(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'.' {
            i += 1;
            continue;
        }
        return Err(invalid_name());
    }
    Ok(parts)
}

fn invalid_name() -> PgError {
    PgError::new(SqlState::INVALID_NAME, "invalid name syntax")
}

fn is_name_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// Clip to NAMEDATALEN - 1 = 63 bytes at a character boundary.
fn truncate_identifier(mut s: String) -> String {
    if s.len() > 63 {
        let mut cut = 63;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

// ---------------------------------------------------------------- current_schema(s)

/// Name of a namespace OID. Builtin/user schemas come from the snapshot. A session temp schema is not
/// in the snapshot; it is named `pg_temp_<oid>` here (PGArbor does not model backend ids in names).
fn namespace_name(ctx: &CatalogCallCtx, ns: Oid) -> Option<String> {
    if let Some(n) = ctx.env.snapshot().namespace(ns) {
        return Some(n.name.clone());
    }
    if ctx.env.path().temp_namespace == Some(ns) {
        return Some(format!("pg_temp_{ns}"));
    }
    None
}

/// `current_schema()`: name of the creation target (first explicit search_path schema), NULL if none.
fn current_schema(ctx: &CatalogCallCtx) -> Result<Datum> {
    match ctx.env.path().creation_namespace {
        None => Ok(Datum::Null),
        Some(ns) => match namespace_name(ctx, ns) {
            Some(name) => Ok(Datum::Text(name)),
            None => Err(errors::cache_lookup_failed("namespace", ns)),
        },
    }
}

/// `current_schemas(bool)`: explicit path names (false) or effective path names (true). Schemas that
/// no longer resolve are skipped, as PG's `get_namespace_name` loop does.
fn current_schemas(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let Some(include_implicit) = bool_arg(args, 0)? else {
        return Ok(Datum::Null);
    };
    let path = ctx.env.path();
    let order = if include_implicit { &path.effective } else { &path.explicit };
    let items: Vec<Datum> = order.iter().filter_map(|ns| namespace_name(ctx, *ns)).map(Datum::Text).collect();
    Ok(Datum::Array(Box::new(Array::from_vec(oids::NAME, items))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec};
    use crate::env::CatalogEnv;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;
    use pgarbor_types::TypeId;
    use pgarbor_types::catalog_data::procs_named;

    fn call_named(env: &CatalogEnv, name: &str, args: &[Datum]) -> Result<Datum> {
        let proc = procs_named(name).next().ok_or_else(|| PgError::internal(format!("no builtin proc {name}")))?;
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = CatalogCallCtx { env, session: &session, io_config: &io };
        call(&ctx, proc, args)
    }

    fn text(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    #[test]
    fn to_regclass_finds_builtin_and_is_null_when_missing() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        assert_eq!(call_named(&env, "to_regclass", &[text("pg_class")])?, Datum::Oid(1259));
        assert_eq!(call_named(&env, "to_regclass", &[text("public.nosuch")])?, Datum::Null);
        assert_eq!(call_named(&env, "to_regclass", &[text("nos.x")])?, Datum::Null);
        assert_eq!(call_named(&env, "to_regclass", &[text("\"unterminated")])?, Datum::Null);
        assert_eq!(call_named(&env, "to_regclass", &[text("a.b.c.d")])?, Datum::Null);
        assert_eq!(call_named(&env, "to_regclass", &[Datum::Null])?, Datum::Null);
        Ok(())
    }

    #[test]
    fn regclass_raises_42p01_with_parsed_name() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        match call_named(&env, "regclass", &[text("nosuch")]) {
            Err(e) => {
                assert_eq!(e.sqlstate.code(), "42P01");
                assert_eq!(e.message, "relation \"nosuch\" does not exist");
            }
            Ok(_) => panic!("regclass of a missing relation must fail"),
        }
        match call_named(&env, "regclass", &[text("MixedName")]) {
            Err(e) => assert_eq!(e.message, "relation \"mixedname\" does not exist"),
            Ok(_) => panic!("regclass of a missing relation must fail"),
        }
        match call_named(&env, "regclass", &[text("\"Mixed\"")]) {
            Err(e) => assert_eq!(e.message, "relation \"Mixed\" does not exist"),
            Ok(_) => panic!("regclass of a missing relation must fail"),
        }
        match call_named(&env, "regclass", &[text("nos.x")]) {
            Err(e) => assert_eq!(e.message, "relation \"nos.x\" does not exist"),
            Ok(_) => panic!("regclass of a missing relation must fail"),
        }
        Ok(())
    }

    #[test]
    fn current_schema_and_current_schemas_default_path() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        assert_eq!(call_named(&env, "current_schema", &[])?, text("public"));
        match call_named(&env, "current_schemas", &[Datum::Bool(true)])? {
            Datum::Array(a) => assert_eq!(a.items, vec![text("pg_catalog"), text("public")]),
            _ => panic!("current_schemas must return an array"),
        }
        match call_named(&env, "current_schemas", &[Datum::Bool(false)])? {
            Datum::Array(a) => assert_eq!(a.items, vec![text("public")]),
            _ => panic!("current_schemas must return an array"),
        }
        assert_eq!(call_named(&env, "current_schemas", &[Datum::Null])?, Datum::Null);
        Ok(())
    }

    #[test]
    fn visibility_of_builtins_and_unknown_oids() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        assert_eq!(call_named(&env, "pg_table_is_visible", &[Datum::Oid(1259)])?, Datum::Bool(true));
        assert_eq!(call_named(&env, "pg_table_is_visible", &[Datum::Oid(999_999)])?, Datum::Null);
        assert_eq!(call_named(&env, "pg_type_is_visible", &[Datum::Oid(23)])?, Datum::Bool(true));
        assert_eq!(call_named(&env, "pg_type_is_visible", &[Datum::Oid(999_999)])?, Datum::Null);
        assert_eq!(call_named(&env, "pg_function_is_visible", &[Datum::Oid(1079)])?, Datum::Bool(true));
        assert_eq!(call_named(&env, "pg_function_is_visible", &[Datum::Oid(999_999)])?, Datum::Null);
        assert_eq!(call_named(&env, "pg_collation_is_visible", &[Datum::Oid(100)])?, Datum::Bool(true));
        assert_eq!(call_named(&env, "pg_collation_is_visible", &[Datum::Oid(999_999)])?, Datum::Null);
        assert_eq!(call_named(&env, "pg_conversion_is_visible", &[Datum::Oid(1)])?, Datum::Null);
        assert_eq!(call_named(&env, "pg_is_other_temp_schema", &[Datum::Oid(2200)])?, Datum::Bool(false));
        assert_eq!(call_named(&env, "pg_my_temp_schema", &[])?, Datum::Oid(0));
        Ok(())
    }

    #[test]
    fn user_table_visible_after_create() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateTableSpec {
            namespace: 2200,
            name: "t".to_string(),
            owner: 10,
            kind: crate::model::RelKind::Table,
            persistence: Default::default(),
            columns: vec![ColumnSpec::new("id", TypeId { oid: 23, typmod: -1 })],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        let oid = match call_named(&env, "to_regclass", &[text("public.t")])? {
            Datum::Oid(o) if o != 0 => o,
            other => return Err(PgError::internal(format!("to_regclass of created table gave {other:?}"))),
        };
        assert_eq!(call_named(&env, "pg_table_is_visible", &[Datum::Oid(oid)])?, Datum::Bool(true));
        assert_eq!(call_named(&env, "regclass", &[text("t")])?, Datum::Oid(oid));
        Ok(())
    }

    #[test]
    fn split_handles_quotes_case_and_dots() -> Result<()> {
        assert_eq!(split_qualified_name("Public.T")?, vec!["public".to_string(), "t".to_string()]);
        assert_eq!(split_qualified_name("\"Mixed\"")?, vec!["Mixed".to_string()]);
        assert_eq!(split_qualified_name("\"a\"\"b\"")?, vec!["a\"b".to_string()]);
        assert_eq!(split_qualified_name("  s . \"T\" ")?, vec!["s".to_string(), "T".to_string()]);
        assert!(split_qualified_name("a b").is_err());
        assert!(split_qualified_name("").is_err());
        assert!(split_qualified_name("a.").is_err());
        Ok(())
    }
}
