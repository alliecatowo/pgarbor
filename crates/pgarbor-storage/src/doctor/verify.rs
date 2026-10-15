//! Structural verification (STORE-19). Lease STO-18.
//!
//! Corruption is REPORTED as `Issue`s (verification continues with sibling subtrees); only
//! non-corruption failures (I/O errors, SQLSTATE class 58/53) abort with `Err`. Distinguish via
//! `err.sqlstate == SqlState::DATA_CORRUPTED`.
use crate::btree::node::{
    decode_node, internal_span_key, internal_spans, leaf_spans, max_inline_value, max_key_len, node_capacity,
    span_key, span_value, LeafValueRef,
};
use crate::btree::overflow::chain_pages;
use crate::container::handle::Container;
use crate::container::options::{OpenOptions, VerifyLevel};
use crate::container::recovery::read_slots;
use crate::container::snapshot::Snapshot;
use crate::doctor::report::{DoctorReport, Issue, IssueSeverity};
use crate::error;
use crate::format::consts::PageKind;
use crate::format::page::PageRef;
use crate::format::superblock::{choose_slot, SlotStatus};
use crate::gc::trace::{collect_roots, trace, KindCounts, ReachSet};
use crate::io::{FileIo, LockMode, OpenMode, Vfs};
use crate::pager::PageSource;
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result, SqlState};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TreeStats {
    pub pages: u64,
    pub leaves: u64,
    pub internal: u64,
    pub overflow_pages: u64,
    pub entries: u64,
    pub height: u16,
}

// CONTRACT
/// Verify one tree. Checks, each failure an `Issue::error(Some(page), Some(label), msg)`:
/// - every page verifies (checksum etc. via `src.read_page`) and is a node/overflow as expected;
/// - `leaf_spans`/`internal_spans` parse; levels decrease by exactly 1 to 0 at every leaf
///   (uniform depth); `height` = root level + 1;
/// - key bounds: all keys of child i lie in `[sep_{i-1}, sep_i)` (track (lo, hi) while descending);
/// - every key `<= max_key_len`; every node `<= node_capacity`;
/// - overflow cells: chain valid (`chain_pages`), `vlen > max_inline_value` (else Warning
///   "overflow value of <n> bytes could be inline");
/// - a page reached twice within ONE tree → Error "page <id> referenced twice (cycle or shared
///   subtree inside one tree)"; use `seen_in_tree` (fresh per tree).
/// - non-root leaf with 0 entries → Warning; root leaf with 0 entries → Warning ("empty tree
///   should use the null root").
/// `seen_global` accumulates every page visited (for unreachable accounting); pages already in it
/// from another tree are still verified only once (skip descent when already global-seen AND
/// verified, i.e. shared subtrees across branches are checked once).
#[allow(unused_variables)]
pub fn verify_tree(
    src: &dyn PageSource,
    root: PageId,
    label: &str,
    seen_global: &mut ReachSet,
    issues: &mut Vec<Issue>,
) -> Result<TreeStats> {
    if root.is_none() {
        return Ok(TreeStats::default());
    }
    let ps = src.page_size();
    let mut tc = TreeCheck {
        src,
        label,
        seen_in_tree: HashSet::new(),
        stats: TreeStats::default(),
        max_key: max_key_len(ps),
        cap: node_capacity(ps),
        max_inline: max_inline_value(ps),
    };
    let mut stack = vec![Frame { id: root, expect_level: None, lo: None, hi: None, is_root: true }];
    while let Some(frame) = stack.pop() {
        let children = tc.check_frame(&frame, seen_global, issues)?;
        stack.extend(children.into_iter().rev());
    }
    Ok(tc.stats)
}

