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
use crate::controller::{Decision, FaultController};
use crate::disk::FileState;
use crate::file::{CapacityCheck, FaultFile};
use crate::plan::{CrashMode, FaultPlan, OpKind};
use pgarbor_common::Result;
use pgarbor_storage::io::{parent_dir, FileIo, OpenMode, Vfs};
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

fn lock_ns(ns: &Mutex<Namespace>) -> std::sync::MutexGuard<'_, Namespace> {
    ns.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_file(f: &Mutex<FileState>) -> std::sync::MutexGuard<'_, FileState> {
    f.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sum of the lengths of every live name's file.
fn used_locked(ns: &Namespace) -> u64 {
    let mut total = 0u64;
    for id in ns.live.values() {
        if let Some(f) = ns.files.get(id) {
            total = total.saturating_add(lock_file(f).len());
        }
    }
    total
}

/// Apply one directory op to a name → id map (the durable view).
fn apply_dir_op(map: &mut BTreeMap<PathBuf, u64>, op: &DirOp) {
    match op {
        DirOp::Create { path, id } => {
            map.insert(path.clone(), *id);
        }
        DirOp::Rename { from, to } => {
            if from == to {
                return;
            }
            // A rename whose source name never became durable has nothing to move.
            if let Some(id) = map.remove(from) {
                map.insert(to.clone(), id);
            }
        }
        DirOp::Remove { path } => {
            map.remove(path);
        }
    }
}

/// Whether a pending op is made durable by `sync_dir(dir)`.
fn op_in_dir(op: &DirOp, dir: &Path) -> bool {
    match op {
        DirOp::Create { path, .. } | DirOp::Remove { path } => parent_dir(path) == dir,
        DirOp::Rename { from, to } => parent_dir(from) == dir || parent_dir(to) == dir,
    }
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("pgarbor-fault: no such file: {}", path.display()))
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
        let plan = self.ctl.plan();
        let mut rng = self.ctl.crash_rng();
        let ns = lock_ns(&self.ns);
        let n = ns.pending_dir_ops.len();
        let keep = match plan.crash_mode {
            CrashMode::DropUnsynced => 0,
            CrashMode::KeepUnsynced => n,
            CrashMode::Random { .. } => rng.below(n as u64 + 1) as usize,
        };
        let mut names = ns.durable.clone();
        for op in ns.pending_dir_ops.iter().take(keep) {
            apply_dir_op(&mut names, op);
        }
        let mut image = DiskImage::default();
        for (path, id) in &names {
            if let Some(f) = ns.files.get(id) {
                let st = lock_file(f);
                let bytes = st.crash_image(plan.crash_mode, plan.sector_size, &mut rng)?;
                image.files.insert(path.clone(), bytes);
            }
        }
        Ok(image)
    }

    /// Clean-shutdown image: every live name with its `current` bytes.
    pub fn live_image(&self) -> Result<DiskImage> {
        let ns = lock_ns(&self.ns);
        let mut image = DiskImage::default();
        for (path, id) in &ns.live {
            if let Some(f) = ns.files.get(id) {
                image.files.insert(path.clone(), lock_file(f).current.clone());
            }
        }
        Ok(image)
    }

    /// Sum of live file lengths (for `disk_capacity`).
    pub fn used_bytes(&self) -> Result<u64> {
        Ok(used_locked(&lock_ns(&self.ns)))
    }

    /// Ask the controller; `Proceed`/`ShortWrite` mean go ahead, anything else is an error
    /// with no effect.
    fn gate(&self, kind: OpKind, path: &Path) -> io::Result<()> {
        match self.ctl.decide(kind, path, 0, 0).map_err(io::Error::other)? {
            Decision::Proceed | Decision::ShortWrite(_) => Ok(()),
            d => Err(FaultController::error_for(d)),
        }
    }

    fn capacity_closure(&self) -> CapacityCheck {
        let cap = self.ctl.plan().disk_capacity;
        let ns = Arc::clone(&self.ns);
        Arc::new(move |grow| match cap {
            None => true,
            Some(cap) => {
                let used = used_locked(&lock_ns(&ns));
                used.saturating_add(grow) <= cap
            }
        })
    }
}

