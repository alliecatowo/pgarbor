//! `pgarbor verify` (lease CLI-14): integrity check, exit 3 on corruption.
//!
//! `pgarbor_storage::doctor::verify_file(Arc::new(OsVfs::default()), path, &BranchRootResolver, level)`
//! (`--level none|roots|full`, default full) reads the file without recovery side effects. Outcomes:
//! * `report.is_ok()` -> exit 0. Warnings and info issues are printed but do not fail.
//! * any `Error` issue -> exit 3 and stderr contains the word `corrupt` (e.g. `database file is
//!   corrupt: 2 errors (first: <message>)`).
//! * file missing -> exit 5; lock held by another process -> exit 4 (hint from `open.rs`).
//! Human output: `verified dev.pga: 1342 pages checked, 1201 reachable, 141 unreachable (garbage) - OK`
//! then one line per issue `error|warning|info: page N (tree T): message`. `--json`: the
//! `DoctorReport` itself (`report.to_json()` shape via serde), printed on stdout also when exit is 3.
use std::sync::Arc;

use pgarbor_branches::BranchRootResolver;
use crate::cli::VerifyArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use pgarbor_storage::doctor::{verify_file, Issue, IssueSeverity};
use pgarbor_storage::{OsVfs, VerifyLevel};

pub fn run(ctx: &mut Ctx, args: &VerifyArgs) -> CliResult<Exit> {
    let path = super::super::dispatch::db_path_of(&args.file, &args.db)?;
    let level = match args.level {
        crate::cli::LevelArg::None => VerifyLevel::None,
        crate::cli::LevelArg::Roots => VerifyLevel::Roots,
        crate::cli::LevelArg::Full => VerifyLevel::Full,
    };
    let report = verify_file(Arc::new(OsVfs::default()), &path, &BranchRootResolver, level)
        .map_err(CliError::from)?;

    if ctx.json {
        ctx.emit_json(&report)?;
    } else {
        let display_path = path.display().to_string();
        ctx.out_line(&format!(
            "verified {display_path}: {} pages checked, {} reachable, {} unreachable (garbage) - {}",
            report.pages_checked,
            report.reachable_pages,
            report.unreachable_pages,
            if report.is_ok() { "OK" } else { "CORRUPT" },
        ));
        for issue in &report.issues {
            ctx.out_line(&format_issue(issue));
        }
    }

    if report.is_ok() {
        Ok(Exit::OK)
    } else {
        Err(CliError::new(Exit::CORRUPT, "database file is corrupt: integrity check found errors"))
    }
}

fn format_issue(issue: &Issue) -> String {
    let severity = match issue.severity {
        IssueSeverity::Error => "error",
        IssueSeverity::Warning => "warning",
        IssueSeverity::Info => "info",
    };
    let mut line = format!("{severity}: ");
    if let Some(page) = issue.page {
        line.push_str(&format!("page {}", page.0));
        if let Some(tree) = &issue.tree {
            line.push_str(&format!(" (tree {tree})"));
        }
        line.push_str(": ");
    }
    line.push_str(&issue.message);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::PageId;

    #[test]
    fn issue_lines_include_severity_and_location() {
        let issue = Issue::error(Some(PageId(7)), Some("main"), "checksum mismatch");
        assert_eq!(format_issue(&issue), "error: page 7 (tree main): checksum mismatch");
    }

    #[test]
    fn issue_lines_without_page_still_include_message() {
        let issue = Issue::warning(None, None, "unused tail bytes");
        assert_eq!(format_issue(&issue), "warning: unused tail bytes");
    }
}