// CONTRACT
/// Verify a snapshot. Level `None`: superblock fields only (no page reads). `Roots`: read and
/// parse the directory root and every live root page (no descent). `Full`: `verify_tree` for the
/// directory and every root from `collect_roots`, then scan all unreachable ids
/// `1..=page_count`: a page that fails to verify there is `Issue::info` ("unreachable page <id>
/// does not verify (garbage)") — garbage is never an error. Fills counts/kinds/roots (roots via
/// `trace(.., per_root = true)` at level Full, else empty). `slots`/`file_len`/`recovery` are
/// filled by the caller (`verify_container`/`verify_file`); here use `[Empty, Empty]`, the pager's
/// file length, and `None`.
#[allow(unused_variables)]
pub fn verify_snapshot(snap: &Snapshot, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
    let sb = *snap.superblock();
    let mut report = DoctorReport {
        path: snap.pager().path().to_path_buf(),
        level,
        file_len: snap.pager().file_len()?,
        slots: [SlotStatus::Empty, SlotStatus::Empty],
        page_size: Some(sb.page_size),
        format_version: Some(sb.format_version),
        db_uuid: Some(sb.db_uuid),
        commit_seq: Some(sb.commit_seq),
        directory_root: Some(sb.directory_root),
        page_count: Some(sb.page_count),
        recovery: None,
        pages_checked: 0,
        reachable_pages: 0,
        unreachable_pages: 0,
        kinds: KindCounts::default(),
        roots: Vec::new(),
        issues: Vec::new(),
    };
    let dir = sb.directory_root;
    if dir.0 > sb.page_count {
        report.issues.push(Issue::error(
            None,
            Some("directory"),
            format!("directory root {} is beyond the end of committed data (page_count {})", dir.0, sb.page_count),
        ));
        return Ok(report);
    }
    match level {
        VerifyLevel::None => {}
        VerifyLevel::Roots => verify_roots(snap, resolver, dir, &mut report)?,
        VerifyLevel::Full => verify_full(snap, resolver, &mut report)?,
    }
    Ok(report)
}

// CONTRACT
/// `verify_snapshot(&c.snapshot(), ..)` + `slots`/`recovery` from `c.recovery_report()` + file length.
#[allow(unused_variables)]
pub fn verify_container(c: &Container, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
    let snap = c.snapshot();
    let mut report = verify_snapshot(&snap, resolver, level)?;
    let rec = c.recovery_report();
    report.slots = rec.slots.clone();
    report.recovery = Some(rec.clone());
    Ok(report)
}

