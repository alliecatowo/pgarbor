//! CREATE [OR REPLACE] [MATERIALIZED] VIEW and REFRESH MATERIALIZED VIEW (SQL-36, sonnet).
//! PG references: `view.c` (`DefineView`, `DefineVirtualRelation`, `checkViewColumns`), `matview.c`
//! (`ExecCreateTableAs` for matviews, `ExecRefreshMatView`), `ruleutils.c` (`pg_get_viewdef` format),
//! `parse_utilcmd.c` (`transformCreateTableAsStmt`).
//!
//! `bind_create_view(b, stmt)` (`Statement::CreateView { or_replace, materialized, name, columns,
//! query, options, with_no_schema_binding, if_not_exists, temporary, to, params, .. }`):
//! * name/namespace as CREATE TABLE (`creation_namespace_for`); `TEMP` views 0A000; `WITH
//!   NO SCHEMA BINDING`, `TO` (ClickHouse), `ON CLUSTER`, `cluster_by`, `secure`: 42601.
//! * The query is bound with `select::bind_query` (top_level false): parameters in a view are an error
//!   (42P02 `there is no parameter $1`, oracle); unknown-typed output columns resolve to `text`
//!   (PG 10+); column names = the optional column list (`CREATE VIEW v(a,b)`: more names than columns
//!   42601 `CREATE VIEW specifies more column names than columns`; FEWER names than columns is accepted and
//!   the remaining columns keep their query names - oracle) else the query's output names;
//!   duplicate output names: 42701 `column "x" specified more than once`; a view must have >= 1 column.
//! * `ViewSpec.columns` = `ColumnSpec::new(name, ty)` for each output (collation from the query output
//!   type's default), `ViewSpec.query` = `StoredExpr { sql: deparse::query::deparse_viewdef(&bound),
//!   depends_on: <relations, columns, functions, types referenced> }` in `pg_get_viewdef` format
//!   (` SELECT a,\n    b\n   FROM t;` — see deparse docs), `or_replace`, `materialized`, `populated =
//!   !WITH NO DATA`, `options` from `WITH (security_barrier=true, check_option=..)`, `check_option` from
//!   `WITH [LOCAL|CASCADED] CHECK OPTION` (not valid on non-updatable views: 0A000).
//! * `OR REPLACE` compatibility with the existing view is checked by the CATALOG (42P16 `cannot change
//!   name of view column "a" to "b"` ...).
//! * Dependencies: every `RangeKind::Relation` OID, referenced columns (`Column` nodes mapped through
//!   range entries; `whole-row` references depend on the relation), user functions, user types; views
//!   referencing views depend on the inner view (not its base tables).
//!
//! `bind_refresh_matview(b, name, concurrently, with_data)` -> `RefreshMatView { view, with_data,
//! concurrently }`: relation must be a matview (42809 `"v" is not a materialized view`);
//! `CONCURRENTLY` requires a unique index (55000 `cannot refresh materialized view "public.v"
//! concurrently` HINT `Create a unique index with no WHERE clause on one or more columns of the
//! materialized view.`) — checked by the executor; `WITH NO DATA` + CONCURRENTLY 0A000.
use std::collections::HashSet;

use pgarbor_catalog::ddl::spec::{ColumnSpec, ViewSpec};
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::{ObjClass, ObjectAddress, RelKind, StoredExpr};
use pgarbor_common::{PgError, Result, SqlState, FIRST_NORMAL_OID};
use pgarbor_types::{oids, TypeId};
use sqlparser::ast::{CreateTableOptions, CreateView, Expr, ObjectName, Query, SetExpr, SqlOption, Statement, Value};

use super::context::Binder;
use super::select::bind_subselect;
use crate::deparse::query::{deparse_viewdef, query_dependencies};
use crate::deparse::DeparseOptions;
use crate::diag;
use crate::ir::{BoundCreateView, BoundQuery, BoundStatement, DistinctSpec, ExprKind, FromItem, QueryBody, RangeKind};
use crate::names::{fold_ident, QualName};

/// Reloption names PostgreSQL accepts for views (`view_reloptions` in `access/common/reloptions.c`).
const VIEW_OPTIONS: [&str; 3] = ["security_barrier", "security_invoker", "check_option"];

