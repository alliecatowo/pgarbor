# Amendment request pgarbor-storage-02: `Vfs` trait and lock methods on `FileIo`

Status: ACCEPTED (2026-10-15, lead engineer delegation to Codex storage safety owner). Previously proposed (architect, pgarbor-storage). Implemented in the v1 skeleton as `pgarbor_storage::io`.

## Problem

ARCHITECTURE §5 lists `FileIo` = `read_at, write_at, sync_data, sync_all, len, set_len` as the ONLY path to
bytes. Three required behaviours are not expressible with that surface:

1. **Exclusive lock (STORE-12)**: fs2 locks belong to the open file; `MemFile`/`FaultFile` must emulate them,
   so locking has to go through the same abstraction, or fault tests cannot cover "second opener is refused".
2. **Compaction / export (STORE-16/17)**: create a temp file, rename it atomically over the database, fsync the
   directory, remove leftovers. If these call `std::fs` directly, pgarbor-fault cannot inject crashes into
   interrupted compaction (RFC 001 §5.2 explicitly requires it) and MemFile-based (wasm) builds cannot compact.
3. **Opening by path** in a way that works for real files, memory and fault-injection alike.

## Proposed change

- `FileIo` gains `try_lock(&self, LockMode) -> io::Result<()>` (`LockMode::{Shared, Exclusive}`, contention =
  `ErrorKind::WouldBlock`) and `unlock(&self)`, plus provided `read_exact_at` / `write_all_at`.
  All methods take `&self`; `FileIo: Send + Sync + Debug`. Errors are `std::io::Error`; ENOSPC must be
  `from_raw_os_error(28)`.
- New `Vfs` trait: `open(path, OpenMode)`, `exists`, `rename`, `remove_file`, `sync_dir`, with POSIX durability
  semantics (directory ops durable only after `sync_dir`). Implementations: `OsVfs` (std::fs + fs2),
  `MemVfs`, and `pgarbor_fault::FaultVfs`.
- `Container::create/open(path)` use `OsVfs`; `Container::create_with/open_with(Arc<dyn Vfs>, ..)` accept any.

`FileIo` stays the only path to file *bytes*; `Vfs` is the only path to *names*.

## Affected crates

- **pgarbor-fault**: implements both traits (done in its skeleton).
- **pgarbor-branches**: `.pgabranch` handle files and reflink/export should use `Vfs` where they touch the
  database file's directory, so detach/export can be fault-tested; reflink itself (platform ioctl) stays outside.
- **pgarbor / pgarbor-cli**: pass `Arc<dyn Vfs>` through `OpenOptions` when embedding in wasm or tests.
- **ARCHITECTURE.md §5**: replace the `FileIo` method list with a reference to this amendment.

## Steward decision (2026-10-15)

Accepted: the implemented interface/layout is required for the existing dependency direction and
safe storage/branch behavior. The owning crate contract records the adopted details. Storage
publication/identity semantics are refined by storage-03 and branches-03 safety amendments.
