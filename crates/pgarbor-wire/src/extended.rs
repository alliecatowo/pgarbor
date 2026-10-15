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
use pgarbor::Session;
use pgarbor_common::PgError;

use crate::error::{WireError, WireResult};
use crate::frontend::Frontend;
use crate::session_loop::SessionCtx;

/// Handle one extended-protocol message (module docs). Any other `Frontend` variant is a
/// programming error and returns `PgError::internal`.
#[allow(unused_variables)]
pub fn handle(ctx: &mut SessionCtx<'_>, session: &mut Session, msg: Frontend) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-12", "extended protocol messages (extended::handle)")))
}