/// `view_query_is_auto_updatable` message for a query that does not select from one relation.
const NOT_SINGLE_TABLE: &str =
    "Views that do not select from a single table or view are not automatically updatable.";

/// `ALTER VIEW v SET (check_option = local|cascaded)` on an existing view (PG `ATExecSetRelOptions`,
/// tablecmds.c: `view_query_is_auto_updatable` over the stored query). The stored definition is
/// re-bound the way FROM expansion binds it; the dependencies that binding records are discarded
/// because an ALTER records none. Other `check_option` values are left to the catalog, which reports
/// them (22023) before this check in PG as well.
pub fn check_alter_view_check_option(b: &mut Binder, view: pgarbor_common::Oid, set: &[String]) -> Result<()> {
    let wants_check = set.iter().any(|opt| match opt.split_once('=') {
        Some((name, value)) => {
            name == "check_option" && matches!(value.to_ascii_lowercase().as_str(), "local" | "cascaded")
        }
        None => false,
    });
    if !wants_check {
        return Ok(());
    }
    let env = b.env;
    let snapshot = env.snapshot();
    let sql = match snapshot.relation(view) {
        Some(rel) => match rel.view.as_ref() {
            Some(def) => def.query.sql.clone(),
            None => return Err(PgError::internal(format!("view \"{}\" has no definition", rel.name))),
        },
        None => return Err(PgError::internal(format!("relation {view} missing from catalog snapshot"))),
    };
    let parsed = crate::parser::parse_one(&sql)?;
    let query: &Query = match parsed.as_ast() {
        Some(Statement::Query(q)) => &**q,
        _ => return Err(PgError::internal("stored definition of a view is not a query")),
    };
    let mark = b.deps.len();
    let bound = bind_subselect(b, query, &[]);
    b.deps.truncate(mark);
    let bq = bound?;
    match auto_updatable_error(&bq) {
        None => Ok(()),
        Some(reason) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "WITH CHECK OPTION is supported only on automatically updatable views",
        )
        .with_hint(reason)),
    }
}

