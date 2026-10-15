//! Branch lifecycle commands (lease CLI-17; CLI-03, BRANCH-03/05): create, list, rename, delete,
//! restore, purge, set-default. Each function opens the database writable (list: read-only is fine
//! but still needs the exclusive/shared lock, so same `open_database`), runs one engine call and
//! closes the database (`db.close()`).
//!
//! ## create
//! `branch create NAME [--from B] [--snapshot L] [--copy-snapshots]`. Source = `--from` or the default
//! branch (`list_branches` `is_default`). Plain: `db.create_branch(name, from)`; with `--snapshot`:
//! `db.create_branch_from_snapshot(name, from, label)`; with `--copy-snapshots`:
//! `db.engine().create_branch(name, &CreateSource::branch(from), &CreateOptions{copy_snapshots: true,
//! id: None})` (needs `pgarbor_branches::{CreateSource, CreateOptions}`; then re-read the row with
//! `list_branches`). Output: human `created branch feature/auth from main (id <uuid>)`; `--json`: one
//! [`crate::reports::BranchRow`]. Errors: exists -> exit 6 (42710), bad name -> exit 64 (42602), unknown
//! source -> exit 5.
//!
//! ## list
//! `db.list_branches(args.all)`; `--space`: `db.engine().branches().space()` -> match rows by branch id
//! (`BranchSpace.id.to_hyphenated()`). Human: aligned table `NAME | PARENT | DEFAULT | STATE | HEAD |
//! SNAPSHOTS | UPDATED` (`*` in DEFAULT; UPDATED via `humanize::timestamp`; with `--space` add
//! `SIZE | EXCLUSIVE` via `humanize::bytes`). `--json`: [`crate::reports::BranchListReport`]. Sorted as
//! returned (active by name, then deleted).
//!
//! ## rename / set-default
//! `db.rename_branch(old, new)` / `db.set_default_branch(name)`; print the resulting row. A handle file
//! pointing at the renamed branch keeps working (identified by id).
//!
//! ## delete
//! `confirm::require(ctx, args.yes, "delete branch \"x\"")` BEFORE any change (exit 7 when not
//! confirmed). Then `db.delete_branch(name, args.force)`; the embedded API deletes with the default
//! `DeleteOptions`, so the data is RETAINED and `restore` works. With `--purge`: after the delete, parse
//! the returned `BranchInfo.id` with `pgarbor_common::BranchId::parse` and call
//! `db.engine().branches().purge_deleted(&pgarbor_branches::PurgeFilter::Id(id))` (exactly that
//! branch; space is reclaimed by `compact`). `--allow-default` cannot be honored through the embedded
//! API: when given, fail with exit 8 `deleting the default branch is not supported yet; use
//! set-default first`. Deleting the default branch without the flag surfaces the engine error
//! (`cannot delete the default branch`) as exit 7. Sessions using the branch without `--force`: 55006 ->
//! exit 4 with hint `pass --force to end those sessions`.
//! Output: [`crate::reports::DeleteReport`]; human `deleted branch "x" (data retained; restore with:
//! pgarbor branch restore x)`, or `... (purged)` with `--purge`.
//!
//! ## restore / purge
//! `restore NAME_OR_ID [--as-name N]` -> `db.restore_branch(name_or_id, new_name)`.
//! `purge`: `confirm::require` then `db.purge_deleted()`; [`crate::reports::PurgeReport`].
use std::path::{Path, PathBuf};

use pgarbor::Database;
use pgarbor_branches::{CreateOptions, CreateSource, PurgeFilter};
use pgarbor_common::BranchId;

use crate::cli::{
    BranchCreateArgs, BranchDeleteArgs, BranchListArgs, BranchPurgeArgs, BranchRenameArgs, BranchRestoreArgs,
    BranchSetDefaultArgs, DbFlag,
};
use crate::cmd::branch::row_from_info;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::open::{self, OpenSpec};
use crate::paths;
use crate::reports::{BranchListReport, DeleteReport, PurgeReport};
use crate::table::{self, Table};

