# Amendment request pgarbor-storage-01: `RootResolver` for roots embedded in directory values

Status: PROPOSED (architect, pgarbor-storage). Implemented in the v1 skeleton as `pgarbor_storage::roots`.

## Problem

ARCHITECTURE §5 says `compact()` "writes a fresh file with only reachable pages (live branches + pinned
roots)", and STORE-14/17 need reachability tracing and export of chosen roots. But the live data-tree roots
(`BranchRecord.data_root`, `BranchRecord.snapshots[*].1`) are serialized inside directory-tree *values* whose
format is owned by pgarbor-branches (§6). Storage must not depend on branches (§1 dependency direction), so it
cannot find those roots. Compaction also renumbers every page, so the embedded PageIds must be **rewritten**.
Without a contract here, compaction would silently drop every branch's data, or branches would have to
reimplement compaction itself.

## Proposed change

Add to the storage contract (already in the skeleton):

```rust
pub struct NamedRoot { pub label: String, pub root: PageId }
pub trait RootResolver: Send + Sync {
    /// Append every tree root referenced by directory entry (key, value).
    fn roots(&self, key: &[u8], value: &[u8], out: &mut Vec<NamedRoot>) -> Result<()>;
    /// Return the value with each embedded root r replaced by map(r), or None if it embeds none.
    fn remap(&self, key: &[u8], value: &[u8],
             map: &mut dyn FnMut(PageId) -> Result<PageId>) -> Result<Option<Vec<u8>>>;
}
```

Storage APIs that need live roots take `&dyn RootResolver`: `Container::compact`, `Container::verify`,
`Container::space_report`, `ExportBuilder::copy_directory`, `gc::trace::collect_roots`.
Storage ships `NoNestedRoots` and `PrefixU64Roots` (test convention: value = 8-byte LE PageId).

## Affected crates

- **pgarbor-branches**: must provide `BranchRootResolver` implementing `RootResolver` for its `B` records
  (data_root + every snapshot root; labels `"branch <name>"`, `"branch <name> snapshot <label>"`), and pass it
  to compaction/doctor/space. Branch export/detach should use `ExportBuilder` (copy_tree of the branch root +
  build a new directory) rather than its own copier.
- **pgarbor-core / pgarbor-cli**: obtain the resolver from pgarbor-branches when calling `compact`,
  `verify`, `space_report` (`pgarbor doctor`, `pgarbor inspect --space`, `pgarbor compact`).
- **ARCHITECTURE.md §5**: add one sentence: "Roots embedded in directory values are discovered and rewritten
  only through the `RootResolver` supplied by the directory owner."

## Also clarified by this amendment (no behaviour change, makes §5 byte-exact)

- All on-disk integers are little-endian. Superblock byte offsets are fixed in
  `contracts/crates/pgarbor-storage.md` (reserved u32 padding after `format_version` and `page_size`; crc32c over
  bytes 0..88 at offset 88).
- `page_count` = highest allocated PageId; valid file length = `8192 + page_count * page_size`.
- An empty tree is `PageId::NONE`; trees never contain empty leaves.
- Reader pins (STORE-13) are process-local and not needed for safety in format v1 (append-only; old snapshots
  keep the old file generation's `Arc<Pager>` across compaction).
