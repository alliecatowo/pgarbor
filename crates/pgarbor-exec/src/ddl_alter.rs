//! `AlterAction` execution: every ALTER other than `ALTER TABLE` commands (lease EXE-36).
//! `exec_alter(cx, action)` = ONE `run_ddl` calling the `CatalogTxn` method named in each
//! `pgarbor_sql::ir::AlterAction` variant's doc, then follow-ups, result tag `action.tag()`.
//!
//! Notes:
//! * Renames and schema moves that change stored expression text pass `&SqlExprRewriter`
//!   (`pgarbor_sql::rewrite::SqlExprRewriter`): `rename_relation`, `set_relation_schema`, `rename_type`,
//!   `set_type_schema`, `rename_function`, `set_function_schema`.
//! * `RenameRelation` with `keyword == Index` uses `rename_index(index, new_name)`; other keywords
//!   `rename_relation(rel, name, rewriter)`.
//! * `Sequence{sequence, options, owned_by}`: `alter_sequence(seq, &options, owned_by)`; its outcome carries
//!   `RestartSequence` when RESTART was given (executed by `run_followups`).
//! * `DomainNotNull{domain, set}` returns the `(table, attnum)` columns using the domain: when
//!   `set == true` scan each table and fail with 23502 `column "a" of table "t" contains null values`
//!   (oracle-verified wording: OF TABLE, not "of relation") if a NULL exists. `AddDomainConstraint` returns `(oid, columns)`; unless `not_valid` validate the new CHECK
//!   against existing values of those columns (23514 `column "a" of table "t" contains values that
//!   violate the new constraint`, oracle-verified). `ValidateDomainConstraint` likewise.
//! * `AddEnumValue`, `RenameEnumValue`, `AlterTypeOwner`, `FunctionAttrs`, `Role`, `RenameRole`, `RoleConfig`,
//!   `RenameDatabase`, `DatabaseOwner`, `DatabaseConfig`, `DatabaseOptions`, `Owner`, `SchemaOwner`,
//!   `RenameSchema`, `IndexOptions`, `ViewOptions`, `DomainDefault`, `RelationOwner`: direct calls.
//! `ALTER TYPE ... ADD VALUE` inside a transaction block is allowed in PG >= 12 (the new label cannot be
//! used until commit; not enforced in v1).
//!
//! Domain validation (PG 18.6 `commands/typecmds.c`, `validateDomainNotNullConstraint` and
//! `validateDomainCheckConstraint`): NULL column values are skipped by CHECK validation (a NULL passes a
//! CHECK), and a NOT NULL validation fails on the first NULL. The scans read the transaction view through
//! `rowstore::HeapCursor` and evaluate the CHECK expression with `eval::eval` on the same executor.
//! The CHECK text refers to the domain value as `VALUE`; it is bound once per statement over a synthetic
//! one-column scope named `value` of the domain's base type (`tabledesc::columns_to_input_refs` turns it
//! into `InputRef(0)`), mirroring the binding recipe of `eval_cast` (module docs there).
use pgarbor_catalog::model::types::TypeDef;
use pgarbor_catalog::{CatalogTxn, DropBehavior};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::binder::expr::bind_expr;
use pgarbor_sql::binder::stored::table_expr_scope;
use pgarbor_sql::binder::{BindContext, BindSession, Binder, ExprCtx};
use pgarbor_sql::ir::{AlterAction, AlterRelKeyword, BoundExpr};
use pgarbor_sql::rewrite::SqlExprRewriter;
use pgarbor_sql::{parse_expr_text, SourceMap};
use pgarbor_types::{Datum, TypeId};

use crate::context::{EngineServices, ExecContext};
use crate::ddl_driver::run_ddl;
use crate::ddl_followup::run_followups;
use crate::eval;
use crate::result::ExecResult;
use crate::rowstore::HeapCursor;
use crate::runtime::{Executor, Runtime};
use crate::tabledesc::{columns_to_input_refs, table_desc, TableDesc};

