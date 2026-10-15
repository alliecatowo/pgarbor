//! `BranchRootResolver`: tells storage which tree roots the directory keeps alive (amendment
//! pgarbor-storage-01). Lease BR-05.
//!
//! Used by `Container::compact`, `Container::verify`, `Container::space_report`,
//! `ExportBuilder::copy_directory` and `gc::collect_roots`. A wrong resolver silently destroys
//! branch data during compaction, so this file is small, total and heavily tested.
use pgarbor_common::{PageId, PgError, Result};
use pgarbor_storage::{NamedRoot, RootResolver};

// CONTRACT
/// Resolver for the v1 directory format.
#[derive(Clone, Copy, Debug, Default)]
pub struct BranchRootResolver;

impl RootResolver for BranchRootResolver {
    /// For `B` keys (`keys::classify == DirKey::Branch`): decode the record (`BranchRecord::decode`,
    /// errors propagate as XX001) and append `record.live_roots()` as `NamedRoot{label, root}` in
    /// that order. A `B` key whose record id differs from the key id → XX001 (same message as
    /// `directory`). `N`, `M` and unknown keys embed nothing (`Ok(())`).
    #[allow(unused_variables)]
    fn roots(&self, key: &[u8], value: &[u8], out: &mut Vec<NamedRoot>) -> Result<()> {
        Err(PgError::unimplemented("BR-05", "branch root enumeration"))
    }

    /// For `B` keys: decode, replace `data_root`, every `extra_databases[i].root` and every
    /// `snapshots[i].root` that is not NONE with `map(root)` — but ONLY for records whose roots are
    /// live (Active, or Deleted with `roots_retained`); a dropped tombstone embeds no roots →
    /// `Ok(None)` (copied unchanged). Re-encode and return `Some(bytes)`. Other keys → `Ok(None)`.
    /// `map` must be called exactly once per live non-NONE root, in `live_roots()` order (the
    /// copier memoizes, so shared roots map to the same new id).
    #[allow(unused_variables)]
    fn remap(
        &self,
        key: &[u8],
        value: &[u8],
        map: &mut dyn FnMut(PageId) -> Result<PageId>,
    ) -> Result<Option<Vec<u8>>> {
        Err(PgError::unimplemented("BR-05", "branch root remapping"))
    }
}
