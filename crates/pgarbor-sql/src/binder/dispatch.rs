//! Statement dispatch: `bind_statement` and the family routing table (SQL-04).
//!
//! ## `bind_statement(stmt, ctx)`
//! 1. `Binder::new(ctx, &stmt.source)`.
//! 2. `classify::check_supported(&stmt.parsed, first_token, Some(stmt.pos))` -> family (errors:
//!    0A000 for recognized-unsupported, 42601 for foreign syntax; normally already raised at parse
//!    time, repeated here for statements constructed by other paths such as re-binding view text).
//! 3. Route by `StmtFamily` / `Statement` variant to the owning module (table below) via
//!    [`bind_inner_statement`].
//! 4. `params::finalize(&mut binder)` -> `param_types` (42P18 if unresolved).
//! 5. Return `BindOutput { statement, param_types, notices: binder.notices }`.
//! A `Statement::Query` with no rows to return and `INTO` is `CreateTableAs` (`SELECT .. INTO t`).
//!
//! ## Routing table
//! | statement | function |
//! |---|---|
//! | `Query` (and `SELECT INTO`) | `select::bind_query_statement` |
//! | `Insert` | `insert::bind_insert` |
//! | `Update` / `Delete` | `update_delete::bind_update` / `bind_delete` |
//! | `CreateTable` (incl. `AS query`, `LIKE`, `PARTITION BY`: 0A000) | `ddl_table::bind_create_table` |
//! | `CreateIndex` | `ddl_index::bind_create_index` |
//! | `CreateView`, extra `RefreshMaterializedView` | `ddl_view::bind_create_view` / `bind_refresh_matview` |
//! | `CreateSchema`/`CreateSequence`/`CreateType`/`CreateDomain`/`CreateRole`/`CreateUser`/`CreateDatabase` | `ddl_objects::*` |
//! | `CreateFunction`/`CreateProcedure`, `Call`, extra `Do` | `ddl_function::*` |
//! | `AlterTable` | `ddl_alter::bind_alter_table`; `AlterView`/`AlterIndex` -> `bind_alter_view`/`bind_alter_index` |
//! | `AlterSchema`/`AlterFunction`/`AlterType`/`AlterRole`/`AlterUser`, extras `AlterSequence`/`AlterDomain`/`AlterDatabase`/`AlterMaterializedView` | `ddl_alter_other::*` |
//! | `Drop`/`DropFunction`/`DropDomain`/`DropProcedure`, `Truncate`, `Comment`, extra `DropOwned`/`ReassignOwned` | `ddl_drop::*` |
//! | `Grant`/`Revoke` | `ddl_grant::bind_grant` |
//! | `StartTransaction`/`Commit`/`Rollback`/`Savepoint`/`ReleaseSavepoint`, `Set`/`Reset`, `ShowVariable`, `Discard`, extra `SetConstraints` | `utility_session::*` |
//! | `Prepare`/`Execute`/`Deallocate`/`Explain`/`LISTEN`/`UNLISTEN`/`NOTIFY`/`Lock`/`Vacuum`/`Analyze`, extras `Reindex`/`Cluster`/`Checkpoint` | `utility::*` |
//! | `Copy` | `copy::bind_copy` |
//!
//! `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT` and friends are bound WITHOUT touching the catalog
//! (the session must be able to run them in a failed transaction).
//!
//! [`bind_inner_statement`] is also used by `EXPLAIN`, `PREPARE`, data-modifying CTEs and
//! `CREATE TABLE AS` for their nested statement; `top_level` is false there.
//!
//! ## Implementation notes
//! * Every `Handled` family of `classify` has an arm. A statement that reaches the wildcard arm is
//!   either not PostgreSQL (42601), recognized but unimplemented (0A000), or a Handled family the
//!   table forgot (XX000 `statement not routed: ...`). Never a silent fallback.
//! * Statements nested under `top_level == false` may not be transaction-control statements (PG's
//!   `PreparableStmt` / `ExplainableStmt` grammar has no room for them): 42601.
//! * `PREPARE` needs the raw text of the statement (`pg_prepared_statements.statement`). It is kept in
//!   `Binder::scratch` by [`bind_statement`] under [`STMT_TEXT_KEY`].

