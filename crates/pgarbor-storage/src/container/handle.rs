//! `Container`: the open `.pga` file. Lease STO-13 (`create_with`, `open_with`); every other body
//! here is implemented.
//!
//! Concurrency model (CONTRACT):
//! - `Container` is `Clone + Send + Sync` (an `Arc` handle). One engine instance per file per
//!   process; other processes are excluded by the OS lock (STORE-12).
//! - Readers: `snapshot()` takes the state `RwLock` for reading only long enough to clone the
//!   current `Arc<Pager>` + `Superblock`; reads then proceed lock-free (cache shards have
//!   their own mutexes).
//! - Writer: `begin_write()` locks the writer `Mutex` for the life of the `WriteTxn` (one writer
//!   at a time; others block). Publishing a commit takes the state `RwLock` for writing briefly.
//!   Compaction holds the writer mutex for its whole duration.
//! - Poisoning: after an I/O failure that leaves the on-disk commit state ambiguous (see
//!   `write.rs`), all further `begin_write`/`compact` calls fail with `error::poisoned`; snapshots
//!   keep working. Reopening runs recovery.
use crate::container::options::{
    CompactOptions, CompactReport, CreateOptions, OpenOptions, RecoveryReport, SyncMode,
};
use crate::container::pins::PinTable;
use crate::container::snapshot::Snapshot;
use crate::container::write::WriteTxn;
use crate::doctor::report::{DoctorReport, Issue, IssueSeverity, SpaceReport};
use crate::gc::trace::ReachSet;
use crate::error;
use crate::format::superblock::Superblock;
#[cfg(not(target_arch = "wasm32"))]
use crate::io::OsVfs;
use crate::io::Vfs;
use crate::pager::{Pager, PagerStats};
use crate::roots::RootResolver;
use crate::container::options::VerifyLevel;
use pgarbor_common::{CommitSeq, DatabaseUuid, PgError, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Published state: what new snapshots/writers start from.
#[derive(Debug, Clone)]
pub(crate) struct ContainerState {
    pub(crate) pager: Arc<Pager>,
    pub(crate) sb: Superblock,
    /// Slot holding `sb`; the next commit writes `1 - slot`.
    pub(crate) slot: usize,
}

/// Writer-exclusive state guarded by the writer mutex.
#[derive(Debug, Default)]
pub(crate) struct WriterState {
    /// Commits published by this process (diagnostics).
    pub(crate) commits: u64,
}

#[derive(Debug)]
pub(crate) struct ContainerInner {
    pub(crate) vfs: Arc<dyn Vfs>,
    pub(crate) path: PathBuf,
    pub(crate) read_only: bool,
    pub(crate) sync: SyncMode,
    pub(crate) cache_pages: usize,
    pub(crate) spill_threshold: usize,
    pub(crate) state: RwLock<ContainerState>,
    pub(crate) writer: Mutex<WriterState>,
    pub(crate) poison: Mutex<Option<String>>,
    pub(crate) pins: PinTable,
    pub(crate) recovery: RecoveryReport,
}

impl ContainerInner {
    /// Current published state (clone).
    pub(crate) fn current(&self) -> ContainerState {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Replace the published state (after a durable commit or compaction swap).
    pub(crate) fn publish(&self, st: ContainerState) {
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = st;
    }
    /// Mark the container unusable for writes; the first reason is kept.
    pub(crate) fn poison(&self, reason: String) {
        let mut p = self.poison.lock().unwrap_or_else(|e| e.into_inner());
        if p.is_none() {
            *p = Some(reason);
        }
    }
    pub(crate) fn poison_reason(&self) -> Option<String> {
        self.poison.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// `Err(error::poisoned)` if poisoned, `Err(error::read_only)` if read-only.
    pub(crate) fn check_writable(&self) -> Result<()> {
        if let Some(r) = self.poison_reason() {
            return Err(error::poisoned(&self.path, &r));
        }
        if self.read_only {
            return Err(error::read_only(&self.path));
        }
        Ok(())
    }
    pub(crate) fn syncing(&self) -> bool {
        self.sync == SyncMode::Full
    }
}

// CONTRACT: compile-time proof of the thread-safety requirements.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Container>();
    assert_send_sync::<Snapshot>();
    assert_send_sync::<crate::container::snapshot::TreeSnapshot>();
    assert_send_sync::<crate::pager::PageCache>();
    assert_send_sync::<Pager>();
};

// CONTRACT
/// Handle to an open container file.
#[derive(Clone, Debug)]
pub struct Container {
    pub(crate) inner: Arc<ContainerInner>,
}

impl Container {
    /// Create a new `.pga` at `path` on the real file system (error if it exists).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create(path: impl AsRef<Path>, opts: &CreateOptions) -> Result<Container> {
        Container::create_with(Arc::new(OsVfs), path.as_ref(), opts)
    }

    /// Open an existing `.pga` on the real file system (runs recovery).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<Path>, opts: &OpenOptions) -> Result<Container> {
        Container::open_with(Arc::new(OsVfs), path.as_ref(), opts)
    }

    /// Create through any `Vfs`. Steps: `recovery::initialize(vfs, path, opts)` (validates page
    /// size, creates + locks + writes slots + syncs file and dir), wrap the file in a `Pager`
    /// (`opts.cache_pages`), build `ContainerInner` (read_only false, sync/spill from opts,
    /// state = {pager, sb, slot 0}, empty PinTable, recovery report with `created = true`).
    /// Errors: existing path → 58P02 via `error::open_failed`; invalid page size → 22023
    /// `invalid page size <n>` (detail "Page size must be a power of two between 4096 and 65536.").
    pub fn create_with(vfs: Arc<dyn Vfs>, path: &Path, opts: &CreateOptions) -> Result<Container> {
        let rec = crate::container::recovery::initialize(vfs.as_ref(), path, opts)?;
        let sb = rec.choice.superblock;
        let slot = rec.choice.slot;
        let pager = Arc::new(Pager::new(rec.file, path.to_path_buf(), sb.page_size, opts.cache_pages));
        Ok(Container {
            inner: Arc::new(ContainerInner {
                vfs,
                path: path.to_path_buf(),
                read_only: false,
                sync: opts.sync,
                cache_pages: opts.cache_pages,
                spill_threshold: opts.spill_threshold_bytes,
                state: RwLock::new(ContainerState { pager, sb, slot }),
                writer: Mutex::new(WriterState::default()),
                poison: Mutex::new(None),
                pins: PinTable::new(),
                recovery: rec.report,
            }),
        })
    }

    /// Open through any `Vfs`. Steps: `recovery::recover(vfs, path, opts)`; Pager over the file;
    /// build inner; then verification per `opts.verify`: `Roots` → if directory_root != NONE,
    /// read it through a fresh `Snapshot` and `btree::node::decode_node` it (any error fails the
    /// open); `Full` → `doctor::verify::verify_tree(&snapshot, directory_root, ..)` and fail with
    /// the first Error-severity issue as XX001 `database file "<path>" failed verification: <msg>`.
    /// Errors: missing file → 58P01; locked → 55006 (`error::locked`); format errors per
    /// `format::superblock::choose_slot`.
    pub fn open_with(vfs: Arc<dyn Vfs>, path: &Path, opts: &OpenOptions) -> Result<Container> {
        let rec = crate::container::recovery::recover(vfs.as_ref(), path, opts)?;
        let sb = rec.choice.superblock;
        let slot = rec.choice.slot;
        let pager = Arc::new(Pager::new(rec.file, path.to_path_buf(), sb.page_size, opts.cache_pages));
        match opts.verify {
            VerifyLevel::None => {}
            VerifyLevel::Roots => {
                if !sb.directory_root.is_none() {
                    let snap = Snapshot::new(pager.clone(), sb, None);
                    let page = snap.pager().read_page(sb.directory_root)?;
                    crate::btree::node::decode_node(&page)?;
                }
            }
            VerifyLevel::Full => {
                let snap = Snapshot::new(pager.clone(), sb, None);
                let mut issues: Vec<Issue> = Vec::new();
                let mut seen = ReachSet::new(sb.page_count);
                crate::doctor::verify::verify_tree(&snap, sb.directory_root, "directory", &mut seen, &mut issues)?;
                if let Some(first) = issues.iter().find(|i| i.severity == IssueSeverity::Error) {
                    return Err(PgError::corruption(format!(
                        "database file \"{}\" failed verification: {}",
                        path.display(),
                        first.message
                    )));
                }
            }
        }
        Ok(Container {
            inner: Arc::new(ContainerInner {
                vfs,
                path: path.to_path_buf(),
                read_only: opts.read_only,
                sync: opts.sync,
                cache_pages: opts.cache_pages,
                spill_threshold: opts.spill_threshold_bytes,
                state: RwLock::new(ContainerState { pager, sb, slot }),
                writer: Mutex::new(WriterState::default()),
                poison: Mutex::new(None),
                pins: PinTable::new(),
                recovery: rec.report,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        &self.inner.vfs
    }
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only
    }
    pub fn superblock(&self) -> Superblock {
        self.inner.current().sb
    }
    /// Latest durable (acknowledged) commit.
    pub fn commit_seq(&self) -> CommitSeq {
        self.inner.current().sb.commit_seq
    }
    pub fn db_uuid(&self) -> DatabaseUuid {
        self.inner.current().sb.db_uuid
    }
    pub fn page_size(&self) -> u32 {
        self.inner.current().sb.page_size
    }
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.inner.recovery
    }
    pub fn is_poisoned(&self) -> bool {
        self.inner.poison_reason().is_some()
    }
    pub fn poison_reason(&self) -> Option<String> {
        self.inner.poison_reason()
    }
    pub fn pins(&self) -> &PinTable {
        &self.inner.pins
    }
    pub fn pager_stats(&self) -> Result<PagerStats> {
        self.inner.current().pager.stats()
    }

    /// Snapshot of the latest durable commit (pinned while alive). Never blocks on the writer.
    pub fn snapshot(&self) -> Snapshot {
        let st = self.inner.current();
        let pin = self.inner.pins.pin(st.sb.commit_seq);
        Snapshot::new(st.pager, st.sb, Some(pin))
    }

    /// Start the single write transaction (blocks while another is active). Errors: poisoned →
    /// 58030 `error::poisoned`; read-only → 25006 `error::read_only`.
    pub fn begin_write(&self) -> Result<WriteTxn<'_>> {
        WriteTxn::begin(&self.inner)
    }

    /// Rewrite the file with only live pages (STORE-16). See `gc::compact::compact`.
    pub fn compact(&self, resolver: &dyn RootResolver, opts: &CompactOptions) -> Result<CompactReport> {
        crate::gc::compact::compact(&self.inner, resolver, opts)
    }

    /// Verify the current snapshot (doctor). See `doctor::verify::verify_container`.
    pub fn verify(&self, resolver: &dyn RootResolver, level: VerifyLevel) -> Result<DoctorReport> {
        crate::doctor::verify::verify_container(self, resolver, level)
    }

    /// Space accounting for the current snapshot (`inspect --space`).
    pub fn space_report(&self, resolver: &dyn RootResolver) -> Result<SpaceReport> {
        crate::doctor::space::space_report(&self.snapshot(), resolver)
    }

    /// Drop this handle. The file lock is released when the last clone AND the last snapshot of
    /// the current file generation are dropped (snapshots keep the Pager, which owns the locked
    /// file handle). There is nothing to flush: every acknowledged commit is already durable.
    pub fn close(self) -> Result<()> {
        drop(self);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemVfs;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn container_is_send_sync() {
        assert_send_sync::<Container>();
    }

    #[test]
    fn create_then_open_round_trip() {
        let vfs = Arc::new(MemVfs::new());
        let path = Path::new("/rt.pga");
        let copts = CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() };
        let c = Container::create_with(vfs.clone(), path, &copts).unwrap();
        let uuid = c.db_uuid();
        assert!(c.recovery_report().created);
        assert_eq!(c.page_size(), copts.page_size);
        c.close().unwrap();
        let o = Container::open_with(vfs, path, &OpenOptions::from_create(&copts)).unwrap();
        assert_eq!(o.db_uuid(), uuid);
        assert!(!o.recovery_report().created);
        assert!(!o.is_poisoned());
    }

    #[test]
    fn create_existing_fails_58p02() {
        let vfs = Arc::new(MemVfs::new());
        let path = Path::new("/dup.pga");
        let copts = CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() };
        let _c = Container::create_with(vfs.clone(), path, &copts).unwrap();
        let before = vfs.read_bytes(path).unwrap();
        let e = Container::create_with(vfs.clone(), path, &copts).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P02");
        assert_eq!(vfs.read_bytes(path).unwrap(), before);
    }

    #[test]
    fn invalid_page_size_rejected() {
        let vfs = Arc::new(MemVfs::new());
        let copts = CreateOptions { page_size: 5000, sync: SyncMode::Off, ..CreateOptions::default() };
        let e = Container::create_with(vfs, Path::new("/bad.pga"), &copts).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
    }

    #[test]
    fn missing_file_open_fails() {
        let vfs = Arc::new(MemVfs::new());
        let e = Container::open_with(vfs, Path::new("/none.pga"), &OpenOptions::default()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P01");
    }

    #[test]
    fn read_only_refuses_writes() {
        let vfs = Arc::new(MemVfs::new());
        let path = Path::new("/ro.pga");
        let copts = CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() };
        Container::create_with(vfs.clone(), path, &copts).unwrap().close().unwrap();
        let o = Container::open_with(vfs, path, &OpenOptions { read_only: true, sync: SyncMode::Off, ..OpenOptions::default() })
            .unwrap();
        assert!(o.is_read_only());
        let e = o.begin_write().err().expect("write must be refused");
        assert_eq!(e.sqlstate.code(), "25006");
    }

    #[test]
    fn verify_full_on_empty_directory() {
        let vfs = Arc::new(MemVfs::new());
        let path = Path::new("/full.pga");
        let copts = CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() };
        Container::create_with(vfs.clone(), path, &copts).unwrap().close().unwrap();
        let o = OpenOptions { verify: VerifyLevel::Full, sync: SyncMode::Off, ..OpenOptions::default() };
        Container::open_with(vfs, path, &o).unwrap();
    }
}
