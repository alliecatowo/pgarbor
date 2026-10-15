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
//! 4. Human output (stdout), aligned `key: value` lines. `--json` emits `StatusReport`.
use pgarbor_branches::{BranchManager, BranchState};
use pgarbor_storage::VerifyLevel;

use crate::cli::FileArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::reports::StatusReport;

pub fn run(ctx: &mut Ctx, args: &FileArgs) -> CliResult<Exit> {
    let path = crate::paths::resolve_db(args.file.as_deref(), args.db.db.as_deref())?;
    let path_text = path.to_string_lossy().into_owned();
    let mut report = StatusReport {
        path: path_text,
        exists: false,
        file_len: None,
        readable: false,
        db_uuid: None,
        format_version: None,
        page_size: None,
        commit_seq: None,
        default_branch: None,
        branches: None,
        deleted_branches: None,
        held_by: None,
        problems: Vec::new(),
    };

    match std::fs::metadata(&path) {
        Ok(metadata) => {
            report.exists = true;
            report.file_len = Some(metadata.len());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.problems.push("file does not exist".to_string());
            emit_report(ctx, &report)?;
            return Err(CliError::not_found(format!("database file \"{}\" does not exist", path.display())));
        }
        Err(error) => return Err(CliError::io("could not stat database file", &error)),
    }

    report.held_by = crate::serve_registry::read_info(&path)?;
    let mut failure = None;
    match crate::open::open_container_readonly(&path, VerifyLevel::Roots) {
        Ok(container) => {
            let sb = container.superblock();
            report.db_uuid = Some(container.db_uuid().to_hyphenated());
            report.format_version = Some(sb.format_version);
            report.page_size = Some(container.page_size());
            report.commit_seq = Some(container.commit_seq().0);
            let manager = BranchManager::new(container);
            match manager.list(true) {
                Ok(branches) => {
                    let mut active = 0;
                    let mut deleted = 0;
                    for branch in &branches {
                        match branch.state {
                            BranchState::Active => active += 1,
                            BranchState::Deleted => deleted += 1,
                        }
                    }
                    report.branches = Some(active);
                    report.deleted_branches = Some(deleted);
                }
                Err(error) => {
                    failure = Some(CliError::from(error.clone()));
                    report.problems.push(error.message);
                }
            }
            if failure.is_none() {
                match manager.default_branch() {
                    Ok(branch) => report.default_branch = branch.map(|record| record.name),
                    Err(error) => {
                        failure = Some(CliError::from(error.clone()));
                        report.problems.push(error.message);
                    }
                }
            }
            if failure.is_none() {
                report.readable = true;
            }
        }
        Err(error) => {
            let locked_and_served = error.exit == Exit::LOCKED && report.held_by.is_some();
            if locked_and_served {
                report.readable = true;
            } else {
                report.problems.push(error.message.clone());
                failure = Some(error);
            }
        }
    }

    emit_report(ctx, &report)?;
    match failure {
        Some(error) => Err(error),
        None => Ok(Exit::OK),
    }
}

fn emit_report(ctx: &mut Ctx, report: &StatusReport) -> CliResult<()> {
    if ctx.json {
        ctx.emit_json(report)
    } else {
        ctx.out_line(&format_status(report));
        Ok(())
    }
}

fn format_status(report: &StatusReport) -> String {
    let mut lines = Vec::new();
    let file_value = match report.file_len {
        Some(bytes) => format!("{} ({})", report.path, format_bytes(bytes)),
        None => report.path.clone(),
    };
    lines.push(("file", file_value));
    if let Some(id) = &report.db_uuid {
        lines.push(("id", id.clone()));
    }
    if let (Some(version), Some(page_size), Some(commit)) =
        (report.format_version, report.page_size, report.commit_seq)
    {
        lines.push(("format", format!("{version} (page size {page_size}, commit {commit})")));
    }
    if let (Some(active), Some(deleted)) = (report.branches, report.deleted_branches) {
        let default = report.default_branch.as_deref().map(|name| format!(" (default: {name})")).unwrap_or_default();
        lines.push(("branches", format!("{active} active, {deleted} deleted{default}")));
    }
    if let Some(info) = &report.held_by {
        let endpoint = info.listeners.first().map(|listener| {
            format!(" on {}:{} (branch {})", listener.host, listener.port, listener.branch)
        }).unwrap_or_default();
        lines.push(("served by", format!("pgarbor pid {}{}", info.pid, endpoint)));
    }
    lines.push(("problems", if report.problems.is_empty() { "none".to_string() } else { report.problems.join("; ") }));

    let width = lines.iter().map(|(key, _)| key.chars().count()).max().unwrap_or(0);
    lines.into_iter().map(|(key, value)| format!("{:<label_width$}{value}", format!("{key}:"), label_width = width + 3)).collect::<Vec<_>>().join("\n")
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    crate::humanize::bytes(bytes).unwrap_or_else(|_| format!("{bytes} B"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_report(path: &str) -> StatusReport {
        StatusReport {
            path: path.to_string(), exists: false, file_len: None, readable: false,
            db_uuid: None, format_version: None, page_size: None, commit_seq: None,
            default_branch: None, branches: None, deleted_branches: None, held_by: None,
            problems: vec!["file does not exist".to_string()],
        }
    }

    #[test]
    fn formats_missing_file() {
        assert_eq!(format_status(&blank_report("dev.pga")), "file:      dev.pga\nproblems:  file does not exist");
    }

    #[test]
    fn formats_healthy_file() {
        let mut report = blank_report("dev.pga");
        report.exists = true;
        report.readable = true;
        report.file_len = Some(1_258_291);
        report.db_uuid = Some("6f1c0000-0000-0000-0000-000000000000".to_string());
        report.format_version = Some(1);
        report.page_size = Some(8192);
        report.commit_seq = Some(42);
        report.branches = Some(3);
        report.deleted_branches = Some(1);
        report.default_branch = Some("main".to_string());
        report.problems.clear();
        assert_eq!(format_status(&report), "file:      dev.pga (1.2 MiB)\nid:        6f1c0000-0000-0000-0000-000000000000\nformat:    1 (page size 8192, commit 42)\nbranches:  3 active, 1 deleted (default: main)\nproblems:  none");
    }
}
