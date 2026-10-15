//! One client connection from accept to close (lease WIR-17). PG: `postmaster/postmaster.c`
//! `BackendInitialize`, `tcop/postgres.c` `PostgresMain` (startup half), `utils/init/postinit.c`.
//!
//! [`serve_connection`] runs on a dedicated thread spawned by `listener.rs`; it never panics out (it
//! wraps the work in `std::panic::catch_unwind`) and always releases its gate permit, registry entry
//! and socket.
//!
//! ## Steps
//! 1. `conn_id = shared.next_conn_id()`; `ConnIo::new(stream, conn_id, &shared.config)`; register a
//!    `try_clone` of the socket in `shared.registry`. Deadline = now + `startup_timeout`.
//! 2. `startup_flow::negotiate`: `CancelHandled` / `Dropped` -> close, done.
//! 3. If `shared.is_stopping()`: FATAL `57P03`  `the database system is shutting down`; close.
//! 4. `shared.sessions.try_acquire()`: `None` -> FATAL `53300` `sorry, too many clients already`; close.
//!    The permit lives until the function returns. (PG sends this before authentication.)
//! 5. `auth::authenticate(io, shared, &startup, deadline)`; on error send it as FATAL (an `Io` /
//!    `Closed` error just closes).
//! 6. Build `SessionOptions { user, database, application_name, client_addr/port from the peer,
//!    cancel_key_len, startup_gucs }` and `shared.db.connect(Some(&shared.config.branch), opts)`. Errors
//!    (28000 unknown role / not allowed to log in, 3D000 unknown database, 53300 full engine, branch
//!    errors, bad startup GUC) are sent as FATAL ErrorResponse after the authentication succeeded
//!    (PG order) and the connection closes. The `Session` lives on THIS thread.
//! 7. Queue, in this order (PG order): `AuthenticationOk` was already sent by `authenticate`;
//!    `ParameterStatus` for every pair of `session.startup_param_status()` (the reporter is seeded
//!    with the same pairs), `BackendKeyData{pid, secret}` from `session.backend_key()`, then
//!    `ReadyForQuery(Idle)`; flush. Register `session.cancel_token()` in `shared.registry`.
//! 8. Wrap `io` in `Rc<RefCell<..>>` (`SharedIo`), try `copy::install` (ignore its unimplemented
//!    error until the amendment lands - COPY then fails with 0A000 at statement time), build
//!    `SessionCtx`, run `session_loop::run` inside `catch_unwind(AssertUnwindSafe(..))`.
//! 9. Outcome handling: `Ok` / `Closed` / `Io` -> nothing more to send; `Shutdown` -> FATAL
//!    `57P01`; a panic -> FATAL `XX000` `internal error` (log the panic message to stderr with the
//!    connection id); other `Pg` errors -> send as FATAL.
//! 10. Cleanup: `session.close()` (rolls back), then `io.shutdown()`.
//!
//! Logging: one line to stderr only for abnormal events (panic, failed bind), never per query.
use std::sync::Arc;

use pgarbor_common::PgError;

use crate::error::{WireError, WireResult};
use crate::shared::Shared;
use crate::transport::Stream;

/// Run one connection to completion (module docs). `_threads_permit` is the accept loop's
/// `threads` gate permit, released when this function returns.
#[allow(unused_variables)]
pub fn serve_connection(shared: Arc<Shared>, stream: Stream, threads_permit: crate::limits::ConnGuard) {
    // The worker replaces this body: call `run_connection` and swallow its result.
    let _ = run_connection;
}

/// The fallible body of [`serve_connection`] (steps 1-10).
#[allow(unused_variables)]
pub fn run_connection(shared: &Arc<Shared>, stream: Stream) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-17", "connection lifecycle (connection::run_connection)")))
}
