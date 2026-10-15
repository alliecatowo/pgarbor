//! STO-21: container lifecycle integration tests (north-star storage slice).
//!
//! Lease STO-21 writes this file. Required tests (use `tempfile::tempdir()` + `OsVfs` for the
//! real-file ones, `MemVfs` otherwise):
//! 1. `create_commit_reopen`: create `dev.pga`, put 1000 rows in a data tree, store its root in
//!    the directory under `b"Rmain"` (PrefixU64Roots convention), commit → CommitSeq(2); drop;
//!    reopen; read back identical data; superblock commit_seq 2; file length ==
//!    8192 + page_count * 8192.
//! 2. `branch_is_o1_and_isolated`: copy main's root to `b"Rfeature"` in one commit (no page
//!    writes besides the directory); mutate feature; main unchanged; both reopen correctly.
//! 3. `second_open_is_locked`: second `Container::open` of the same path (same process, new
//!    handle) fails with SQLSTATE 55006; read-only open of a read-write-locked file also fails.
//! 4. `concurrent_readers`: 8 threads read snapshots in a loop while the writer commits 200
//!    times; every reader observes a consistent (prefix-closed) state; no errors.
//! 5. `compaction_shrinks_and_preserves`: after many overwrites, `compact(&PrefixU64Roots)`
//!    reduces file size, keeps every root's contents, commit_seq increases by 1, an old
//!    snapshot taken before compaction still reads its data, reopen works.
//! 6. `export_branch`: ExportBuilder copies one root into `feature.pga` with a fresh UUID while
//!    the source keeps committing; the export opens and contains exactly the snapshot contents.
//! 7. `bad_files_fail_closed`: random bytes file → 'not a PGArbor database' (XX001); superblock
//!    with format_version 2 (valid crc) → unsupported version (XX001); both slots corrupt → XX001.
//! 8. `tail_garbage_truncated`: append 3 garbage pages past page_count; reopen truncates them and
//!    reports `truncated_tail_bytes`.
//! 9. `doctor_reports`: verify/space_report JSON contains the expected counts on a known layout.

use pgarbor_common::{CommitSeq, KvRead, PageId, Result, SqlState};
use pgarbor_storage::{
    CompactOptions, Container, CreateOptions, ExportBuilder, ExportOptions, IssueSeverity, MemVfs,
    OpenOptions, PrefixU64Roots, Snapshot, SyncMode, VerifyLevel, Vfs, WriteTxn,
};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Rows = Vec<(Vec<u8>, Vec<u8>)>;

fn key(i: usize) -> Vec<u8> {
    format!("key-{i:06}").into_bytes()
}

fn val(i: usize, generation: usize) -> Vec<u8> {
    format!("value-{i}-gen{generation}-").repeat(8).into_bytes()
}

fn resolver() -> PrefixU64Roots {
    PrefixU64Roots::new(b"R")
}

fn decode_root(v: &[u8]) -> PageId {
    let b: [u8; 8] = v.try_into().expect("root value is 8 bytes");
    PageId(u64::from_le_bytes(b))
}

/// Point the directory entry `name` at `root` (PrefixU64Roots convention).
fn set_root(w: &mut WriteTxn<'_>, name: &[u8], root: PageId) -> Result<()> {
    let dir = w.directory_root();
    let new_dir = w.put(dir, name, &root.0.to_le_bytes())?.root;
    w.set_directory_root(new_dir);
    Ok(())
}

fn get_root_w(w: &WriteTxn<'_>, name: &[u8]) -> Result<Option<PageId>> {
    Ok(w.get(w.directory_root(), name)?.map(|v| decode_root(&v)))
}

fn lookup_root(snap: &Snapshot, name: &[u8]) -> Option<PageId> {
    snap.directory().get(name).unwrap().map(|v| decode_root(&v))
}

/// Full contents of the tree named `name` in the directory (empty if the name is absent).
fn dump(snap: &Snapshot, name: &[u8]) -> Rows {
    match lookup_root(snap, name) {
        None => Vec::new(),
        Some(root) => {
            let t = snap.tree(root).unwrap();
            let rows: Rows = t
                .scan(Bound::Unbounded, Bound::Unbounded)
                .unwrap()
                .collect::<Result<Rows>>()
                .unwrap();
            rows
        }
    }
}

fn expected_rows(n: usize, generation: usize) -> Rows {
    (0..n).map(|i| (key(i), val(i, generation))).collect()
}

