//! CREATE / ALTER / DROP POLICY binding (amendment pgarbor-sql-rewrite-rls-merge). PG reference:
//! `commands/policy.c` (`CreatePolicy`, `AlterPolicy`, `rename_policy`, `RemovePolicyById`).
use pgarbor_catalog::ddl::policy::{PolicyChanges, PolicySpec};
use pgarbor_catalog::model::RelKind;
use pgarbor_catalog::StoredExpr;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use sqlparser::ast::{AlterPolicy, AlterPolicyOperation, CreatePolicy, CreatePolicyCommand, CreatePolicyType, DropPolicy, Expr, ObjectName, Owner};

use super::context::{Binder, BoundArg, ExprCtx};
use super::stored::{bind_relation_expr, store_expr};
use crate::diag::ExprClause;
use crate::ir::{BoundStatement, PolicyStmt};

fn policy_ctx() -> ExprCtx {
    ExprCtx {
        clause: ExprClause::PolicyExpression,
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: true,
        columns_ok: true,
    }
}

/// `polcmd` of a `FOR ...` clause (`ALL` = `*`).
fn command_char(cmd: Option<&CreatePolicyCommand>) -> u8 {
    match cmd {
        None | Some(CreatePolicyCommand::All) => b'*',
        Some(CreatePolicyCommand::Select) => b'r',
        Some(CreatePolicyCommand::Insert) => b'a',
        Some(CreatePolicyCommand::Update) => b'w',
        Some(CreatePolicyCommand::Delete) => b'd',
    }
}

/// PG `policy_role_list_to_array`: no list = PUBLIC; PUBLIC alone wins over other roles (WARNING).
fn role_list(b: &mut Binder, roles: Option<&Vec<Owner>>) -> Result<Vec<Oid>> {
    let Some(list) = roles else { return Ok(vec![0]) };
    let mut out: Vec<Oid> = Vec::with_capacity(list.len());
    for r in list {
        match r {
            Owner::Ident(id) if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("public") => {
                if list.len() != 1 {
                    b.notices.push(
                        PgError::new(SqlState(*b"22023"), "ignoring specified roles other than PUBLIC")
                            .with_severity(pgarbor_common::Severity::Warning)
                            .with_hint("All roles are members of the PUBLIC role."),
                    );
                }
                return Ok(vec![0]);
            }
            Owner::Ident(id) => {
                let name = b.ident(id);
                out.push(b.env.role_oid(&name)?);
            }
            Owner::CurrentRole | Owner::CurrentUser => out.push(b.env.current_user()),
            Owner::SessionUser => out.push(b.env.session_user()),
        }
    }
    Ok(out)
}

/// The policy's table (PG `RangeVarGetRelidExtended` with `RangeVarCallbackForPolicy` for CREATE/ALTER).
fn policy_table(b: &mut Binder, name: &ObjectName, missing_ok: bool) -> Result<Option<Oid>> {
    let qn = b.qual(name)?;
    let env = b.env;
    if let Some(schema) = qn.schema.as_deref() {
        if env.namespace_oid(schema).is_err() {
            if missing_ok {
                b.notices.push(PgError::notice(format!("schema \"{schema}\" does not exist, skipping")));
                return Ok(None);
            }
            return Err(PgError::new(SqlState(*b"3F000"), format!("schema \"{schema}\" does not exist")));
        }
    }
    match env.resolve_relation(qn.schema.as_deref(), &qn.name) {
        Ok(rel) => Ok(Some(rel.oid)),
        Err(e) if missing_ok && (e.sqlstate == SqlState::UNDEFINED_TABLE || e.sqlstate == SqlState(*b"3F000")) => {
            let msg = if e.sqlstate == SqlState::UNDEFINED_TABLE {
                format!("relation \"{}\" does not exist, skipping", qn.name)
            } else {
                format!("schema \"{}\" does not exist, skipping", qn.schema.clone().unwrap_or_default())
            };
            b.notices.push(PgError::notice(msg));
            Ok(None)
        }
        Err(mut e) => {
            e.position = None;
            Err(e)
        }
    }
}

