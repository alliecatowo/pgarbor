//! Batch application and bottom-up bulk build. Lease STO-10.
#[allow(unused_imports)]
use crate::btree::node::{
    max_inline_value, max_key_len, node_capacity, underflow_threshold, InternalEntry, InternalNode,
    LeafEntry, LeafNode, LeafValue,
};
#[allow(unused_imports)]
use crate::btree::{delete::delete, insert::put, overflow::write_overflow};
use crate::batch::WriteBatch;
use crate::pager::PageSink;
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
/// Leaf/internal fill target for bulk builds: nodes are closed when adding the next entry would
/// exceed `BULK_FILL_PERCENT`% of `node_capacity` (PG btree leaf fillfactor default is 90).
pub const BULK_FILL_PERCENT: usize = 90;

// CONTRACT
/// Apply every operation of `batch` (ascending key order) to the tree at `root`; returns the new
/// root. `Some(v)` → `put`, `None` → `delete` (absent keys are fine). Fast path: when
/// `root == NONE` and the batch has no deletes, use `build_from_sorted` (identical logical
/// result). Errors propagate from put/delete (e.g. 54000 key too large); on error the sink may
/// hold partially written private pages — callers abandon the whole write txn.
///
/// This is how pgarbor-core commits a transaction overlay (`BTreeMap<Vec<u8>, Option<Vec<u8>>>`)
/// onto a branch head.
#[allow(unused_variables)]
pub fn apply_batch(sink: &mut dyn PageSink, root: PageId, batch: &WriteBatch) -> Result<PageId> {
    Err(PgError::unimplemented("STO-10", "b-tree batch apply"))
}

// CONTRACT
/// Build a new tree from strictly ascending `(key, value)` items; returns its root (`NONE` for no
/// items). Streaming, O(height) memory: keep one open node per level; when a node is closed
/// (fill rule above; always >= 1 entry, and a single entry that alone exceeds the fill target is
/// still placed), write it with `sink.write_new` and push `(first_key, id)` into the parent
/// level (first child of a parent becomes `first_child`, later ones become separators with that
/// child's first key). At the end close all levels bottom-up; a level with exactly one node is
/// the root. Before closing the last node of a level, if it is below `underflow_threshold` and a
/// previous sibling exists, rebalance the last two nodes of that level by bytes (requires keeping
/// the previous closed node unwritten until its successor closes — "write one behind").
/// Values longer than `max_inline_value` go through `write_overflow`.
///
/// Errors: keys not strictly ascending → `PgError::internal("bulk build input keys are not
/// strictly ascending")`; key too long → `error::key_too_large`; item `Err` propagates.
#[allow(unused_variables)]
pub fn build_from_sorted(
    sink: &mut dyn PageSink,
    items: &mut dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>,
) -> Result<PageId> {
    Err(PgError::unimplemented("STO-10", "b-tree bulk build"))
}