/// Write `n` rows of generation `generation` into a new data tree and name it `name`.
fn put_rows(w: &mut WriteTxn<'_>, name: &[u8], n: usize, generation: usize) -> Result<PageId> {
    let mut root = PageId::NONE;
    for i in 0..n {
        root = w.put(root, &key(i), &val(i, generation))?.root;
    }
    set_root(w, name, root)?;
    Ok(root)
}

fn fast_create() -> CreateOptions {
    CreateOptions { sync: SyncMode::Off, ..CreateOptions::default() }
}

fn fast_open() -> OpenOptions {
    OpenOptions { sync: SyncMode::Off, ..OpenOptions::default() }
}

fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("dev.pga")
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn assert_verifies(c: &Container) {
    let rep = c.verify(&resolver(), VerifyLevel::Full).unwrap();
    let errs: Vec<String> = rep.errors().map(|i| i.message.clone()).collect();
    assert!(errs.is_empty(), "verify reported errors: {errs:?}");
}

#[test]
fn create_commit_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let uuid;
    {
        let c = Container::create(&path, &CreateOptions::default()).unwrap();
        assert_eq!(c.commit_seq(), CommitSeq(1));
        uuid = c.db_uuid();
        let mut w = c.begin_write().unwrap();
        put_rows(&mut w, b"Rmain", 1000, 0).unwrap();
        let seq = w.commit().unwrap();
        assert_eq!(seq, CommitSeq(2));
        assert_eq!(c.commit_seq(), CommitSeq(2));
    }
    let c = Container::open(&path, &OpenOptions::default()).unwrap();
    assert_eq!(c.commit_seq(), CommitSeq(2));
    assert_eq!(c.superblock().commit_seq, CommitSeq(2));
    assert_eq!(c.db_uuid(), uuid);
    {
        let snap = c.snapshot();
        assert_eq!(dump(&snap, b"Rmain"), expected_rows(1000, 0));
        let tree = snap.tree(lookup_root(&snap, b"Rmain").unwrap()).unwrap();
        assert_eq!(tree.get(&key(500)).unwrap(), Some(val(500, 0)));
        assert_eq!(tree.get(b"missing").unwrap(), None);
    }
    let page_count = c.superblock().page_count;
    let ps = c.page_size() as u64;
    assert_verifies(&c);
    drop(c);
    assert_eq!(file_len(&path), 8192 + page_count * ps);
}

#[test]
fn branch_is_o1_and_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let main_rows = 2000;
    {
        let c = Container::create(&path, &fast_create()).unwrap();
        let mut w = c.begin_write().unwrap();
        put_rows(&mut w, b"Rmain", main_rows, 0).unwrap();
        w.commit().unwrap();

        // Branch: copy the root pointer only.
        let pages_before = c.superblock().page_count;
        let mut w = c.begin_write().unwrap();
        let main_root = get_root_w(&w, b"Rmain").unwrap().unwrap();
        set_root(&mut w, b"Rfeature", main_root).unwrap();
        w.commit().unwrap();
        let pages_after = c.superblock().page_count;
        // Only the (tiny, single-leaf) directory is rewritten; no data page is copied.
        assert!(
            pages_after - pages_before <= 2,
            "branching wrote {} pages",
            pages_after - pages_before
        );
        {
            let snap = c.snapshot();
            assert_eq!(lookup_root(&snap, b"Rfeature"), Some(main_root));
        }

        // Mutate the feature branch.
        let mut w = c.begin_write().unwrap();
        let mut froot = get_root_w(&w, b"Rfeature").unwrap().unwrap();
        for i in 0..100 {
            froot = w.put(froot, &key(i), &val(i, 1)).unwrap().root;
        }
        froot = w.delete(froot, &key(main_rows - 1)).unwrap().root;
        froot = w.put(froot, b"zzz-new", b"new").unwrap().root;
        set_root(&mut w, b"Rfeature", froot).unwrap();
        w.commit().unwrap();

        let snap = c.snapshot();
        assert_eq!(dump(&snap, b"Rmain"), expected_rows(main_rows, 0));
        assert_eq!(dump(&snap, b"Rfeature").len(), main_rows);
    }
    // Both branches survive a reopen.
    let c = Container::open(&path, &fast_open()).unwrap();
    let snap = c.snapshot();
    assert_eq!(dump(&snap, b"Rmain"), expected_rows(main_rows, 0));
    let mut want: BTreeMap<Vec<u8>, Vec<u8>> = expected_rows(main_rows, 0).into_iter().collect();
    for i in 0..100 {
        want.insert(key(i), val(i, 1));
    }
    want.remove(&key(main_rows - 1));
    want.insert(b"zzz-new".to_vec(), b"new".to_vec());
    let want: Rows = want.into_iter().collect();
    assert_eq!(dump(&snap, b"Rfeature"), want);
    drop(snap);
    assert_verifies(&c);
}

