//! Option and selector types for branch operations. CONTRACT. Data only, fully implemented.
use pgarbor_common::BranchId;
use pgarbor_storage::{SyncMode, VerifyLevel};
use std::fmt;

// CONTRACT
/// How a caller names a branch. Names resolve through the `N` index (active branches only);
/// ids resolve through the `B` record (an id of a deleted branch is reported as deleted).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BranchRef {
    Name(String),
    Id(BranchId),
}

impl BranchRef {
    pub fn name(n: impl Into<String>) -> BranchRef {
        BranchRef::Name(n.into())
    }
    pub fn id(id: BranchId) -> BranchRef {
        BranchRef::Id(id)
    }
}

impl From<&str> for BranchRef {
    fn from(s: &str) -> BranchRef {
        BranchRef::Name(s.to_string())
    }
}

impl From<BranchId> for BranchRef {
    fn from(id: BranchId) -> BranchRef {
        BranchRef::Id(id)
    }
}

impl fmt::Display for BranchRef {
    /// Name as-is, id in hyphenated form (used inside error messages: `branch "<this>"`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BranchRef::Name(n) => f.write_str(n),
            BranchRef::Id(id) => write!(f, "{id}"),
        }
    }
}

// CONTRACT
/// Where a new branch's data comes from. Both are O(1): the new record copies root pointers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateSource {
    /// The source branch's CURRENT committed head (all database roots). `parent` = source id.
    Branch(BranchRef),
    /// A named snapshot of a branch. `parent` = that branch's id.
    Snapshot { branch: BranchRef, label: String },
}

impl CreateSource {
    pub fn branch(name: &str) -> CreateSource {
        CreateSource::Branch(BranchRef::name(name))
    }
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// Copy the source branch's named snapshots into the new branch (default false: a new branch
    /// starts with no snapshots).
    pub copy_snapshots: bool,
    /// Fixed id (tests / import). `None` → random UUIDv4.
    pub id: Option<BranchId>,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteOptions {
    /// Keep the tombstone's roots (data stays restorable with `restore` until purged). Default
    /// true ("no surprise destructive actions", RFC 001 §10). false = drop roots immediately;
    /// space is reclaimed by the next `compact`.
    pub retain: bool,
    /// Allow deleting the default branch (the default then becomes unset). Default false.
    pub allow_default: bool,
}

impl Default for DeleteOptions {
    fn default() -> Self {
        DeleteOptions { retain: true, allow_default: false }
    }
}

// CONTRACT
/// Which tombstones `purge_deleted` removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PurgeFilter {
    /// Every deleted branch.
    All,
    /// Deleted branches whose `deleted_at_seq` is strictly below this commit.
    DeletedBefore(pgarbor_common::CommitSeq),
    /// One deleted branch.
    Id(BranchId),
}

// CONTRACT
/// Target of a reset (BRANCH-06). The branch's data roots are replaced by the target's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResetTarget {
    /// One of this branch's own named snapshots.
    Snapshot(String),
    /// Another branch's current head (e.g. reset `feature` to `main`).
    Branch(BranchRef),
    /// The current head of this branch's parent (error 55000 `branch "<x>" has no parent` when
    /// `parent` is None or the parent is deleted).
    Parent,
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResetOptions {
    /// Save the pre-reset head as a named snapshot with this label (kind `ResetRecovery`) in the
    /// same commit. `None` = no recovery snapshot (the old head becomes garbage at next compact).
    pub recovery_snapshot: Option<String>,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompareOptions {
    /// Stop descending after this many differing keys have been counted (the report is then
    /// marked `truncated`). Default 10_000_000.
    pub max_differences: u64,
    /// Keep up to this many example differing keys per space (default 5).
    pub samples_per_space: usize,
    /// Also compute page-level shared/exclusive counts (a trace; O(pages of both trees)).
    pub include_pages: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions { max_differences: 10_000_000, samples_per_space: 5, include_pages: true }
    }
}

// CONTRACT
/// `.pgabranch` creation options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandleOptions {
    /// Replace an existing handle file (default false → 58P02 `file "<path>" already exists`).
    pub overwrite: bool,
    /// Store an absolute `db_path_hint` instead of a path relative to the handle's directory.
    pub absolute_db_path: bool,
}

// CONTRACT
/// `pgarbor branch detach --strategy` (RFC 001 §4.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DetachStrategy {
    /// Try a filesystem reflink clone + logical prune; fall back to a streaming copy export
    /// (reporting the fallback reason).
    #[default]
    Auto,
    /// Reflink or fail with 0A000 `reflink cloning is not supported for "<path>"`.
    ReflinkRequired,
    /// Always the streaming compact export (`branch export`).
    Copy,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetachOptions {
    pub strategy: DetachStrategy,
    /// Branch name inside the new file (default "main").
    pub target_name: String,
    /// Carry the branch's named snapshots into the new file (default false).
    pub include_snapshots: bool,
    /// Replace an existing destination (default false → 58P02).
    pub overwrite: bool,
    pub sync: SyncMode,
    /// Page size for copy exports (`None` = source page size). Reflink always keeps the source's.
    pub page_size: Option<u32>,
    /// After a reflink clone + logical prune, also `compact()` the new file. Compaction rewrites
    /// every page, which defeats extent sharing, so the default is false (dead pages stay in the
    /// clone until the user compacts it; the report says how many).
    pub compact_after_reflink: bool,
    /// Verification level of the reopen-before-success check (default Full).
    pub verify: VerifyLevel,
    /// The container's Vfs is the real OS file system (`OsVfs`), so a reflink may be attempted
    /// on `container.path()`. Default FALSE (safe: never reflink a real file that merely shares a
    /// path with a MemVfs/FaultVfs container). Callers that know set it: pgarbor-core/pgarbor via
    /// `EngineOptions::vfs.is_none()`, the CLI always (it opens real files).
    pub os_filesystem: bool,
    /// Fixed ids for deterministic tests (`None` → random).
    pub new_db_uuid: Option<pgarbor_common::DatabaseUuid>,
    pub new_branch_id: Option<BranchId>,
}

impl Default for DetachOptions {
    fn default() -> Self {
        DetachOptions {
            strategy: DetachStrategy::Auto,
            target_name: "main".to_string(),
            include_snapshots: false,
            overwrite: false,
            sync: SyncMode::Full,
            page_size: None,
            compact_after_reflink: false,
            verify: VerifyLevel::Full,
            os_filesystem: false,
            new_db_uuid: None,
            new_branch_id: None,
        }
    }
}