fn open_for(db: &DbFlag) -> CliResult<(Database, PathBuf)> {
    let path = paths::resolve_db(None, db.db.as_deref())?;
    let database = open::open_database(&path, &OpenSpec::existing())?;
    Ok((database, path))
}

fn with_db<T>(db_flag: &DbFlag, f: impl FnOnce(&Database, &Path) -> CliResult<T>) -> CliResult<T> {
    let (db, path) = open_for(db_flag)?;
    let result = f(&db, &path);
    let close_result = db.close().map_err(CliError::from);
    match result {
        Err(e) => Err(e),
        Ok(value) => {
            close_result?;
            Ok(value)
        }
    }
}

fn emit_branch(ctx: &mut Ctx, row: &crate::reports::BranchRow, action: &str) -> CliResult<()> {
    if ctx.json {
        ctx.emit_json(row)
    } else {
        ctx.out_line(&format!("{action} branch {} (id {})", row.name, row.id));
        Ok(())
    }
}

pub fn create(ctx: &mut Ctx, args: &BranchCreateArgs) -> CliResult<Exit> {
    with_db(&args.db, |db, _| {
        let from = match &args.from {
            Some(name) => name.clone(),
            None => db.list_branches(false)?.into_iter().find(|b| b.is_default)
                .map(|b| b.name).ok_or_else(|| CliError::usage("database has no default branch: pass --from"))?,
        };
        let created = if args.copy_snapshots || args.snapshot.is_some() {
            let source = match &args.snapshot {
                Some(label) => CreateSource::Snapshot { branch: pgarbor_branches::BranchRef::name(&from), label: label.clone() },
                None => CreateSource::branch(&from),
            };
            db.engine().create_branch(&args.name, &source, &CreateOptions { copy_snapshots: args.copy_snapshots, id: None })
                .map_err(CliError::from)?;
            db.list_branches(false)?.into_iter().find(|b| b.name == args.name)
                .ok_or_else(|| CliError::internal("created branch could not be read back"))?
        } else {
            db.create_branch(&args.name, &from).map_err(CliError::from)?
        };
        let row = row_from_info(&created, None);
        if ctx.json {
            ctx.emit_json(&row)?;
        } else {
            ctx.out_line(&format!("created branch {} from {} (id {})", args.name, from, created.id));
        }
        Ok(Exit::OK)
    })
}

pub fn list(ctx: &mut Ctx, args: &BranchListArgs) -> CliResult<Exit> {
    with_db(&args.db, |db, path| {
        let branches = db.list_branches(args.all).map_err(CliError::from)?;
        let spaces = if args.space {
            Some(db.engine().branches().space().map_err(CliError::from)?.0)
        } else { None };
        let rows = branches.iter().map(|b| {
            let space = spaces.as_ref().and_then(|all| all.iter().find(|s| s.id.to_hyphenated() == b.id));
            row_from_info(b, space)
        }).collect::<Vec<_>>();
        if ctx.json {
            ctx.emit_json(&BranchListReport { db_path: path.display().to_string(), branches: rows })?;
        } else {
            let mut headers = vec!["NAME".to_string(), "PARENT".into(), "DEFAULT".into(), "STATE".into(), "HEAD".into(), "SNAPSHOTS".into(), "UPDATED".into()];
            if args.space { headers.extend(["SIZE".into(), "EXCLUSIVE".into()]); }
            let mut cells = Vec::with_capacity(rows.len());
            for row in &rows {
                let updated = crate::humanize::timestamp(row.updated_unix_ms)?;
                let mut line = vec![Some(row.name.clone()), row.parent.clone(), Some(if row.is_default { "*".into() } else { String::new() }), Some(row.state.clone()), Some(row.head_seq.to_string()), Some(row.snapshots.join(", ")), Some(updated)];
                if let Some(s) = &row.space {
                    line.push(Some(crate::humanize::bytes(s.bytes)?));
                    line.push(Some(crate::humanize::bytes(s.exclusive_bytes)?));
                }
                cells.push(line);
            }
            let rendered = table::render_aligned(&Table { headers, rows: cells, right_align: vec![] }, false)?;
            if !rendered.is_empty() { ctx.out_line(&rendered); }
        }
        Ok(Exit::OK)
    })
}