#[test]
fn second_open_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let c = Container::create(&path, &fast_create()).unwrap();

    let e = Container::open(&path, &fast_open()).err().expect("second open must fail");
    assert_eq!(e.sqlstate, SqlState::OBJECT_IN_USE);
    assert_eq!(e.sqlstate.code(), "55006");

    let ro = OpenOptions { read_only: true, ..fast_open() };
    let e = Container::open(&path, &ro).err().expect("read-only open of a locked file must fail");
    assert_eq!(e.sqlstate.code(), "55006");

    // The original handle is unaffected and releasing it frees the lock.
    let mut w = c.begin_write().unwrap();
    put_rows(&mut w, b"Rmain", 10, 0).unwrap();
    w.commit().unwrap();
    drop(c);
    let c2 = Container::open(&path, &fast_open()).unwrap();
    assert_eq!(c2.commit_seq(), CommitSeq(2));
    drop(c2);

    // A read-only handle (shared lock) excludes a writable one.
    let ro_handle = Container::open(&path, &ro).unwrap();
    assert!(ro_handle.is_read_only());
    let e = Container::open(&path, &fast_open()).err().expect("writable open must fail");
    assert_eq!(e.sqlstate.code(), "55006");
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn concurrent_readers() {
    const COMMITS: usize = 200;
    const READERS: usize = 8;
    let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
    let c = Container::create_with(vfs, Path::new("concurrent.pga"), &fast_create()).unwrap();

    // Expected full state of the "main" tree per commit sequence, recorded BEFORE the commit
    // publishes, so any reader that sees sequence N can find its expectation.
    let expected: Mutex<BTreeMap<u64, Rows>> = Mutex::new(BTreeMap::new());
    expected.lock().unwrap().insert(1, Vec::new());
    let stop = AtomicBool::new(false);

    let finals: Vec<u64> = std::thread::scope(|s| {
        let expected = &expected;
        let stop = &stop;
        let handles: Vec<_> = (0..READERS)
            .map(|_| {
                let c = c.clone();
                s.spawn(move || {
                    let mut last = 0u64;
                    loop {
                        let done = stop.load(Ordering::Acquire);
                        let snap = c.snapshot();
                        let seq = snap.commit_seq().0;
                        assert!(seq >= last, "commit_seq went backwards: {seq} < {last}");
                        last = seq;
                        let got = dump(&snap, b"Rmain");
                        let want = expected
                            .lock()
                            .unwrap()
                            .get(&seq)
                            .cloned()
                            .unwrap_or_else(|| panic!("no expectation recorded for commit {seq}"));
                        assert_eq!(got, want, "snapshot at commit {seq} is not the writer's state");
                        if done {
                            break;
                        }
                    }
                    last
                })
            })
            .collect();

        {
            let _guard = StopOnDrop(stop);
            let mut state: Rows = Vec::new();
            for i in 0..COMMITS {
                let mut w = c.begin_write().unwrap();
                let root = get_root_w(&w, b"Rmain").unwrap().unwrap_or(PageId::NONE);
                let (k, v) = (key(i), val(i, 0));
                let root = w.put(root, &k, &v).unwrap().root;
                set_root(&mut w, b"Rmain", root).unwrap();
                state.push((k, v));
                let seq = w.commit_seq().0;
                expected.lock().unwrap().insert(seq, state.clone());
                assert_eq!(w.commit().unwrap().0, seq);
            }
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert_eq!(c.commit_seq(), CommitSeq(1 + COMMITS as u64));
    for last in finals {
        // The final iteration starts after `stop`, so every reader ends on the final commit.
        assert_eq!(last, 1 + COMMITS as u64);
    }
    assert_verifies(&c);
}

#[test]
fn compaction_shrinks_and_preserves() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let c = Container::create(&path, &fast_create()).unwrap();

    let mut w = c.begin_write().unwrap();
    put_rows(&mut w, b"Rmain", 300, 0).unwrap();
    put_rows(&mut w, b"Rother", 50, 7).unwrap();
    w.commit().unwrap();

    // Many overwrites, one commit each, to generate lots of dead pages.
    let rounds = 30;
    for g in 1..=rounds {
        let mut w = c.begin_write().unwrap();
        let mut root = get_root_w(&w, b"Rmain").unwrap().unwrap();
        for i in 0..300 {
            root = w.put(root, &key(i), &val(i, g)).unwrap().root;
        }
        set_root(&mut w, b"Rmain", root).unwrap();
        w.commit().unwrap();
    }

    let old_snap = c.snapshot();
    let old_seq = c.commit_seq();
    assert_eq!(old_snap.commit_seq(), old_seq);
    let len_before = file_len(&path);

    let rep = c.compact(&resolver(), &CompactOptions::default()).unwrap();
    let len_after = file_len(&path);
    assert!(len_after < len_before, "compaction did not shrink: {len_before} -> {len_after}");
    assert_eq!(rep.old_commit_seq, old_seq);
    assert_eq!(rep.new_commit_seq, CommitSeq(old_seq.0 + 1));
    assert_eq!(c.commit_seq(), CommitSeq(old_seq.0 + 1));
    assert!(rep.new_page_count < rep.old_page_count);

    // Contents preserved for every root.
    {
        let snap = c.snapshot();
        assert_eq!(dump(&snap, b"Rmain"), expected_rows(300, rounds));
        assert_eq!(dump(&snap, b"Rother"), expected_rows(50, 7));
    }
    // The pre-compaction snapshot still reads its own data (from the unlinked old file).
    assert_eq!(old_snap.commit_seq(), old_seq);
    assert_eq!(dump(&old_snap, b"Rmain"), expected_rows(300, rounds));
    assert_eq!(dump(&old_snap, b"Rother"), expected_rows(50, 7));

    // The container keeps working after compaction.
    let mut w = c.begin_write().unwrap();
    let root = get_root_w(&w, b"Rother").unwrap().unwrap();
    let root = w.put(root, b"after-compact", b"1").unwrap().root;
    set_root(&mut w, b"Rother", root).unwrap();
    assert_eq!(w.commit().unwrap(), CommitSeq(old_seq.0 + 2));
    assert_verifies(&c);

    drop(old_snap);
    drop(c);
    let c = Container::open(&path, &fast_open()).unwrap();
    assert_eq!(c.commit_seq(), CommitSeq(old_seq.0 + 2));
    {
        let snap = c.snapshot();
        assert_eq!(dump(&snap, b"Rmain"), expected_rows(300, rounds));
        assert_eq!(dump(&snap, b"Rother").len(), 51);
    }
    assert_verifies(&c);
}

#[test]
fn export_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let dest = dir.path().join("feature.pga");
    let c = Container::create(&path, &fast_create()).unwrap();

    let mut w = c.begin_write().unwrap();
    put_rows(&mut w, b"Rmain", 400, 0).unwrap();
    put_rows(&mut w, b"Rfeature", 700, 3).unwrap();
    w.commit().unwrap();

    let snap = c.snapshot();
    let feature_root = lookup_root(&snap, b"Rfeature").unwrap();
    let want = dump(&snap, b"Rfeature");

    let mut b = ExportBuilder::create(c.vfs().clone(), &dest, &ExportOptions::default()).unwrap();
    let data = b.copy_tree(&snap, feature_root).unwrap();

    // The source keeps committing while the export is in flight.
    for g in 1..=5 {
        let mut w = c.begin_write().unwrap();
        let mut root = get_root_w(&w, b"Rfeature").unwrap().unwrap();
        for i in 0..50 {
            root = w.put(root, &key(i), &val(i, 100 + g)).unwrap().root;
        }
        set_root(&mut w, b"Rfeature", root).unwrap();
        w.commit().unwrap();
    }

    let dir_root = b.put(PageId::NONE, b"Rmain", &data.0.to_le_bytes()).unwrap();
    let report = b.finish(dir_root).unwrap();
    assert_eq!(report.path, dest);
    assert_eq!(report.file_len, file_len(&dest));
    assert!(!dir.path().join("feature.pga.export-tmp").exists());
    assert_ne!(report.db_uuid, c.db_uuid(), "export must have a fresh identity");

    let ex = Container::open(&dest, &fast_open()).unwrap();
    assert_eq!(ex.db_uuid(), report.db_uuid);
    assert_eq!(ex.commit_seq(), report.commit_seq);
    {
        let esnap = ex.snapshot();
        assert_eq!(dump(&esnap, b"Rmain"), want);
        assert_eq!(lookup_root(&esnap, b"Rfeature"), None);
    }
    assert_verifies(&ex);

    // The source is untouched by the export: it holds the newer feature data.
    let now = c.snapshot();
    assert_eq!(dump(&now, b"Rmain"), expected_rows(400, 0));
    assert_eq!(dump(&now, b"Rfeature")[0].1, val(0, 105));
    // The old snapshot still sees the exported state.
    assert_eq!(dump(&snap, b"Rfeature"), want);
}

#[test]
fn bad_files_fail_closed() {
    let dir = tempfile::tempdir().unwrap();

    // (a) random bytes
    let p = dir.path().join("random.pga");
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let junk: Vec<u8> = (0..20000)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect();
    std::fs::write(&p, &junk).unwrap();
    let e = Container::open(&p, &fast_open()).err().expect("random file must not open");
    assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
    assert!(e.message.contains("is not a PGArbor database"), "message: {}", e.message);

    // An empty file is not a database either.
    let p0 = dir.path().join("empty.pga");
    std::fs::write(&p0, b"").unwrap();
    let e = Container::open(&p0, &fast_open()).err().expect("empty file must not open");
    assert_eq!(e.sqlstate.code(), "XX001");

    // (b) format_version 2 with a valid checksum
    let good = dir.path().join("v2.pga");
    drop(Container::create(&good, &fast_create()).unwrap());
    let mut bytes = std::fs::read(&good).unwrap();
    bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
    let crc = crc32c::crc32c(&bytes[0..88]);
    bytes[88..92].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&good, &bytes).unwrap();
    let e = Container::open(&good, &fast_open()).err().expect("future version must not open");
    assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
    assert!(e.message.contains("unsupported format version 2"), "message: {}", e.message);

    // (c) both slots corrupt (checksum mismatch in each)
    let both = dir.path().join("both.pga");
    {
        let c = Container::create(&both, &fast_create()).unwrap();
        let mut w = c.begin_write().unwrap();
        put_rows(&mut w, b"Rmain", 5, 0).unwrap();
        w.commit().unwrap();
    }
    let mut bytes = std::fs::read(&both).unwrap();
    bytes[50] ^= 0xFF; // slot 0, inside commit_seq
    bytes[4096 + 50] ^= 0xFF; // slot 1
    std::fs::write(&both, &bytes).unwrap();
    let e = Container::open(&both, &fast_open()).err().expect("corrupt slots must not open");
    assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);

    // A truncated committed region is refused as well.
    let trunc = dir.path().join("trunc.pga");
    {
        let c = Container::create(&trunc, &fast_create()).unwrap();
        let mut w = c.begin_write().unwrap();
        put_rows(&mut w, b"Rmain", 500, 0).unwrap();
        w.commit().unwrap();
    }
    let len = file_len(&trunc);
    let f = std::fs::OpenOptions::new().write(true).open(&trunc).unwrap();
    f.set_len(len - 8192).unwrap();
    drop(f);
    let e = Container::open(&trunc, &fast_open()).err().expect("truncated file must not open");
    assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
    assert!(e.message.contains("is truncated"), "message: {}", e.message);
}

