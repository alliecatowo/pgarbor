//! `pgarbor init FILE` / `pgarbor create FILE` (lease CLI-06; CLI-01, harness-02 item 1).
//!
//! ## Behavior
//! 1. `FILE` exists:
//!    * with `--if-not-exists`: open it read-only (`open::open_container_readonly`, Roots verify); a
//!      valid PGArbor file -> success with `created = false`; anything else -> exit 3 (not a PGArbor
//!      database) so a typo never "succeeds";
//!    * otherwise exit 6 (`EXISTS`): `cannot create "<file>": file already exists` (stderr, non-zero as
//!      harness-02 requires). Never truncate or modify the existing file.
//! 2. Parent directory missing: exit 5 `directory "<dir>" does not exist`.
//! 3. Validate flags: `--page-size` a power of two in 4096..=65536 else exit 64; `--branch` must pass
//!    `pgarbor_branches::name::validate_branch_name` (error -> exit 64 with the validator's message).
//! 4. `Database::create(file, OpenOptions { page_size, initial_branch, sync: SyncMode::Full, ..})`
//!    creates the container with branch `main` (or `--branch`) and bootstraps the PG catalog (built-in
//!    `postgres` database and role). On ANY failure after the call started, delete the file if this
//!    invocation created it (never leave a half-initialized file), then report the error.
//! 5. `db.close()` (flush), then report.
//! 6. Unix: leave the file mode to the process umask (typically 0644); do not chmod.
//!
//! ## Output
//! Human (stdout): `created dev.pga (branch main, page size 8192, id <db uuid>)`; with `--quiet` nothing.
//! `--json`: [`crate::reports::InitReport`]. `db_uuid` from `db.engine().container().db_uuid().to_hyphenated()`.
use crate::cli::InitArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

#[allow(unused_variables)]
pub fn run(ctx: &mut Ctx, args: &InitArgs) -> CliResult<Exit> {
    let requested_page_size = args.page_size.unwrap_or(8192);
    if !(4096..=65536).contains(&requested_page_size) || !requested_page_size.is_power_of_two() {
        return Err(CliError::usage("page size must be a power of two in 4096..=65536"));
    }
    pgarbor_branches::name::validate_branch_name(&args.branch).map_err(|e| CliError::usage(e.message))?;
    let parent = args.file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    if !parent.is_dir() { return Err(CliError::not_found(format!("directory \"{}\" does not exist", parent.display()))); }
    let (created, uuid, page_size) = if args.file.exists() {
        if !args.if_not_exists { return Err(CliError::new(Exit::EXISTS, format!("cannot create \"{}\": file already exists", args.file.display()))); }
        let db = crate::open::open_container_readonly(&args.file, pgarbor_storage::VerifyLevel::Roots)?;
        (false, db.db_uuid(), db.page_size())
    } else {
        let opts = pgarbor::OpenOptions { page_size: requested_page_size, initial_branch: args.branch.clone(), ..Default::default() };
        // CreateNew in storage prevents clobbering a concurrent creator. Do not unlink on failure:
        // without an ownership token that could remove the concurrent creator's database.
        let db = pgarbor::Database::create(&args.file, opts)?;
        let uuid = db.engine().container().db_uuid();
        db.close()?;
        (true, uuid, requested_page_size)
    };
    let report = crate::reports::InitReport { path: args.file.display().to_string(), db_uuid: uuid.to_hyphenated(), branch: args.branch.clone(), page_size, created };
    if ctx.json { ctx.emit_json(&report)?; }
    else if !ctx.quiet { ctx.out_line(&format!("{} {} (branch {}, page size {}, id {})", if created { "created" } else { "exists" }, report.path, report.branch, report.page_size, report.db_uuid)); }
    Ok(Exit::OK)
}
