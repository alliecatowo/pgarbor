//! Opening database files with CLI-quality errors (lease CLI-03). RFC 001 sections 5.2, 8.
//!
//! * [`open_database`]: `Database::open` (or `open_or_create`/`create` per [`OpenSpec`]) with
//!   `OpenOptions { read_only, sync: SyncMode::Full, verify, .. }`.
//!   - missing file (58P01) -> exit 5 `database file "<path>" does not exist`, hint `create it with:
//!     pgarbor init <path>`;
//!   - not a PGArbor file / unsupported version (XX001 from `not_a_database` / `unsupported_version`) ->
//!     exit 3 with the storage message (must contain the word `corrupt` or `not a PGArbor database`;
//!     `CliError::from(PgError)` adds `corrupt` when missing);
//!   - lock held by another process (55006) -> exit 4 `database file "<path>" is in use by another
//!     process`, enriched from `serve_registry::read_info(path)` when a live `pgarbor serve` owns it:
//!     `... in use by pgarbor serve (pid N, branch B on 127.0.0.1:P)` plus hint `use pgarbor branch
//!     exec to reach other branches of a served database, or stop the server`;
//!   - read-only violation 25006 -> exit 7.
//! * [`open_container_readonly`]: raw `pgarbor_storage::Container::open` read-only without any catalog
//!   bootstrap, for `status`/`inspect`/`verify`/`doctor` which must work on files the SQL layer cannot
//!   open. A shared-lock failure falls back to `Err` with the same exit-4 enrichment.
//! * [`require_exists`]: stat the path; missing -> exit 5 as above (used before commands that must not
//!   create files).
use pgarbor_common::{PgError, SqlState};
use std::path::Path;

use pgarbor::Database;
use pgarbor_storage::{Container, VerifyLevel};

use crate::ctx::{CliError, CliResult};

/// How to open.
// CONTRACT
#[derive(Clone, Debug)]
pub struct OpenSpec {
    pub read_only: bool,
    /// Create the file when missing (`import --create`).
    pub create: bool,
    /// Verification level performed at open.
    pub verify: VerifyLevel,
    /// Name of the first branch when creating.
    pub initial_branch: String,
    /// Page size when creating.
    pub page_size: Option<u32>,
}

impl OpenSpec {
    /// Writable open of an existing database, `VerifyLevel::Roots`.
    pub fn existing() -> OpenSpec {
        OpenSpec { read_only: false, create: false, verify: VerifyLevel::Roots, initial_branch: "main".to_string(), page_size: None }
    }
    pub fn read_only() -> OpenSpec {
        OpenSpec { read_only: true, ..OpenSpec::existing() }
    }
}

/// Open through the embedded API (module docs).
pub fn open_database(path: &Path, spec: &OpenSpec) -> CliResult<Database> {
    let opts = pgarbor::OpenOptions {
        read_only: spec.read_only,
        verify: spec.verify,
        page_size: spec.page_size.unwrap_or(8192),
        initial_branch: spec.initial_branch.clone(),
        ..pgarbor::OpenOptions::default()
    };
    let result = if spec.create {
        if spec.read_only { Database::open(path, opts) } else { Database::open_or_create(path, opts) }
    } else {
        Database::open(path, opts)
    };
    result.map_err(|err| cli_open_error(path, err))
}

/// Raw read-only container open (module docs).
pub fn open_container_readonly(path: &Path, verify: VerifyLevel) -> CliResult<Container> {
    let opts = pgarbor_storage::container::options::OpenOptions {
        read_only: true,
        verify,
        ..pgarbor_storage::container::options::OpenOptions::default()
    };
    Container::open(path, &opts).map_err(|err| cli_open_error(path, err))
}

/// Fail with exit 5 when `path` does not exist.
pub fn require_exists(path: &Path) -> CliResult<()> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(missing_file(path)),
        Err(err) => Err(CliError::io(&format!("could not access database file \"{}\"", path.display()), &err)),
    }
}

fn missing_file(path: &Path) -> CliError {
    CliError::not_found(format!("database file \"{}\" does not exist", path.display()))
        .with_hint(format!("create it with: pgarbor init {}", path.display()))
}

fn cli_open_error(path: &Path, err: PgError) -> CliError {
    if err.sqlstate == SqlState::OBJECT_IN_USE {
        let mut cli = CliError::from(err);
        cli.message = format!("database file \"{}\" is in use by another process", path.display());
        cli.hint = Some("Close the other PGArbor process, or connect to it through its server.".to_string());
        if let Ok(Some(info)) = crate::serve_registry::read_info(path) {
            if let Some(listener) = info.listeners.first() {
                cli.message = format!(
                    "database file \"{}\" is in use by pgarbor serve (pid {}, branch {} on {}:{})",
                    path.display(), info.pid, listener.branch, listener.host, listener.port
                );
                cli.hint = Some("use pgarbor branch exec to reach other branches of a served database, or stop the server".to_string());
            }
        }
        return cli;
    }
    if err.sqlstate == SqlState::UNDEFINED_FILE {
        return missing_file(path);
    }
    CliError::from(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_has_init_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.pga");
        let err = require_exists(&path).expect_err("missing path must fail");
        assert_eq!(err.exit, crate::exit::Exit::NOT_FOUND);
        assert_eq!(err.message, format!("database file \"{}\" does not exist", path.display()));
        assert!(err.hint.as_deref().unwrap_or_default().starts_with("create it with: pgarbor init "));
    }

    #[test]
    fn require_exists_accepts_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("present.pga");
        std::fs::write(&path, b"").expect("write test file");
        assert!(require_exists(&path).is_ok());
    }

    #[test]
    fn garbage_file_is_reported_as_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("garbage.pga");
        std::fs::write(&path, b"not a database").expect("write test file");
        let err = open_container_readonly(&path, VerifyLevel::Roots).expect_err("garbage must fail");
        assert_eq!(err.exit, crate::exit::Exit::CORRUPT);
        assert!(err.message.contains("not a PGArbor database") || err.message.contains("corrupt"));
    }
}
