//! Extensibility DDL (operators, aggregates, operator classes/families, casts, conversions,
//! collations, extensions; amendment pgarbor-sql-05): one `run_ddl` with `CatalogTxn::run_ext`,
//! then the outcome's follow-ups (CASCADE drops may remove relations), tag = `ExtDdl::tag`.
use pgarbor_catalog::ddl::ext_spec::ExtDdl;
use pgarbor_common::Result;

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::ddl_followup::run_followups;
use crate::result::ExecResult;

pub fn exec_ext(cx: &mut ExecContext, stmt: &ExtDdl) -> Result<ExecResult> {
    let tag = stmt.tag();
    cx.txn.check_writable(tag)?;
    let (_, outcome) = run_ddl(cx, |ddl| ddl.run_ext(stmt))?;
    run_followups(cx, &outcome.follow_ups, None, &[])?;
    Ok(ExecResult::command(tag))
}
