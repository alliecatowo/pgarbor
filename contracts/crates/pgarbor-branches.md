# pgarbor-branches — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-branches.json`
(BR-01..BR-11).
Amendments: `pgarbor-branches-01-branch-record-v1`, `pgarbor-branches-02-storage-reidentify-and-export-publish`.
Depends on: pgarbor-common, pgarbor-storage (frozen API, see `pgarbor-storage.md`).

## Purpose

This crate owns:

- the **branch directory**, the storage tree whose root sits in the superblock;
- branch lifecycle: create, list, get, rename, delete with deferred reclaim, restore, purge,
  default branch;
- named snapshots and reset with an optional recovery snapshot;
- compare (a divergence report);
- per-branch space accounting;
- `.pgabranch` handles;
- standalone export and detach, using a reflink clone when the filesystem supports it.

Branching itself is pointer copying. A branch is a set of tree roots, and creating one copies
8-byte root pointers inside one directory commit (O(1), BRANCH-02).

## Public API

| Item | Use |
|---|---|
| `BranchManager` (Clone) | `open(Container)` (initializes `main` on an empty file), `write(f)` (one storage commit per op), `list/get/default_branch`, `create/rename/delete/restore/purge_deleted/set_default`, `create_snapshot/drop_snapshot/reset`, `compare`, `space`, `compact`, `verify`, `write_handle`, `detach`, `export` |
| `directory::*` | typed reads over `&dyn KvRead`: `check_format`, `load_record`, `lookup_name`, `resolve`, `resolve_any_state`, `list`, `default_branch`, `origin`; writes `store_record/store_name/remove_name/store_format/store_default_branch/store_origin` |
| `ops::*` / `history::*` | pure directory mutations over `&mut dyn KvWrite` + `OpCtx{commit_seq, now_unix_ms}`: `initialize`, `create`, `rename`, `delete`, `restore`, `purge_deleted`, `set_default`, **`advance_head`** (the directory half of every data commit), `create_snapshot`, `drop_snapshot`, `reset` |
| `BranchRecord`, `BranchState`, `NamedSnapshot`, `SnapshotKind`, `DatabaseRoot` | the `B` value (`encode/decode`, `database_root(oid)`, `set_database_root`, `live_roots`) |
| `BranchRootResolver` | `pgarbor_storage::RootResolver` for compaction/doctor/space/export |
| `BranchRef`, `CreateSource`, `CreateOptions`, `DeleteOptions`, `PurgeFilter`, `ResetTarget`, `ResetOptions`, `CompareOptions`, `HandleOptions`, `DetachOptions`, `DetachStrategy` | selectors / options |
| `CompareReport`, `SpaceDiff`, `SpaceKind`, `BranchSpace`, `ResetReport`, `DetachReport`, `StrategyUsed` | serde-serializable reports (CLI `--json`) |
| `handle::{HandleFile, write_handle, read_handle, resolve_db_path}` | `.pgabranch` |
| `reflink::clone_file`, `detach::{detach_branch, reidentify_file, prune_to_branch}`, `export::{export_branch, verify_and_publish}` | standalone files |
| `error::*` | every branch error (exact messages below) |

### How downstream crates use it

- **pgarbor-core**:
  - Begin and statement snapshots: `directory::load_record(&snap.directory(), &id)` and
    `rec.database_root(db)`, always from the same storage snapshot.
  - Commit, inside `container.begin_write()`:
    1. `w.apply(root, batch)`;
    2. `ops::advance_head(&mut w.tree(dir), &OpCtx{..}, &id, generation, db, new_root)`;
    3. `w.set_directory_root`;
    4. `w.commit()`.
  - Branch admin goes through `Engine`, which adds the session checks, then `ops` and
    `BranchManager`.
- **pgarbor-cli**:
  - `branch create/list/rename/delete/restore/reset/snapshot/compare/link/detach/export` →
    `BranchManager` (offline) or `Engine` (online).
  - `compact`, `doctor` and `inspect --space` → `compact()`, `verify()` and `space()`, always with
    `BranchRootResolver`.