/// Execute one non-table ALTER statement (module docs).
pub fn exec_alter(cx: &mut ExecContext, action: &AlterAction) -> Result<ExecResult> {
    let rw = SqlExprRewriter;
    match action {
        AlterAction::RenameRelation { rel, new_name, keyword } => {
            let rel = *rel;
            if *keyword == AlterRelKeyword::Index {
                run_plain(cx, |d| d.rename_index(rel, new_name))?;
            } else {
                run_plain(cx, |d| d.rename_relation(rel, new_name, &rw))?;
            }
        }
        AlterAction::SetRelationSchema { rel, new_schema, keyword: _ } => {
            let (rel, new_schema) = (*rel, *new_schema);
            run_plain(cx, |d| d.set_relation_schema(rel, new_schema, &rw))?;
        }
        AlterAction::RelationOwner { rel, new_owner, keyword: _ } => {
            let (rel, new_owner) = (*rel, *new_owner);
            run_plain(cx, |d| d.alter_relation_owner(rel, new_owner))?;
        }
        AlterAction::IndexOptions { index, set, reset } => {
            let index = *index;
            run_plain(cx, |d| d.alter_index_options(index, set, reset))?;
        }
        AlterAction::ViewOptions { view, set, reset } => {
            let view = *view;
            run_plain(cx, |d| d.set_view_options(view, set, reset))?;
        }
        AlterAction::Sequence { sequence, options, owned_by } => {
            let sequence = *sequence;
            let owned_by = *owned_by;
            let ((), outcome) = run_ddl(cx, |d| d.alter_sequence(sequence, options, owned_by))?;
            run_followups(cx, &outcome.follow_ups, None, &[])?;
        }
        AlterAction::RenameSchema { old, new } => {
            run_plain(cx, |d| d.rename_schema(old, new))?;
        }
        AlterAction::SchemaOwner { name, new_owner } => {
            let new_owner = *new_owner;
            run_plain(cx, |d| d.alter_schema_owner(name, new_owner))?;
        }
        AlterAction::RenameType { ty, new_name } => {
            let ty = *ty;
            run_plain(cx, |d| d.rename_type(ty, new_name, &rw))?;
        }
        AlterAction::SetTypeSchema { ty, new_schema } => {
            let (ty, new_schema) = (*ty, *new_schema);
            run_plain(cx, |d| d.set_type_schema(ty, new_schema, &rw))?;
        }
        AlterAction::TypeOwner { ty, new_owner } => {
            let (ty, new_owner) = (*ty, *new_owner);
            run_plain(cx, |d| d.alter_type_owner(ty, new_owner))?;
        }
        AlterAction::AddEnumValue { ty, label, position, if_not_exists } => {
            let ty = *ty;
            let if_not_exists = *if_not_exists;
            run_plain(cx, |d| d.add_enum_value(ty, label, position, if_not_exists).map(|_added| ()))?;
        }
        AlterAction::RenameEnumValue { ty, old, new } => {
            let ty = *ty;
            run_plain(cx, |d| d.rename_enum_value(ty, old, new))?;
        }
        AlterAction::DomainDefault { domain, default } => {
            let domain = *domain;
            let default = default.clone();
            run_plain(cx, |d| d.alter_domain_default(domain, default))?;
        }
        AlterAction::DomainNotNull { domain, set } => {
            let domain = *domain;
            let set = *set;
            let (cols, outcome) = run_ddl(cx, |d| d.alter_domain_not_null(domain, set))?;
            if set {
                validate_domain_not_null(cx, &cols)?;
            }
            run_followups(cx, &outcome.follow_ups, None, &[])?;
        }
        AlterAction::AddDomainConstraint { domain, spec } => {
            let domain = *domain;
            let ((con, cols), outcome) = run_ddl(cx, |d| d.add_domain_constraint(domain, spec))?;
            if !cols.is_empty() {
                let base = domain_base(cx, domain)?;
                let sql = check_sql_of(cx, con)
                    .ok_or_else(|| PgError::internal(format!("domain constraint {con} has no check expression")))?;
                validate_domain_check(cx, &cols, base, &sql)?;
            }
            run_followups(cx, &outcome.follow_ups, None, &[])?;
        }
        AlterAction::DropDomainConstraint { domain, name, missing_ok, cascade } => {
            let domain = *domain;
            let missing_ok = *missing_ok;
            let behavior = if *cascade { DropBehavior::Cascade } else { DropBehavior::Restrict };
            run_plain(cx, |d| d.drop_domain_constraint(domain, name, missing_ok, behavior).map(|_dropped| ()))?;
        }
        AlterAction::RenameDomainConstraint { domain, old, new } => {
            let domain = *domain;
            run_plain(cx, |d| d.rename_domain_constraint(domain, old, new))?;
        }
        AlterAction::ValidateDomainConstraint { domain, name } => {
            let domain = *domain;
            // The CHECK text is read before the catalog call; validation does not change it.
            let sql = check_sql_by_name(cx, domain, name);
            let (cols, outcome) = run_ddl(cx, |d| d.validate_domain_constraint(domain, name))?;
            if !cols.is_empty() {
                let base = domain_base(cx, domain)?;
                let sql = sql.ok_or_else(|| PgError::internal(format!("domain constraint \"{name}\" has no check expression")))?;
                validate_domain_check(cx, &cols, base, &sql)?;
            }
            run_followups(cx, &outcome.follow_ups, None, &[])?;
        }
        AlterAction::FunctionAttrs { func, changes } => {
            let func = *func;
            for item in &changes.set_config {
                if let Some((guc, value)) = item.split_once('=') {
                    cx.se.guc.check_setting(guc, value)?;
                }
            }
            run_plain(cx, |d| d.alter_function_attrs(func, changes))?;
        }
        AlterAction::RenameFunction { func, new_name } => {
            let func = *func;
            run_plain(cx, |d| d.rename_function(func, new_name, &rw))?;
        }
        AlterAction::SetFunctionSchema { func, new_schema } => {
            let (func, new_schema) = (*func, *new_schema);
            run_plain(cx, |d| d.set_function_schema(func, new_schema, &rw))?;
        }
        AlterAction::FunctionOwner { func, new_owner } => {
            let (func, new_owner) = (*func, *new_owner);
            run_plain(cx, |d| d.alter_function_owner(func, new_owner))?;
        }
        AlterAction::Role { role, changes } => {
            let role = *role;
            run_plain(cx, |d| d.alter_role(role, changes))?;
        }
        AlterAction::RenameRole { role, new_name } => {
            let role = *role;
            run_plain(cx, |d| d.rename_role(role, new_name))?;
        }
        AlterAction::RoleConfig { role, guc, value } => {
            let role = *role;
            if let Some(v) = value {
                cx.se.guc.check_setting(guc, v)?;
            }
            run_plain(cx, |d| d.set_role_config(role, guc, value.as_deref()))?;
        }
        AlterAction::RenameDatabase { name, new_name } => {
            run_plain(cx, |d| d.rename_database(name, new_name))?;
        }
        AlterAction::DatabaseOwner { name, new_owner } => {
            let new_owner = *new_owner;
            run_plain(cx, |d| d.alter_database_owner(name, new_owner))?;
        }
        AlterAction::DatabaseConfig { name, guc, value } => {
            if let Some(v) = value {
                cx.se.guc.check_setting(guc, v)?;
            }
            run_plain(cx, |d| d.set_database_config(name, guc, value.as_deref()))?;
        }
        AlterAction::DatabaseOptions { name, allow_conn, conn_limit, is_template } => {
            let (allow_conn, conn_limit, is_template) = (*allow_conn, *conn_limit, *is_template);
            run_plain(cx, |d| d.alter_database_options(name, allow_conn, conn_limit, is_template))?;
        }
        AlterAction::Owner { addr, new_owner } => {
            let new_owner = *new_owner;
            run_plain(cx, |d| d.alter_owner(addr, new_owner))?;
        }
    }
    Ok(ExecResult::command(action.tag()))
}

