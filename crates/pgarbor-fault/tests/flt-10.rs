//! FLT-10: interrupted compaction / export and lock tests (STORE-12, STORE-16, STORE-17).
//!
//! Lease FLT-10 writes this file. Required tests:
//! 1. `crash_during_compaction_every_op`: workload, then `compact()`; crash at every mutating op
//!    inside the compaction window (count ops before/after to find the window) → recovery yields
//!    the pre-compaction acked state (or the compacted equivalent: identical logical contents and
//!    commit_seq + 1); `<path>.compact-tmp` is removed by the next writable open.
//! 2. `rename_not_durable_without_dir_sync`: DropUnsynced crash right after the rename but before
//!    `sync_dir` → the old file is at the path and opens with the acked state.
//! 3. `export_crash_leaves_dest_absent_or_complete`: crash at every op of an ExportBuilder run →
//!    the destination either does not exist or opens with exactly the exported snapshot.
//! 4. `old_snapshot_survives_compaction`: a snapshot taken before compaction keeps returning its
//!    data afterwards (FaultVfs keeps the unlinked file object alive).
//! 5. `second_open_locked`: opening the same FaultVfs path twice → 55006.
