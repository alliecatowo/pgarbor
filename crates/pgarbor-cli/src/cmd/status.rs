//! `pgarbor status` (lease CLI-12): one-screen summary that works on files the SQL layer cannot open.
//!
//! 1. `resolve_db(file, db)`. `StatusReport.exists/file_len` from `std::fs::metadata`. Missing ->
//!    report with `exists = false`, `readable = false`, problem `file does not exist`, exit 5.
//! 2. `serve_registry::read_info(path)` -> `held_by` (a live `pgarbor serve`).
//! 3. `open::open_container_readonly(path, VerifyLevel::Roots)` (shared lock). Success -> fill
//!    `db_uuid`, `format_version`, `page_size`, `commit_seq` from `Container::superblock()` /
//!    accessors; `pgarbor_branches::BranchManager::new(container)` then `list(true)` ->
//!    `branches` (active), `deleted_branches`, `default_branch` (`default_branch()` name).
//!    Failure: `readable = false` and a problem string with the error message; exit code from
//!    `exit::classify` (3 corrupt, 4 locked: when `held_by` is `Some` a locked file is NOT a
//!    problem - say `served by pgarbor pid N`), report still printed.
//! 4. Human output (stdout), aligned `key: value` lines:
//!    ```text
//!    file:      dev.pga (1.2 MiB)
//!    id:        6f1c...-...
//!    format:    1 (page size 8192, commit 42)
//!    branches:  3 active, 1 deleted (default: main)
//!    served by: pgarbor pid 4242 on 127.0.0.1:5432 (branch main)
//!    problems:  none
//!    ```
//!    `--json`: [`crate::reports::StatusReport`]. Exit 0 when readable (or served) without problems.
use pgarbor_common::PgError;
use crate::cli::FileArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &FileArgs) -> CliResult<Exit> {
    Err(CliError::from(PgError::unimplemented("CLI-12", "status (cmd::status::run)")))
}
