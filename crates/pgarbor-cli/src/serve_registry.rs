//! Discovery file of a serving process (lease CLI-23). Purpose: the `.pga` is exclusively locked by
//! the process that opened it (ADR-004), so other `pgarbor` invocations (`status`, `branch exec`,
//! lock-error diagnostics) need a way to find the owner and talk to it.
//!
//! ## Files (next to the database)
//! * `<db>.serve.json` - [`crate::reports::ServeInfo`] as pretty JSON, `format = "pgarbor-serve/1"`.
//!   Written atomically (write `<db>.serve.json.tmp`, `fsync`, `rename`) by `serve` after the listener
//!   is up; rewritten whenever listeners change (on-demand listeners); removed on clean shutdown.
//! * `<db>.serve.sock` - control socket (unix only, see `control.rs`). If the path would exceed
//!   100 bytes use `<temp_dir>/pgarbor-<16 hex of DefaultHasher(db absolute path)>.sock`.
//!
//! ## Liveness (stale files are normal after SIGKILL)
//! [`read_info`] returns `Some` only if: the file parses, `format` matches, `pid` is alive
//! (`signals::send_signal(pid, 0)`; EPERM counts as not alive: same-user assumption, documented), and
//! `db_path` canonicalizes to the same file as `db`. Otherwise it returns `None`; a stale file
//! (dead pid) is best-effort removed.
use pgarbor_common::PgError;
use std::path::{Path, PathBuf};

use crate::ctx::CliError;
use crate::reports::ServeInfo;

pub const INFO_FORMAT: &str = "pgarbor-serve/1";

/// `<db>.serve.json`.
pub fn info_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".serve.json");
    PathBuf::from(s)
}

/// Control socket path (module docs).
pub fn control_socket_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".serve.sock");
    let direct = PathBuf::from(s);
    if direct.as_os_str().len() <= 100 {
        return direct;
    }
    use std::hash::{Hash, Hasher};
    let abs = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    abs.hash(&mut h);
    std::env::temp_dir().join(format!("pgarbor-{:016x}.sock", h.finish()))
}

/// Atomically write the info file (module docs).
#[allow(unused_variables)]
pub fn write_info(db: &Path, info: &ServeInfo) -> Result<(), CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-23", "write the serve info file (serve_registry::write_info)")))
}

/// Read and validate (liveness, identity) the info file; `None` when absent or stale.
#[allow(unused_variables)]
pub fn read_info(db: &Path) -> Result<Option<ServeInfo>, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-23", "read the serve info file (serve_registry::read_info)")))
}

/// Remove the info file and the control socket file (best effort, ignores `NotFound`).
#[allow(unused_variables)]
pub fn remove_info(db: &Path) -> Result<(), CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-23", "remove the serve info file (serve_registry::remove_info)")))
}

/// Is `pid` a live process (`kill(pid, 0)`)?
pub fn process_alive(pid: u32) -> bool {
    crate::signals::send_signal(pid, 0)
}