/// `Statement::CreateView`.
///
/// PG reference: `commands/view.c` `DefineView` (parse analysis first, then the WITH CHECK OPTION
/// check, then the column alias list, then `DefineVirtualRelation`). Name resolution follows
/// `creation_namespace_for` as CREATE TABLE does.
pub fn bind_create_view(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let cv: &CreateView = match stmt {
        Statement::CreateView(cv) => cv,
        _ => return Err(PgError::internal("ddl_view::bind_create_view called with a non CREATE VIEW statement")),
    };
    // gram.y has no CREATE OR REPLACE MATERIALIZED VIEW production.
    if cv.materialized && cv.or_replace {
        return Err(diag::syntax_error_near("MATERIALIZED", None));
    }
    reject_unsupported_syntax(cv)?;
    let qn = b.qual(&cv.name)?;
    check_catalog_qualifier(b, &qn)?;
    reject_query_forms(&cv.query)?;
    // Views referenced directly by the query are recorded in `b.deps` while it binds (see from.rs).
    let dep_mark = b.deps.len();

    // DefineView runs parse analysis of the query before anything else, so query errors win over
    // name and option errors.
    let mut bq = bind_subselect(b, &cv.query, &[])?;
    // Parameters are not allowed in a view definition (`fixed_paramref_hook` with zero parameters).
    if b.params.max_referenced > 0 {
        return Err(diag::no_such_parameter(b.params.max_referenced, None));
    }

    let (options, option_check) = bind_view_options(b, &cv.options, cv.materialized)?;
    // The trailing `WITH [CASCADED|LOCAL] CHECK OPTION` (`view_trailer_opts`) sets the same option.
    let check_option = b.view_trailer.as_ref().and_then(|t| t.check_option.clone()).or(option_check);
    // The trailing `WITH [NO] DATA` of a materialized view; absent means WITH DATA.
    let with_data = b.view_trailer.as_ref().and_then(|t| t.with_data).unwrap_or(true);
    if check_option.is_some() {
        if let Some(reason) = auto_updatable_error(&bq) {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "WITH CHECK OPTION is supported only on automatically updatable views",
            )
            .with_hint(reason));
        }
    }

    // Column alias list: fewer names than columns is accepted (the rest keep their query names),
    // more names than columns is 42601 (view.c `DefineView`).
    if cv.columns.len() > bq.output.len() {
        return Err(diag::syntax("CREATE VIEW specifies more column names than columns", None));
    }
    for (i, col) in cv.columns.iter().enumerate() {
        let name = b.ident(&col.name);
        if let Some(out) = bq.output.get_mut(i) {
            out.name = name;
        }
    }

    {
        let mut seen: HashSet<&str> = HashSet::with_capacity(bq.output.len());
        for out in &bq.output {
            if !seen.insert(out.name.as_str()) {
                return Err(diag::column_specified_twice(&out.name, None));
            }
        }
    }

    // Unknown-typed output columns resolve to text (PG 10+). The binder normally resolves them
    // already; this is the defensive equivalent for the stored column type.
    let mut columns: Vec<ColumnSpec> = Vec::with_capacity(bq.output.len());
    for out in &bq.output {
        let ty = if out.ty.oid == oids::UNKNOWN { TypeId::TEXT } else { out.ty };
        columns.push(ColumnSpec::new(out.name.clone(), ty));
    }

    let names: Vec<String> = bq.output.iter().map(|c| c.name.clone()).collect();
    let sql = deparse_viewdef(b.env, &bq, &names, DeparseOptions::default())?;

    let mut deps = query_dependencies(&bq)?;
    for dep in b.deps.split_off(dep_mark) {
        if !deps.contains(&dep) {
            deps.push(dep);
        }
    }
    for out in &bq.output {
        if out.ty.oid >= FIRST_NORMAL_OID {
            let dep = ObjectAddress::new(ObjClass::Type, out.ty.oid);
            if !deps.contains(&dep) {
                deps.push(dep);
            }
        }
    }

    let namespace = b.env.creation_namespace_for(qn.schema_str(), &qn.name)?;
    let spec = ViewSpec {
        namespace,
        name: qn.name.clone(),
        owner: b.env.current_user(),
        columns,
        query: StoredExpr::new(sql).with_deps(deps),
        or_replace: cv.or_replace,
        materialized: cv.materialized,
        // `WITH NO DATA` leaves the matview unpopulated (scans fail 55000 until a plain REFRESH).
        populated: with_data,
        options,
        check_option,
        if_not_exists: cv.if_not_exists,
    };
    Ok(BoundStatement::CreateView(Box::new(BoundCreateView { spec, query: Box::new(bq) })))
}

/// `REFRESH MATERIALIZED VIEW`.
///
/// PG reference: `commands/matview.c` `ExecRefreshMatView` and the `RangeVarCallbackMaintainsTable`
/// callback in `commands/tablecmds.c`. The concurrent-unique-index check is the executor's.
pub fn bind_refresh_matview(b: &mut Binder, name: &ObjectName, concurrently: bool, with_data: bool) -> Result<BoundStatement> {
    let qn = b.qual(name)?;
    check_catalog_qualifier(b, &qn)?;
    let oid = match b.env.resolve_class(qn.schema_str(), &qn.name)? {
        ClassRef::Relation(oid) => oid,
        _ => {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a table or materialized view", qn.name),
            ))
        }
    };
    let rel = b
        .env
        .snapshot()
        .relation(oid)
        .ok_or_else(|| PgError::internal("ddl_view: relation missing from catalog snapshot"))?;
    match rel.kind {
        RelKind::MatView => {}
        // A table passes the callback and fails in ExecRefreshMatView (0A000 in PG 18.6).
        RelKind::Table | RelKind::PartitionedTable => {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("\"{}\" is not a materialized view", rel.name),
            ))
        }
        _ => {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a table or materialized view", rel.name),
            ))
        }
    }
    if concurrently && !with_data {
        return Err(PgError::new(
            SqlState::SYNTAX_ERROR,
            "CONCURRENTLY and WITH NO DATA options cannot be used together",
        ));
    }
    Ok(BoundStatement::RefreshMatView { view: oid, with_data, concurrently })
}

fn feature_not_supported(msg: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, msg)
}

