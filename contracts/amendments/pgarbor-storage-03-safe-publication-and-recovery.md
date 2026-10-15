# Storage 03: exclusive temporary ownership, publication, identity, address limits

Status: ACCEPTED (2026-10-15, lead engineer delegation to Codex storage safety owner).
Decision: implement the following together to close review-01 P1 export/header and P2 recovery findings.
No on-disk field layout changes.

## Name and handle operations

- `Vfs::rename_noreplace(from, to)` atomically publishes only to an absent name.
  `AlreadyExists` leaves both names unchanged. Unsupported implementations return an error;
  no check-then-rename fallback is permitted. OsVfs uses Linux/Android renameat2 with
  RENAME_NOREPLACE; other platforms currently fail closed. MemVfs and FaultVfs implement
  the condition and move under the namespace mutex; FaultVfs records the existing Rename op.
- `FileIo::identity() -> io::Result<(u64, u64)>` identifies an open file object while the
  handle lives. OsFile uses device/inode on Unix. MemFile/FaultFile use separate implementation
  tags and the live Arc object's address. These identities are process-local comparison values,
  never serialized database identities. Unsupported implementations return an error.
- `Vfs::is_current_file(path, file)` compares current pathname identity against the handle.
  Its provided implementation opens the name read-only and compares identities; missing names
  return false. Recovery checks after attempting the advisory lock, retries retired handles,
  and returns a lock error after 16 changing generations. It never reads/truncates a retired
  generation. Under PGArbor's locking protocol, only the currently locked owner can replace
  a published file through compaction, so a successful identity check while holding that
  generation's lock prevents another supported compaction from racing after the check.
- Export/compaction create temporary files with CreateNew and never unlink existing temporaries
  during construction. A conflicting or stale name must be recovered or removed by its owner;
  a failed contender never owns cleanup. Recovery only removes temporaries it has successfully
  opened, exclusively locked and identity-checked; live locked builders are preserved.
- ExportOptions.overwrite=false publishes with rename_noreplace and reports AlreadyExists as
  58P02, including destinations created after construction. overwrite=true retains atomic replace.
  Fixed sibling temp names intentionally serialize builders for the same destination.

## Address and sequence bounds

Superblock decode marks a slot Invalid when page_count is at least
`floor((i64::MAX - 8192) / page_size)`, or commit_seq is u64::MAX. Encode rejects the same
states with XX001. The strict page-count inequality reserves one representable allocation
position, and respects signed OS file offsets. Existing public arithmetic helper signatures
remain unchanged and operate on validated/allocated positions.

WriteTxn and ExportBuilder reject an allocation reaching the limit before incrementing IDs.
WriteTxn::begin and compact reject sequence advancement from u64::MAX-1 with XX001; no
acknowledged commit is written that could not be reopened. Export rejects u64::MAX up front.
The last accepted sequence remains readable. Exhaustion is an explicit error, never wrapping.

## Evidence

Regression targets: export_publication.rs, hostile_headers.rs, recovery_identity.rs.
The full storage/fault suite is required after these changes; results are reported to the lead
for the durable final review. No tests/expected files are changed.
