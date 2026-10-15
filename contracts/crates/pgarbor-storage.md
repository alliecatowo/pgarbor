# pgarbor-storage — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-storage.json` (STO-01..STO-21).
Amendments: `contracts/amendments/pgarbor-storage-01-root-resolver.md`,
`contracts/amendments/pgarbor-storage-02-vfs-and-lock-io.md`,
`contracts/amendments/pgarbor-storage-03-safe-publication-and-recovery.md` (accepted safety refinements).

## Purpose

The single `.pga` file: dual superblocks, checksummed pages, a copy-on-write B+tree of byte keys/values, the
single-writer commit protocol with crash recovery, an exclusive file lock, reachability tracing, compaction to a
new file with atomic rename, streaming export, and doctor/verify/inspect. Storage knows nothing about SQL, types,
catalogs or branch records (ARCHITECTURE §1, §5).

## Public API (what downstream crates use)

| Item | Use |
|---|---|
| `Container` (`Clone + Send + Sync`) | `create(path, &CreateOptions)`, `open(path, &OpenOptions)`, `create_with/open_with(Arc<dyn Vfs>, ..)`, `snapshot()`, `begin_write()`, `compact(&dyn RootResolver, &CompactOptions)`, `verify(..)`, `space_report(..)`, `commit_seq()`, `superblock()`, `db_uuid()`, `recovery_report()`, `is_poisoned()`, `pins()` |
| `Snapshot` (`Clone + Send + Sync`) | frozen committed state: `commit_seq()`, `directory_root()`, `directory() -> TreeSnapshot`, `tree(root) -> Result<TreeSnapshot>`; implements `PageSource` |
| `TreeSnapshot` | **implements `pgarbor_common::KvRead`** — pgarbor-core's per-transaction read snapshot of a branch; `root()`, `range()` |
| `WriteTxn<'c>` | the single writer (holds the writer mutex): `directory_root()`, `set_directory_root()`, `get/scan/put/delete/delete_range(root, ..)`, `apply(root, &WriteBatch) -> new root`, `build_tree(sorted iter)`, `tree(root) -> TreeWriter`, `commit() -> Result<CommitSeq>`; drop = abort |
| `TreeWriter` | `KvRead + KvWrite` over one tree inside a `WriteTxn` (`root()` after writes) |
| `WriteBatch` | ordered `BTreeMap<Vec<u8>, Option<Vec<u8>>>` (ARCHITECTURE §7 overlay shape) |
| `RootResolver`, `NamedRoot`, `NoNestedRoots`, `PrefixU64Roots` | how storage finds/rewrites roots embedded in directory values (amendment 01) |
| `container::recovery::reidentify` | assign a fresh UUID to a closed unpublished private file (branches-02) |
| `ExportBuilder`, `ExportOptions`, `ExportReport` | write a new standalone `.pga` from snapshot trees (`copy_tree`, `copy_directory`, `build_tree`, `put`, `finish(dir_root)`) |
| `FileIo`, `Vfs`, `OsFile/OsVfs`, `MemFile/MemVfs`, `OpenMode`, `LockMode` | byte and name I/O (amendment 02) |
| `DoctorReport`, `SpaceReport`, `Issue`, `doctor::{verify_file, inspect_page}` | `pgarbor doctor`, `pgarbor inspect --space` (JSON via serde) |
| `btree::*`, `pager::*`, `format::*`, `gc::*` | public for tools/tests; downstream engine crates should not need them |

### Typical flows

```rust
// pgarbor-branches: create container + main branch (one commit)
let c = Container::create(path, &CreateOptions::default())?;          // commit_seq 1, empty directory
let mut w = c.begin_write()?;
let dir = w.directory_root();
let mut t = w.tree(dir);
t.put(b"Bxxxxxxxxxxxxxxxx", &record_bytes)?;  t.put(b"Nmain", &id)?;
let dir = t.root();
w.set_directory_root(dir);
let seq = w.commit()?;                                               // CommitSeq(2), durable

// pgarbor-core: transaction snapshot of a branch
let snap = c.snapshot();
let rec = decode(snap.directory().get(b"B..")?);                     // branches decodes
let tree: TreeSnapshot = snap.tree(rec.data_root)?;                  // KvRead for overlays

// pgarbor-core commit: apply overlay to the CURRENT head under the writer
let mut w = c.begin_write()?;
let head = /* re-read BranchRecord via w.get(w.directory_root(), ..) */;
let new_root = w.apply(head.data_root, &WriteBatch::from_map(overlay))?;
/* write updated BranchRecord into directory via w.tree(..) */
w.set_directory_root(new_dir);
let seq = w.commit()?;                                               // ack COMMIT only after Ok
```

