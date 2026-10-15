//! PGArbor single-file copy-on-write storage engine (ARCHITECTURE.md §5, STORE-01..19).
//!
//! CONTRACT: every `pub` item re-exported here is frozen at `contracts-v1`
//! (see `contracts/crates/pgarbor-storage.md`). Changes need an amendment under
//! `contracts/amendments/`.
//!
//! Layering inside the crate (lower never imports higher):
//!
//! ```text
//! io (FileIo, Vfs, OsFile/OsVfs, MemFile/MemVfs)
//!   -> format (superblock + page codecs, constants)
//!     -> pager (PageSource/PageSink traits, PageCache, Pager)
//!       -> btree (node layout, overflow, read/insert/delete/bulk)
//!         -> container (Container, WriteTxn, Snapshot, recovery, pins)
//!           -> gc (trace, copy, compact), export, doctor
//! ```
//!
//! Storage knows nothing about SQL, types, catalogs or branch records. Branch records
//! that embed tree roots are interpreted only through the [`RootResolver`] callback.
//!
//! North-star usage (what pgarbor-branches / pgarbor-core do):
//!
//! ```ignore
//! use pgarbor_storage::{Container, CreateOptions, OpenOptions};
//! let c = Container::create("dev.pga", &CreateOptions::default())?;
//! let mut w = c.begin_write()?;
//! let data = w.put(PageId::NONE, b"k", b"v")?.root;          // new data tree
//! let dir = w.put(w.directory_root(), b"Rmain", &data.0.to_le_bytes())?.root;
//! w.set_directory_root(dir);
//! let seq = w.commit()?;                                      // durable once Ok
//! let snap = c.snapshot();                                    // concurrent readers
//! let tree = snap.tree(data)?;                                // implements KvRead
//! assert_eq!(tree.get(b"k")?, Some(b"v".to_vec()));
//! ```

pub mod batch;
pub mod btree;
pub mod container;
pub mod doctor;
pub mod error;
pub mod export;
pub mod format;
pub mod gc;
pub mod io;
pub mod pager;
pub mod roots;

pub use batch::WriteBatch;
pub use btree::{DeleteResult, PutResult, RangeIter};
pub use container::{
    CompactOptions, CompactReport, Container, CreateOptions, OpenOptions, PinGuard, RecoveryPolicy,
    RecoveryReport, Snapshot, SyncMode, TreeSnapshot, TreeWriter, VerifyLevel, WriteTxn,
};
pub use doctor::{DoctorReport, Issue, IssueSeverity, SpaceReport};
pub use export::{ExportBuilder, ExportOptions, ExportReport};
pub use format::{PageKind, SlotStatus, Superblock};
pub use io::{FileIo, LockMode, MemFile, MemVfs, OpenMode, Vfs};
#[cfg(not(target_arch = "wasm32"))]
pub use io::{OsFile, OsVfs};
pub use pager::{Page, PageCache, PageRef, PageSink, PageSource, Pager};
pub use roots::{NamedRoot, NoNestedRoots, PrefixU64Roots, RootResolver};
