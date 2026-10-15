//! `pgarbor branch snapshot create|list|drop` (lease CLI-18; BRANCH snapshots, RFC 001 section 9 reset
//! recovery). Named snapshots are cheap root pointers inside the branch record.
//!
//! * `create BRANCH LABEL`: `db.create_snapshot(branch, label)`; output `snapshot "L" created on
//!   branch "B" at commit <seq>`; label errors (42602) -> exit 64, exists -> exit 6, branch missing ->
//!   exit 5, too many (`MAX_SNAPSHOTS_PER_BRANCH`) -> exit 1.
//! * `list BRANCH`: `db.engine().branches().get(&BranchRef::name(b))` -> `BranchRecord.snapshots`
//!   -> [`crate::reports::SnapshotRow`] (`kind` = `format!("{:?}", kind)` lowercased with underscores:
//!   `User` -> `user`, `ResetRecovery` -> `reset_recovery`). Human table `LABEL | KIND | COMMIT |
//!   CREATED`. `--json`: an array under `{"branch": "...", "snapshots": [...]}`.
//! * `drop BRANCH LABEL`: `db.drop_snapshot(branch, label)`; not found -> exit 5.
//! All open the database like `basic.rs` and call `db.close()` at the end.
use crate::cli::DbFlag;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{open, paths, reports};
use pgarbor::Database;
use pgarbor_branches::{BranchRef, SnapshotKind};
use serde::Serialize;
use std::path::PathBuf;

fn open_for_branch(db_flag: &DbFlag) -> CliResult<Database> {
    let path: PathBuf = paths::resolve_db(None, db_flag.db.as_deref())?;
    open::open_database(&path, &open::OpenSpec::existing())
}

fn kind_name(kind: SnapshotKind) -> &'static str {
    match kind {
        SnapshotKind::User => "user",
        SnapshotKind::ResetRecovery => "reset_recovery",
        SnapshotKind::Inherited => "system",
    }
}

#[derive(Serialize)]
struct SnapshotList<'a> {
    branch: &'a str,
    snapshots: Vec<reports::SnapshotRow>,
}

pub fn create(ctx: &mut Ctx, branch: &str, label: &str, db_flag: &DbFlag) -> CliResult<Exit> {
    let db = open_for_branch(db_flag)?;
    let operation = (|| {
        db.create_snapshot(branch, label).map_err(CliError::from)?;
        let rec = db.engine().branches().get(&BranchRef::name(branch)).map_err(CliError::from)?;
        let snapshot = rec.snapshots.iter().find(|s| s.label == label).ok_or_else(|| {
            CliError::internal("snapshot was created but is missing from the branch record")
        })?;
        if ctx.json {
            ctx.emit_json(&reports::SnapshotRow {
                label: snapshot.label.clone(),
                kind: kind_name(snapshot.kind).to_string(),
                created_seq: snapshot.created_seq.0,
                created_unix_ms: snapshot.created_unix_ms,
            })?;
        } else {
            ctx.out_line(&format!("snapshot \"{label}\" created on branch \"{branch}\" at commit {}", snapshot.created_seq.0));
        }
        Ok(Exit::OK)
    })();
    let _ = db.close();
    operation
}

pub fn list(ctx: &mut Ctx, branch: &str, db_flag: &DbFlag) -> CliResult<Exit> {
    let db = open_for_branch(db_flag)?;
    let operation = (|| {
        let rec = db.engine().branches().get(&BranchRef::name(branch)).map_err(CliError::from)?;
        let snapshots: Vec<reports::SnapshotRow> = rec.snapshots.iter().map(|s| reports::SnapshotRow {
            label: s.label.clone(),
            kind: kind_name(s.kind).to_string(),
            created_seq: s.created_seq.0,
            created_unix_ms: s.created_unix_ms,
        }).collect();
        if ctx.json {
            ctx.emit_json(&SnapshotList { branch, snapshots })?;
        } else {
            ctx.out_line("LABEL | KIND | COMMIT | CREATED");
            for s in &snapshots {
                ctx.out_line(&format!("{} | {} | {} | {}", s.label, s.kind, s.created_seq, format_timestamp(s.created_unix_ms)));
            }
        }
        Ok(Exit::OK)
    })();
    let _ = db.close();
    operation
}

pub fn drop_snapshot(ctx: &mut Ctx, branch: &str, label: &str, db_flag: &DbFlag) -> CliResult<Exit> {
    let db = open_for_branch(db_flag)?;
    let operation = (|| {
        db.drop_snapshot(branch, label).map_err(CliError::from)?;
        if ctx.json {
            #[derive(Serialize)]
            struct Dropped<'a> { branch: &'a str, label: &'a str, dropped: bool }
            ctx.emit_json(&Dropped { branch, label, dropped: true })?;
        } else {
            ctx.out_line(&format!("snapshot \"{label}\" dropped from branch \"{branch}\""));
        }
        Ok(Exit::OK)
    })();
    let _ = db.close();
    operation
}

fn format_timestamp(unix_ms: u64) -> String {
    if unix_ms == 0 { return "-".to_string(); }
    let seconds = unix_ms / 1000;
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", day_seconds / 3600, (day_seconds % 3600) / 60, day_seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::kind_name;
    use pgarbor_branches::SnapshotKind;

    #[test]
    fn snapshot_kinds_have_cli_names() {
        assert_eq!(kind_name(SnapshotKind::User), "user");
        assert_eq!(kind_name(SnapshotKind::ResetRecovery), "reset_recovery");
        assert_eq!(kind_name(SnapshotKind::Inherited), "system");
    }
}
