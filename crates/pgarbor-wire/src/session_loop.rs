//! The per-connection command loop (lease WIR-16). PG: `tcop/postgres.c` `PostgresMain`.
//!
//! [`run`] is entered after ReadyForQuery('I') was sent and returns when the connection ends.
//! `Ok(())` = orderly end (Terminate, EOF, shutdown with FATAL already sent); `Err` = abnormal end
//! (the caller just closes).
//!
//! ## Loop body
//! 1. **Shutdown**: if `shared.is_stopping()`: queue FATAL `57P01` `terminating connection due to
//!    administrator command`, flush, return Ok.
//! 2. **Notifications** (only when idle): `notify_out::deliver(io, session)` when
//!    `session.txn_status() == Idle` and not skipping until Sync.
//! 3. **Poll** `io.poll(Some(notify_out::poll_interval(shared, session)))`:
//!    * `Eof` -> return Ok (client vanished; the caller closes, session rollback happens on drop);
//!    * `TimedOut` -> idle timeouts: track `idle_since` (reset after each message); in an open
//!      transaction block (`InTransaction`/`Failed`) compare against
//!      `idle_in_transaction_session_timeout`; when idle against `idle_session_timeout` (both read with
//!      `session.setting(name)`, SHOW form parsed by [`parse_pg_duration_ms`], `0` = disabled);
//!      expiry sends FATAL `25P03` `terminating connection due to idle-in-transaction timeout` or FATAL
//!      `57P05` `terminating connection due to idle-session timeout` and returns Ok;
//!    * `Ready(msg)` -> step 4.
//!    A decode error (`WireError::Pg`, non-fatal 08P01) is reported with `send_error_cycle`; in the
//!    simple-protocol state it is followed by ReadyForQuery, in extended state it enters
//!    skip-until-Sync. Fatal framing errors propagate.
//! 4. **Dispatch** (`msg`):
//!    * while `ctx.skipping_until_sync` (an extended-protocol error happened): ignore EVERYTHING except
//!      `Sync` (PG `ignore_till_sync`, including Query, Flush and Terminate; the client's EOF ends it);
//!    * `Query(sql)` -> `simple_query::handle`; extended messages `Parse/Bind/Describe/Execute/Close/Sync/
//!      Flush` -> `extended::handle`;
//!    * `Terminate` -> return Ok;
//!    * `CopyData/CopyDone/CopyFail` outside COPY mode -> ignored (PG accepts and ignores them);
//!    * `FunctionCall` -> ErrorResponse `0A000` `fastpath function calls are not supported`, then
//!      ReadyForQuery (PG parity gap, documented);
//!    * `Password` -> FATAL `08P01` `invalid frontend message type 112`.
//! 5. After every Query and every Sync: `notify_out::deliver` when idle (NotificationResponse after
//!    ReadyForQuery, as PG does).
//!
//! ## Panics
//! A panic inside `Session` code must not take the server down: the connection thread
//! (`connection.rs`) wraps `run` in `catch_unwind`; the client gets FATAL `XX000` `internal error`
//! and the connection closes.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pgarbor::{FormatCode, Session, TxnStatus};
use pgarbor_common::{PgError, Severity, SqlState};

use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::frontend::Frontend;
use crate::param_report::ParamReporter;
use crate::shared::Shared;
use crate::framing::Polled;
use crate::{extended, notify_out, result_encode, simple_query};

/// Connection I/O shared with the COPY channel (single-threaded: `Rc<RefCell<..>>`).
pub type SharedIo = Rc<RefCell<ConnIo>>;

/// Mutable protocol state of one connection.
pub struct SessionCtx<'a> {
    pub io: SharedIo,
    pub shared: &'a Shared,
    pub reporter: ParamReporter,
    /// Result formats recorded at Bind, by portal name (Describe portal needs them; `ColumnInfo`
    /// has no format). Entries are overwritten by the next Bind of the same name and removed by
    /// Close portal.
    pub portal_formats: HashMap<String, Vec<FormatCode>>,
    /// An extended-protocol error happened: discard everything until Sync.
    pub skipping_until_sync: bool,
    /// Handle to the COPY channel installed in the session (`None` until amendment
    /// pgarbor-wire-01 lands; see `copy.rs`).
    pub copy: Option<crate::copy::CopyHandle>,
}