#[test]
fn tail_garbage_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let committed;
    {
        let c = Container::create(&path, &fast_create()).unwrap();
        let mut w = c.begin_write().unwrap();
        put_rows(&mut w, b"Rmain", 100, 0).unwrap();
        w.commit().unwrap();
        committed = 8192 + c.superblock().page_count * c.page_size() as u64;
    }
    assert_eq!(file_len(&path), committed);

    {
        use std::io::Write;
        let garbage = vec![0xA5u8; 3 * 8192];
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&garbage).unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(file_len(&path), committed + 3 * 8192);

    let c = Container::open(&path, &fast_open()).unwrap();
    let rep = c.recovery_report();
    assert_eq!(rep.truncated_tail_bytes, 3 * 8192);
    assert_eq!(rep.committed_len, committed);
    assert_eq!(file_len(&path), committed);
    assert_eq!(c.commit_seq(), CommitSeq(2));
    {
        let snap = c.snapshot();
        assert_eq!(dump(&snap, b"Rmain"), expected_rows(100, 0));
    }

    // A subsequent commit appends right after the committed region.
    let mut w = c.begin_write().unwrap();
    let root = get_root_w(&w, b"Rmain").unwrap().unwrap();
    let root = w.put(root, b"extra", b"1").unwrap().root;
    set_root(&mut w, b"Rmain", root).unwrap();
    w.commit().unwrap();
    assert_verifies(&c);
    drop(c);

    // Reopening a clean file reports no truncation.
    let c = Container::open(&path, &fast_open()).unwrap();
    assert_eq!(c.recovery_report().truncated_tail_bytes, 0);
}

