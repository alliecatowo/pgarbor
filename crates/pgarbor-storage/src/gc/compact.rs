//! Compaction to a new file + atomic rename (STORE-15, STORE-16). Lease STO-17 (opus).
//!
//! # Protocol (CONTRACT)
//! 1. `inner.check_writable()`; lock `inner.writer` for the whole compaction (writers block;
//!    snapshots/readers continue on the old Pager); re-check writable.
//! 2. `st = inner.current()`; `src = Snapshot::new(st.pager.clone(), st.sb, None)`.
//! 3. `ExportBuilder::create_internal(vfs, path, sibling_path(path, COMPACT_TEMP_SUFFIX), opts)`
//!    with page_size, db_uuid, created_unix_ms of `st.sb`, `commit_seq = st.sb.commit_seq + 1`,
//!    `sync = inner.sync`. (An existing temp name is refused.) The temp file is locked Exclusive.
//! 4. `new_dir = builder.copy_directory(&src, st.sb.directory_root, resolver)`; then each
//!    `extra_roots` entry via `builder.copy_tree` (memo shares pages with the directory's trees).
//! 5. `(file, new_sb) = builder.finish_in_place(new_dir)` — slots written, `sync_all` (Full).
//!    Any failure in 3–5: remove the temp file (best effort), return the error; NOT poisoned.
//! 6. `vfs.rename(temp, path)`. Failure: remove temp, return `error::rename_failed`; NOT poisoned
//!    (the original file is untouched and still ours).
//! 7. Publish `ContainerState{ pager: Arc::new(Pager::new(file, path, ps, inner.cache_pages)),
//!    sb: new_sb, slot: 0 }` (from here the path names the new file).
//! 8. `vfs.sync_dir(parent_dir(path))` (Full). Failure → `inner.poison(..)` and return the error
//!    (after a crash either file may be at the path; both are complete and consistent, but this
//!    process must not continue writing on an ambiguous name).
//! 9. Report: `roots = collect_roots(new snapshot, new_dir, resolver)`, lens, page counts,
//!    `remapped_extra_roots`.
//!
//! Crash safety: before step 6 the original is untouched (temp garbage is removed by the next
//! writable open). After step 6 the path holds a fully synced container whose commit_seq is
//! higher. Old snapshots keep reading the old (unlinked) file through their Arc<Pager>; the old
//! file's lock is released when the last of them drops. The new file is locked because the lock
//! was taken on the temp handle before the rename (flock follows the open file).
use crate::container::handle::{ContainerInner, ContainerState};
use crate::container::options::{CompactOptions, CompactReport};
use crate::container::recovery::COMPACT_TEMP_SUFFIX;
use crate::container::snapshot::Snapshot;
use crate::error;
use crate::export::{ExportBuilder, ExportOptions};
use crate::format::superblock::Superblock;
use crate::gc::trace::collect_roots;
use crate::io::{parent_dir, sibling_path, FileIo};
use crate::pager::Pager;
use crate::roots::RootResolver;
use pgarbor_common::{CommitSeq, PageId, Result};
use std::path::Path;
use std::sync::Arc;

