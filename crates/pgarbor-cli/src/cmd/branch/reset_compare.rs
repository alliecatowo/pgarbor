//! `pgarbor branch reset` and `branch compare` (lease CLI-20; BRANCH-06/08, RFC 001 section 9).
//!
//! ## reset
//! Exactly one target flag (`--to-snapshot L`, `--to-branch B`, `--to-parent`), else exit 64
//! `specify exactly one of --to-snapshot, --to-branch, --to-parent`. Mapping to
//! `pgarbor_branches::ResetTarget::{Snapshot, Branch(BranchRef::name), Parent}`.
//! 1. `confirm::require(ctx, yes, "reset branch \"x\" to <target>: all changes since are discarded")`.
//! 2. `--recovery-snapshot L`: FIRST `db.create_snapshot(name, L)` (the Database API's `reset_branch`
//!    takes no `ResetOptions`; two commits, snapshot first so a failure never loses data). Without it
//!    the old head becomes garbage at the next compaction - print that as a note on stderr unless
//!    `--quiet`.
//! 3. `db.reset_branch(name, target, args.force)` (55006 when other sessions use the branch and no
//!    `--force`: exit 4 with hint `pass --force to end those sessions or stop the server`).
//! 4. Output: the resulting branch row; human `reset branch "x" to snapshot "s" (head commit N)`;
//!    `--json`: [`crate::reports::BranchRow`] plus `"recovery_snapshot"` as a wrapper
//!    `{"branch": <row>, "recovery_snapshot": "L" | null}`.
//!
//! ## compare
//! `db.engine().branches().compare(&BranchRef::name(a), &BranchRef::name(b), &CompareOptions{
//! max_differences, samples_per_space: samples, include_pages: !no_pages})` -> `CompareReport`.
//! `--json`: the report as-is. Human:
//! ```text
//! a: main (commit 42)   b: feature (commit 45)
//! identical: no   (b is a child of a)
//! SPACE       ONLY IN a   ONLY IN b   CHANGED
//! catalog             0          12         3
//! sequences           0           0         1
//! data (16384+)       0         100         7
//! pages: 120 shared, 0 exclusive to a, 14 exclusive to b
//! ```
//! (`SpaceDiff.space` names via Debug; counts from the report; sample keys listed below the table with
//! `-v`.) `truncated` -> note `difference limit reached; counts are lower bounds`. Exit 0 whether or
//! not they differ (use `--json` + `identical` for scripts). Unknown branch -> exit 5.
use pgarbor_branches::{BranchRef, CompareOptions, ResetTarget};
use crate::cli::{BranchCompareArgs, BranchResetArgs};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{confirm, open, paths};

fn select_reset_target(args: &BranchResetArgs) -> Result<ResetTarget, CliError> {
    let selected = (if args.to_snapshot.is_some() { 1 } else { 0 })
        + (if args.to_branch.is_some() { 1 } else { 0 })
        + (if args.to_parent { 1 } else { 0 });
    if selected != 1 {
        return Err(CliError::usage(
            "specify exactly one of --to-snapshot, --to-branch, --to-parent",
        ));
    }
    if let Some(label) = &args.to_snapshot {
        Ok(ResetTarget::Snapshot(label.clone()))
    } else if let Some(branch) = &args.to_branch {
        Ok(ResetTarget::Branch(BranchRef::name(branch.clone())))
    } else {
        Ok(ResetTarget::Parent)
    }
}

pub fn reset(ctx: &mut Ctx, args: &BranchResetArgs) -> CliResult<Exit> {
    let target = select_reset_target(args)?;
    let target_text = match &target {
        ResetTarget::Snapshot(label) => format!("snapshot \"{label}\""),
        ResetTarget::Branch(branch) => format!("branch \"{branch}\""),
        ResetTarget::Parent => "parent".to_string(),
    };
    confirm::require(
        ctx,
        args.yes,
        &format!(
            "reset branch \"{}\" to {target_text}: all changes since are discarded",
            args.name
        ),
    )?;

    let path = paths::resolve_db(None, args.db.db.as_deref())?;
    let db = open::open_database(&path, &open::OpenSpec::existing())?;
    if let Some(label) = &args.recovery_snapshot {
        db.create_snapshot(&args.name, label).map_err(CliError::from)?;
    }
    let info = db
        .reset_branch(&args.name, target, args.force)
        .map_err(|error| {
            let mut cli = CliError::from(error);
            if cli.sqlstate.as_deref() == Some("55006") {
                cli.hint = Some("pass --force to end those sessions or stop the server".to_string());
            }
            cli
        })?;
    db.close().map_err(CliError::from)?;

    if args.recovery_snapshot.is_none() {
        ctx.info("note: the old head becomes garbage at the next compaction");
    }
    let row = crate::cmd::branch::row_from_info(&info, None);
    if ctx.json {
        ctx.emit_json(&serde_json::json!({
            "branch": row,
            "recovery_snapshot": &args.recovery_snapshot,
        }))?;
    } else if let Some(label) = &args.recovery_snapshot {
        ctx.out_line(&format!(
            "reset branch \"{}\" to {target_text} (recovery snapshot \"{label}\", head commit {})",
            info.name, info.head_seq
        ));
    } else {
        ctx.out_line(&format!(
            "reset branch \"{}\" to {target_text} (head commit {})",
            info.name, info.head_seq
        ));
    }
    Ok(Exit::OK)
}

