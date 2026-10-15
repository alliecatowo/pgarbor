//! `pgarbor inspect` (lease CLI-13): structure and space of a database file (RFC 001 section 5.3).
//!
//! * Always: superblock facts (uuid, format version, page size, commit seq, page count, file length),
//!   branches (rows as in `branch list`).
//! * `--space`: `BranchManager::space()` -> `(Vec<BranchSpace>, SpaceReport)`. Human: totals block
//!   (`file`, `committed`, `reachable`, `garbage`, `reclaimable` via `humanize::bytes`) then a table
//!   `branch | pages | exclusive | bytes | exclusive bytes`. JSON: `InspectOutput.space = Some(report)`,
//!   per-branch numbers in `BranchRow.space`.
//! * `--page N`: `pgarbor_storage::doctor::inspect_page(&container.snapshot(), PageId(N))` ->
//!   `InspectOutput.page`; human: header fields, kind, level, entry count, first/last key hex, children.
//!   Page 0 or beyond `page_count`: exit 64 `page N is out of range (1..=M)`.
//! Opens the container read-only through `open::open_container_readonly` (works while no process holds
//! the exclusive lock; when `pgarbor serve` owns the file print the exit-4 error with the serve hint).
use pgarbor_branches::BranchManager;
use pgarbor_storage::VerifyLevel;
use std::collections::HashMap;
use std::fs;

use crate::cli::InspectArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{humanize, open, paths, reports::{BranchRow, BranchSpaceRow, InspectOutput}, table::{self, Table}};

pub fn run(ctx: &mut Ctx, args: &InspectArgs) -> CliResult<Exit> {
    let path = paths::resolve_db(args.file.as_deref(), args.db.db.as_deref())?;
    open::require_exists(&path)?;
    let container = open::open_container_readonly(&path, VerifyLevel::Roots)?;
    let sb = container.superblock();
    let file_len = fs::metadata(&path).map_err(|e| CliError::io("could not stat database file", &e))?.len();
    let manager = BranchManager::new(container.clone());
    let records = manager.list(true).map_err(CliError::from)?;
    let default_id = manager.default_branch().map_err(CliError::from)?.map(|r| r.id);
    let space_result = if args.space { Some(manager.space().map_err(CliError::from)?) } else { None };
    let spaces: HashMap<_, _> = space_result.as_ref().map(|(rows, _)| {
        rows.iter().map(|s| (s.id, s)).collect()
    }).unwrap_or_default();
    let names: HashMap<_, _> = records.iter().map(|r| (r.id, r.name.as_str())).collect();
    let branches = records.iter().map(|r| {
        let space = spaces.get(&r.id).copied();
        BranchRow {
            name: r.name.clone(),
            id: r.id.to_hyphenated(),
            parent: r.parent.and_then(|id| names.get(&id).map(|n| (*n).to_string())),
            is_default: Some(r.id) == default_id,
            state: r.state.as_str().to_string(),
            created_unix_ms: r.created_unix_ms,
            updated_unix_ms: r.updated_unix_ms,
            head_seq: r.head_seq.0,
            snapshots: r.snapshots.iter().map(|s| s.label.clone()).collect(),
            space: space.map(|s| BranchSpaceRow { pages: s.pages, exclusive_pages: s.exclusive_pages, bytes: s.bytes, exclusive_bytes: s.exclusive_bytes }),
        }
    }).collect::<Vec<_>>();
    let page = if let Some(n) = args.page {
        if n == 0 || n > sb.page_count {
            return Err(CliError::usage(format!("page {n} is out of range (1..={})", sb.page_count)));
        }
        Some(pgarbor_storage::doctor::inspect_page(&container.snapshot(), pgarbor_common::PageId(n)).map_err(CliError::from)?)
    } else { None };
    let space = space_result.map(|(_, report)| report);
    let result = InspectOutput {
        path: path.display().to_string(),
        db_uuid: Some(sb.db_uuid.to_hyphenated()),
        format_version: Some(sb.format_version),
        page_size: Some(sb.page_size),
        commit_seq: Some(sb.commit_seq.0),
        file_len,
        page_count: Some(sb.page_count),
        default_branch: records.iter().find(|r| Some(r.id) == default_id).map(|r| r.name.clone()),
        branches,
        space,
        page,
    };
    if ctx.json {
        ctx.emit_json(&result)?;
    } else {
        render_human(ctx, &result)?;
    }
    Ok(Exit::OK)
}