/// One catalog statement that returns nothing, followed by its data-level follow-ups.
fn run_plain<F>(cx: &mut ExecContext, f: F) -> Result<()>
where
    F: FnOnce(&mut CatalogTxn) -> Result<()>,
{
    let ((), outcome) = run_ddl(cx, f)?;
    run_followups(cx, &outcome.follow_ups, None, &[])
}

// ---------------------------------------------------------------------------------------------
// Errors (oracle-verified wording; the shared `error` constructors use "of relation" instead)
// ---------------------------------------------------------------------------------------------

/// 23502 raised by `ALTER DOMAIN ... SET NOT NULL` over an existing NULL.
fn not_null_error(schema: &str, table: &str, column: &str) -> PgError {
    PgError::new(SqlState::NOT_NULL_VIOLATION, format!("column \"{column}\" of table \"{table}\" contains null values"))
        .with_table(schema, table)
        .with_column(column)
}

/// 23514 raised by `ALTER DOMAIN ... ADD CONSTRAINT` / `VALIDATE CONSTRAINT` over a violating value.
fn domain_check_error(schema: &str, table: &str, column: &str) -> PgError {
    PgError::new(
        SqlState::CHECK_VIOLATION,
        format!("column \"{column}\" of table \"{table}\" contains values that violate the new constraint"),
    )
    .with_table(schema, table)
    .with_column(column)
}