// CONTRACT
/// See module docs. Called by `Container::compact`.
pub(crate) fn compact(
    inner: &ContainerInner,
    resolver: &dyn RootResolver,
    opts: &CompactOptions,
) -> Result<CompactReport> {
    inner.check_writable()?;
    // Hold the writer mutex for the whole compaction: writers block, readers continue.
    let _writer = inner.writer.lock().unwrap_or_else(|e| e.into_inner());
    inner.check_writable()?;

    let st = inner.current();
    if st.sb.commit_seq.0 >= u64::MAX - 1 {
        return Err(error::corrupt("commit sequence exhausted"));
    }
    let src = Snapshot::new(st.pager.clone(), st.sb, None);
    let old_file_len = st.pager.file_len()?;
    let tmp = sibling_path(&inner.path, COMPACT_TEMP_SUFFIX);

    let ex_opts = ExportOptions {
        page_size: st.sb.page_size,
        db_uuid: Some(st.sb.db_uuid),
        created_unix_ms: Some(st.sb.created_unix_ms),
        commit_seq: CommitSeq(st.sb.commit_seq.0 + 1),
        sync: inner.sync,
        overwrite: true,
        buffer_bytes: ExportOptions::default().buffer_bytes,
    };

    // Steps 3-5: build the compacted file. Any failure removes the temp file; not poisoned.
    let (file, new_sb, new_dir, remapped) =
        match build_compacted(inner, &src, &st, &tmp, resolver, opts, &ex_opts) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

    // Step 6: atomic rename; the temp handle (and its lock) becomes the container's file.
    if let Err(e) = inner.vfs.rename(&tmp, &inner.path) {
        let _ = inner.vfs.remove_file(&tmp);
        drop(file);
        return Err(error::rename_failed(&tmp, &inner.path, &e));
    }

    // Step 7: publish the new generation.
    let new_pager = Arc::new(Pager::new(file, inner.path.clone(), new_sb.page_size, inner.cache_pages));
    inner.publish(ContainerState { pager: new_pager.clone(), sb: new_sb, slot: 0 });

    // Step 8: make the new name durable.
    if inner.syncing() {
        let dir = parent_dir(&inner.path);
        if let Err(e) = inner.vfs.sync_dir(&dir) {
            let err = error::fsync_failed(&dir, &e);
            inner.poison(format!("directory sync after compaction failed: {e}"));
            return Err(err);
        }
    }

    // Step 9: report.
    let new_file_len = new_pager.file_len()?;
    let new_snap = Snapshot::new(new_pager, new_sb, None);
    let roots = collect_roots(&new_snap, new_dir, resolver)?;
    Ok(CompactReport {
        old_commit_seq: st.sb.commit_seq,
        new_commit_seq: new_sb.commit_seq,
        old_file_len,
        new_file_len,
        old_page_count: st.sb.page_count,
        new_page_count: new_sb.page_count,
        remapped_extra_roots: remapped,
        roots,
    })
}

type Built = (Box<dyn FileIo>, Superblock, PageId, Vec<(PageId, PageId)>);