fn render_human(ctx: &mut Ctx, report: &InspectOutput) -> CliResult<()> {
    ctx.out_line(&format!("file: {}", report.path));
    ctx.out_line(&format!("id: {}", report.db_uuid.as_deref().unwrap_or("unknown")));
    ctx.out_line(&format!("format: {} (page size {}, commit {})", report.format_version.unwrap_or(0), report.page_size.unwrap_or(0), report.commit_seq.unwrap_or(0)));
    ctx.out_line(&format!("pages: {} (file length {})", report.page_count.unwrap_or(0), report.file_len));
    ctx.out_line(&format!("default branch: {}", report.default_branch.as_deref().unwrap_or("none")));
    if let Some(space) = &report.space {
        ctx.out_line(&format!("file: {}", humanize::bytes(space.file_bytes)?));
        ctx.out_line(&format!("committed: {}", humanize::bytes(space.committed_bytes)?));
        ctx.out_line(&format!("reachable: {}", humanize::bytes(space.reachable_bytes)?));
        ctx.out_line(&format!("garbage: {}", humanize::bytes(space.garbage_bytes)?));
        ctx.out_line(&format!("reclaimable: {}", humanize::bytes(space.reclaimable_bytes)?));
        let mut table = Table { headers: vec!["branch".into(), "pages".into(), "exclusive".into(), "bytes".into(), "exclusive bytes".into()], right_align: vec![false, true, true, true, true], ..Table::default() };
        for b in &report.branches {
            let s = b.space.as_ref();
            table.rows.push(vec![Some(b.name.clone()), s.map(|x| x.pages.to_string()), s.map(|x| x.exclusive_pages.to_string()), s.map(|x| x.bytes.to_string()), s.map(|x| x.exclusive_bytes.to_string())]);
        }
        let rendered = table::render_aligned(&table, false)?;
        if !rendered.is_empty() { ctx.out_line(&rendered); }
    } else {
        let mut table = Table { headers: vec!["branch".into(), "parent".into(), "state".into(), "default".into(), "head".into()], right_align: vec![false, false, false, false, true], ..Table::default() };
        for b in &report.branches {
            table.rows.push(vec![Some(b.name.clone()), b.parent.clone(), Some(b.state.clone()), Some(if b.is_default { "*" } else { "" }.to_string()), Some(b.head_seq.to_string())]);
        }
        let rendered = table::render_aligned(&table, false)?;
        if !rendered.is_empty() { ctx.out_line(&rendered); }
    }
    if let Some(page) = &report.page {
        ctx.out_line(&format!("page {}: kind {}, level {}, entries {}, payload {}", page.id.0, page.kind.as_deref().unwrap_or("unknown"), page.level.map(|n| n.to_string()).as_deref().unwrap_or("-"), page.entries.map(|n| n.to_string()).as_deref().unwrap_or("-"), page.payload_len.map(|n| n.to_string()).as_deref().unwrap_or("-")));
        if let Some(h) = &page.header { ctx.out_line(&format!("header: {:?}", h)); }
        ctx.out_line(&format!("first key: {}", page.first_key_hex.as_deref().unwrap_or("-")));
        ctx.out_line(&format!("last key: {}", page.last_key_hex.as_deref().unwrap_or("-")));
        ctx.out_line(&format!("children: {}", page.children.iter().map(|p| p.0.to_string()).collect::<Vec<_>>().join(", ")));
        if let Some(error) = &page.error { ctx.out_line(&format!("page error: {error}")); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_page_message_includes_valid_range() {
        let err = CliError::usage(format!("page {} is out of range (1..={})", 0, 7));
        assert_eq!(err.message, "page 0 is out of range (1..=7)");
        assert_eq!(err.exit.code(), 64);
    }
}
