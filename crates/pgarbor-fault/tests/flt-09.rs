//! FLT-09: write-failure behaviour (STORE-18): no success acknowledgment after ENOSPC, short
//! write or fsync failure; poisoning semantics.
//!
//! Lease FLT-09 writes this file. Required tests (FaultVfs + Container directly):
//! 1. `enospc_during_page_write_is_not_acked`: `FaultKind::Enospc` at the first page write of a
//!    commit → `commit()` Err with SQLSTATE 53100 and hint "Check free disk space."; container NOT
//!    poisoned; a retry commit after clearing faults succeeds; reopen shows only acked data.
//! 2. `short_write_is_not_acked`: ShortWrite on a page write → Err (53100 via WriteZero→ENOSPC or
//!    the following ENOSPC) and no ack.
//! 3. `fsync_failure_poisons`: FsyncFail on the data sync of a commit → Err 58030; afterwards
//!    `begin_write()` fails with "is unavailable for writing after an earlier I/O failure";
//!    snapshots still read the last acked state; crash image recovers to acked or in-flight.
//! 4. `superblock_write_failure_poisons`: IoError at the superblock write → poisoned; recovery
//!    from the crash image is acked-or-inflight.
//! 5. `disk_capacity_limit`: `disk_capacity: Some(64 KiB)` → inserting until full yields 53100,
//!    never a panic, never corruption; after reopen data == acked.
//! 6. `random_faults_property`: 64 seeds of `explore` with random enospc/short/fsync faults pass.