use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::{BindContext, BindOutput, Binder};
use super::{
    copy, ddl_alter, ddl_alter_other, ddl_drop, ddl_function, ddl_grant, ddl_index, ddl_objects, ddl_table,
    ddl_view, insert, params, select, update_delete, utility, utility_session,
};
use crate::classify::{self, StmtClass, StmtFamily};
use crate::diag;
use crate::extra_stmt::ExtraStatement;
use crate::ir::BoundStatement;
use crate::parser::{Parsed, RawStatement};

/// Key under which [`bind_statement`] stores the statement text in `Binder::scratch` (read by PREPARE).
const STMT_TEXT_KEY: &str = "statement text";

/// Bind one statement (module docs).
pub fn bind_statement(stmt: &RawStatement, ctx: &BindContext) -> Result<BindOutput> {
    let mut b = Binder::new(ctx, &stmt.source);
    b.identity_groups = stmt.identity_groups.iter().cloned().collect();
    classify::check_supported(&stmt.parsed, first_token(&stmt.text), Some(stmt.pos))?;
    if let Parsed::Ast(ast) = &stmt.parsed {
        if matches!(&**ast, Statement::Prepare { .. }) {
            b.scratch.insert(STMT_TEXT_KEY, stmt.text.clone());
        }
    }
    let statement = match &stmt.parsed {
        Parsed::Ast(ast) => bind_inner_statement(&mut b, &**ast, true)?,
        Parsed::Extra(extra) => bind_extra_statement(&mut b, extra)?,
    };
    let param_types = params::finalize(&mut b)?;
    Ok(BindOutput { statement, param_types, notices: b.notices })
}

/// Route an AST statement to its binder (routing table). `top_level` = it is the statement the
/// client submitted (matters for data-modifying CTE placement and transaction-control restrictions).
pub fn bind_inner_statement(b: &mut Binder, stmt: &Statement, top_level: bool) -> Result<BoundStatement> {
    if !top_level && is_transaction_control(stmt) {
        return Err(diag::syntax_error_near(&first_token_of(stmt), None));
    }
    match stmt {
        // --- queries and DML ---
        // `SELECT ... INTO` is handled inside `bind_query_statement` (select.rs, module docs).
        Statement::Query(q) => select::bind_query_statement(b, &**q),
        Statement::Insert(ins) => insert::bind_insert(b, ins),
        Statement::Update(u) => update_delete::bind_update(b, u),
        Statement::Delete(d) => update_delete::bind_delete(b, d),
        Statement::Copy { .. } => copy::bind_copy(b, stmt),
        Statement::Truncate(_) => ddl_drop::bind_truncate(b, stmt),

        // --- CREATE ---
        Statement::CreateTable(ct) => ddl_table::bind_create_table(b, ct),
        Statement::CreateIndex(ci) => ddl_index::bind_create_index(b, ci),
        Statement::CreateView(_) => ddl_view::bind_create_view(b, stmt),
        Statement::CreateSchema { .. } => ddl_objects::bind_create_schema(b, stmt),
        Statement::CreateType { .. } => ddl_objects::bind_create_type(b, stmt),
        Statement::CreateDomain(_) => ddl_objects::bind_create_domain(b, stmt),
        Statement::CreateDatabase { .. } => ddl_objects::bind_create_database(b, stmt),
        Statement::CreateFunction(_) | Statement::CreateProcedure { .. } => ddl_function::bind_create_function(b, stmt),

        // --- ALTER ---
        Statement::AlterTable(at) => ddl_alter::bind_alter_table(b, at),
        Statement::AlterIndex { .. } => ddl_alter::bind_alter_index(b, stmt),
        Statement::AlterView { .. } => ddl_alter::bind_alter_view(b, stmt),
        Statement::AlterSchema(_) => ddl_alter_other::bind_alter_schema(b, stmt),
        Statement::AlterFunction(_) => ddl_alter_other::bind_alter_function(b, stmt),
        Statement::AlterType(_) => ddl_alter_other::bind_alter_type(b, stmt),
        Statement::AlterRole { .. } | Statement::AlterUser(_) => ddl_alter_other::bind_alter_role(b, stmt),

        // --- DROP / TRUNCATE / COMMENT ---
        Statement::Drop { .. } | Statement::DropFunction(_) | Statement::DropDomain(_) | Statement::DropProcedure { .. } => {
            ddl_drop::bind_drop(b, stmt)
        }
        Statement::Comment { .. } => ddl_drop::bind_comment(b, stmt),

        // --- privileges ---
        // Without `ON <objects>` the statement is role membership (`GRANT r TO m`, `REVOKE r FROM m`):
        // PostgreSQL's grammar requires `ON` for privilege grants, so `objects: None` is unambiguous.
        Statement::Grant(g) if g.objects.is_none() => ddl_grant::bind_grant_role(b, stmt),
        Statement::Revoke(r) if r.objects.is_none() => ddl_grant::bind_grant_role(b, stmt),
        Statement::Grant(_) | Statement::Revoke(_) => ddl_grant::bind_grant(b, stmt),

        // --- transactions, session, SHOW / SET / DISCARD ---
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => utility_session::bind_transaction(b, stmt),
        Statement::Set(_) | Statement::Reset(_) => utility_session::bind_set(b, stmt),
        Statement::ShowVariable { .. } => utility_session::bind_show(b, stmt),
        Statement::Discard { .. } => utility_session::bind_discard(b, stmt),

        // --- prepared statements, EXPLAIN, LISTEN/NOTIFY, LOCK, maintenance, EXECUTE ---
        Statement::Prepare { .. } => {
            let text = b
                .scratch
                .get(STMT_TEXT_KEY)
                .cloned()
                .ok_or_else(|| PgError::internal("statement text not available for PREPARE"))?;
            utility::bind_prepare(b, stmt, &text)
        }
        Statement::Execute { .. } => utility::bind_execute(b, stmt),
        Statement::Deallocate { .. } => utility::bind_deallocate(b, stmt),
        Statement::Explain { .. } => utility::bind_explain(b, stmt),
        Statement::LISTEN { .. } | Statement::UNLISTEN { .. } | Statement::NOTIFY { .. } => utility::bind_notify(b, stmt),
        Statement::Lock(_) => utility::bind_lock(b, stmt),
        Statement::Analyze(_) | Statement::Vacuum(_) => utility::bind_maintenance(b, Some(stmt), None),

        // --- statements routed by classify but without a dedicated arm above ---
        Statement::Call(_) => ddl_function::bind_call(b, stmt),

        other => Err(unrouted(other)),
    }
}

