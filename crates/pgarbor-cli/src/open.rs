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
use pgarbor_common::PgError;
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
#[allow(unused_variables)]
pub fn open_database(path: &Path, spec: &OpenSpec) -> CliResult<Database> {
    Err(CliError::from(PgError::unimplemented("CLI-03", "open a database (open::open_database)")))
}

/// Raw read-only container open (module docs).
#[allow(unused_variables)]
pub fn open_container_readonly(path: &Path, verify: VerifyLevel) -> CliResult<Container> {
    Err(CliError::from(PgError::unimplemented("CLI-03", "open a container read-only (open::open_container_readonly)")))
}

/// Fail with exit 5 when `path` does not exist.
#[allow(unused_variables)]
pub fn require_exists(path: &Path) -> CliResult<()> {
    Err(CliError::from(PgError::unimplemented("CLI-03", "existence check (open::require_exists)")))
}
