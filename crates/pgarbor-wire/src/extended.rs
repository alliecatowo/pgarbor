//! Extended Query protocol (lease WIR-12). PG: `tcop/postgres.c` `exec_parse_message`,
//! `exec_bind_message`, `exec_describe_*_message`, `exec_execute_message`, `finish_xact_command`,
//! `protocol.sgml` "Extended Query".
//!
//! [`handle`] processes ONE already-decoded message among `Parse, Bind, Describe, Execute, Close,
//! Sync, Flush`. Responses are queued; they are flushed only at `Sync`, `Flush`, or when the writer
//! auto-flushes (pipelining clients rely on this).
//!
//! | message | session call | queued answer |
//! |---|---|---|
//! | Parse | `session.parse(name, query, &oids)` | `ParseComplete` |
//! | Bind | convert `param_formats`/`result_formats` with `FormatCode::from_i16` (error `08P01 unsupported format code: N`), then `session.bind(portal, statement, &pf, &params, &rf)`; remember `rf` in `ctx.portal_formats[portal]` | `BindComplete` |
//! | Describe `S` | `session.describe_statement(name)` | `ParameterDescription(param_oids)` then `RowDescription` (formats all 0) or `NoData` |
//! | Describe `P` | `session.describe_portal(name)` | `RowDescription` with the formats recorded at Bind (`result_encode::row_fields(cols, &formats)`) or `NoData` |
//! | Execute | `session.execute(portal, max_rows)`; `max_rows <= 0` -> 0 (unlimited) | notices, `DataRow*` (rows are already encoded in the portal's formats), then `CommandComplete{tag}` / `EmptyQueryResponse` / `PortalSuspended`; `param_status` -> reporter |
//! | Close `S` / `P` | `close_statement` / `close_portal` (missing object is NOT an error); `P` also removes `ctx.portal_formats[name]` | `CloseComplete` |
//! | Sync | `session.sync()` | pending ParameterStatus, `ReadyForQuery(status)`, flush; clears `ctx.skipping_until_sync` |
//! | Flush | - | flush only |
//!
//! ## COPY via Execute
//! `COPY ... FROM STDIN / TO STDOUT` executed through a portal uses the same `copy` channel as the
//! simple protocol: after `session.execute(..)` returns call `ctx.copy.finish_statement(ok)` before
//! queueing the portal's CommandComplete.
//!
//! ## Error handling (PG `ignore_till_sync`)
//! When a session call fails with `e` (and for protocol decode errors of these messages):
//! 1. queue `ErrorResponse(e)` (via `result_encode::send_error_cycle`; a FATAL error is flushed and
//!    returned as `Err(WireError::Pg(e))`, closing the connection);
//! 2. set `ctx.skipping_until_sync = true`; send NO ReadyForQuery now;
//! 3. everything up to the next `Sync` is discarded by `session_loop`; `Sync` then goes through the
//!    normal Sync path above (`session.sync()` returns the status; its own error, e.g. a commit
//!    failure, is sent as ErrorResponse before ReadyForQuery).
//! The session has already rolled the implicit transaction back (`abort_after_error`).
//!
//! ## Format conventions
//! * Bind parameter format list: empty = all text, one = applies to all, else one per parameter
//!   (validated by `session.bind`).
//! * Describe statement ALWAYS reports format 0 for every column (the statement has no portal yet).
//! * Unnamed statement/portal semantics belong to `Session` (replacement, destruction at Sync/commit).
use pgarbor::{FormatCode, PortalStatus, Session};
use pgarbor_common::{PgError, Severity};

use crate::backend::Backend;
use crate::error::{WireError, WireResult};
use crate::frontend::{Frontend, TargetKind};
use crate::result_encode;
use crate::session_loop::SessionCtx;

