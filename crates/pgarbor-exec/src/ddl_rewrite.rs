//! CREATE / ALTER / DROP POLICY and CREATE / ALTER / DROP RULE execution through the catalog writer
//! (amendment pgarbor-exec-merge-wco-rls).
use pgarbor_common::Result;
use pgarbor_sql::ir::PolicyStmt;

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::result::ExecResult;

/// One policy statement.
pub fn exec_policy(cx: &mut ExecContext, p: &PolicyStmt) -> Result<ExecResult> {
    let tag = match p {
        PolicyStmt::Create(_) => "CREATE POLICY",
        PolicyStmt::Alter { .. } | PolicyStmt::Rename { .. } => "ALTER POLICY",
        PolicyStmt::Drop { .. } => "DROP POLICY",
    };
    run_ddl(cx, |ddl| match p {
        PolicyStmt::Create(spec) => ddl.create_policy(spec),
        PolicyStmt::Alter { table, name, changes } => ddl.alter_policy(*table, name, changes),
        PolicyStmt::Rename { table, name, new_name } => ddl.rename_policy(*table, name, new_name),
        PolicyStmt::Drop { table, name, missing_ok } => ddl.drop_policy(*table, name, *missing_ok),
    })?;
    Ok(ExecResult::command(tag))
}
