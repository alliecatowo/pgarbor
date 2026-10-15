//! Simple Query protocol (lease WIR-11). PG: `tcop/postgres.c` `exec_simple_query`,
//! `protocol.sgml` "Simple Query".
//!
//! [`handle`] runs one `Q` message:
//! 1. `let out = session.simple_query_raw(sql)` (never fails; semantics - whole-string parse, implicit
//!    transaction block for multi-statement strings, error handling - live in `pgarbor`);
//!    COPY: if `ctx.copy` is `Some(h)`, call `h.finish_statement(true)` right after `simple_query_raw`
//!    returns and BEFORE step 2 (queues `CopyDone` when a COPY TO STDOUT streamed data; nothing
//!    otherwise); on a failing statement call `h.finish_statement(false)` (no CopyDone, PG sends the
//!    ErrorResponse straight after the last CopyData);
//! 2. for each `out.statements[i]`: `result_encode::send_statement_output(io, session, reporter, st)`.
//!    If it fails with a row-encoding error `e`: queue `send_error_cycle(.., &[], &e, &[])` and treat the
//!    remaining statements as skipped (the statement's effects already happened; PG would have raised
//!    the error during execution - documented gap);
//! 3. if `out.error` is `Some(e)`: `send_error_cycle(io, reporter, &out.error_notices, e,
//!    &out.error_param_status)`. A FATAL error (severity Fatal/Panic, e.g. 57P01 from
//!    `CancelToken::terminate`) is flushed and returned as `Err(WireError::Pg(e))` WITHOUT
//!    ReadyForQuery (the connection closes);
//! 4. `result_encode::finish_cycle(io, reporter, out.status)` (ParameterStatus, `Z`, flush).
//!
//! An empty query string yields `StatementResult::Empty` from the session -> EmptyQueryResponse then
//! ReadyForQuery (PG behavior). Multi-statement strings produce one CommandComplete (and RowDescription
//! block) per statement, in order.
//!
//! Known gap (documented in `contracts/crates/pgarbor-wire.md`): the session returns all results after
//! the whole string ran, so `COPY` inside a multi-statement string cannot interleave correctly (see
//! amendment pgarbor-wire-01).
use pgarbor::Session;
use pgarbor_common::PgError;

use crate::error::{WireError, WireResult};
use crate::session_loop::SessionCtx;

/// Handle one Query message (module docs).
#[allow(unused_variables)]
pub fn handle(ctx: &mut SessionCtx<'_>, session: &mut Session, sql: &str) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-11", "simple Query handling (simple_query::handle)")))
}