## Directory format (CONTRACT, byte-exact)

| key | value |
|---|---|
| `B` + branch_id(16) | `BranchRecord` v1 (below) |
| `N` + name UTF-8 | branch_id (16 raw bytes); ACTIVE branches only |
| `Mformat` | u32 LE directory format = 1 (required in any non-empty directory; any other value → refuse XX001) |
| `Mdefault_branch` | branch_id (16) |
| `Morigin` | compact JSON `ExportOrigin{source_db_uuid, source_branch_id, source_branch_name, source_commit_seq, exported_unix_ms, strategy}` (exported files only) |

Unknown `M` keys are preserved and ignored. Unknown key prefixes are ignored by readers, and
`doctor` warns about them.

### BranchRecord v1 (little-endian, no padding, no trailing bytes)

```
0   u8   version = 1
1   [16] id
17  u8   state (0 active, 1 deleted)
18  u8   flags (bit0 has_parent, bit1 roots_retained)
19  [16] parent (zeros if none)
35  u64  data_root            (database oid 5 "postgres"; 0 = empty tree)
43  u64  created_at_seq
51  u64  head_seq
59  u64  created_unix_ms
67  u64  updated_unix_ms
75  u64  generation           (1 at create; +1 on reset/delete/restore)
83  u64  deleted_at_seq       (0 if active)
91  u64  deleted_unix_ms      (0 if active)
99  u16  name_len, name
    u32  snapshot_count, then { u16 label_len, label, u64 root, u64 created_seq, u64 created_unix_ms, u8 kind(0 user,1 reset_recovery,2 inherited) }
    u32  database_count, then { u32 db_oid, u64 root }      (always 0 in v1)
```

Names and labels: `[A-Za-z0-9._/-]{1,200}`, no leading `-` or `/`, case-sensitive.

### `.pgabranch` (format `pgabranch/1`)

The file is pretty-printed JSON with members in this order: `format`, `db_uuid`, `branch_id`,
`branch_name_hint`, `db_path_hint`, `created_unix_ms`, `crc32c`.

- `db_uuid` and `branch_id` are lowercase hyphenated UUIDs.
- `crc32c` is 8 lowercase hex digits of crc32c over the compact JSON of the first six members.
- `db_path_hint` is relative to the handle's directory when possible.

Resolving a handle:

1. Find the master: an explicit `--db` path, else the hint relative to the handle's directory,
   else the hint as given.
2. The UUID must match, or the open fails loudly with 55000. A renamed branch still resolves
   (handles store the id).
3. A deleted branch → 42704 `branch "x" has been deleted`.

A handle is never a standalone database. Deleting one never deletes data.

## Key invariants

1. **Atomicity.** Each operation is one storage commit (`BranchManager::write`). `ops` and
   `history` validate everything before their first write.
2. **O(1) create.** The new record copies root pointers. No page is copied. Parent and child then
   diverge by copy-on-write.
3. **Isolation.** Branch A's commits change only A's record (`advance_head`). A sibling's roots are
   never touched.
4. **Ids are stable.** Rename changes only the name and the `N` entry. Handles, sessions and
   `generation` are unaffected.
5. **Generation guard.** Reset, delete and restore bump `generation`. `advance_head` refuses a
   stale generation, so a commit is never applied onto a foreign root.
6. **Deferred reclaim.**
   - Delete keeps a tombstone `B` record. By default `roots_retained = true`, so the branch can be
     restored.
   - `purge_deleted` removes the record, or `DeleteOptions{retain:false}` drops its roots.
   - Pages are freed only when `compact()` rewrites the file, using `BranchRootResolver`, which
     keeps every Active root and every retained root.
   - Open snapshots keep reading the pre-compaction file.
7. **Resolver exactness.**
   - `roots` lists exactly the live roots (data_root, extra databases, snapshots).
   - `remap` rewrites exactly those roots and calls `map` once per root.
   - Anything else is data loss in compaction, so tests are mandatory (BR-05).