/// Handle one extended-protocol message (module docs). Any other `Frontend` variant is a
/// programming error and returns `PgError::internal`.
pub fn handle(ctx: &mut SessionCtx<'_>, session: &mut Session, msg: Frontend) -> WireResult<()> {
    match msg {
        Frontend::Parse { name, query, param_oids } => {
            if let Err(e) = session.parse(&name, &query, &param_oids) { return report_error(ctx, e); }
            send(ctx, Backend::ParseComplete)
        }
        Frontend::Bind { portal, statement, param_formats, params, result_formats } => {
            let pf = match formats(&param_formats) { Ok(v) => v, Err(e) => return report_error(ctx, e) };
            let rf = match formats(&result_formats) { Ok(v) => v, Err(e) => return report_error(ctx, e) };
            if let Err(e) = session.bind(&portal, &statement, &pf, &params, &rf) { return report_error(ctx, e); }
            ctx.portal_formats.insert(portal, rf);
            send(ctx, Backend::BindComplete)
        }
        Frontend::Describe { kind: TargetKind::Statement, name } => {
            let desc = match session.describe_statement(&name) { Ok(v) => v, Err(e) => return report_error(ctx, e) };
            send(ctx, Backend::ParameterDescription(desc.param_oids))?;
            match desc.columns {
                Some(cols) => send(ctx, Backend::RowDescription(result_encode::row_fields(&cols, &[]))),
                None => send(ctx, Backend::NoData),
            }
        }
        Frontend::Describe { kind: TargetKind::Portal, name } => {
            let cols = match session.describe_portal(&name) { Ok(v) => v, Err(e) => return report_error(ctx, e) };
            match cols {
                Some(cols) => {
                    let fmts = ctx.portal_formats.get(&name).map(Vec::as_slice).unwrap_or(&[]);
                    send(ctx, Backend::RowDescription(result_encode::row_fields(&cols, fmts)))
                }
                None => send(ctx, Backend::NoData),
            }
        }
        Frontend::Execute { portal, max_rows } => {
            let output = match session.execute(&portal, if max_rows <= 0 { 0 } else { max_rows as u32 }) {
                Ok(v) => v, Err(e) => return report_error(ctx, e),
            };
            ctx.reporter.note_all(&output.param_status);
            let mut io = ctx.io.borrow_mut();
            result_encode::send_notices(&mut io, &output.notices)?;
            for row in output.rows { io.send(&Backend::DataRow(row))?; }
            drop(io);
            let complete = match output.status {
                PortalStatus::Suspended => Backend::PortalSuspended,
                PortalStatus::Complete { tag } => Backend::CommandComplete { tag },
                PortalStatus::Empty => Backend::EmptyQueryResponse,
            };
            if let Some(copy) = &ctx.copy {
                copy.finish_statement(true, &ctx.io)?;
            }
            send(ctx, complete)
        }
        Frontend::Close { kind, name } => {
            let result = match kind {
                TargetKind::Statement => session.close_statement(&name),
                TargetKind::Portal => session.close_portal(&name),
            };
            if let Err(e) = result { return report_error(ctx, e); }
            if kind == TargetKind::Portal { ctx.portal_formats.remove(&name); }
            send(ctx, Backend::CloseComplete)
        }
        Frontend::Sync => {
            let result = session.sync();
            let status = match result {
                Ok(status) => status,
                Err(e) => {
                    if e.severity == Severity::Fatal || e.severity == Severity::Panic {
                        ctx.skipping_until_sync = false;
                        let mut io = ctx.io.borrow_mut();
                        result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &e, &[])?;
                        return Err(WireError::Pg(e));
                    }
                    {
                        let mut io = ctx.io.borrow_mut();
                        result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &e, &[])?;
                    }
                    session.txn_status()
                }
            };
            ctx.skipping_until_sync = false;
            let mut io = ctx.io.borrow_mut();
            result_encode::finish_cycle(&mut io, &mut ctx.reporter, status)
        }
        Frontend::Flush => ctx.io.borrow_mut().flush(),
        _ => Err(WireError::Pg(PgError::internal("unexpected message passed to extended protocol handler"))),
    }
}

fn formats(raw: &[i16]) -> Result<Vec<FormatCode>, PgError> {
    raw.iter().copied().map(FormatCode::from_i16).collect()
}

fn send(ctx: &mut SessionCtx<'_>, msg: Backend) -> WireResult<()> {
    ctx.io.borrow_mut().send(&msg)
}

fn report_error(ctx: &mut SessionCtx<'_>, err: PgError) -> WireResult<()> {
    if err.severity == Severity::Fatal || err.severity == Severity::Panic {
        let mut io = ctx.io.borrow_mut();
        result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &err, &[])?;
        ctx.skipping_until_sync = true;
        return Err(WireError::Pg(err));
    }
    let mut io = ctx.io.borrow_mut();
    result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &err, &[])?;
    ctx.skipping_until_sync = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_format_codes_and_rejects_unknown_values() {
        assert_eq!(formats(&[]).expect("empty formats"), Vec::<FormatCode>::new());
        assert_eq!(formats(&[0, 1]).expect("valid formats"), vec![FormatCode::Text, FormatCode::Binary]);
        let err = formats(&[2]).expect_err("invalid format code");
        assert_eq!(err.sqlstate.code(), "08P01");
        assert_eq!(err.message, "unsupported format code: 2");
    }
}
