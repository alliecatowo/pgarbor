//! `pgarbor branch ...` dispatch and shared helpers. Frozen index, not leased (fully implemented).
//!
//! Every branch subcommand opens the database through `open::open_database`, which fails with exit 4
//! when another process (a running `pgarbor serve`) holds the file; only `branch exec` (via the control
//! socket) and `branch list/resolve` (read-only fallbacks are NOT provided in v1) cope with a served
//! database. `--json` output types are in `reports.rs`.
use pgarbor::BranchInfo;
use pgarbor_branches::BranchSpace;

use crate::cli::{BranchCommand, SnapshotCommand};
use crate::cmd::serve::{run_serve, ServeParams};
use crate::ctx::{CliResult, Ctx};
use crate::exit::Exit;
use crate::paths;
use crate::reports::{BranchRow, BranchSpaceRow};

pub mod basic;
pub mod detach_export;
pub mod exec;
pub mod link;
pub mod reset_compare;
pub mod snapshot;

/// Route a `branch` subcommand.
pub fn run(ctx: &mut Ctx, cmd: &BranchCommand) -> CliResult<Exit> {
    match cmd {
        BranchCommand::Create(a) => basic::create(ctx, a),
        BranchCommand::List(a) => basic::list(ctx, a),
        BranchCommand::Link(a) => link::link(ctx, a),
        BranchCommand::Resolve(a) => link::resolve(ctx, a),
        BranchCommand::Exec(a) => exec::run(ctx, a),
        BranchCommand::Serve(a) => {
            let target = paths::resolve_branch_target(a.branch.as_deref(), &a.target)?;
            run_serve(ctx, ServeParams { target, opts: a.opts.clone() })
        }
        BranchCommand::Rename(a) => basic::rename(ctx, a),
        BranchCommand::Delete(a) => basic::delete(ctx, a),
        BranchCommand::Reset(a) => reset_compare::reset(ctx, a),
        BranchCommand::Compare(a) => reset_compare::compare(ctx, a),
        BranchCommand::Detach(a) => detach_export::detach(ctx, a),
        BranchCommand::Export(a) => detach_export::export(ctx, a),
        BranchCommand::Snapshot(s) => match s {
            SnapshotCommand::Create { branch, label, db } => snapshot::create(ctx, branch, label, db),
            SnapshotCommand::List { branch, db } => snapshot::list(ctx, branch, db),
            SnapshotCommand::Drop { branch, label, db } => snapshot::drop_snapshot(ctx, branch, label, db),
        },
        BranchCommand::Restore(a) => basic::restore(ctx, a),
        BranchCommand::Purge(a) => basic::purge(ctx, a),
        BranchCommand::SetDefault(a) => basic::set_default(ctx, a),
    }
}

/// `BranchInfo` (+ optional space) -> JSON/table row.
pub fn row_from_info(info: &BranchInfo, space: Option<&BranchSpace>) -> BranchRow {
    BranchRow {
        name: info.name.clone(),
        id: info.id.clone(),
        parent: info.parent.clone(),
        is_default: info.is_default,
        state: if info.deleted { "deleted".to_string() } else { "active".to_string() },
        created_unix_ms: info.created_unix_ms,
        updated_unix_ms: info.updated_unix_ms,
        head_seq: info.head_seq,
        snapshots: info.snapshots.clone(),
        space: space.map(|s| BranchSpaceRow {
            pages: s.pages,
            exclusive_pages: s.exclusive_pages,
            bytes: s.bytes,
            exclusive_bytes: s.exclusive_bytes,
        }),
    }
}