/// Route an `ExtraStatement` (the PostgreSQL statements `sqlparser` cannot parse).
pub fn bind_extra_statement(b: &mut Binder, stmt: &ExtraStatement) -> Result<BoundStatement> {
    match stmt {
        ExtraStatement::RefreshMaterializedView { name, concurrently, with_data } => {
            ddl_view::bind_refresh_matview(b, name, *concurrently, *with_data)
        }
        ExtraStatement::Reindex { .. } | ExtraStatement::Cluster { .. } | ExtraStatement::Checkpoint => {
            utility::bind_maintenance(b, None, Some(stmt))
        }
        ExtraStatement::SetConstraints { names, deferred } => {
            utility_session::bind_set_constraints(b, names.as_deref(), *deferred)
        }
        ExtraStatement::Do { language, code } => ddl_function::bind_do(b, language.as_ref(), code),
        ExtraStatement::AlterSequence { if_exists, name, action } => {
            ddl_alter_other::bind_alter_sequence(b, *if_exists, name, action)
        }
        ExtraStatement::CreateSequence { temporary, unlogged, if_not_exists, name, options } => {
            ddl_objects::bind_create_sequence(b, *temporary, *unlogged, *if_not_exists, name, options)
        }
        ExtraStatement::CreateRole { kind, name, options } => ddl_objects::bind_create_role(b, *kind, name, options),
        ExtraStatement::AlterRole { name, options } => ddl_alter_other::bind_alter_role_options(b, name, options),
        ExtraStatement::AlterDomain { name, action } => ddl_alter_other::bind_alter_domain(b, name, action),
        ExtraStatement::AlterDatabase { name, action } => ddl_alter_other::bind_alter_database(b, name, action),
        ExtraStatement::AlterMaterializedView { if_exists, name, action } => {
            ddl_alter_other::bind_alter_matview(b, *if_exists, name, action)
        }
        ExtraStatement::DropOwned { .. } | ExtraStatement::ReassignOwned { .. } => ddl_drop::bind_owned(b, stmt),
        ExtraStatement::Unsupported { feature } => Err(diag::unsupported_construct(feature, None)),
    }
}

/// True for the transaction-control statements (BEGIN, COMMIT, ROLLBACK, SAVEPOINT, RELEASE).
fn is_transaction_control(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
    )
}

