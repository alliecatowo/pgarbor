//! Statement execution entry points (ARCHITECTURE §10). Fully implemented, frozen (v1).
//!
//! ```text
//! execute(stmt, plan, cx)            one bound statement -> ExecResult
//! execute_with_channel(.., chan)     same, for COPY (needs a CopyChannel)
//! execute_plan(plan, cx)             a planned query/DML run to completion
//! open_plan (build.rs)               a resumable PlanStream (portals)
//! ```
//! ## Which statements the executor runs
//! * plannable (`BoundStatement::is_plannable()`): `Query`, `Insert`, `Update`, `Delete` need `plan` (the
//!   output of `pgarbor_plan::plan`); `CreateTableAs` needs the plan too.
//! * DDL: `Create*`, `AlterTable`, `Alter`, `Drop*`, `Truncate`, `Comment`, `Grant*`, `Revoke*`,
//!   `ReassignOwned`, `DropOwned`, `RefreshMatView`.
//! * utility: `Lock`, `Do`, `Call`, `Maintenance`, `Explain`, `Copy`, `Empty`, `Noop`.
//! * `Copy` needs a [`CopyChannel`] for STDIN/STDOUT endpoints ([`NoCopyChannel`] raises 0A000).
//! * SESSION statements are NOT executed here and return XX000 `statement {tag} must be executed by the
//!   session layer`: `Transaction`, `Set`, `SetRole`, `SetSessionAuthorization`, `Reset`, `Show`, `Prepare`,
//!   `Execute`, `Deallocate`, `Discard`, `Listen`, `Unlisten`, `Notify`.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{Plan, PlanKind};
use pgarbor_sql::ir::BoundStatement;

use crate::build::open_plan;
use crate::context::ExecContext;
use crate::copy::CopyChannel;
use crate::result::ExecResult;
use crate::{ctas, ddl_alter, ddl_drop, ddl_objects, ddl_table, explain_exec, utility};

/// Run a planned statement to completion and shape the result: `Rows` for queries and DML with
/// RETURNING (tag from the stream), `Command` for DML without RETURNING.
pub fn execute_plan(plan: &Plan, cx: &mut ExecContext) -> Result<ExecResult> {
    let mut stream = open_plan(plan, cx)?;
    let rows = stream.fetch(cx, None)?;
    let tag = stream.command_tag();
    let has_result = matches!(plan.kind, PlanKind::Select) || !plan.output.is_empty();
    if has_result {
        Ok(ExecResult::Rows { desc: plan.output.clone(), rows, tag })
    } else {
        Ok(ExecResult::Command { tag })
    }
}

/// A [`CopyChannel`] for sessions without a client COPY endpoint: any attempt to use it raises 0A000
/// (`COPY ... STDIN/STDOUT` needs `Session::set_copy_channel`; COPY to/from a server file never
/// touches the channel).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCopyChannel;

fn no_channel() -> PgError {
    PgError::new(
        pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED,
        "COPY FROM STDIN / TO STDOUT requires a COPY channel in this session",
    )
    .with_hint("Install one with Session::set_copy_channel, or use the wire protocol.")
}

impl CopyChannel for NoCopyChannel {
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Err(no_channel())
    }
    fn write_chunk(&mut self, _data: &[u8]) -> Result<()> {
        Err(no_channel())
    }
}

/// Execute one bound statement (module docs). `COPY ... STDIN/STDOUT` fails with 0A000 because there is
/// no channel; use [`execute_with_channel`].
pub fn execute(stmt: &BoundStatement, plan: Option<&Plan>, cx: &mut ExecContext) -> Result<ExecResult> {
    let mut chan = NoCopyChannel;
    execute_with_channel(stmt, plan, cx, &mut chan)
}

fn need_plan<'p>(plan: Option<&'p Plan>, what: &str) -> Result<&'p Plan> {
    plan.ok_or_else(|| PgError::internal(format!("{what} requires a plan")))
}