impl<'a> SessionCtx<'a> {
    pub fn new(io: SharedIo, shared: &'a Shared, initial_params: &[(String, String)]) -> SessionCtx<'a> {
        SessionCtx {
            io,
            shared,
            reporter: ParamReporter::new(initial_params),
            portal_formats: HashMap::new(),
            skipping_until_sync: false,
            copy: None,
        }
    }
}

/// Parse a GUC value in SHOW form into milliseconds: plain integer (`"0"`, `"5000"` = ms) or integer
/// with unit `us|ms|s|min|h|d` (`"5s"` -> 5000, `"1min"` -> 60000, `"1h"`, `"1d"`). Returns 0 for
/// unparsable input (treated as disabled).
pub fn parse_pg_duration_ms(value: &str) -> pgarbor_common::Result<u64> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    if split == 0 { return Ok(0); }
    let Ok(number) = value[..split].parse::<u64>() else { return Ok(0); };
    let unit = value[split..].trim();
    let multiplier: u64 = match unit {
        "" | "ms" => 1,
        "us" => return Ok(number / 1000),
        "s" => 1_000,
        "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Ok(0),
    };
    Ok(number.checked_mul(multiplier).unwrap_or(0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action { Ignore, Sync, Query, Extended, Terminate, FunctionCall, Password }

fn classify(msg: &Frontend, skipping: bool) -> Action {
    if skipping { return if matches!(msg, Frontend::Sync) { Action::Sync } else { Action::Ignore }; }
    match msg {
        Frontend::Query(_) => Action::Query,
        Frontend::Parse { .. } | Frontend::Bind { .. } | Frontend::Describe { .. }
        | Frontend::Execute { .. } | Frontend::Close { .. } | Frontend::Flush => Action::Extended,
        Frontend::Sync => Action::Sync,
        Frontend::Terminate => Action::Terminate,
        Frontend::CopyData(_) | Frontend::CopyDone | Frontend::CopyFail(_) => Action::Ignore,
        Frontend::FunctionCall(_) => Action::FunctionCall,
        Frontend::Password(_) => Action::Password,
    }
}

fn send_fatal(io: &mut ConnIo, reporter: &mut ParamReporter, error: &PgError) -> WireResult<()> {
    match result_encode::send_error_cycle(io, reporter, &[], error, &[]) {
        Ok(()) => io.flush(),
        Err(WireError::Pg(e)) if matches!(e.severity, Severity::Fatal | Severity::Panic) => io.flush(),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_values() {
        for (input, expected) in [("0", 0), ("5000", 5000), ("5s", 5000), ("1min", 60_000),
            ("250ms", 250), ("1h", 3_600_000), ("1d", 86_400_000), ("garbage", 0)] {
            assert_eq!(parse_pg_duration_ms(input).ok(), Some(expected));
        }
    }

    #[test]
    fn dispatch_table_and_skip_until_sync() {
        let cases = [
            (Frontend::Query(String::new()), Action::Query),
            (Frontend::Parse { name: String::new(), query: String::new(), param_oids: vec![] }, Action::Extended),
            (Frontend::Bind { portal: String::new(), statement: String::new(), param_formats: vec![], params: vec![], result_formats: vec![] }, Action::Extended),
            (Frontend::Execute { portal: String::new(), max_rows: 0 }, Action::Extended),
            (Frontend::Describe { kind: crate::frontend::TargetKind::Statement, name: String::new() }, Action::Extended),
            (Frontend::Close { kind: crate::frontend::TargetKind::Portal, name: String::new() }, Action::Extended),
            (Frontend::Sync, Action::Sync), (Frontend::Flush, Action::Extended),
            (Frontend::Terminate, Action::Terminate), (Frontend::CopyData(vec![]), Action::Ignore),
            (Frontend::CopyDone, Action::Ignore), (Frontend::CopyFail(String::new()), Action::Ignore),
            (Frontend::FunctionCall(vec![]), Action::FunctionCall), (Frontend::Password(vec![]), Action::Password),
        ];
        for (msg, expected) in cases {
            assert_eq!(classify(&msg, false), expected);
            assert_eq!(classify(&msg, true), if matches!(msg, Frontend::Sync) { Action::Sync } else { Action::Ignore });
        }
    }
}

/// The command loop (module docs).
pub fn run(ctx: &mut SessionCtx<'_>, session: &mut Session) -> WireResult<()> {
    let mut idle_since = Instant::now();
    loop {
        if ctx.shared.is_stopping() {
            let error = PgError::new(SqlState::ADMIN_SHUTDOWN, "terminating connection due to administrator command")
                .with_severity(Severity::Fatal);
            let mut io = ctx.io.borrow_mut();
            send_fatal(&mut io, &mut ctx.reporter, &error)?;
            return Ok(());
        }

        if !ctx.skipping_until_sync && session.txn_status() == TxnStatus::Idle {
            let mut io = ctx.io.borrow_mut();
            notify_out::deliver(&mut io, session)?;
        }

        let polled = {
            let mut io = ctx.io.borrow_mut();
            io.poll(Some(notify_out::poll_interval(ctx.shared, session)))
        };
        match polled {
            Ok(Polled::Eof) => return Ok(()),
            Ok(Polled::TimedOut) => {
                let status = session.txn_status();
                let (setting, state, message) = if status != TxnStatus::Idle {
                    ("idle_in_transaction_session_timeout", SqlState(*b"25P03"), "terminating connection due to idle-in-transaction timeout")
                } else {
                    ("idle_session_timeout", SqlState(*b"57P05"), "terminating connection due to idle-session timeout")
                };
                let configured = session.setting(setting).ok().and_then(|v| parse_pg_duration_ms(&v).ok()).unwrap_or(0);
                if configured > 0 && idle_since.elapsed() >= Duration::from_millis(configured) {
                    let err = PgError::new(state, message).with_severity(Severity::Fatal);
                    let mut io = ctx.io.borrow_mut();
                    send_fatal(&mut io, &mut ctx.reporter, &err)?;
                    return Ok(());
                }
                continue;
            }
            Err(WireError::Pg(error)) => {
                if matches!(error.severity, Severity::Fatal | Severity::Panic) {
                    return Err(WireError::Pg(error));
                }
                let extended = ctx.skipping_until_sync;
                {
                    let mut io = ctx.io.borrow_mut();
                    result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &error, &[])?;
                    if !extended { result_encode::finish_cycle(&mut io, &mut ctx.reporter, session.txn_status())?; }
                }
                if extended { ctx.skipping_until_sync = true; }
                continue;
            }
            Err(e) => return Err(e),
            Ok(Polled::Ready(msg)) => {
                idle_since = Instant::now();
                match classify(&msg, ctx.skipping_until_sync) {
                    Action::Ignore => continue,
                    Action::Sync => extended::handle(ctx, session, msg)?,
                    Action::Query => {
                        if let Frontend::Query(sql) = msg { simple_query::handle(ctx, session, &sql)?; }
                    }
                    Action::Extended => extended::handle(ctx, session, msg)?,
                    Action::Terminate => return Ok(()),
                    Action::FunctionCall => {
                        let err = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "fastpath function calls are not supported");
                        let mut io = ctx.io.borrow_mut();
                        result_encode::send_error_cycle(&mut io, &mut ctx.reporter, &[], &err, &[])?;
                        result_encode::finish_cycle(&mut io, &mut ctx.reporter, session.txn_status())?;
                    }
                    Action::Password => {
                        let err = PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid frontend message type 112").with_severity(Severity::Fatal);
                        let mut io = ctx.io.borrow_mut();
                        send_fatal(&mut io, &mut ctx.reporter, &err)?;
                        return Ok(());
                    }
                }
                if !ctx.skipping_until_sync && session.txn_status() == TxnStatus::Idle {
                    let mut io = ctx.io.borrow_mut();
                    notify_out::deliver(&mut io, session)?;
                }
            }
        }
    }
}