fn build_compacted(
    inner: &ContainerInner,
    src: &Snapshot,
    st: &ContainerState,
    tmp: &Path,
    resolver: &dyn RootResolver,
    opts: &CompactOptions,
    ex_opts: &ExportOptions,
) -> Result<Built> {
    let mut builder = ExportBuilder::create_internal(inner.vfs.clone(), &inner.path, tmp.to_path_buf(), ex_opts)?;
    let new_dir = builder.copy_directory(src, st.sb.directory_root, resolver)?;
    let mut remapped = Vec::with_capacity(opts.extra_roots.len());
    for &old in &opts.extra_roots {
        let new = if old.is_none() { old } else { builder.copy_tree(src, old)? };
        remapped.push((old, new));
    }
    let (file, new_sb) = builder.finish_in_place(new_dir)?;
    Ok((file, new_sb, new_dir, remapped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::handle::Container;
    use crate::container::options::{CreateOptions, OpenOptions};
    use crate::io::{MemVfs, Vfs};
    use crate::roots::PrefixU64Roots;
    use pgarbor_common::KvRead;
    use std::path::PathBuf;

    fn mk() -> (Arc<MemVfs>, PathBuf, Container) {
        let vfs = Arc::new(MemVfs::new());
        let path = PathBuf::from("/db/test.pga");
        let c = Container::create_with(vfs.clone(), &path, &CreateOptions::default()).unwrap();
        (vfs, path, c)
    }

    /// Set key `k` of the tree named `name` to `val`, in its own commit.
    fn commit_put(c: &Container, name: &str, k: u64, val: &[u8]) {
        let mut t = c.begin_write().unwrap();
        let dir = t.directory_root();
        let dkey = format!("R{name}").into_bytes();
        let root = match t.get(dir, &dkey).unwrap() {
            Some(v) => PageId(u64::from_le_bytes(v.as_slice().try_into().unwrap())),
            None => PageId::NONE,
        };
        let nr = t.put(root, &k.to_be_bytes(), val).unwrap().root;
        let nd = t.put(dir, &dkey, &nr.0.to_le_bytes()).unwrap().root;
        t.set_directory_root(nd);
        t.commit().unwrap();
    }

    fn read_val(snap: &Snapshot, name: &str, k: u64) -> Option<Vec<u8>> {
        let dkey = format!("R{name}").into_bytes();
        let v = snap.directory().get(&dkey).unwrap()?;
        let root = PageId(u64::from_le_bytes(v.as_slice().try_into().unwrap()));
        snap.tree(root).unwrap().get(&k.to_be_bytes()).unwrap()
    }

    #[test]
    fn compaction_shrinks_and_preserves() {
        let (vfs, path, c) = mk();
        let resolver = PrefixU64Roots::new(b"R");
        for i in 0..100u64 {
            commit_put(&c, "a", i % 5, format!("value-{i}").as_bytes());
            commit_put(&c, "b", i % 3, format!("bee-{i}").as_bytes());
        }
        let before = c.snapshot();
        let seq = c.commit_seq();
        let rep = c.compact(&resolver, &CompactOptions::default()).unwrap();
        assert_eq!(rep.old_commit_seq, seq);
        assert_eq!(rep.new_commit_seq, CommitSeq(seq.0 + 1));
        assert_eq!(c.commit_seq(), rep.new_commit_seq);
        assert!(rep.new_file_len < rep.old_file_len);
        assert!(rep.new_page_count < rep.old_page_count);
        assert_eq!(rep.roots.len(), 3);
        // The old snapshot still reads from the old file.
        assert_eq!(read_val(&before, "a", 4).unwrap(), b"value-99");
        // Identical logical contents afterwards.
        let after = c.snapshot();
        for k in 0..5 {
            assert_eq!(read_val(&before, "a", k), read_val(&after, "a", k));
        }
        for k in 0..3 {
            assert_eq!(read_val(&before, "b", k), read_val(&after, "b", k));
        }
        // Temp file gone.
        assert!(!vfs.exists(&sibling_path(&path, COMPACT_TEMP_SUFFIX)).unwrap());
        // Writes continue after compaction.
        commit_put(&c, "a", 9, b"post");
        assert_eq!(c.commit_seq(), CommitSeq(seq.0 + 2));
        assert_eq!(read_val(&c.snapshot(), "a", 9).unwrap(), b"post");
        // Reopen works.
        drop(before);
        drop(after);
        drop(c);
        let c2 = Container::open_with(vfs.clone(), &path, &OpenOptions::default()).unwrap();
        assert_eq!(read_val(&c2.snapshot(), "a", 9).unwrap(), b"post");
        assert_eq!(read_val(&c2.snapshot(), "b", 2).unwrap(), b"bee-98");
    }

    #[test]
    fn none_extra_root_stays_none() {
        let (_vfs, _path, c) = mk();
        let resolver = PrefixU64Roots::new(b"R");
        commit_put(&c, "a", 1, b"x");
        let rep = c
            .compact(&resolver, &CompactOptions { extra_roots: vec![PageId::NONE] })
            .unwrap();
        assert_eq!(rep.remapped_extra_roots, vec![(PageId::NONE, PageId::NONE)]);
    }

    #[test]
    fn compact_refused_when_poisoned() {
        let (_vfs, _path, c) = mk();
        c.inner.poison("test".to_string());
        let r = c.compact(&PrefixU64Roots::new(b"R"), &CompactOptions::default());
        assert!(r.is_err());
    }
}