/// [`execute`] with a COPY channel.
pub fn execute_with_channel(
    stmt: &BoundStatement,
    plan: Option<&Plan>,
    cx: &mut ExecContext,
    chan: &mut dyn CopyChannel,
) -> Result<ExecResult> {
    use BoundStatement as S;
    match stmt {
        S::Empty => Ok(ExecResult::Empty),
        S::Noop { tag, notices } => {
            for n in notices {
                cx.notice(n.clone());
            }
            Ok(ExecResult::command(tag.clone()))
        }
        S::Query(_) | S::Insert(_) | S::Update(_) | S::Delete(_) | S::Merge(_) => execute_plan(need_plan(plan, &stmt.tag())?, cx),
        S::CreateTableAs(b) => ctas::exec_create_table_as(cx, b, plan),
        S::Truncate(t) => ddl_drop::exec_truncate(cx, t),
        S::CreateSchema { name, owner, if_not_exists } => ddl_objects::exec_create_schema(cx, name, *owner, *if_not_exists),
        S::CreateTable(spec) => ddl_table::exec_create_table(cx, spec),
        S::CreateIndex(spec) => ddl_objects::exec_create_index(cx, spec),
        S::CreateView(v) => ddl_objects::exec_create_view(cx, v),
        S::CreateSequence(spec) => ddl_objects::exec_create_sequence(cx, spec),
        S::CreateEnum(spec) => ddl_objects::exec_create_enum(cx, spec),
        S::CreateDomain(spec) => ddl_objects::exec_create_domain(cx, spec),
        S::CreateComposite(spec) => ddl_objects::exec_create_composite(cx, spec),
        S::CreateFunction(f) => ddl_objects::exec_create_function(cx, f),
        S::CreateRole(spec) => ddl_objects::exec_create_role(cx, spec),
        S::CreateDatabase(d) => ddl_objects::exec_create_database(cx, d),
        S::AlterTable(a) => ddl_table::exec_alter_table(cx, a),
        S::Alter(a) => ddl_alter::exec_alter(cx, a),
        S::Drop(d) => ddl_drop::exec_drop(cx, d),
        S::DropDatabase { name, if_exists, force } => ddl_drop::exec_drop_database(cx, name, *if_exists, *force),
        S::Comment { addr, text } => ddl_objects::exec_comment(cx, addr, text.as_deref()),
        S::Ext(e) => crate::ddl_ext::exec_ext(cx, e),
        S::Grant { specs, revoke } => ddl_objects::exec_grant(cx, specs, *revoke),
        S::GrantRole(specs) => ddl_objects::exec_grant_role(cx, specs),
        S::RevokeRole { specs, admin_option_only, cascade } => {
            ddl_objects::exec_revoke_role(cx, specs, *admin_option_only, *cascade)
        }
        S::ReassignOwned { old_roles, new_owner } => ddl_drop::exec_reassign_owned(cx, old_roles, *new_owner),
        S::DropOwned { roles, cascade } => ddl_drop::exec_drop_owned(
            cx,
            roles,
            if *cascade { pgarbor_catalog::DropBehavior::Cascade } else { pgarbor_catalog::DropBehavior::Restrict },
        ),
        S::RefreshMatView { view, with_data, concurrently } => {
            ddl_objects::exec_refresh_matview(cx, *view, *with_data, *concurrently)
        }
        S::Maintenance { kind, targets } => utility::exec_maintenance(cx, *kind, targets),
        S::Lock(l) => utility::exec_lock(cx, l),
        S::Policy(p) => crate::ddl_rewrite::exec_policy(cx, p),
        S::Do(d) => utility::exec_do(cx, d),
        S::Call(c) => utility::exec_call(cx, c),
        S::Explain(e) => explain_exec::exec_explain(cx, e),
        S::Copy(c) => crate::copy::execute_copy(cx, c, plan, chan),
        S::Transaction(_)
        | S::Set(_)
        | S::SetRole { .. }
        | S::SetSessionAuthorization { .. }
        | S::Reset(_)
        | S::Show(_)
        | S::Prepare(_)
        | S::Execute(_)
        | S::Deallocate(_)
        | S::Discard(_)
        | S::Listen(_)
        | S::Unlisten(_)
        | S::Notify { .. } => Err(PgError::internal(format!(
            "statement {} must be executed by the session layer",
            stmt.tag()
        ))),
    }
}
