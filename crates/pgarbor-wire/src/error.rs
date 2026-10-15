//! Wire-level error type. Fully implemented, frozen (v1).
//!
//! `WireError` separates "the socket is gone / the stream is unusable" (which must close the
//! connection WITHOUT trying to write an ErrorResponse) from "send this PgError and carry on"
//! and "send this FATAL error and close".
use pgarbor_common::{PgError, Severity, SqlState};

/// Result alias used across the crate.
// CONTRACT
pub type WireResult<T> = std::result::Result<T, WireError>;

/// Why a connection (or one step of it) failed.
// CONTRACT
#[derive(Debug)]
pub enum WireError {
    /// Socket I/O failure or EOF mid-message: close silently (PG logs "unexpected EOF").
    Io(std::io::Error),
    /// The peer closed the connection cleanly at a message boundary (EOF before a tag byte).
    Closed,
    /// A protocol violation (08P01) or other error that PG would send as ErrorResponse. The loop
    /// decides: in the main loop it is reported and the connection continues unless severity is FATAL.
    Pg(PgError),
    /// The server is shutting down; send FATAL 57P01 `terminating connection due to administrator
    /// command` and close.
    Shutdown,
}

impl WireError {
    /// 08P01 protocol violation with PG wording.
    pub fn protocol(message: impl Into<String>) -> WireError {
        WireError::Pg(PgError::new(SqlState::PROTOCOL_VIOLATION, message))
    }
    /// Same, FATAL (closes the connection after sending).
    pub fn fatal(err: PgError) -> WireError {
        WireError::Pg(err.with_severity(Severity::Fatal))
    }
    /// True when the error forces the connection to close after being reported.
    pub fn is_fatal(&self) -> bool {
        match self {
            WireError::Io(_) | WireError::Closed | WireError::Shutdown => true,
            WireError::Pg(e) => matches!(e.severity, Severity::Fatal | Severity::Panic),
        }
    }
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> WireError {
        WireError::Io(e)
    }
}

impl From<PgError> for WireError {
    fn from(e: PgError) -> WireError {
        WireError::Pg(e)
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "I/O error: {e}"),
            WireError::Closed => f.write_str("connection closed by peer"),
            WireError::Pg(e) => write!(f, "{}: {}", e.sqlstate.code(), e.message),
            WireError::Shutdown => f.write_str("server shutting down"),
        }
    }
}

impl std::error::Error for WireError {}
