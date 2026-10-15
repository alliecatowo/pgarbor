# Amendment request pgarbor-branches-02: storage re-identify + export publish hook

Status: ACCEPTED IN PART (2026-10-15, lead engineer delegation to Codex storage safety owner).

## Problem

1. **Re-identifying a reflinked clone.** A clone made with `branch detach --strategy auto|reflink-required`
   is a byte copy of the source, so it carries the source's `DatabaseUuid`. An exported file must get
   a fresh UUID (ARCHITECTURE §2, RFC 001 §4.3). Storage offers no API to change a file's identity.
   `choose_slot` refuses two valid slots that disagree on UUID, so a normal commit cannot change it
   either. The skeleton has `pgarbor_branches::detach::reidentify_file`, which rewrites superblock
   slots through storage's public format API (`read_slots`, `choose_slot`, `Superblock::encode`).
   It works, but a format-level operation is living outside storage.
2. **Verifying before publishing.** `ExportBuilder::finish` flushes, renames to `dest` and syncs the
   directory in one call. `finish_in_place` is crate-private. RFC 001 §9 requires the detached file
   to be reopened and verified *before* success is reported. Today branches export to an
   intermediate `<dest>.detach-tmp`, reopen and verify it, then rename it a second time.

## Proposed change

- Add `pgarbor_storage::container::recovery::reidentify(vfs: &dyn Vfs, path: &Path, new_uuid:
  DatabaseUuid, sync: SyncMode) -> Result<Superblock>`. It applies only to closed, unpublished
  files. It writes the new-UUID superblock into the adopted slot, zeroes the other slot, and syncs.
- Either make `ExportBuilder::finish_in_place` public and add `ExportBuilder::publish(tmp_handle,
  dest)`, or add `ExportOptions::verify: Option<Box<dyn Fn(&Path) -> Result<()>>>`, which runs
  between the temp sync and the rename.

## Affected crates

pgarbor-storage (new API). pgarbor-branches: BR-11 switches to the storage API, and BR-10 can drop
the extra rename. No on-disk format change.

## Steward decision (2026-10-15)

Accept storage-owned reidentify: implemented as the proposed recovery::reidentify function,
returning Superblock. The branch reidentify_file function remains a compatibility wrapper.
It additionally revalidates pathname identity after locking before changing slots.

Reject the proposed public finish_in_place/callback publication alternatives for now. Keep the
intermediate file, which permits verification through the existing Container API. branches-03
adds a stable destination guard across construction, verification and publication, and keeps
the verified file locked until publication completes. This closes the actual race without a
callback or leaked temporary-handle lifecycle API. No on-disk format change.