/// Clauses that the grammar of PostgreSQL does not have (or that PGArbor does not implement).
fn reject_unsupported_syntax(cv: &CreateView) -> Result<()> {
    if cv.temporary {
        return Err(diag::unsupported_construct("temporary views", None));
    }
    if cv.or_alter {
        return Err(diag::syntax_error_near("ALTER", None));
    }
    if cv.secure {
        return Err(diag::syntax_error_near("SECURE", None));
    }
    if cv.with_no_schema_binding {
        return Err(diag::syntax_error_near("WITH", None));
    }
    if cv.to.is_some() {
        return Err(diag::syntax_error_near("TO", None));
    }
    if !cv.cluster_by.is_empty() {
        return Err(diag::syntax_error_near("CLUSTER", None));
    }
    if cv.comment.is_some() {
        return Err(diag::syntax_error_near("COMMENT", None));
    }
    if cv.copy_grants {
        return Err(diag::syntax_error_near("COPY", None));
    }
    if cv.params.is_some() {
        return Err(diag::syntax_error_near("ALGORITHM", None));
    }
    for col in &cv.columns {
        if let Some(dt) = &col.data_type {
            return Err(diag::syntax_error_near(&dt.to_string(), None));
        }
        if col.options.is_some() {
            return Err(diag::syntax_error_near(&col.name.value, None));
        }
    }
    Ok(())
}

/// `SELECT INTO` and data-modifying statements in WITH are not allowed in a view (view.c).
fn reject_query_forms(q: &Query) -> Result<()> {
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            if matches!(
                &*cte.query.body,
                SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_)
            ) {
                return Err(feature_not_supported("views must not contain data-modifying statements in WITH"));
            }
        }
    }
    if let SetExpr::Select(sel) = &*q.body {
        if sel.into.is_some() {
            return Err(feature_not_supported("views must not contain SELECT INTO"));
        }
    }
    Ok(())
}

/// The catalog qualifier of a name must be the connected database (3-part names).
fn check_catalog_qualifier(b: &Binder, qn: &QualName) -> Result<()> {
    match &qn.catalog {
        Some(cat) if cat.as_str() != b.ctx.session.database.as_str() => {
            Err(diag::cross_database(&format!("{}.{}", cat, qn.display()), qn.pos))
        }
        _ => Ok(()),
    }
}

/// `WITH (...)` of a view: returns the stored `name=value` reloptions and the `check_option`.
///
/// PG reference: `access/common/reloptions.c` (`build_reloptions`, `parse_one_reloption`,
/// `parse_bool`). `check_option` is kept out of the reloption list (it has its own spec field).
fn bind_view_options(b: &mut Binder, opts: &CreateTableOptions, materialized: bool) -> Result<(Vec<String>, Option<String>)> {
    let list: &[SqlOption] = match opts {
        CreateTableOptions::None => return Ok((Vec::new(), None)),
        CreateTableOptions::With(list) => list.as_slice(),
        _ => return Err(diag::syntax_error_near("OPTIONS", None)),
    };
    let mut stored: Vec<String> = Vec::with_capacity(list.len());
    let mut check_option: Option<String> = None;
    let mut seen: Vec<String> = Vec::with_capacity(list.len());
    for opt in list {
        let (name, value): (String, Option<String>) = match opt {
            SqlOption::KeyValue { key, value } => {
                let name = b.ident(key);
                let text = option_value_text(value)
                    .ok_or_else(|| diag::syntax(format!("invalid value for option \"{name}\""), None))?;
                (name, Some(text))
            }
            SqlOption::Ident(id) => (b.ident(id), None),
            _ => return Err(diag::syntax("syntax error in WITH options", None)),
        };
        if materialized {
            if VIEW_OPTIONS.contains(&name.as_str()) {
                return Err(diag::invalid_parameter_value(format!("unrecognized parameter \"{name}\""), None));
            }
            return Err(diag::unsupported_construct(&format!("storage parameter \"{name}\""), None));
        }
        if seen.iter().any(|s| *s == name) {
            return Err(diag::invalid_parameter_value(
                format!("parameter \"{name}\" specified more than once"),
                None,
            ));
        }
        seen.push(name.clone());
        match name.as_str() {
            "security_barrier" | "security_invoker" => {
                // A bare name means `true` (reloptions: a missing value is the boolean true).
                let text = value.unwrap_or_else(|| "true".to_string());
                if parse_pg_bool(&text).is_none() {
                    return Err(diag::invalid_parameter_value(
                        format!("invalid value for boolean option \"{name}\": {text}"),
                        None,
                    ));
                }
                stored.push(format!("{name}={text}"));
            }
            "check_option" => {
                let text = value.ok_or_else(|| diag::syntax(format!("{name} requires a parameter"), None))?;
                let lower = text.to_ascii_lowercase();
                if lower != "local" && lower != "cascaded" {
                    return Err(diag::invalid_parameter_value(
                        format!("invalid value for enum option \"{name}\": {text}"),
                        None,
                    )
                    .with_detail("Valid values are \"local\" and \"cascaded\"."));
                }
                check_option = Some(lower);
            }
            _ => {
                return Err(diag::invalid_parameter_value(format!("unrecognized parameter \"{name}\""), None));
            }
        }
    }
    Ok((stored, check_option))
}

