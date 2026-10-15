//! CancelRequest handling (WIRE-09). Fully implemented, frozen (v1).
//!
//! The cancel key map lives in the engine: `Database::cancel_request(pid, secret)` compares the
//! secret in constant time and requests cancellation of the running statement (57014
//! `canceling statement due to user request`). The wire layer only forwards. PG never replies to a
//! CancelRequest and never reveals whether the key matched: the connection is simply closed.
//! Errors from the engine are swallowed for the same reason (an unimplemented engine means
//! "no such backend").
use pgarbor::Database;

/// Handle one CancelRequest packet (module docs). Returns whether a backend was signalled
/// (diagnostics only; never sent to the client).
pub fn handle(db: &Database, pid: i32, secret: &[u8]) -> bool {
    db.cancel_request(pid, secret).unwrap_or(false)
}