// ---------------------------------------------------------------------------------------------
// Catalog lookups
// ---------------------------------------------------------------------------------------------

/// Stored CHECK text of constraint `con` (after the catalog call, so the new constraint is visible).
fn check_sql_of(cx: &ExecContext, con: Oid) -> Option<String> {
    cx.se.env.snapshot().constraint(con).and_then(|c| c.check.as_ref().map(|s| s.sql.clone()))
}

/// Stored CHECK text of the domain constraint named `name` on `domain`.
fn check_sql_by_name(cx: &ExecContext, domain: Oid, name: &str) -> Option<String> {
    cx.se
        .env
        .snapshot()
        .domain_constraints(domain)
        .into_iter()
        .find(|c| c.name.as_str() == name)
        .and_then(|c| c.check.as_ref().map(|s| s.sql.clone()))
}

/// Base type of domain `domain` (the type of `VALUE`).
fn domain_base(cx: &ExecContext, domain: Oid) -> Result<TypeId> {
    match cx.se.env.snapshot().type_obj(domain) {
        Some(t) => match &t.def {
            TypeDef::Domain { base, .. } => Ok(*base),
            _ => Err(PgError::internal(format!("type {domain} is not a domain"))),
        },
        None => Err(PgError::internal(format!("cache lookup failed for type {domain}"))),
    }
}

/// Column name of `attnum` in `td`.
fn column_name(td: &TableDesc, attnum: i16) -> Result<String> {
    match td.col(attnum) {
        Some(c) => Ok(c.name.clone()),
        None => Err(PgError::internal(format!("attribute {attnum} of relation {} not found", td.oid))),
    }
}

/// Index of attnum `attnum` in a completed row (attribute slots are 1-based).
fn slot_index(attnum: i16) -> Result<usize> {
    usize::try_from(i32::from(attnum) - 1).map_err(|_| PgError::internal(format!("invalid attribute number {attnum}")))
}

/// Whether the value in `slot` is NULL. A slot beyond the row is NULL (`TableDesc::complete_row` pads rows).
fn is_null_at(row: &[Datum], slot: usize) -> bool {
    match row.get(slot) {
        Some(d) => d.is_null(),
        None => true,
    }
}

