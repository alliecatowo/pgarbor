//! Compaction to a new file + atomic rename (STORE-15, STORE-16). Lease STO-17 (opus).
//!
//! # Protocol (CONTRACT)
//! 1. `inner.check_writable()`; lock `inner.writer` for the whole compaction (writers block;
//!    snapshots/readers continue on the old Pager); re-check writable.
//! 2. `st = inner.current()`; `src = Snapshot::new(st.pager.clone(), st.sb, None)`.
//! 3. `ExportBuilder::create_internal(vfs, path, sibling_path(path, COMPACT_TEMP_SUFFIX), opts)`
//!    with page_size, db_uuid, created_unix_ms of `st.sb`, `commit_seq = st.sb.commit_seq + 1`,
//!    `sync = inner.sync`. (A stale temp file is replaced.) The temp file is locked Exclusive.
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
#[allow(unused_imports)]
use crate::container::handle::{ContainerInner, ContainerState};
#[allow(unused_imports)]
use crate::container::recovery::COMPACT_TEMP_SUFFIX;
use crate::container::options::{CompactOptions, CompactReport};
#[allow(unused_imports)]
use crate::container::snapshot::Snapshot;
#[allow(unused_imports)]
use crate::export::{ExportBuilder, ExportOptions};
#[allow(unused_imports)]
use crate::gc::trace::collect_roots;
use crate::roots::RootResolver;
use pgarbor_common::{PgError, Result};

// CONTRACT
/// See module docs. Called by `Container::compact`.
#[allow(unused_variables)]
pub(crate) fn compact(
    inner: &ContainerInner,
    resolver: &dyn RootResolver,
    opts: &CompactOptions,
) -> Result<CompactReport> {
    Err(PgError::unimplemented("STO-17", "container compaction"))
}
