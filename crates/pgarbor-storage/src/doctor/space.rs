//! Space accounting and page inspection (STORE-15 accounting, STORE-19). Lease STO-19.
#[allow(unused_imports)]
use crate::btree::node::{internal_spans, leaf_spans, span_key};
use crate::container::snapshot::Snapshot;
use crate::doctor::report::{PageInspection, SpaceReport};
#[allow(unused_imports)]
use crate::format::page::{decode_page, PageHeader};
#[allow(unused_imports)]
use crate::gc::trace::{collect_roots, trace};
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
/// Build a `SpaceReport` for `snap`: `collect_roots` (directory + resolver roots), `trace(..,
/// per_root = true)`, file length from `snap.pager().file_len()`, arithmetic per the field docs
/// of `SpaceReport` (all subtraction saturating). `pins` is left empty here (`Container` callers
/// may fill it from `Container::pins().list()`).
///
/// Example: fresh container after one commit writing a 1-leaf data tree and a 1-leaf directory:
/// page_count 2, reachable 2, garbage 0, roots [directory (1 page, exclusive 1), data (1, 1)].
#[allow(unused_variables)]
pub fn space_report(snap: &Snapshot, resolver: &dyn RootResolver) -> Result<SpaceReport> {
    Err(PgError::unimplemented("STO-19", "space accounting"))
}

// CONTRACT
/// Inspect one page without failing on damage: `id` outside `1..=page_count` → Err
/// `error::page_out_of_range`; otherwise read raw bytes (`pager.read_raw`), parse the header
/// (`PageHeader::parse`), try `decode_page`; on failure fill `error` with the message and return
/// Ok. For valid node pages fill level/entries/first/last key hex (first 64 bytes of the key,
/// lowercase hex) and children; for overflow pages `children = [next]` when next != 0.
#[allow(unused_variables)]
pub fn inspect_page(snap: &Snapshot, id: PageId) -> Result<PageInspection> {
    Err(PgError::unimplemented("STO-19", "page inspection"))
}