## Key invariants

1. **Never overwrite reachable pages.** A write txn only writes ids `> base.page_count`. Committed pages are
   immutable until compaction writes a different file. Hence: no page-level locking for readers, no cache
   invalidation, recovery = pick the newest valid superblock.
2. **Acknowledge only after durability.** `WriteTxn::commit` returns `Ok` only after: pages written →
   `sync_data` → superblock written to the *other* slot with `commit_seq + 1` → `sync_data` (SyncMode::Full).
3. **Poisoning (STORE-18).** A failed sync or failed superblock write poisons the container: further
   `begin_write`/`compact` fail with SQLSTATE 58030 until reopen (PG treats fsync failure as PANIC). A failed
   *page* write (e.g. ENOSPC, 53100) only fails that commit.
4. **Single writer, many readers.** Writer mutex held for the life of a `WriteTxn` (and of `compact`).
   `snapshot()` never blocks on the writer.
5. **One process per file.** Read-write open takes an exclusive advisory lock (fs2/flock); read-only takes a
   shared one. Contention → 55006.
6. **Fail closed.** Pathname identity is revalidated after locking; a retired inode is retried.
   Temp files use exclusive creation; no-replace exports publish atomically (amendment 03).
   Unknown format version / required feature bit / no valid superblock / truncated committed
   region → XX001 and refuse to open.
7. **Empty tree = `PageId::NONE`.** Trees never contain empty leaves; internal nodes always have >= 1
   separator.
8. **CommitSeq is monotonic for the container lifetime**, including across compaction (compaction = commit).
9. **Copy-on-write sharing.** A branch = a root pointer; creating one copies 8 bytes. Shared subtrees are
   copied once by compaction/export (memoized structural copy).

## Data formats (byte-exact, CONTRACT; all integers little-endian)

### File layout
```
[0, 4096)       superblock slot 0
[4096, 8192)    superblock slot 1
8192 + (n-1)*ps page n (n >= 1), ps = page_size (power of two, 4096..=65536, default 8192)
valid length    8192 + page_count * ps; anything beyond is an uncommitted tail (truncated on writable open)
page_count      < floor((i64::MAX - 8192) / ps), reserving address headroom
commit_seq      1..u64::MAX (exclusive); writers refuse advancement from u64::MAX-1
```

### Superblock slot
| off | len | field |
|---|---|---|
| 0 | 8 | magic `PGARBOR\0` |
| 8 | 4 | format_version = 1 |
| 12 | 4 | reserved = 0 |
| 16 | 8 | feature_flags (bits 32..63 required, 0..31 compatible; none defined in v1) |
| 24 | 4 | page_size |
| 28 | 4 | reserved = 0 |
| 32 | 16 | db_uuid (raw bytes) |
| 48 | 8 | commit_seq (>= 1) |
| 56 | 8 | directory_root (PageId, 0 = empty) |
| 64 | 8 | page_count (highest allocated PageId) |
| 72 | 8 | freelist_root (0 in v1) |
| 80 | 8 | created_unix_ms |
| 88 | 4 | crc32c(bytes[0..88]) |
| 92 | 4004 | zero padding (unchecked) |

Slot choice: unsupported version/required feature in *either* slot → refuse; else highest valid commit_seq;
ties only if identical (slot 0); valid slots disagreeing on uuid/page_size/created → refuse.
New file: slot 0 = commit 1 (empty directory, 0 pages), slot 1 = zeros, length 8192.

