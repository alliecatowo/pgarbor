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
