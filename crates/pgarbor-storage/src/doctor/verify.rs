//! Structural verification (STORE-19). Lease STO-18.
//!
//! Corruption is REPORTED as `Issue`s (verification continues with sibling subtrees); only
//! non-corruption failures (I/O errors, SQLSTATE class 58/53) abort with `Err`. Distinguish via
//! `err.sqlstate == SqlState::DATA_CORRUPTED`.
#[allow(unused_imports)]
use crate::btree::node::{
    internal_spans, leaf_spans, max_inline_value, max_key_len, node_capacity, span_key, span_value,
    LeafValueRef,
};
#[allow(unused_imports)]
use crate::btree::overflow::chain_pages;
use crate::container::handle::Container;
use crate::container::options::VerifyLevel;
use crate::container::snapshot::Snapshot;
use crate::doctor::report::{DoctorReport, Issue};
use crate::gc::trace::ReachSet;
#[allow(unused_imports)]
use crate::gc::trace::{collect_roots, trace};
use crate::io::Vfs;
use crate::pager::PageSource;
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result};
use std::path::Path;
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
    Err(PgError::unimplemented("STO-18", "tree verification"))
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
    Err(PgError::unimplemented("STO-18", "snapshot verification"))
}

// CONTRACT
/// `verify_snapshot(&c.snapshot(), ..)` + `slots`/`recovery` from `c.recovery_report()` + file length.
#[allow(unused_variables)]
pub fn verify_container(c: &Container, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
    Err(PgError::unimplemented("STO-18", "container verification"))
}

// CONTRACT
/// Doctor for a path that may not even open. Opens the file read-only via `vfs.open(ReadOnly)`,
/// reads slots with `container::recovery::read_slots`; if `choose_slot` fails, returns a report
/// with the slot statuses and one Error issue carrying the open error message (Ok, not Err). If it
/// succeeds, closes that handle and opens `Container::open_with(vfs, path, read_only, verify None)`
/// and returns `verify_container`. A file locked exclusively by another process → Err 55006.
#[allow(unused_variables)]
pub fn verify_file(vfs: Arc<dyn Vfs>, path: &Path, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
    Err(PgError::unimplemented("STO-18", "file verification"))
}