### Page (ps bytes)
| off | len | field |
|---|---|---|
| 0 | 4 | magic `PGAp` |
| 4 | 1 | kind: 1 leaf, 2 internal, 3 overflow, 4 freelist (reserved) |
| 5 | 1 | flags = 0 |
| 6 | 2 | reserved = 0 |
| 8 | 8 | page_id (must equal the slot read; catches misdirected writes) |
| 16 | 8 | commit_seq of the writing commit (must be <= reader's snapshot commit) |
| 24 | 4 | crc32c(header with bytes 24..28 zeroed ++ payload[0..payload_len]) |
| 28 | 4 | payload_len (<= ps - 32) |
| 32 | … | payload, then zero padding (unchecked) |

### B+tree payloads
- Leaf: `nkeys u16 | level u16 (=0) | slot[nkeys] u16 | cells`; cell = `key_len u16 | vtag u8 | vlen u32 | key |
  value` where vtag 0 = inline value (vlen bytes), vtag 1 = overflow (8-byte first PageId; vlen = total length).
- Internal: `nkeys u16 (>=1) | level u16 (>=1) | child0 u64 | slot[nkeys] u16 | cells`; cell = `key_len u16 |
  child u64 | key`. Child for key k = number of separators <= k.
- Cells are packed right after the slot array in key order. Keys strictly ascending, bytewise.
- Limits: `max_key_len = ps/4 - 64` (1984 @ 8 KiB) else 54000; values longer than `ps/4` (2048 @ 8 KiB) go to
  overflow; values up to `u32::MAX`.
- Overflow page payload: `next u64 | chunk`; chunks full (`ps - 40` bytes) except the last; ids ascending.

## Error conventions

All errors are `pgarbor_common::PgError`, built through `pgarbor_storage::error` helpers:

| Condition | SQLSTATE | Message |
|---|---|---|
| bad checksum | XX001 | `page verification failed, calculated checksum <c> but expected <s>` (PG bufpage.c) |
| structural damage | XX001 | `invalid page in block <n> of database file` + detail reason |
| not a database | XX001 | `file "<path>" is not a PGArbor database` |
| unsupported version | XX001 | `database file "<path>" has unsupported format version <v>` |
| truncated committed region | XX001 | `database file "<path>" is truncated: ...` |
| file locked | 55006 | `could not lock database file "<path>": it is already open in another process` |
| ENOSPC / short write | 53100 | `could not write to file "<path>": ...` hint `Check free disk space.` |
| other I/O | 58030 | `could not read file/write to file/fsync file "<path>": ...` |
| missing file | 58P01 | `could not open file "<path>": ...` |
| create over existing | 58P02 | `could not open file "<path>": ...` |
| poisoned | 58030 | `database file "<path>" is unavailable for writing after an earlier I/O failure` |
| read-only container | 25006 | `database file "<path>" is opened read-only` |
| key too large | 54000 | `key size <n> exceeds maximum <m> for storage tree` |
| bad page size | 22023 | `invalid page size <n>` |
| unimplemented | 0A000 | `PgError::unimplemented("STO-xx", ..)` |

Corruption never panics; doctor reports corruption as `Issue`s instead of failing.

## Concurrency and threading

No async, no background threads. `Container`, `Snapshot`, `TreeSnapshot`, `Pager`, `PageCache` are
`Send + Sync`. `WriteTxn` is not `Send` (holds a `MutexGuard`). Lock order: writer mutex → state RwLock (write) →
cache shard mutexes. Readers take only the state RwLock (read, briefly) and cache shards.

## Non-goals / known gaps (v1)

- **No in-place space reuse.** Append-only; `freelist_root` always 0; space returns only via `compact()`.
  `PinTable` is the hook a future free-list must consult (reuse only pages freed before the oldest pin).
- **Compaction blocks writers** for its duration (readers continue). Incremental/online-for-writers GC later.
- **No WAL / no group commit**: each commit = 2 fsyncs. Batching commits is a pgarbor-core concern.
- **No reverse iteration** in `KvRead` (ORDER BY DESC via index scans backwards is a future amendment).
- **No suffix truncation / prefix compression** of keys; separators are full keys.
- **Windows**: no directory fsync (documented); rename-over-open-file semantics differ (compaction with live
  old snapshots is Unix-first). Locks via fs2 `LockFileEx`.
- **Old snapshots after compaction** read the unlinked old file; its disk space is freed when the last such
  snapshot drops.
- Reflink-based branch export lives in pgarbor-branches, not here.