8. **Export identity.** An exported or detached file has:
   - a fresh `DatabaseUuid` and a fresh `BranchId`;
   - one branch, default name `main`, no parent;
   - a `Morigin` recording the mapping.

   It is verified by reopening it before it is renamed into place. Failure leaves `dest`
   untouched and removes the temp file.

## Databases

One branch can hold several logical PostgreSQL databases, each its own tree. In v1 only oid 5
(`postgres`, `data_root`) exists. It also carries the cluster-shared catalog rows.
`extra_databases` is reserved in the format. CREATE DATABASE stays unimplemented until the core can
overlay two trees in one transaction (amendment pgarbor-core-02).

## Export / detach

- **Copy strategy.** `ExportBuilder` copies only the pages reachable from the branch's roots. It
  writes `<dest>.detach-tmp`, reopens and verifies it, then renames it to `dest` and syncs the
  directory.
- **Reflink strategy.** Linux `FICLONE`, macOS `clonefile`.
  1. Hold the container writer (`begin_write`) as a barrier while cloning the whole file.
  2. Re-identify the temp file: new UUID in one slot, the other slot zeroed.
  3. Logically prune: rewrite the directory to the single branch.
  4. Optionally compact (off by default, because compaction defeats extent sharing).
  5. Verify, then rename.

  Other branches' pages remain as garbage in the clone until the user compacts it. The report
  gives `garbage_pages`.
- **Strategy selection.**
  - `auto` falls back to copy and reports `fallback_reason`.
  - `reflink-required` fails with 0A000 `reflink cloning is not supported for "<path>"`, with
    hint `Use --strategy auto or --strategy copy.`
  - When `DetachOptions::os_filesystem` is false (MemVfs/FaultVfs), reflink is never attempted.

## Error conventions

| Condition | SQLSTATE | Message |
|---|---|---|
| unknown branch | 42704 | `branch "x" does not exist` / `branch with id <uuid> does not exist` |
| deleted branch (by id/handle) | 42704 | `branch "x" has been deleted` (detail `Branch id: <uuid>.`) |
| duplicate | 42710 | `branch "x" already exists` |
| bad name | 42602 | `invalid branch name "x"` / `invalid snapshot name "x"` (detail = rule) |
| snapshot missing / dup | 42704 / 42710 | `snapshot "l" of branch "b" does not exist` / `snapshot "l" already exists on branch "b"` |
| too many snapshots | 54000 | `branch "b" has too many snapshots` (limit 1024) |
| delete default | 55000 | `cannot delete the default branch "main"` |
| restore active / purged | 55000 | `branch "x" is not deleted` / `deleted branch "x" was purged and cannot be restored` |
| stale generation | 55000 | `branch "x" was reset or deleted concurrently` |
| sessions on branch (core) | 55006 | `branch "x" is being accessed by other sessions` |
| handle mismatch | 55000 | `branch handle "h" belongs to database <u1>, but "db" is database <u2>` |
| handle db missing | 58P01 | `could not find the database file for branch handle "h"` |
| bad handle | 22023 | `invalid branch handle file "h": <reason>` |
| destination exists | 58P02 | `file "p" already exists` |
| reflink unsupported | 0A000 | `reflink cloning is not supported for "p"` (not an unimplemented marker) |
| directory corruption | XX001 | `branch directory is corrupt: <what>` / `unsupported branch directory format version <v>` |
| export verification | XX001 | `exported database "p" failed verification: <msg>` |

## Non-goals / known gaps (v1)

- No SQL-level merge of divergent branches. Compare only reports divergence (RFC 001 §4.4).
- Compare groups differences by keyspace prefix and relation OID. Mapping OIDs to names is done
  by callers with the catalog.
- Per-branch space accounting costs O(branches × pages).
- No in-place space reuse: freeing pages needs `compact`, and compaction blocks writers.
- Reflink on macOS is written but is not compiled by the Linux CI `cargo check`.
- Windows: no reflink and no directory fsync.
- Only database oid 5 exists per branch. CREATE DATABASE is unimplemented.
- `reidentify_file` lives here until amendment pgarbor-branches-02 moves it into storage.