/// Bind one USING / WITH CHECK expression over the table and store it in canonical form.
fn bind_policy_expr(b: &mut Binder, table: Oid, e: &Expr) -> Result<StoredExpr> {
    // PG binds policy expressions with a parse state that has no source text: errors carry no position.
    let strip = |mut err: PgError| {
        err.position = None;
        err
    };
    let bound = bind_relation_expr(b, e, table, policy_ctx()).map_err(strip)?;
    let bound = if bound.ty.oid == pgarbor_types::oids::BOOL {
        bound
    } else {
        super::coerce::coerce_to_boolean(b, &BoundArg::new(bound, None), "POLICY").map_err(strip)?
    };
    let env = b.env;
    let rel = env.snapshot().relation(table).ok_or_else(|| PgError::internal("policy table vanished"))?;
    let names: Vec<(i16, String)> = rel.columns.iter().map(|c| (c.attnum, c.name.clone())).collect();
    let namer = move |a: i16| names.iter().find(|(n, _)| *n == a).map(|(_, s)| s.clone());
    store_expr(b, &bound, Some(table), &namer)
}

/// CREATE POLICY.
pub fn bind_create_policy(b: &mut Binder, p: &CreatePolicy) -> Result<BoundStatement> {
    let cmd = command_char(p.command.as_ref());
    if matches!(cmd, b'r' | b'd') && p.with_check.is_some() {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "WITH CHECK cannot be applied to SELECT or DELETE"));
    }
    if cmd == b'a' && p.using.is_some() {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "only WITH CHECK expression allowed for INSERT"));
    }
    let roles = role_list(b, p.to.as_ref())?;
    let table = match policy_table(b, &p.table_name, false)? {
        Some(t) => t,
        None => return Err(PgError::internal("policy table missing")),
    };
    check_policy_table(b, table)?;
    let name = b.ident(&p.name);
    let using = match &p.using {
        Some(e) => Some(bind_policy_expr(b, table, e)?),
        None => None,
    };
    let with_check = match &p.with_check {
        Some(e) => Some(bind_policy_expr(b, table, e)?),
        None => None,
    };
    let permissive = !matches!(p.policy_type, Some(CreatePolicyType::Restrictive));
    Ok(BoundStatement::Policy(Box::new(PolicyStmt::Create(PolicySpec {
        table,
        name,
        cmd,
        permissive,
        roles,
        using,
        with_check,
    }))))
}

/// PG `RangeVarCallbackForPolicy` checks that do not need the catalog writer.
fn check_policy_table(b: &Binder, table: Oid) -> Result<()> {
    let env = b.env;
    let rel = env.snapshot().relation(table).ok_or_else(|| PgError::internal("policy table vanished"))?;
    if rel.is_virtual() {
        return Err(PgError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied: \"{}\" is a system catalog", rel.name),
        ));
    }
    if !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable) {
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", rel.name)));
    }
    Ok(())
}

/// ALTER POLICY.
pub fn bind_alter_policy(b: &mut Binder, p: &AlterPolicy) -> Result<BoundStatement> {
    match &p.operation {
        AlterPolicyOperation::Rename { new_name } => {
            let table = policy_table(b, &p.table_name, false)?.ok_or_else(|| PgError::internal("policy table missing"))?;
            check_policy_table(b, table)?;
            let name = b.ident(&p.name);
            let new_name = b.ident(new_name);
            Ok(BoundStatement::Policy(Box::new(PolicyStmt::Rename { table, name, new_name })))
        }
        AlterPolicyOperation::Apply { to, using, with_check } => {
            let roles = match to {
                Some(list) => Some(role_list(b, Some(list))?),
                None => None,
            };
            let table = policy_table(b, &p.table_name, false)?.ok_or_else(|| PgError::internal("policy table missing"))?;
            check_policy_table(b, table)?;
            let name = b.ident(&p.name);
            let using = match using {
                Some(e) => Some(bind_policy_expr(b, table, e)?),
                None => None,
            };
            let with_check = match with_check {
                Some(e) => Some(bind_policy_expr(b, table, e)?),
                None => None,
            };
            Ok(BoundStatement::Policy(Box::new(PolicyStmt::Alter {
                table,
                name,
                changes: PolicyChanges { roles, using, with_check },
            })))
        }
    }
}

/// DROP POLICY [IF EXISTS] name ON table [CASCADE | RESTRICT].
pub fn bind_drop_policy(b: &mut Binder, p: &DropPolicy) -> Result<BoundStatement> {
    let name = b.ident(&p.name);
    match policy_table(b, &p.table_name, p.if_exists)? {
        Some(table) => Ok(BoundStatement::Policy(Box::new(PolicyStmt::Drop { table, name, missing_ok: p.if_exists }))),
        None => Ok(BoundStatement::Noop { tag: "DROP POLICY".into(), notices: std::mem::take(&mut b.notices) }),
    }
}