/// Text of a reloption value (`def_arg`): keywords and identifiers fold, strings stay as written.
fn option_value_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Boolean(flag) => Some(if *flag { "true".to_string() } else { "false".to_string() }),
            Value::SingleQuotedString(s) => Some(s.clone()),
            Value::Number(_, _) => Some(v.value.to_string()),
            _ => None,
        },
        Expr::Identifier(id) => Some(fold_ident(id)),
        _ => None,
    }
}

/// PG `parse_bool`: case-insensitive; `true/false`, `yes/no`, `on/off`, `1/0`, and unique prefixes
/// of the words (`t`, `tr`, `y`, `f`, `n`); the lone prefix `o` is ambiguous and rejected.
fn parse_pg_bool(text: &str) -> Option<bool> {
    let lower = text.to_ascii_lowercase();
    match lower.as_str() {
        "1" | "on" => return Some(true),
        "0" | "off" => return Some(false),
        _ => {}
    }
    if lower.is_empty() {
        return None;
    }
    if "true".starts_with(lower.as_str()) || "yes".starts_with(lower.as_str()) {
        return Some(true);
    }
    if "false".starts_with(lower.as_str()) || "no".starts_with(lower.as_str()) {
        return Some(false);
    }
    None
}

/// `view_query_is_auto_updatable` (rewrite/rewriteHandler.c) over the bound query: `None` when the
/// view can be updated, otherwise PG's reason. Views expanded by the binder appear as `Subquery`
/// entries, which are accepted here (approximation: PG would inspect the inner view).
fn auto_updatable_error(q: &BoundQuery) -> Option<&'static str> {
    if !q.ctes.is_empty() {
        return Some("Views containing WITH are not automatically updatable.");
    }
    if q.limit.is_some() || q.offset.is_some() {
        return Some("Views containing LIMIT or OFFSET are not automatically updatable.");
    }
    let sel = match &q.body {
        QueryBody::Select(sel) => sel,
        QueryBody::SetOp(_) => {
            return Some("Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.")
        }
        QueryBody::Values(_) => return Some(NOT_SINGLE_TABLE),
    };
    if !matches!(sel.distinct, DistinctSpec::None) {
        return Some("Views containing DISTINCT are not automatically updatable.");
    }
    if sel.group_by.sets.is_some() || !sel.group_by.exprs.is_empty() {
        return Some("Views containing GROUP BY are not automatically updatable.");
    }
    if sel.having.is_some() {
        return Some("Views containing HAVING are not automatically updatable.");
    }
    if sel.has_aggs {
        return Some("Views that return aggregate functions are not automatically updatable.");
    }
    if sel.has_windows {
        return Some("Views that return window functions are not automatically updatable.");
    }
    if sel.has_target_srfs {
        return Some("Views that return set-returning functions are not automatically updatable.");
    }
    let base = match sel.from.as_slice() {
        [FromItem::Range(idx)] => *idx,
        _ => return Some(NOT_SINGLE_TABLE),
    };
    let entry = match (base as usize).checked_sub(1).and_then(|k| sel.range_table.get(k)) {
        Some(entry) => entry,
        None => return Some(NOT_SINGLE_TABLE),
    };
    match &entry.kind {
        RangeKind::Relation { rel_kind, .. } => {
            if !matches!(
                rel_kind,
                RelKind::Table | RelKind::View | RelKind::PartitionedTable | RelKind::ForeignTable
            ) {
                return Some(NOT_SINGLE_TABLE);
            }
        }
        RangeKind::Subquery { .. } => {}
        _ => return Some(NOT_SINGLE_TABLE),
    }
    let has_updatable = sel.target_list.iter().any(|t| {
        !t.junk && matches!(&t.expr.kind, ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == base && c.attnum > 0)
    });
    if !has_updatable {
        return Some("Views that have no updatable columns are not automatically updatable.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::bind;
    use crate::binder::context::{BindContext, BindSession};
    use crate::parser::parse;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;

    /// Parse and bind one statement against a bootstrapped catalog with the default search path.
    fn bind_sql(sql: &str) -> Result<BoundStatement> {
        let (_kv, env) = testing::bootstrapped()?;
        let stmts = parse(sql)?;
        let raw = stmts.first().ok_or_else(|| PgError::internal("test statement missing"))?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        Ok(bind(raw, &ctx)?.statement)
    }

    fn error_state(sql: &str) -> Option<SqlState> {
        match bind_sql(sql) {
            Err(e) => Some(e.sqlstate),
            Ok(_) => None,
        }
    }

    #[test]
    fn parse_bool_accepts_pg_spellings() {
        assert_eq!(parse_pg_bool("true"), Some(true));
        assert_eq!(parse_pg_bool("TR"), Some(true));
        assert_eq!(parse_pg_bool("yes"), Some(true));
        assert_eq!(parse_pg_bool("on"), Some(true));
        assert_eq!(parse_pg_bool("0"), Some(false));
        assert_eq!(parse_pg_bool("f"), Some(false));
        assert_eq!(parse_pg_bool("off"), Some(false));
        assert_eq!(parse_pg_bool("o"), None);
        assert_eq!(parse_pg_bool("maybe"), None);
        assert_eq!(parse_pg_bool(""), None);
    }

    #[test]
    fn temporary_view_is_unsupported() {
        assert_eq!(error_state("create temp view v as select 1"), Some(SqlState::FEATURE_NOT_SUPPORTED));
    }

    #[test]
    fn or_replace_materialized_view_is_syntax_error() {
        assert_eq!(
            error_state("create or replace materialized view m as select 1"),
            Some(SqlState::SYNTAX_ERROR)
        );
    }

    #[test]
    fn refresh_of_unknown_relation_is_42p01() {
        assert_eq!(
            error_state("refresh materialized view no_such_matview"),
            Some(SqlState::UNDEFINED_TABLE)
        );
    }

    #[test]
    #[ignore = "needs SQL-16 (query binding) and SQL-44 (view deparse)"]
    fn view_parameter_is_42p02() {
        assert_eq!(error_state("create view v as select $1"), Some(SqlState::UNDEFINED_PARAMETER));
    }

    #[test]
    #[ignore = "needs SQL-16 (query binding) and SQL-44 (view deparse)"]
    fn view_column_list_renames_and_checks_count() {
        match bind_sql("create view v(x, y) as select 1, 2") {
            Ok(BoundStatement::CreateView(v)) => {
                let names: Vec<&str> = v.spec.columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["x", "y"]);
            }
            _ => panic!("expected CREATE VIEW"),
        }
        match bind_sql("create view v(x) as select 1, 2") {
            Ok(BoundStatement::CreateView(v)) => {
                let names: Vec<&str> = v.spec.columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["x", "?column?"]);
            }
            _ => panic!("expected CREATE VIEW"),
        }
        assert_eq!(
            error_state("create view v(x, y) as select 1"),
            Some(SqlState::SYNTAX_ERROR)
        );
    }

    #[test]
    #[ignore = "needs SQL-16 (query binding) and SQL-44 (view deparse)"]
    fn unknown_output_column_resolves_to_text() {
        match bind_sql("create view v as select 'a'") {
            Ok(BoundStatement::CreateView(v)) => {
                assert_eq!(v.spec.columns[0].type_id, TypeId::TEXT);
            }
            _ => panic!("expected CREATE VIEW"),
        }
    }

    #[test]
    fn materialized_view_with_no_data_is_not_populated() {
        match bind_sql("create materialized view m as select 1 with no data") {
            Ok(BoundStatement::CreateView(v)) => {
                assert!(v.spec.materialized);
                assert!(!v.spec.populated);
            }
            _ => panic!("expected CREATE MATERIALIZED VIEW"),
        }
    }
}
