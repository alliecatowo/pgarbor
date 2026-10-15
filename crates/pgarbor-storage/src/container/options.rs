//! Container options and reports. CONTRACT. Fully implemented (data only).
use crate::format::consts::DEFAULT_PAGE_SIZE;
use crate::format::superblock::SlotStatus;
use crate::roots::NamedRoot;
use pgarbor_common::{CommitSeq, DatabaseUuid, PageId};
use serde::Serialize;
use std::path::PathBuf;

// CONTRACT
/// Durability mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum SyncMode {
    /// Every commit: page writes → `sync_data` → superblock write → `sync_data` (ARCHITECTURE §5).
    #[default]
    Full,
    /// No fsyncs at all. Crash-UNSAFE; only for throwaway test databases and benchmarks.
    /// Ordering of writes is unchanged, so a clean shutdown is still consistent.
    Off,
}

// CONTRACT
/// Verification performed when opening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum VerifyLevel {
    /// Superblock checks only.
    None,
    /// Also read and verify the directory root page (cheap; default).
    #[default]
    Roots,
    /// Doctor-level verification of every reachable page of the directory tree and (when a
    /// resolver is given to `doctor`) of nested trees. At open time, only the directory tree.
    Full,
}

// CONTRACT
/// What to do when the newest valid superblock references pages beyond the end of the file
/// (only possible if the platform violated the fsync contract).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum RecoveryPolicy {
    /// Refuse to open (XX001). Default: never silently drop an acknowledged commit.
    #[default]
    Strict,
    /// Use the older valid slot if it fits the file, and report `fell_back_to_older_slot`.
    FallbackToOlderSlot,
}

// CONTRACT
/// Options for `Container::create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateOptions {
    /// Power of two in 4096..=65536. Default 8192.
    pub page_size: u32,
    /// Fixed UUID (tests); `None` → `random_db_uuid()`.
    pub db_uuid: Option<DatabaseUuid>,
    /// Fixed creation time (tests); `None` → now.
    pub created_unix_ms: Option<u64>,
    pub sync: SyncMode,
    /// Page cache capacity in pages. Default 8192 (64 MiB at 8 KiB pages).
    pub cache_pages: usize,
    /// Dirty bytes a write txn keeps in memory before spilling private pages to the file.
    /// Default 64 MiB.
    pub spill_threshold_bytes: usize,
}

impl Default for CreateOptions {
    fn default() -> Self {
        CreateOptions {
            page_size: DEFAULT_PAGE_SIZE,
            db_uuid: None,
            created_unix_ms: None,
            sync: SyncMode::Full,
            cache_pages: 8192,
            spill_threshold_bytes: 64 << 20,
        }
    }
}

// CONTRACT
/// Options for `Container::open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    /// Open read-only with a shared lock (no writes, no tail truncation, no temp cleanup).
    pub read_only: bool,
    pub sync: SyncMode,
    pub cache_pages: usize,
    pub verify: VerifyLevel,
    pub recovery: RecoveryPolicy,
    pub spill_threshold_bytes: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            read_only: false,
            sync: SyncMode::Full,
            cache_pages: 8192,
            verify: VerifyLevel::Roots,
            recovery: RecoveryPolicy::Strict,
            spill_threshold_bytes: 64 << 20,
        }
    }
}

impl OpenOptions {
    /// Same cache/sync settings as a create, for reopening a freshly created file in tests.
    pub fn from_create(c: &CreateOptions) -> OpenOptions {
        OpenOptions {
            sync: c.sync,
            cache_pages: c.cache_pages,
            spill_threshold_bytes: c.spill_threshold_bytes,
            ..OpenOptions::default()
        }
    }
}

// CONTRACT
/// What recovery found when opening (STORE-11). Serialized by `pgarbor doctor`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
    /// Slot whose superblock was adopted.
    pub chosen_slot: usize,
    pub commit_seq: CommitSeq,
    pub slots: [SlotStatus; 2],
    /// File length before any truncation.
    pub file_len: u64,
    /// `file_len_for(page_size, page_count)` of the adopted superblock.
    pub committed_len: u64,
    /// Uncommitted tail bytes removed (writable opens only; 0 for read-only).
    pub truncated_tail_bytes: u64,
    /// Leftover `<path>.compact-tmp` / `<path>.export-tmp` files removed (writable opens only).
    pub removed_temp_files: Vec<PathBuf>,
    /// RecoveryPolicy::FallbackToOlderSlot was used.
    pub fell_back_to_older_slot: bool,
    /// Newly created file (create path), not recovered.
    pub created: bool,
}

// CONTRACT
/// Options for `Container::compact`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactOptions {
    /// Additional roots to keep that are not referenced by the directory (e.g. a root a caller
    /// is about to publish). Their new ids are returned in `CompactReport::remapped_extra_roots`.
    pub extra_roots: Vec<PageId>,
}

// CONTRACT
/// Result of a successful compaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactReport {
    pub old_commit_seq: CommitSeq,
    /// Compaction is itself a commit: `old_commit_seq + 1`.
    pub new_commit_seq: CommitSeq,
    pub old_file_len: u64,
    pub new_file_len: u64,
    pub old_page_count: u64,
    pub new_page_count: u64,
    /// `(old, new)` for each `CompactOptions::extra_roots` entry, same order.
    pub remapped_extra_roots: Vec<(PageId, PageId)>,
    /// Live roots after compaction (new ids), as reported by the resolver.
    pub roots: Vec<NamedRoot>,
}