// CONTRACT
/// Doctor for a path that may not even open. Opens the file read-only via `vfs.open(ReadOnly)`,
/// reads slots with `container::recovery::read_slots`; if `choose_slot` fails, returns a report
/// with the slot statuses and one Error issue carrying the open error message (Ok, not Err). If it
/// succeeds, closes that handle and opens `Container::open_with(vfs, path, read_only, verify None)`
/// and returns `verify_container`. A file locked exclusively by another process → Err 55006.
#[allow(unused_variables)]
pub fn verify_file(vfs: Arc<dyn Vfs>, path: &Path, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
    let file = vfs.open(path, OpenMode::ReadOnly).map_err(|e| error::open_failed(path, &e))?;
    file.try_lock(LockMode::Shared).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            error::locked(path)
        } else {
            error::read_failed(path, &e)
        }
    })?;
    let slots = read_slots(&*file, path)?;
    let file_len = file.len().map_err(|e| error::read_failed(path, &e))?;
    let chosen = choose_slot(path, &slots);
    // Release the handle (and its shared lock) before the container opens the same file.
    drop(file);
    if let Err(e) = chosen {
        if !is_corruption(&e) {
            return Err(e);
        }
        let mut report = unopened_report(path, level, file_len, slots);
        report.issues.push(Issue::error(None, None, describe(&e)));
        return Ok(report);
    }
    let opts = OpenOptions { read_only: true, verify: VerifyLevel::None, ..OpenOptions::default() };
    match Container::open_with(vfs, path, &opts) {
        Ok(c) => verify_container(&c, resolver, level),
        Err(e) if is_corruption(&e) => {
            let mut report = unopened_report(path, level, file_len, slots);
            report.issues.push(Issue::error(None, None, describe(&e)));
            Ok(report)
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn is_corruption(e: &PgError) -> bool {
    e.sqlstate == SqlState::DATA_CORRUPTED
}

/// Message plus detail, for issue text.
fn describe(e: &PgError) -> String {
    match &e.detail {
        Some(d) => format!("{} ({})", e.message, d),
        None => e.message.clone(),
    }
}

/// The page a corruption error names, if its message or detail carries one.
fn page_named_by(e: &PgError) -> Option<PageId> {
    fn number_after(s: &str, prefix: &str) -> Option<u64> {
        let digits: String = s.strip_prefix(prefix)?.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }
    number_after(&e.message, "invalid page in block ")
        .or_else(|| number_after(&e.message, "page reference "))
        .or_else(|| e.detail.as_deref().and_then(|d| number_after(d, "page ")))
        .map(PageId)
}

/// Error issue for a corruption error; `fallback` is used when the message names no page.
fn corrupt_issue(e: &PgError, fallback: Option<PageId>, tree: Option<&str>) -> Issue {
    Issue::error(page_named_by(e).or(fallback), tree, describe(e))
}

/// Half-open key range `[lo, hi)` test.
fn in_bounds(key: &[u8], lo: Option<&[u8]>, hi: Option<&[u8]>) -> bool {
    lo.map_or(true, |l| key >= l) && hi.map_or(true, |h| key < h)
}

fn has_error_since(issues: &[Issue], from: usize) -> bool {
    issues[from..].iter().any(|i| i.severity == IssueSeverity::Error)
}

fn unopened_report(path: &Path, level: VerifyLevel, file_len: u64, slots: [SlotStatus; 2]) -> DoctorReport {
    DoctorReport {
        path: PathBuf::from(path),
        level,
        file_len,
        slots,
        page_size: None,
        format_version: None,
        db_uuid: None,
        commit_seq: None,
        directory_root: None,
        page_count: None,
        recovery: None,
        pages_checked: 0,
        reachable_pages: 0,
        unreachable_pages: 0,
        kinds: KindCounts::default(),
        roots: Vec::new(),
        issues: Vec::new(),
    }
}

/// `verify_tree` where a corruption error escaping at top level becomes an Error issue.
fn tree_checked(
    snap: &Snapshot,
    root: PageId,
    label: &str,
    seen: &mut ReachSet,
    issues: &mut Vec<Issue>,
) -> Result<TreeStats> {
    match verify_tree(snap, root, label, seen, issues) {
        Ok(stats) => Ok(stats),
        Err(e) if is_corruption(&e) => {
            issues.push(corrupt_issue(&e, Some(root), Some(label)));
            Ok(TreeStats::default())
        }
        Err(e) => Err(e),
    }
}

fn read_and_decode(snap: &Snapshot, id: PageId) -> Result<()> {
    let page = snap.read_page(id)?;
    decode_node(&page)?;
    Ok(())
}

/// Level Roots: directory root page plus every live root page, no descent into data trees.
fn verify_roots(snap: &Snapshot, resolver: &dyn RootResolver, dir: PageId, report: &mut DoctorReport) -> Result<()> {
    if dir.is_none() {
        return Ok(());
    }
    if let Err(e) = read_and_decode(snap, dir) {
        if is_corruption(&e) {
            report.issues.push(corrupt_issue(&e, Some(dir), Some("directory")));
            return Ok(());
        }
        return Err(e);
    }
    report.pages_checked += 1;
    let roots = match collect_roots(snap, dir, resolver) {
        Ok(r) => r,
        Err(e) => {
            if is_corruption(&e) {
                report.issues.push(corrupt_issue(&e, None, Some("directory")));
                return Ok(());
            }
            return Err(e);
        }
    };
    // collect_roots lists the directory first (dir is non-NONE here); it was read above.
    for r in roots.iter().skip(1) {
        match read_and_decode(snap, r.root) {
            Ok(()) => report.pages_checked += 1,
            Err(e) if is_corruption(&e) => report.issues.push(corrupt_issue(&e, Some(r.root), Some(r.label.as_str()))),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Level Full: every tree, reachability, and the garbage scan.
fn verify_full(snap: &Snapshot, resolver: &dyn RootResolver, report: &mut DoctorReport) -> Result<()> {
    let dir = snap.directory_root();
    let page_count = snap.page_count();
    let mut seen = ReachSet::new(page_count);
    let mut checked = 0u64;

    let before_dir = report.issues.len();
    let roots = if dir.is_none() {
        Vec::new()
    } else {
        let stats = tree_checked(snap, dir, "directory", &mut seen, &mut report.issues)?;
        checked += stats.pages;
        match collect_roots(snap, dir, resolver) {
            Ok(r) => r,
            Err(e) if is_corruption(&e) => {
                if !has_error_since(&report.issues, before_dir) {
                    report.issues.push(corrupt_issue(&e, None, Some("directory")));
                }
                Vec::new()
            }
            Err(e) => return Err(e),
        }
    };
    // collect_roots lists the directory first; it was verified above.
    let skip = if dir.is_none() { 0 } else { 1 };
    for r in roots.iter().skip(skip) {
        let stats = tree_checked(snap, r.root, &r.label, &mut seen, &mut report.issues)?;
        checked += stats.pages;
    }

    match trace(snap, page_count, &roots, true) {
        Ok(t) => {
            report.kinds = t.kinds;
            report.roots = t.roots;
        }
        Err(e) if is_corruption(&e) => {
            if !report.issues.iter().any(|i| i.severity == IssueSeverity::Error) {
                report.issues.push(Issue::error(None, None, format!("reachability trace failed: {}", describe(&e))));
            }
        }
        Err(e) => return Err(e),
    }

    report.pages_checked = checked;
    report.reachable_pages = seen.len();
    report.unreachable_pages = seen.unmarked();

    let pager = snap.pager();
    for id in seen.iter_unmarked() {
        match pager.read_page_uncached(id) {
            Ok(_) => {}
            Err(e) if is_corruption(&e) => report.issues.push(Issue::info(
                Some(id),
                None,
                format!("unreachable page {} does not verify (garbage)", id.0),
            )),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Tree walk
// ---------------------------------------------------------------------------------------------

/// One pending page of the explicit DFS stack, with the key range inherited from its parent.
struct Frame {
    id: PageId,
    /// Level the page must have (children of a level-L node have level L-1); None for the root.
    expect_level: Option<u16>,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    is_root: bool,
}

struct TreeCheck<'a> {
    src: &'a dyn PageSource,
    label: &'a str,
    seen_in_tree: HashSet<PageId>,
    stats: TreeStats,
    max_key: usize,
    cap: usize,
    max_inline: usize,
}

impl<'a> TreeCheck<'a> {
    /// Corruption becomes an Error issue on `page`; anything else aborts.
    fn fail(&self, e: PgError, page: PageId, issues: &mut Vec<Issue>) -> Result<Vec<Frame>> {
        if is_corruption(&e) {
            issues.push(corrupt_issue(&e, Some(page), Some(self.label)));
            Ok(Vec::new())
        } else {
            Err(e)
        }
    }

    /// Check one page; returns the children to visit, in ascending key order.
    fn check_frame(&mut self, f: &Frame, seen_global: &mut ReachSet, issues: &mut Vec<Issue>) -> Result<Vec<Frame>> {
        let id = f.id;
        if !self.seen_in_tree.insert(id) {
            issues.push(Issue::error(
                Some(id),
                Some(self.label),
                format!("page {} referenced twice (cycle or shared subtree inside one tree)", id.0),
            ));
            return Ok(Vec::new());
        }
        if seen_global.contains(id) {
            // Already verified through another tree of this snapshot: checked once.
            return Ok(Vec::new());
        }
        seen_global.insert(id);
        let page = match self.src.read_page(id) {
            Ok(p) => p,
            Err(e) => return self.fail(e, id, issues),
        };
        self.stats.pages += 1;
        match page.kind {
            PageKind::Leaf => self.check_leaf(f, &page, seen_global, issues),
            PageKind::Internal => self.check_internal(f, &page, issues),
            other => {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("expected a leaf or internal node page, found {} page", other.name()),
                ));
                Ok(Vec::new())
            }
        }
    }

    fn check_leaf(
        &mut self,
        f: &Frame,
        page: &PageRef,
        seen_global: &mut ReachSet,
        issues: &mut Vec<Issue>,
    ) -> Result<Vec<Frame>> {
        let id = f.id;
        if let Some(want) = f.expect_level {
            if want != 0 {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("leaf page found where a level {want} node was expected (non-uniform depth)"),
                ));
                return Ok(Vec::new());
            }
        }
        let spans = match leaf_spans(id, &page.payload) {
            Ok(s) => s,
            Err(e) => return self.fail(e, id, issues),
        };
        self.stats.leaves += 1;
        self.stats.entries += spans.len() as u64;
        if f.is_root {
            self.stats.height = 1;
        }
        if page.payload.len() > self.cap {
            issues.push(Issue::error(
                Some(id),
                Some(self.label),
                format!("leaf of {} bytes exceeds node capacity {}", page.payload.len(), self.cap),
            ));
        }
        if spans.is_empty() {
            let msg = if f.is_root {
                "root leaf has no entries (empty tree should use the null root)"
            } else {
                "non-root leaf has no entries"
            };
            issues.push(Issue::warning(Some(id), Some(self.label), msg));
        }
        let payload: &[u8] = &page.payload;
        for (i, s) in spans.iter().enumerate() {
            let key = span_key(payload, s);
            if key.len() > self.max_key {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("key {i} of {} bytes exceeds maximum key length {}", key.len(), self.max_key),
                ));
            }
            if !in_bounds(key, f.lo.as_deref(), f.hi.as_deref()) {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("key {i} lies outside the key range of its parent separators"),
                ));
            }
            if let LeafValueRef::Overflow { first, len } = span_value(payload, s) {
                if (len as usize) <= self.max_inline {
                    issues.push(Issue::warning(
                        Some(id),
                        Some(self.label),
                        format!("overflow value of {len} bytes could be inline"),
                    ));
                }
                match chain_pages(self.src, first, len) {
                    Ok(chain) => {
                        for p in chain {
                            if !self.seen_in_tree.insert(p) {
                                issues.push(Issue::error(
                                    Some(p),
                                    Some(self.label),
                                    format!(
                                        "page {} referenced twice (cycle or shared subtree inside one tree)",
                                        p.0
                                    ),
                                ));
                                continue;
                            }
                            if seen_global.contains(p) {
                                continue;
                            }
                            seen_global.insert(p);
                            self.stats.pages += 1;
                            self.stats.overflow_pages += 1;
                        }
                    }
                    Err(e) if is_corruption(&e) => {
                        issues.push(corrupt_issue(&e, Some(id), Some(self.label)));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(Vec::new())
    }

    fn check_internal(&mut self, f: &Frame, page: &PageRef, issues: &mut Vec<Issue>) -> Result<Vec<Frame>> {
        let id = f.id;
        let (level, first_child, spans) = match internal_spans(id, &page.payload) {
            Ok(v) => v,
            Err(e) => return self.fail(e, id, issues),
        };
        if let Some(want) = f.expect_level {
            if want != level {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("internal node at level {level} where level {want} was expected (non-uniform depth)"),
                ));
                return Ok(Vec::new());
            }
        }
        if f.is_root {
            self.stats.height = level.saturating_add(1);
        }
        self.stats.internal += 1;
        if page.payload.len() > self.cap {
            issues.push(Issue::error(
                Some(id),
                Some(self.label),
                format!("internal node of {} bytes exceeds node capacity {}", page.payload.len(), self.cap),
            ));
        }
        let payload: &[u8] = &page.payload;
        let mut prev: Option<&[u8]> = None;
        for (i, s) in spans.iter().enumerate() {
            let key = internal_span_key(payload, s);
            if key.len() > self.max_key {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("separator {i} of {} bytes exceeds maximum key length {}", key.len(), self.max_key),
                ));
            }
            if !in_bounds(key, f.lo.as_deref(), f.hi.as_deref()) {
                issues.push(Issue::error(
                    Some(id),
                    Some(self.label),
                    format!("separator {i} lies outside the key range of its parent separators"),
                ));
            }
            if let Some(p) = prev {
                if key <= p {
                    issues.push(Issue::error(
                        Some(id),
                        Some(self.label),
                        format!("separator keys out of order at separator {i}"),
                    ));
                }
            }
            prev = Some(key);
        }
        let n = spans.len();
        let child_level = level.saturating_sub(1);
        let mut children = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let child = if i == 0 { first_child } else { spans[i - 1].child };
            let lo = if i == 0 { f.lo.clone() } else { Some(internal_span_key(payload, &spans[i - 1]).to_vec()) };
            let hi = if i < n { Some(internal_span_key(payload, &spans[i]).to_vec()) } else { f.hi.clone() };
            children.push(Frame { id: child, expect_level: Some(child_level), lo, hi, is_root: false });
        }
        Ok(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::options::CreateOptions;
    use crate::format::consts::page_offset;
    use crate::io::MemVfs;
    use crate::roots::NoNestedRoots;
    use pgarbor_common::KvWrite;
    use std::ops::Range;

    /// Commit one directory write inserting `keys`; returns the new directory root.
    fn put_keys(c: &Container, keys: Range<u32>) -> Result<PageId> {
        let mut w = c.begin_write()?;
        let mut dir = w.directory_root();
        {
            let mut t = w.tree(dir);
            for i in keys {
                t.put(format!("Nkey{i:05}").as_bytes(), &i.to_le_bytes())?;
            }
            dir = t.root();
        }
        w.set_directory_root(dir);
        w.commit()?;
        Ok(c.superblock().directory_root)
    }

    fn flip_byte(vfs: &MemVfs, path: &Path, offset: u64) {
        let mut bytes = vfs.read_bytes(path).expect("file exists");
        bytes[offset as usize] ^= 0xFF;
        vfs.write_bytes(path, bytes);
    }

    fn verify(vfs: &MemVfs, path: &Path) -> DoctorReport {
        verify_file(Arc::new(vfs.clone()), path, &NoNestedRoots, VerifyLevel::Full)
            .expect("doctor reports corruption instead of failing")
    }

    #[test]
    fn healthy_container_verifies_clean() {
        let vfs = MemVfs::new();
        let path = Path::new("healthy.pga");
        {
            let c = Container::create_with(Arc::new(vfs.clone()), path, &CreateOptions::default()).unwrap();
            put_keys(&c, 0..200).unwrap();
        }
        let report = verify(&vfs, path);
        assert!(report.is_ok(), "{:?}", report.issues);
        assert!(report.reachable_pages >= 1);
        let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert!(json["issues"].is_array());
    }

    #[test]
    fn corrupted_reachable_page_is_an_error_naming_it() {
        let vfs = MemVfs::new();
        let path = Path::new("corrupt.pga");
        let root = {
            let c = Container::create_with(Arc::new(vfs.clone()), path, &CreateOptions::default()).unwrap();
            put_keys(&c, 0..200).unwrap()
        };
        flip_byte(&vfs, path, page_offset(8192, root) + 40);
        let report = verify(&vfs, path);
        assert!(!report.is_ok());
        assert!(report.errors().any(|i| i.page == Some(root)), "{:?}", report.issues);
    }

    #[test]
    fn corrupted_unreachable_page_is_only_info() {
        let vfs = MemVfs::new();
        let path = Path::new("garbage.pga");
        let old_root = {
            let c = Container::create_with(Arc::new(vfs.clone()), path, &CreateOptions::default()).unwrap();
            let r1 = put_keys(&c, 0..200).unwrap();
            put_keys(&c, 200..400).unwrap();
            r1
        };
        flip_byte(&vfs, path, page_offset(8192, old_root) + 40);
        let report = verify(&vfs, path);
        assert!(report.is_ok(), "{:?}", report.issues);
        assert!(
            report.issues.iter().any(|i| i.severity == IssueSeverity::Info && i.page == Some(old_root)),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn unopenable_file_is_reported_not_failed() {
        let vfs = MemVfs::new();
        let path = Path::new("junk.pga");
        vfs.write_bytes(path, vec![0x41; 9000]);
        let report = verify(&vfs, path);
        assert!(!report.is_ok());
        assert!(report.page_size.is_none());
        assert_eq!(report.file_len, 9000);
    }

    #[test]
    fn named_pages_come_from_messages() {
        let e = error::checksum_mismatch(PageId(7), 1, 2);
        assert_eq!(page_named_by(&e), Some(PageId(7)));
        let e = error::invalid_page(PageId(9), "overflow chain ends early");
        assert_eq!(page_named_by(&e), Some(PageId(9)));
    }

    #[test]
    fn key_bounds_are_half_open() {
        assert!(in_bounds(b"b", Some(&b"a"[..]), Some(&b"c"[..])));
        assert!(!in_bounds(b"c", None, Some(&b"c"[..])));
        assert!(in_bounds(b"a", Some(&b"a"[..]), None));
    }
}