pub fn compare(ctx: &mut Ctx, args: &BranchCompareArgs) -> CliResult<Exit> {
    let path = paths::resolve_db(None, args.db.db.as_deref())?;
    let db = open::open_database(&path, &open::OpenSpec::read_only())?;
    let report = db
        .engine()
        .branches()
        .compare(
            &BranchRef::name(args.a.clone()),
            &BranchRef::name(args.b.clone()),
            &CompareOptions {
                max_differences: args.max_differences,
                samples_per_space: args.samples,
                include_pages: !args.no_pages,
            },
        )
        .map_err(CliError::from)?;
    db.close().map_err(CliError::from)?;

    if ctx.json {
        ctx.emit_json(&report)?;
        return Ok(Exit::OK);
    }

    let lineage = if report.a_is_parent_of_b {
        " (b is a child of a)"
    } else if report.b_is_parent_of_a {
        " (a is a child of b)"
    } else {
        ""
    };
    ctx.out_line(&format!(
        "a: {} (commit {})   b: {} (commit {})",
        report.a.name, report.a.head_seq.0, report.b.name, report.b.head_seq.0
    ));
    ctx.out_line(&format!(
        "identical: {}{}",
        if report.identical { "yes" } else { "no" },
        lineage
    ));

    let mut table = crate::table::Table {
        headers: vec!["SPACE".into(), "ONLY IN a".into(), "ONLY IN b".into(), "CHANGED".into()],
        right_align: vec![false, true, true, true],
        ..crate::table::Table::default()
    };
    for diff in &report.spaces {
        table.rows.push(vec![
            Some(format!("{:?}", diff.space)),
            Some(diff.only_in_a.to_string()),
            Some(diff.only_in_b.to_string()),
            Some(diff.changed.to_string()),
        ]);
    }
    let rendered = crate::table::render_aligned(&table, false)?;
    for line in rendered.lines() {
        ctx.out_line(line);
    }
    if let (Some(shared), Some(exclusive_a), Some(exclusive_b)) = (
        report.shared_pages,
        report.exclusive_pages_a,
        report.exclusive_pages_b,
    ) {
        ctx.out_line(&format!(
            "pages: {shared} shared, {exclusive_a} exclusive to a, {exclusive_b} exclusive to b"
        ));
    }
    if ctx.verbose > 0 {
        for diff in &report.spaces {
            for sample in &diff.samples {
                ctx.out_line(&format!("{:?} {} {}", diff.space, sample.change, sample.key_hex));
            }
        }
    }
    if report.truncated {
        ctx.info("difference limit reached; counts are lower bounds");
    }
    Ok(Exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(snapshot: Option<&str>, branch: Option<&str>, parent: bool) -> BranchResetArgs {
        BranchResetArgs {
            name: "feature".into(),
            to_snapshot: snapshot.map(str::to_string),
            to_branch: branch.map(str::to_string),
            to_parent: parent,
            recovery_snapshot: None,
            force: false,
            yes: true,
            db: Default::default(),
        }
    }

    #[test]
    fn reset_requires_exactly_one_target() {
        assert!(select_reset_target(&args(None, None, false)).is_err());
        assert!(select_reset_target(&args(Some("before"), Some("main"), false)).is_err());
        assert!(matches!(select_reset_target(&args(Some("before"), None, false)), Ok(ResetTarget::Snapshot(_))));
        assert!(matches!(select_reset_target(&args(None, Some("main"), false)), Ok(ResetTarget::Branch(_))));
        assert!(matches!(select_reset_target(&args(None, None, true)), Ok(ResetTarget::Parent)));
    }
}