/// Error for a statement that reached the router without an arm. `classify` decides the error:
/// `Handled` families are a binder bug (XX000), the rest are the same errors `check_supported` gives.
fn unrouted(stmt: &Statement) -> PgError {
    match classify::classify(stmt) {
        StmtClass::Handled(family) => PgError::internal(format!("statement not routed: {}", family_name(family))),
        StmtClass::Unsupported(feature) => diag::unsupported_construct(feature, None),
        StmtClass::NotPostgres => diag::syntax_error_near(&first_token_of(stmt), None),
    }
}

/// Debug name of a family for the internal-error text.
fn family_name(family: StmtFamily) -> String {
    format!("{family:?}")
}

/// First whitespace-delimited token of a statement text (the token PG names in 42601).
fn first_token(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or("")
}

/// First token of a parsed statement, as written in its canonical rendering.
fn first_token_of(stmt: &Statement) -> String {
    let rendered = stmt.to_string();
    first_token(&rendered).to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_common::SqlState;
    use pgarbor_types::context::IoConfig;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    use super::{bind_extra_statement, bind_inner_statement};
    use crate::binder::context::{BindContext, BindSession, Binder};
    use crate::binder::bind;
    use crate::extra_stmt::ExtraStatement;
    use crate::parser::{Parsed, RawStatement};
    use crate::source::SourceMap;

    fn parse_one_ast(sql: &str) -> sqlparser::ast::Statement {
        Parser::parse_sql(&PostgreSqlDialect {}, sql)
            .expect("test SQL parses with sqlparser")
            .into_iter()
            .next()
            .expect("test SQL has one statement")
    }

    fn raw(sql: &str) -> RawStatement {
        RawStatement {
            parsed: Parsed::Ast(Box::new(parse_one_ast(sql))),
            identity_groups: Vec::new(),
            text: sql.to_string(),
            byte_start: 0,
            byte_end: sql.len(),
            pos: 1,
            index: 0,
            source: Arc::new(SourceMap::new(sql)),
        }
    }

    fn ctx(env: &CatalogEnv) -> BindContext<'_> {
        BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        }
    }

    #[test]
    fn unsupported_family_is_0a000() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let err = bind(&raw("DROP TRIGGER t ON x"), &c).expect_err("triggers are not supported");
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn unrouted_unsupported_is_0a000_not_xx000() {
        // Reaching the router directly (not via check_supported): the classify error is raised,
        // never an internal error. Handled families without an arm would be XX000 instead.
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let src = SourceMap::new("DROP TRIGGER t ON x");
        let mut b = Binder::new(&c, &src);
        let stmt = parse_one_ast("DROP TRIGGER t ON x");
        let err = bind_inner_statement(&mut b, &stmt, true).expect_err("triggers are not supported");
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn unsupported_extra_is_0a000() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let src = SourceMap::new("");
        let mut b = Binder::new(&c, &src);
        let extra = ExtraStatement::Unsupported { feature: "CREATE RULE".to_string() };
        let err = bind_extra_statement(&mut b, &extra).expect_err("rules are not supported");
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn nested_transaction_control_is_42601() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let src = SourceMap::new("BEGIN");
        let mut b = Binder::new(&c, &src);
        let stmt = parse_one_ast("BEGIN");
        let err = bind_inner_statement(&mut b, &stmt, false).expect_err("BEGIN cannot be nested");
        assert_eq!(err.sqlstate, SqlState::SYNTAX_ERROR);
    }

    #[test]
    #[ignore = "needs SQL-40 and SQL-06"]
    fn begin_binds_without_catalog() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let out = bind(&raw("BEGIN"), &c).expect("BEGIN binds");
        assert_eq!(out.statement.tag(), "BEGIN");
        assert!(out.param_types.is_empty());
    }

    #[test]
    #[ignore = "needs SQL-16 and SQL-06"]
    fn select_one_has_no_params() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let out = bind(&raw("SELECT 1"), &c).expect("SELECT 1 binds");
        assert!(out.param_types.is_empty());
    }

    #[test]
    #[ignore = "needs SQL-16 and SQL-06"]
    fn select_cast_param_is_int4() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let out = bind(&raw("SELECT $1::int"), &c).expect("SELECT $1::int binds");
        assert_eq!(out.param_types.len(), 1);
        assert_eq!(out.param_types[0].oid, pgarbor_types::oids::INT4);
    }

    #[test]
    #[ignore = "needs SQL-40 and SQL-06"]
    fn show_search_path_binds() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap catalog");
        let c = ctx(&env);
        let out = bind(&raw("SHOW search_path"), &c).expect("SHOW binds");
        assert_eq!(out.statement.tag(), "SHOW");
    }
}
