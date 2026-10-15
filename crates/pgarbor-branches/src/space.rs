//! Per-branch space accounting for `pgarbor inspect --space` and `branch list --json`
//! (RFC 001 §6 "retained bytes pinned by branches"). Lease BR-07.
use crate::directory;
use crate::resolver::BranchRootResolver;
use pgarbor_common::{BranchId, PageId, Result};
use pgarbor_storage::{Container, NamedRoot, SpaceReport};

// CONTRACT
/// Space retained by one branch (all of its live roots: head, extra databases, snapshots).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BranchSpace {
    pub id: BranchId,
    pub name: String,
    /// "active" | "deleted".
    pub state: String,
    /// Pages reachable from any of the branch's roots (shared pages counted).
    pub pages: u64,
    /// Pages reachable ONLY from this branch's roots (what deleting+purging it would free after
    /// compaction).
    pub exclusive_pages: u64,
    pub bytes: u64,
    pub exclusive_bytes: u64,
    pub snapshot_count: usize,
}

/// Compute per-branch space.
/// 1. `snap = container.snapshot()`; `report = pgarbor_storage::doctor::space_report(&snap,
///    &BranchRootResolver)` and `records = directory::list(&snap.directory(), true)` — both from the
///    SAME snapshot so they agree.
/// 2. For each record with live roots (`record.live_roots()` non-empty; dropped tombstones get
///    zeros): `R_b = gc::trace(&snap, snap.page_count(), &roots_of_b, false)?.reachable`;
///    `R_o = trace(all live roots of every OTHER record)`; `pages = R_b.len()`;
///    `exclusive_pages = |{p in R_b : !R_o.contains(p)}|` (iterate pages 1..=page_count and test
///    both `ReachSet`s). Cost O(branches x pages) — documented v1 limit.
/// 3. `bytes = pages * page_size`, `exclusive_bytes` likewise; `state` = `BranchState::as_str`.
/// 4. Sort active branches by name, then deleted ones by name.
pub fn branch_space(container: &Container) -> Result<(Vec<BranchSpace>, SpaceReport)> {
    let snap = container.snapshot();
    let report = pgarbor_storage::doctor::space_report(&snap, &BranchRootResolver)?;
    let mut records = directory::list(&snap.directory(), true)?;
    // Active before deleted (`false < true`), then by name.
    records.sort_by(|a, b| (!a.is_active(), a.name.as_str()).cmp(&(!b.is_active(), b.name.as_str())));

    let page_count = snap.page_count();
    let page_size = u64::from(report.page_size);

    // Live roots of every record in sorted order; dropped tombstones contribute nothing.
    let per_record: Vec<Vec<NamedRoot>> = records
        .iter()
        .map(|r| r.live_roots().into_iter().map(|(label, root)| NamedRoot { label, root }).collect())
        .collect();

    let mut out = Vec::with_capacity(records.len());
    for (i, rec) in records.iter().enumerate() {
        let roots_live = rec.is_active() || rec.roots_retained;
        let mut row = BranchSpace {
            id: rec.id,
            name: rec.name.clone(),
            state: rec.state.as_str().to_string(),
            pages: 0,
            exclusive_pages: 0,
            bytes: 0,
            exclusive_bytes: 0,
            snapshot_count: if roots_live { rec.snapshots.len() } else { 0 },
        };
        let own = &per_record[i];
        if own.is_empty() {
            out.push(row);
            continue;
        }

        let reach_b = pgarbor_storage::gc::trace(&snap, page_count, own, false)?.reachable;
        let others: Vec<NamedRoot> = per_record
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .flat_map(|(_, roots)| roots.iter().cloned())
            .collect();
        let reach_o = pgarbor_storage::gc::trace(&snap, page_count, &others, false)?.reachable;

        let mut exclusive = 0u64;
        for p in 1..=page_count {
            let id = PageId(p);
            if reach_b.contains(id) && !reach_o.contains(id) {
                exclusive += 1;
            }
        }

        row.pages = reach_b.len();
        row.exclusive_pages = exclusive;
        row.bytes = row.pages.saturating_mul(page_size);
        row.exclusive_bytes = exclusive.saturating_mul(page_size);
        out.push(row);
    }
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory;
    use crate::manager::BranchManager;
    use crate::options::{BranchRef, CreateOptions, CreateSource, DeleteOptions};
    use pgarbor_storage::{CreateOptions as StorageCreateOptions, MemVfs, Vfs};
    use std::path::Path;
    use std::sync::Arc;

    fn setup() -> BranchManager {
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let c = Container::create_with(vfs, Path::new("space.pga"), &StorageCreateOptions::default()).unwrap();
        let m = BranchManager::new(c);
        m.initialize("main").unwrap();
        m
    }

    /// Write `range` keys into the data tree of branch `name` and store the new root in its record,
    /// all in one storage write transaction.
    fn write_keys(m: &BranchManager, name: &str, range: std::ops::Range<u32>) {
        let c = m.container();
        let rec = m.get(&BranchRef::name(name)).unwrap();
        let mut w = c.begin_write().unwrap();
        let mut root = rec.data_root;
        for i in range {
            root = w.put(root, format!("key-{i:05}").as_bytes(), &[0xabu8; 64]).unwrap().root;
        }
        let dir = w.directory_root();
        let new_dir = {
            let mut t = w.tree(dir);
            let mut updated = rec.clone();
            updated.data_root = root;
            directory::store_record(&mut t, &updated).unwrap();
            t.root()
        };
        w.set_directory_root(new_dir);
        w.commit().unwrap();
    }

    fn row<'a>(rows: &'a [BranchSpace], name: &str) -> &'a BranchSpace {
        rows.iter().find(|r| r.name == name).unwrap()
    }

    #[test]
    fn shared_root_is_counted_in_both_branches_but_never_exclusive() {
        let m = setup();
        write_keys(&m, "main", 0..100);
        m.create("feat", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();

        let (rows, report) = branch_space(m.container()).unwrap();
        let main = row(&rows, "main");
        let feat = row(&rows, "feat");
        assert!(main.pages > 0);
        assert_eq!(main.pages, feat.pages);
        assert_eq!(main.exclusive_pages, 0);
        assert_eq!(feat.exclusive_pages, 0);
        assert_eq!(main.bytes, main.pages * u64::from(report.page_size));
        assert_eq!(main.exclusive_bytes, 0);
        let total_exclusive: u64 = rows.iter().map(|r| r.exclusive_pages).sum();
        assert!(total_exclusive <= report.reachable_pages);
    }

    #[test]
    fn writes_on_one_branch_make_only_its_pages_exclusive() {
        let m = setup();
        m.create("feat", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        // `main` is still an empty tree, so nothing `feat` writes is shared with it.
        write_keys(&m, "feat", 0..100);

        let (rows, _) = branch_space(m.container()).unwrap();
        let feat = row(&rows, "feat");
        let main = row(&rows, "main");
        assert!(feat.exclusive_pages > 0);
        assert_eq!(feat.exclusive_pages, feat.pages);
        assert_eq!(main.pages, 0);
        assert_eq!(main.exclusive_pages, 0);
    }

    #[test]
    fn dropped_tombstone_reports_zero_pages_and_retained_one_does_not() {
        let m = setup();
        m.create("dropped", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        write_keys(&m, "dropped", 0..100);
        m.create("kept", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        write_keys(&m, "kept", 0..50);

        m.delete(&BranchRef::name("dropped"), &DeleteOptions { retain: false, allow_default: false }).unwrap();
        m.delete(&BranchRef::name("kept"), &DeleteOptions { retain: true, allow_default: false }).unwrap();

        let (rows, _) = branch_space(m.container()).unwrap();
        let dropped = row(&rows, "dropped");
        assert_eq!(dropped.state, "deleted");
        assert_eq!(dropped.pages, 0);
        assert_eq!(dropped.exclusive_pages, 0);
        assert_eq!(dropped.bytes, 0);

        let kept = row(&rows, "kept");
        assert_eq!(kept.state, "deleted");
        assert!(kept.pages > 0);
        assert_eq!(kept.exclusive_pages, kept.pages);
    }

    #[test]
    fn rows_are_sorted_active_first_then_by_name() {
        let m = setup();
        m.create("b", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        m.create("a", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        m.create("gone", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        m.delete(&BranchRef::name("gone"), &DeleteOptions { retain: true, allow_default: false }).unwrap();

        let (rows, _) = branch_space(m.container()).unwrap();
        let names: Vec<(&str, &str)> = rows.iter().map(|r| (r.state.as_str(), r.name.as_str())).collect();
        assert_eq!(names, vec![("active", "a"), ("active", "b"), ("active", "main"), ("deleted", "gone")]);
    }
}