impl Vfs for FaultVfs {
    /// Decide (`Open` or `Create` kind) then: ReadOnly/ReadWrite need a live name (NotFound);
    /// CreateNew fails with AlreadyExists if live; CreateTruncate reuses the live file object and
    /// `set_len(0)` it (pending) or creates one. New names push `DirOp::Create`. Returns a
    /// `FaultFile` with a capacity closure enforcing `plan.disk_capacity` via `used_bytes`.
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FileIo>> {
        let kind = match mode {
            OpenMode::ReadOnly | OpenMode::ReadWrite => OpKind::Open,
            OpenMode::CreateNew | OpenMode::CreateTruncate => OpKind::Create,
        };
        self.gate(kind, path)?;
        let capacity = self.capacity_closure();
        let state = {
            let mut ns = lock_ns(&self.ns);
            let existing = ns.live.get(path).copied();
            match (mode, existing) {
                (OpenMode::ReadOnly | OpenMode::ReadWrite, None) => return Err(not_found(path)),
                (OpenMode::ReadOnly | OpenMode::ReadWrite, Some(id)) => {
                    Arc::clone(ns.files.get(&id).ok_or_else(|| not_found(path))?)
                }
                (OpenMode::CreateNew, Some(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("pgarbor-fault: file exists: {}", path.display()),
                    ))
                }
                (OpenMode::CreateTruncate, Some(id)) => {
                    let f = Arc::clone(ns.files.get(&id).ok_or_else(|| not_found(path))?);
                    lock_file(&f).set_len(0);
                    f
                }
                (OpenMode::CreateNew | OpenMode::CreateTruncate, None) => {
                    let id = ns.next_id;
                    ns.next_id += 1;
                    let f = Arc::new(Mutex::new(FileState::default()));
                    ns.files.insert(id, Arc::clone(&f));
                    ns.live.insert(path.to_path_buf(), id);
                    ns.pending_dir_ops.push(DirOp::Create { path: path.to_path_buf(), id });
                    f
                }
            }
        };
        Ok(Box::new(FaultFile::new(state, Arc::clone(&self.ctl), path.to_path_buf(), mode.is_writable(), capacity)))
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.gate(OpKind::Exists, path)?;
        Ok(lock_ns(&self.ns).live.contains_key(path))
    }

    /// Live rename (replace target); push `DirOp::Rename`. NotFound if `from` missing.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.gate(OpKind::Rename, from)?;
        let mut ns = lock_ns(&self.ns);
        let id = ns.live.get(from).copied().ok_or_else(|| not_found(from))?;
        if from == to {
            return Ok(());
        }
        ns.live.remove(from);
        ns.live.insert(to.to_path_buf(), id);
        ns.pending_dir_ops.push(DirOp::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
        Ok(())
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.gate(OpKind::Rename, from)?;
        let mut ns = lock_ns(&self.ns);
        if ns.live.contains_key(to) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
        }
        let id = ns.live.remove(from).ok_or_else(|| not_found(from))?;
        ns.live.insert(to.to_path_buf(), id);
        ns.pending_dir_ops.push(DirOp::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.gate(OpKind::Remove, path)?;
        let mut ns = lock_ns(&self.ns);
        if ns.live.remove(path).is_none() {
            return Err(not_found(path));
        }
        ns.pending_dir_ops.push(DirOp::Remove { path: path.to_path_buf() });
        Ok(())
    }

    /// Decide (`SyncDir`); FsyncFail → EIO and pending dir ops under `dir` are randomly applied
    /// or dropped (cleared); Proceed → apply them to `durable` in order.
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let d = self.ctl.decide(OpKind::SyncDir, dir, 0, 0).map_err(io::Error::other)?;
        let failed = match d {
            Decision::Proceed | Decision::ShortWrite(_) => false,
            Decision::FsyncFail => true,
            other => return Err(FaultController::error_for(other)),
        };
        let dir: PathBuf = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir.to_path_buf() };
        let mut rng = if failed { Some(self.ctl.crash_rng()) } else { None };
        let mut guard = lock_ns(&self.ns);
        let ns = &mut *guard;
        let pending = std::mem::take(&mut ns.pending_dir_ops);
        for op in pending {
            if op_in_dir(&op, &dir) {
                let apply = match rng.as_mut() {
                    Some(r) => r.below(2) == 0,
                    None => true,
                };
                if apply {
                    apply_dir_op(&mut ns.durable, &op);
                }
            } else {
                ns.pending_dir_ops.push(op);
            }
        }
        if failed {
            Err(FaultController::error_for(Decision::FsyncFail))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_plan() -> FaultPlan {
        FaultPlan { crash_mode: CrashMode::DropUnsynced, ..FaultPlan::none(1) }
    }
    fn keep_plan() -> FaultPlan {
        FaultPlan { crash_mode: CrashMode::KeepUnsynced, ..FaultPlan::none(1) }
    }
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn created_unsynced_vanishes_then_persists_after_sync_dir() {
        let v = FaultVfs::new(drop_plan());
        let f = v.open(&p("/d/a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"hello").unwrap();
        f.sync_all().unwrap();
        assert!(v.crash_image().unwrap().files.is_empty());
        v.sync_dir(&p("/d")).unwrap();
        let img = v.crash_image().unwrap();
        assert_eq!(img.files.get(&p("/d/a")).map(|b| b.as_slice()), Some(&b"hello"[..]));
    }

    #[test]
    fn keep_unsynced_keeps_names() {
        let v = FaultVfs::new(keep_plan());
        let f = v.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"x").unwrap();
        assert_eq!(v.crash_image().unwrap().files.get(&p("a")).cloned(), Some(b"x".to_vec()));
    }

    #[test]
    fn rename_reverts_without_sync_dir() {
        let mut base = DiskImage::default();
        base.files.insert(p("/d/a"), b"old".to_vec());
        let v = FaultVfs::from_image(&base, drop_plan());
        let f = v.open(&p("/d/a.tmp"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"new").unwrap();
        f.sync_all().unwrap();
        v.sync_dir(&p("/d")).unwrap();
        v.rename(&p("/d/a.tmp"), &p("/d/a")).unwrap();
        let img = v.crash_image().unwrap();
        assert_eq!(img.files.get(&p("/d/a")).cloned(), Some(b"old".to_vec()));
        assert_eq!(img.files.get(&p("/d/a.tmp")).cloned(), Some(b"new".to_vec()));
        v.sync_dir(&p("/d")).unwrap();
        let img = v.crash_image().unwrap();
        assert_eq!(img.files.get(&p("/d/a")).cloned(), Some(b"new".to_vec()));
        assert!(!img.files.contains_key(&p("/d/a.tmp")));
    }

    #[test]
    fn handles_survive_rename_and_remove() {
        let v = FaultVfs::new(drop_plan());
        let f = v.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"abc").unwrap();
        v.rename(&p("a"), &p("b")).unwrap();
        assert!(!v.exists(&p("a")).unwrap());
        assert!(v.exists(&p("b")).unwrap());
        v.remove_file(&p("b")).unwrap();
        assert!(!v.exists(&p("b")).unwrap());
        let mut buf = [0u8; 3];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(v.remove_file(&p("b")).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(v.rename(&p("zz"), &p("y")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn open_modes() {
        let v = FaultVfs::new(drop_plan());
        assert_eq!(v.open(&p("a"), OpenMode::ReadOnly).unwrap_err().kind(), io::ErrorKind::NotFound);
        let f = v.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"zz").unwrap();
        assert_eq!(v.open(&p("a"), OpenMode::CreateNew).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let g = v.open(&p("a"), OpenMode::CreateTruncate).unwrap();
        assert_eq!(g.len().unwrap(), 0);
        assert_eq!(v.used_bytes().unwrap(), 0);
    }

    #[test]
    fn image_roundtrip() {
        let v = FaultVfs::new(drop_plan());
        let f = v.open(&p("/d/a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"12345").unwrap();
        let live = v.live_image().unwrap();
        assert_eq!(v.used_bytes().unwrap(), 5);
        let w = FaultVfs::from_image(&live, drop_plan());
        assert_eq!(w.live_image().unwrap(), live);
        assert_eq!(w.crash_image().unwrap(), live);
    }

    #[test]
    fn crash_image_deterministic() {
        let run = || {
            let v = FaultVfs::new(FaultPlan::none(9));
            for i in 0..6 {
                let f = v.open(&p(&format!("/d/f{i}")), OpenMode::CreateNew).unwrap();
                f.write_at(0, &[i as u8; 700]).unwrap();
            }
            v.crash_image().unwrap()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn capacity_enforced() {
        let v = FaultVfs::new(FaultPlan { disk_capacity: Some(4), ..FaultPlan::none(1) });
        let f = v.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(0, b"1234").unwrap();
        assert!(f.write_at(4, b"5").is_err());
    }
}