#[test]
fn doctor_reports() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
    let c = Container::create_with(vfs, Path::new("doctor.pga"), &fast_create()).unwrap();

    // One small data tree (1 leaf) + 1-leaf directory => 2 pages.
    let mut w = c.begin_write().unwrap();
    put_rows(&mut w, b"Rmain", 10, 0).unwrap();
    w.commit().unwrap();

    assert_eq!(c.superblock().page_count, 2);
    let rep = c.verify(&resolver(), VerifyLevel::Full).unwrap();
    assert!(rep.is_ok(), "{}", rep.to_json());
    assert_eq!(rep.commit_seq, Some(CommitSeq(2)));
    assert_eq!(rep.page_count, Some(2));
    assert_eq!(rep.reachable_pages, 2);
    assert_eq!(rep.unreachable_pages, 0);
    assert_eq!(rep.kinds.leaf, 2);
    assert_eq!(rep.kinds.internal, 0);
    assert_eq!(rep.kinds.overflow, 0);
    assert!(rep.issues.iter().all(|i| i.severity != IssueSeverity::Error));
    let main = rep.roots.iter().find(|r| r.label == "main").expect("root 'main' reported");
    assert_eq!(main.pages, 1);
    assert_eq!(main.exclusive_pages, 1);

    let sp = c.space_report(&resolver()).unwrap();
    assert_eq!(sp.page_size, 8192);
    assert_eq!(sp.page_count, 2);
    assert_eq!(sp.reachable_pages, 2);
    assert_eq!(sp.garbage_pages, 0);
    assert_eq!(sp.tail_bytes, 0);
    assert_eq!(sp.committed_bytes, 8192 + 2 * 8192);
    assert_eq!(sp.file_bytes, sp.committed_bytes);
    assert_eq!(sp.superblock_bytes, 8192);

    // Overwrite one row: the old data leaf and old directory leaf become garbage.
    let mut w = c.begin_write().unwrap();
    let root = get_root_w(&w, b"Rmain").unwrap().unwrap();
    let root = w.put(root, &key(3), b"changed").unwrap().root;
    set_root(&mut w, b"Rmain", root).unwrap();
    w.commit().unwrap();

    let rep = c.verify(&resolver(), VerifyLevel::Full).unwrap();
    assert!(rep.is_ok(), "{}", rep.to_json());
    assert_eq!(rep.page_count, Some(4));
    assert_eq!(rep.reachable_pages, 2);
    assert_eq!(rep.unreachable_pages, 2);

    let sp = c.space_report(&resolver()).unwrap();
    assert_eq!(sp.page_count, 4);
    assert_eq!(sp.reachable_pages, 2);
    assert_eq!(sp.garbage_pages, 2);
    assert_eq!(sp.garbage_bytes, 2 * 8192);
    assert_eq!(sp.reachable_bytes, 2 * 8192);

    // JSON output is well formed and carries the counts.
    let json: serde_json::Value = serde_json::from_str(&rep.to_json()).unwrap();
    assert_eq!(json["unreachable_pages"].as_u64(), Some(2));
    assert_eq!(json["reachable_pages"].as_u64(), Some(2));
    let sjson: serde_json::Value = serde_json::to_value(&sp).unwrap();
    assert_eq!(sjson["garbage_pages"].as_u64(), Some(2));

    // Compaction reclaims the garbage and the report agrees.
    c.compact(&resolver(), &CompactOptions::default()).unwrap();
    let sp = c.space_report(&resolver()).unwrap();
    assert_eq!(sp.garbage_pages, 0);
    assert_eq!(sp.page_count, 2);
}
