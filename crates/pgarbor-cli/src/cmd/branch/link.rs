//! `pgarbor branch link` and `branch resolve` (lease CLI-19; BRANCH-07, RFC 001 section 4.1, CLI-05).
//! A `.pgabranch` handle is a tiny JSON file naming a branch by IDENTITY (`format:"pgabranch/1"`,
//! `db_uuid`, `branch_id`, name/path hints, crc32c); see `pgarbor_branches::handle`.
//!
//! ## link
//! `branch link BRANCH HANDLE [--overwrite] [--absolute] [--db FILE]`: resolve the db (`paths::resolve_db`),
//! open it (`open::open_database`, writable not required but the shared helper is fine), then
//! `db.engine().branches().write_handle(&BranchRef::name(branch), handle_path, &HandleOptions{overwrite,
//! absolute_db_path})`. Existing handle without `--overwrite`: exit 6 (58P02). The handle's directory
//! must exist (exit 5). Output [`crate::reports::LinkReport`]; human `linked branch "x" -> ./x.pgabranch
//! (database dev.pga)`.
//!
//! ## resolve
//! `branch resolve HANDLE [--db FILE] [--rewrite-hint]`: validate WITHOUT starting anything.
//! 1. `pgarbor_branches::handle::resolve_db_path(&OsVfs::default(), handle, explicit_db)` finds the master
//!    (hint path relative to the handle, then absolute, then `--db`); not found -> exit 5 listing the
//!    tried paths; malformed/checksum failure -> exit 65.
//! 2. Open the master READ-ONLY (`OpenSpec::read_only()`), compare `Container::db_uuid` with the handle's
//!    `db_uuid`: mismatch -> exit 65 `handle "H" belongs to database <uuid>, but "<path>" is database
//!    <uuid2>` (loud, per RFC; use `HandleFile::check`).
//! 3. Find the branch by id among `list_branches(true)`: present+active -> `branch_exists = true` with
//!    its CURRENT name (may differ from the hint after a rename); deleted -> `branch_exists = false`
//!    plus note `branch was deleted`; absent -> `branch_exists = false`.
//! 4. `relocated` = found path differs (after lexical normalization) from the handle's resolved hint
//!    path. With `--rewrite-hint` and `relocated`, rewrite the handle via `write_handle(.., overwrite:
//!    true, ..)` so the new relative path is stored (`hint_rewritten = true`).
//! Exit: 0 when the branch exists; 5 when the branch is gone. Output [`crate::reports::ResolveReport`].
use std::path::{Component, Path, PathBuf};

use crate::cli::{BranchLinkArgs, BranchResolveArgs};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{open, paths};
use crate::reports::{LinkReport, ResolveReport};
use pgarbor_branches::{BranchRef, HandleOptions};
use pgarbor_branches::handle;
use pgarbor_storage::OsVfs;

pub fn link(ctx: &mut Ctx, args: &BranchLinkArgs) -> CliResult<Exit> {
    let db_path = paths::resolve_db(None, args.db.db.as_deref())?;
    let db = open::open_database(&db_path, &open::OpenSpec::existing())?;
    let written = db.engine().branches().write_handle(
        &BranchRef::name(args.branch.clone()),
        &args.handle,
        &HandleOptions { overwrite: args.overwrite, absolute_db_path: args.absolute },
    ).map_err(CliError::from)?;
    let report = LinkReport {
        handle_path: args.handle.display().to_string(),
        db_path: db.path().display().to_string(),
        db_uuid: written.db_uuid.to_hyphenated(),
        branch: written.branch_name_hint,
        branch_id: written.branch_id.to_hyphenated(),
    };
    if ctx.json {
        ctx.emit_json(&report)?;
    } else {
        ctx.out_line(&format!("linked branch \"{}\" -> {} (database {})", report.branch, args.handle.display(), db.path().display()));
    }
    Ok(Exit::OK)
}

pub fn resolve(ctx: &mut Ctx, args: &BranchResolveArgs) -> CliResult<Exit> {
    let resolved = handle::resolve_db_path(&OsVfs::default(), &args.handle, args.db.db.as_deref())
        .map_err(|e| {
            if e.sqlstate.code() == "22023" { CliError::data(e.message.clone()) }
            else { CliError::from(e) }
        })?;
    let db = open::open_database(&resolved.db_path, &open::OpenSpec::read_only())?;
    let actual_uuid = db.engine().container().db_uuid();
    if actual_uuid != resolved.handle.db_uuid {
        return Err(CliError::data(format!(
            "handle \"{}\" belongs to database {}, but \"{}\" is database {}",
            args.handle.display(), resolved.handle.db_uuid.to_hyphenated(), resolved.db_path.display(), actual_uuid.to_hyphenated()
        )));
    }
    let branches = db.list_branches(true).map_err(CliError::from)?;
    let found = branches.iter().find(|b| b.id == resolved.handle.branch_id.to_hyphenated());
    let exists = found.map(|b| !b.deleted).unwrap_or(false);
    let hint_path = if Path::new(&resolved.handle.db_path_hint).is_absolute() {
        lexical_normalize(Path::new(&resolved.handle.db_path_hint))
    } else {
        lexical_normalize(&args.handle.parent().unwrap_or_else(|| Path::new(".")).join(&resolved.handle.db_path_hint))
    };
    let relocated = lexical_normalize(&resolved.db_path) != hint_path;
    let mut hint_rewritten = false;
    if args.rewrite_hint && relocated && exists {
        let options = HandleOptions { overwrite: true, absolute_db_path: false };
        db.engine().branches().write_handle(&BranchRef::id(resolved.handle.branch_id), &args.handle, &options)
            .map_err(CliError::from)?;
        hint_rewritten = true;
    }
    if found.map(|b| b.deleted).unwrap_or(false) {
        ctx.info("branch was deleted");
    }
    let report = ResolveReport {
        handle_path: args.handle.display().to_string(),
        db_path: resolved.db_path.display().to_string(),
        db_uuid: actual_uuid.to_hyphenated(),
        branch_id: resolved.handle.branch_id.to_hyphenated(),
        branch_name: found.filter(|b| !b.deleted).map(|b| b.name.clone()),
        branch_exists: exists,
        relocated,
        hint_rewritten,
    };
    if ctx.json {
        ctx.emit_json(&report)?;
    } else {
        ctx.out_line(&format!("{}", serde_json::to_string_pretty(&report).map_err(|e| CliError::internal(format!("json serialization failed: {e}")))?));
    }
    if exists { Ok(Exit::OK) } else { Ok(Exit::NOT_FOUND) }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) { out.pop(); }
                else if !out.is_absolute() { out.push(".."); }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { out.push("."); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocation_uses_lexically_normalized_paths() {
        assert_eq!(lexical_normalize(Path::new("/work/a/../db.pga")), PathBuf::from("/work/db.pga"));
        assert_eq!(lexical_normalize(Path::new("./branches/../db.pga")), PathBuf::from("db.pga"));
        assert_ne!(lexical_normalize(Path::new("/old/db.pga")), lexical_normalize(Path::new("/new/db.pga")));
    }
}
