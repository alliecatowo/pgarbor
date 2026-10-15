//! `FaultVfs`: crash-simulating file system namespace. Lease FLT-04 (sonnet).
//!
//! Namespace model (CONTRACT): file objects (`Arc<Mutex<FileState>>`, identified by a `FileId`)
//! are separate from names. `live` maps names → ids as seen by the running process; `durable`
//! maps names → ids as they would be after power loss; `pending_dir_ops` is the ordered list of
//! unsynced create/rename/remove ops. `sync_dir(dir)` applies (to `durable`, in order) every
//! pending op whose affected path(s) have parent == `dir` and removes them from the list.
//! A crash image keeps a CrashMode-dependent prefix of pending dir ops (Drop → none, Keep → all,
//! Random → `rng.below(n+1)` first ops), then takes each surviving durable name's
//! `FileState::crash_image`. Handles stay bound to file objects across rename/remove.
use crate::controller::FaultController;
use crate::disk::FileState;
#[allow(unused_imports)]
use crate::file::FaultFile;
use crate::plan::FaultPlan;
#[allow(unused_imports)]
use crate::plan::OpKind;
use pgarbor_common::{PgError, Result};
use pgarbor_storage::io::{FileIo, OpenMode, Vfs};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// CONTRACT
/// Bytes of every file by name — what a disk holds (after a crash, or a clean shutdown).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskImage {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirOp {
    Create { path: PathBuf, id: u64 },
    Rename { from: PathBuf, to: PathBuf },
    Remove { path: PathBuf },
}

#[derive(Debug, Default)]
pub(crate) struct Namespace {
    pub(crate) files: HashMap<u64, Arc<Mutex<FileState>>>,
    pub(crate) live: BTreeMap<PathBuf, u64>,
    pub(crate) durable: BTreeMap<PathBuf, u64>,
    pub(crate) pending_dir_ops: Vec<DirOp>,
    pub(crate) next_id: u64,
}

// CONTRACT
/// Cloning shares the namespace and controller.
#[derive(Clone, Debug)]
pub struct FaultVfs {
    pub(crate) ns: Arc<Mutex<Namespace>>,
    pub(crate) ctl: Arc<FaultController>,
}

fn stub(what: &str) -> io::Error {
    io::Error::other(PgError::unimplemented("FLT-04", what))
}

impl FaultVfs {
    /// Empty disk.
    pub fn new(plan: FaultPlan) -> FaultVfs {
        FaultVfs { ns: Arc::new(Mutex::new(Namespace::default())), ctl: FaultController::new(plan) }
    }
    /// Disk whose files are exactly `image` (all durable, nothing pending).
    pub fn from_image(image: &DiskImage, plan: FaultPlan) -> FaultVfs {
        let mut ns = Namespace::default();
        for (path, bytes) in &image.files {
            let id = ns.next_id;
            ns.next_id += 1;
            ns.files.insert(id, Arc::new(Mutex::new(FileState::new(bytes.clone()))));
            ns.live.insert(path.clone(), id);
            ns.durable.insert(path.clone(), id);
        }
        FaultVfs { ns: Arc::new(Mutex::new(ns)), ctl: FaultController::new(plan) }
    }
    pub fn controller(&self) -> &Arc<FaultController> {
        &self.ctl
    }

    /// Power-loss image (see module docs). Does not modify the running namespace; uses
    /// `ctl.crash_rng()` and `ctl.plan().crash_mode/sector_size`.
    pub fn crash_image(&self) -> Result<DiskImage> {
        Err(PgError::unimplemented("FLT-04", "crash disk image"))
    }

    /// Clean-shutdown image: every live name with its `current` bytes.
    pub fn live_image(&self) -> Result<DiskImage> {
        Err(PgError::unimplemented("FLT-04", "live disk image"))
    }

    /// Sum of live file lengths (for `disk_capacity`).
    pub fn used_bytes(&self) -> Result<u64> {
        Err(PgError::unimplemented("FLT-04", "disk usage"))
    }
}

impl Vfs for FaultVfs {
    /// Decide (`Open` or `Create` kind) then: ReadOnly/ReadWrite need a live name (NotFound);
    /// CreateNew fails with AlreadyExists if live; CreateTruncate reuses the live file object and
    /// `set_len(0)` it (pending) or creates one. New names push `DirOp::Create`. Returns a
    /// `FaultFile` with a capacity closure enforcing `plan.disk_capacity` via `used_bytes`.
    #[allow(unused_variables)]
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>> {
        Err(stub("fault vfs open"))
    }
    #[allow(unused_variables)]
    fn exists(&self, path: &Path) -> io::Result<bool> {
        Err(stub("fault vfs exists"))
    }
    /// Live rename (replace target); push `DirOp::Rename`. NotFound if `from` missing.
    #[allow(unused_variables)]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        Err(stub("fault vfs rename"))
    }
    #[allow(unused_variables)]
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        Err(stub("fault vfs remove"))
    }
    /// Decide (`SyncDir`); FsyncFail → EIO and pending dir ops under `dir` are randomly applied
    /// or dropped (cleared); Proceed → apply them to `durable` in order.
    #[allow(unused_variables)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        Err(stub("fault vfs sync_dir"))
    }
}