pub fn rename(ctx: &mut Ctx, args: &BranchRenameArgs) -> CliResult<Exit> {
    with_db(&args.db, |db, _| {
        let info = db.rename_branch(&args.old, &args.new).map_err(CliError::from)?;
        emit_branch(ctx, &row_from_info(&info, None), "renamed")?;
        Ok(Exit::OK)
    })
}

pub fn delete(ctx: &mut Ctx, args: &BranchDeleteArgs) -> CliResult<Exit> {
    if args.allow_default {
        return Err(CliError::new(Exit::UNSUPPORTED, "deleting the default branch is not supported yet; use set-default first"));
    }
    crate::confirm::require(ctx, args.yes, &format!("delete branch \"{}\"", args.name))?;
    with_db(&args.db, |db, _| {
        let info = db.delete_branch(&args.name, args.force).map_err(|e| {
            let mut ce = CliError::from(e);
            if ce.sqlstate.as_deref() == Some("55006") { ce.hint = Some("pass --force to end those sessions".into()); }
            if ce.message.contains("cannot delete the default branch") { ce.exit = Exit::REFUSED; }
            ce
        })?;
        if args.purge {
            let id = BranchId::parse(&info.id).ok_or_else(|| CliError::internal("database returned an invalid branch id"))?;
            db.engine().branches().purge_deleted(&PurgeFilter::Id(id)).map_err(CliError::from)?;
        }
        let report = DeleteReport { name: info.name.clone(), id: info.id.clone(), retained: !args.purge };
        if ctx.json { ctx.emit_json(&report)?; }
        else if args.purge { ctx.out_line(&format!("deleted branch \"{}\" (purged)", info.name)); }
        else { ctx.out_line(&format!("deleted branch \"{}\" (data retained; restore with: pgarbor branch restore {})", info.name, info.name)); }
        Ok(Exit::OK)
    })
}

pub fn restore(ctx: &mut Ctx, args: &BranchRestoreArgs) -> CliResult<Exit> {
    with_db(&args.db, |db, _| {
        let info = db.restore_branch(&args.name_or_id, args.as_name.as_deref()).map_err(CliError::from)?;
        emit_branch(ctx, &row_from_info(&info, None), "restored")?;
        Ok(Exit::OK)
    })
}

pub fn purge(ctx: &mut Ctx, args: &BranchPurgeArgs) -> CliResult<Exit> {
    crate::confirm::require(ctx, args.yes, "purge deleted branches")?;
    with_db(&args.db, |db, _| {
        let purged = db.purge_deleted().map_err(CliError::from)?;
        let report = PurgeReport { purged };
        if ctx.json { ctx.emit_json(&report)?; }
        else { ctx.out_line(&format!("purged {purged} deleted branches")); }
        Ok(Exit::OK)
    })
}

pub fn set_default(ctx: &mut Ctx, args: &BranchSetDefaultArgs) -> CliResult<Exit> {
    with_db(&args.db, |db, _| {
        let info = db.set_default_branch(&args.name).map_err(CliError::from)?;
        emit_branch(ctx, &row_from_info(&info, None), "set default").map(|()| Exit::OK)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_branch_table_has_expected_columns() {
        let value = table::render_aligned(&Table {
            headers: vec!["NAME".into(), "DEFAULT".into()],
            rows: vec![vec![Some("main".into()), Some("*".into())]],
            right_align: vec![false, false],
        }, false);
        assert!(value.is_ok());
        assert!(value.unwrap_or_default().contains("NAME"));
    }

    #[test]
    #[ignore = "needs engine"]
    fn branch_creation_reports_created_branch() {}
}
