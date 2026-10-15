//! One client connection from accept to close (lease WIR-17). PG: `postmaster/postmaster.c`
//! `BackendInitialize`, `tcop/postgres.c` `PostgresMain` (startup half), `utils/init/postinit.c`.
//!
//! [`serve_connection`] is the panic boundary for one accepted socket. Setup and protocol work are
//! delegated to the startup, authentication, and session-loop modules.
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use pgarbor::{SessionOptions, TxnStatus};
use pgarbor_common::{PgError, Severity, SqlState};

use crate::backend::Backend;
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::limits::ConnGuard;
use crate::shared::Shared;
use crate::transport::Stream;

/// Run one connection to completion. `_threads_permit` is released on every return or unwind.
pub fn serve_connection(shared: Arc<Shared>, stream: Stream, _threads_permit: ConnGuard) {
    let conn_id = shared.next_conn_id();
    let result = catch_unwind(AssertUnwindSafe(|| run_connection_inner(&shared, stream, conn_id)));
    if let Err(payload) = result {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("non-string panic payload");
        eprintln!("pgarbor-wire: connection {conn_id} panicked: {message}");
    }
}

fn send_fatal(io: &mut ConnIo, error: PgError) {
    let fatal = error.with_severity(Severity::Fatal);
    let _ = io.send(&crate::error_fields::error_message(&fatal));
    let _ = io.flush();
}

fn fatal_state(state: SqlState, message: impl Into<String>) -> PgError {
    PgError::new(state, message).with_severity(Severity::Fatal)
}

/// The fallible body of [`serve_connection`] (steps 1-10).
pub fn run_connection(shared: &Arc<Shared>, stream: Stream) -> WireResult<()> {
    let conn_id = shared.next_conn_id();
    run_connection_inner(shared, stream, conn_id)
}

fn run_connection_inner(shared: &Arc<Shared>, stream: Stream, conn_id: u64) -> WireResult<()> {
    let socket = match stream.try_clone() {
        Ok(socket) => socket,
        Err(error) => return Err(WireError::Io(error)),
    };
    let _registry_guard = shared.registry.register(conn_id, socket);
    let mut io = ConnIo::new(stream, conn_id, &shared.config)?;
    let deadline = Instant::now() + shared.config.startup_timeout;
    let startup = match crate::startup_flow::negotiate(&mut io, shared, deadline) {
        Ok(crate::startup_flow::Outcome::Proceed(info)) => info,
        Ok(crate::startup_flow::Outcome::CancelHandled | crate::startup_flow::Outcome::Dropped) => {
            io.shutdown();
            return Ok(());
        }
        Err(error) => {
            if let WireError::Pg(pg) = &error { send_fatal(&mut io, pg.clone()); }
            io.shutdown();
            return Err(error);
        }
    };

    if shared.is_stopping() {
        let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'3']), "the database system is shutting down");
        send_fatal(&mut io, error);
        io.shutdown();
        return Ok(());
    }
    let _session_permit = match shared.sessions.try_acquire() {
        Some(permit) => permit,
        None => {
            send_fatal(&mut io, fatal_state(SqlState([b'5', b'3', b'3', b'0', b'0']), "sorry, too many clients already"));
            io.shutdown();
            return Ok(());
        }
    };

    if let Err(error) = crate::auth::authenticate(&mut io, shared, &startup, deadline) {
        if let WireError::Pg(pg) = &error { send_fatal(&mut io, pg.clone()); }
        io.shutdown();
        return Err(error);
    }

    let peer = io.peer().clone();
    let options = SessionOptions {
        user: startup.user,
        database: startup.database,
        application_name: startup.application_name,
        client_addr: peer.client_addr(),
        client_port: peer.client_port(),
        cancel_key_len: startup.cancel_key_len,
        startup_gucs: startup.startup_gucs,
    };
    let mut session = match shared.db.connect(Some(&shared.config.branch), options.clone()) {
        Ok(session) => session,
        Err(error) => {
            send_fatal(&mut io, error);
            io.shutdown();
            return Ok(());
        }
    };

    let initial_params = match session.startup_param_status() {
        Ok(params) => params,
        Err(error) => {
            send_fatal(&mut io, error);
            session.close();
            io.shutdown();
            return Ok(());
        }
    };
    for (name, value) in &initial_params {
        if let Err(error) = io.send(&Backend::ParameterStatus { name: name.clone(), value: value.clone() }) {
            session.close();
            io.shutdown();
            return Err(error);
        }
    }
    let (pid, secret) = session.backend_key();
    let startup_messages = io.send(&Backend::BackendKeyData { pid, secret })
        .and_then(|_| io.send(&Backend::ReadyForQuery(TxnStatus::Idle)))
        .and_then(|_| io.flush());
    if let Err(error) = startup_messages {
        session.close();
        io.shutdown();
        return Err(error);
    }
    shared.registry.set_session(conn_id, session.cancel_token(), &options.user);

    let shared_io = std::rc::Rc::new(std::cell::RefCell::new(io));
    let _copy_handle = match crate::copy::install(&mut session, &shared_io) {
        Ok(handle) => Some(handle),
        Err(_) => None,
    };
    let mut ctx = crate::session_loop::SessionCtx::new(
        std::rc::Rc::clone(&shared_io), shared, &initial_params,
    );
    let outcome = catch_unwind(AssertUnwindSafe(|| crate::session_loop::run(&mut ctx, &mut session)));
    match outcome {
        Ok(Ok(())) | Ok(Err(WireError::Closed | WireError::Io(_))) => {}
        Ok(Err(WireError::Shutdown)) => {
            let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'1']), "terminating connection due to administrator command");
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
        Ok(Err(WireError::Pg(error))) => {
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
        Err(payload) => {
            let message = payload.downcast_ref::<String>().map(String::as_str)
                .or_else(|| payload.downcast_ref::<&'static str>().copied()).unwrap_or("non-string panic payload");
            eprintln!("pgarbor-wire: connection {conn_id} panicked: {message}");
            let error = fatal_state(SqlState([b'X', b'X', b'0', b'0', b'0']), "internal error");
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
    }
    session.close();
    if let Ok(mut io) = shared_io.try_borrow_mut() { io.shutdown(); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_state_preserves_state_and_sets_fatal_severity() {
        let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'3']), "the database system is shutting down");
        assert_eq!(error.sqlstate.code(), "57P03");
        assert_eq!(error.severity, Severity::Fatal);
        assert_eq!(error.message, "the database system is shutting down");
    }
}