/// Bind the CHECK text of a domain whose value has type `value_type` (module docs). The result is
/// executable over the one-slot row `[value]`.
fn bind_domain_check(cx: &ExecContext, value_type: TypeId, sql: &str) -> Result<BoundExpr> {
    let bctx = BindContext {
        env: &cx.se.env,
        session: BindSession { database: cx.se.services.database_name() },
        io_config: cx.se.io.clone(),
        statement_timestamp: cx.se.statement_ts,
        declared_params: Vec::new(),
    };
    let src = SourceMap::new(sql);
    let mut b = Binder::new(&bctx, &src);
    table_expr_scope(&mut b, "domain", &[("value".to_string(), value_type)])?;
    let parsed = parse_expr_text(sql)?;
    let bound = bind_expr(&mut b, &parsed, ExprCtx::check())?;
    columns_to_input_refs(bound)
}

// ---------------------------------------------------------------------------------------------
// Validation scans (PG `validateDomainNotNullConstraint` / `validateDomainCheckConstraint`)
// ---------------------------------------------------------------------------------------------

/// SET NOT NULL over existing rows: the first NULL in any listed column fails with 23502.
fn validate_domain_not_null(cx: &mut ExecContext, cols: &[(Oid, i16)]) -> Result<()> {
    for &(table, attnum) in cols {
        let td = table_desc(cx, table)?;
        let column = column_name(&td, attnum)?;
        let slot = slot_index(attnum)?;
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut *cx);
        let mut cursor = HeapCursor::new(table);
        while let Some((_rowid, row)) = cursor.next(&mut ex, &td, None)? {
            if is_null_at(&row, slot) {
                return Err(not_null_error(&td.schema, &td.name, &column));
            }
        }
    }
    Ok(())
}

/// Validate a domain CHECK over existing values: a NULL passes, a FALSE fails with 23514, TRUE passes.
fn validate_domain_check(cx: &mut ExecContext, cols: &[(Oid, i16)], value_type: TypeId, sql: &str) -> Result<()> {
    if cols.is_empty() {
        return Ok(());
    }
    let check = bind_domain_check(cx, value_type, sql)?;
    for &(table, attnum) in cols {
        let td = table_desc(cx, table)?;
        let column = column_name(&td, attnum)?;
        let slot = slot_index(attnum)?;
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut *cx);
        let mut cursor = HeapCursor::new(table);
        while let Some((_rowid, row)) = cursor.next(&mut ex, &td, None)? {
            if is_null_at(&row, slot) {
                continue;
            }
            let value = row[slot].clone();
            let verdict = eval::eval(&mut ex, &check, &[value])?;
            if matches!(verdict, Datum::Bool(false)) {
                return Err(domain_check_error(&td.schema, &td.name, &column));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_index_tag_is_alter_index() {
        let a = AlterAction::RenameRelation {
            rel: 16400,
            new_name: "i2".to_string(),
            keyword: AlterRelKeyword::Index,
        };
        assert_eq!(a.tag(), "ALTER INDEX");
        let s = AlterAction::RenameSchema { old: "a".to_string(), new: "b".to_string() };
        assert_eq!(s.tag(), "ALTER SCHEMA");
    }

    #[test]
    fn domain_error_messages_match_oracle() {
        let n = not_null_error("public", "t", "a");
        assert_eq!(n.sqlstate, SqlState::NOT_NULL_VIOLATION);
        assert_eq!(n.message, "column \"a\" of table \"t\" contains null values");
        assert_eq!(n.table.as_deref(), Some("t"));
        assert_eq!(n.column.as_deref(), Some("a"));

        let c = domain_check_error("public", "t", "a");
        assert_eq!(c.sqlstate, SqlState::CHECK_VIOLATION);
        assert_eq!(c.message, "column \"a\" of table \"t\" contains values that violate the new constraint");
    }

    #[test]
    fn attnum_to_slot_and_null_padding() {
        assert!(slot_index(0).is_err());
        assert_eq!(slot_index(1).ok(), Some(0));
        assert_eq!(slot_index(3).ok(), Some(2));
        let row = vec![Datum::Int4(1), Datum::Null];
        assert!(!is_null_at(&row, 0));
        assert!(is_null_at(&row, 1));
        assert!(is_null_at(&row, 5));
    }
}
