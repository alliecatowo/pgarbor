//! PGArbor branch directory, branch lifecycle, `.pgabranch` handles and standalone export
//! (ARCHITECTURE §6, BRANCH-01..12, RFC 001 §4 and §9).
//!
//! CONTRACT: every `pub` item re-exported here is frozen at `contracts-v1`
//! (see `contracts/crates/pgarbor-branches.md`). Changes need an amendment under
//! `contracts/amendments/`.
//!
//! Layering inside the crate (lower never imports higher):
//!
//! ```text
//! error, name, keys, options              (no storage access)
//!   -> record, meta                       (byte formats of directory values)
//!     -> directory (reads over &dyn KvRead), ops + history (mutations over &mut dyn KvWrite)
//!       -> resolver (RootResolver for storage GC/export/doctor)
//!         -> compare, space, handle, reflink, export, detach   (need a Container/Snapshot)
//!           -> manager (BranchManager: one storage commit per operation)
//! ```
//!
//! Directory operations (`directory`, `ops`, `history`) are pure functions over the
//! `pgarbor_common::KvRead`/`KvWrite` traits, so they are unit-testable with `MemKv` and can be
//! composed by pgarbor-core inside the same storage `WriteTxn` as a data commit.
//!
//! North-star usage:
//!
//! ```ignore
//! use pgarbor_branches::{BranchManager, BranchRef, CreateSource, CreateOptions};
//! let c = pgarbor_storage::Container::create("dev.pga", &Default::default())?;
//! let mgr = BranchManager::open(c)?;                       // initializes "main" on an empty file
//! let feature = mgr.create("feature/auth", &CreateSource::branch("main"), &CreateOptions::default())?;
//! assert_eq!(feature.data_root, mgr.get(&BranchRef::name("main"))?.data_root); // O(1) fork
//! ```

pub mod compare;
pub mod detach;
pub mod directory;
pub mod error;
pub mod export;
pub mod handle;
pub mod history;
pub mod keys;
pub mod manager;
pub mod meta;
pub mod name;
pub mod ops;
pub mod options;
pub mod record;
pub mod reflink;
pub mod resolver;
pub mod space;

pub use compare::{CompareReport, KeyDiffSample, SpaceDiff, SpaceKind};
pub use detach::{detach_branch, DetachReport, StrategyUsed};
pub use export::export_branch;
pub use history::ResetReport;
pub use handle::{HandleFile, ResolvedHandle, HANDLE_FORMAT, HANDLE_EXTENSION};
pub use manager::BranchManager;
pub use meta::{ExportOrigin, DIRECTORY_FORMAT_VERSION};
pub use ops::OpCtx;
pub use options::{
    BranchRef, CompareOptions, CreateOptions, CreateSource, DeleteOptions, DetachOptions, DetachStrategy,
    HandleOptions, PurgeFilter, ResetOptions, ResetTarget,
};
pub use record::{
    BranchRecord, BranchState, DatabaseRoot, NamedSnapshot, SnapshotKind, MAX_SNAPSHOTS_PER_BRANCH,
    POSTGRES_DATABASE_OID, RECORD_FORMAT_VERSION,
};
pub use reflink::CloneOutcome;
pub use resolver::BranchRootResolver;
pub use space::BranchSpace;
