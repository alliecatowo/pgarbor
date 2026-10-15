//! Deterministic exit codes. FROZEN (v1). Fully implemented, no lease.
//!
//! | code | name | meaning |
//! |---|---|---|
//! | 0 | `OK` | success (for `verify`: no errors; warnings allowed) |
//! | 1 | `FAILURE` | operation failed: SQL error in `exec`/`import`/`shell -c`, I/O error, anything unclassified |
//! | 2 | `ADDR_IN_USE` | `serve`/`branch exec`: listen address already in use (stderr contains `address in use`) |
//! | 3 | `CORRUPT` | file corrupt or not a PGArbor database (XX001/XX002, `verify` found errors; stderr contains `corrupt`) |
//! | 4 | `LOCKED` | database file is open in another process (55006) and no usable listener was found |
//! | 5 | `NOT_FOUND` | database file, branch, snapshot or handle does not exist (58P01, 42704) |
//! | 6 | `EXISTS` | target already exists (58P02, 42710) |
//! | 7 | `REFUSED` | refused to do something risky without explicit consent: destructive command without `--yes`, `DATABASE_URL` already set without `--force-env`, non-loopback bind, read-only violation (25006) |
//! | 8 | `UNSUPPORTED` | feature not implemented yet (0A000, `PgError::is_unimplemented`) |
//! | 64 | `USAGE` | bad command line (clap errors, conflicting flags, no database specified) |
//! | 65 | `DATA_ERR` | input file malformed (unparsable SQL script / dump, invalid handle file) |
//! | 70 | `INTERNAL` | internal error (XX000) |
//! | 126 | `CANNOT_EXEC` | `branch exec`: the command exists but is not executable |
//! | 127 | `CMD_NOT_FOUND` | `branch exec`: command not found |
//! | 128+N | | `branch exec`: the child was killed by signal N |
//!
//! `--help` / `--version` exit 0. A Rust panic exits 101 (default hook; the harness classifies it as
//! CRASH). `branch exec` returns the CHILD's exit status verbatim, so codes 1-8 are ambiguous there:
//! failures of pgarbor itself before the child starts are reported on stderr (as JSON with `--json`).
use pgarbor_common::PgError;

/// A process exit status.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit(pub u8);

impl Exit {
    pub const OK: Exit = Exit(0);
    pub const FAILURE: Exit = Exit(1);
    pub const ADDR_IN_USE: Exit = Exit(2);
    pub const CORRUPT: Exit = Exit(3);
    pub const LOCKED: Exit = Exit(4);
    pub const NOT_FOUND: Exit = Exit(5);
    pub const EXISTS: Exit = Exit(6);
    pub const REFUSED: Exit = Exit(7);
    pub const UNSUPPORTED: Exit = Exit(8);
    pub const USAGE: Exit = Exit(64);
    pub const DATA_ERR: Exit = Exit(65);
    pub const INTERNAL: Exit = Exit(70);
    pub const CANNOT_EXEC: Exit = Exit(126);
    pub const CMD_NOT_FOUND: Exit = Exit(127);

    /// `128 + signal`.
    pub fn from_signal(sig: i32) -> Exit {
        Exit((128 + sig.clamp(0, 127)) as u8)
    }
    pub fn code(self) -> u8 {
        self.0
    }
    pub fn is_ok(self) -> bool {
        self.0 == 0
    }
    /// Stable machine name used in `--json` error objects (`"ok"`, `"failure"`, `"addr_in_use"`, ...).
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "ok",
            1 => "failure",
            2 => "addr_in_use",
            3 => "corrupt",
            4 => "locked",
            5 => "not_found",
            6 => "exists",
            7 => "refused",
            8 => "unsupported",
            64 => "usage",
            65 => "data_err",
            70 => "internal",
            126 => "cannot_exec",
            127 => "cmd_not_found",
            c if c > 128 => "signal",
            _ => "failure",
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(e: Exit) -> std::process::ExitCode {
        std::process::ExitCode::from(e.0)
    }
}

/// Map an engine error to an exit code (table above). Order matters: unimplemented first (it is
/// 0A000), then the message-based "address in use" marker emitted by `pgarbor-wire`'s listener.
pub fn classify(err: &PgError) -> Exit {
    if err.is_unimplemented() {
        return Exit::UNSUPPORTED;
    }
    if err.message.contains("address in use") {
        return Exit::ADDR_IN_USE;
    }
    match err.sqlstate.code() {
        "XX001" | "XX002" => Exit::CORRUPT,
        "55006" => Exit::LOCKED,
        "58P01" | "42704" | "3D000" | "3F000" => Exit::NOT_FOUND,
        "58P02" | "42710" | "42P04" => Exit::EXISTS,
        "25006" | "42501" => Exit::REFUSED,
        "0A000" => Exit::UNSUPPORTED,
        "42601" | "42602" | "22023" => Exit::USAGE,
        "XX000" => Exit::INTERNAL,
        _ => Exit::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::SqlState;

    #[test]
    fn classification() {
        assert_eq!(classify(&PgError::corruption("x")), Exit::CORRUPT);
        assert_eq!(classify(&PgError::new(SqlState::OBJECT_IN_USE, "locked")), Exit::LOCKED);
        let lease = "TEST-00";
        assert_eq!(classify(&PgError::unimplemented(lease, "x")), Exit::UNSUPPORTED);
        assert_eq!(Exit::from_signal(15).code(), 143);
    }
}
