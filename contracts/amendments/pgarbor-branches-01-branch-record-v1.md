# Amendment request pgarbor-branches-01: BranchRecord v1 fields and byte format

Status: PROPOSED (architect, pgarbor-branches). Already implemented in the v1 skeleton
(`crates/pgarbor-branches/src/record.rs`, `keys.rs`, `meta.rs`).

## Problem

ARCHITECTURE §6 lists `BranchRecord { id, name, data_root, parent, created_at_seq, head_seq,
created_unix_ms, state, snapshots: Vec<(label, PageId)> }` and gives no encoding. The frozen design
needs more than that:

1. **A guard for forced reset/delete.** pgarbor-core commits by re-reading the branch record and
   applying the overlay to its current root. If the branch was reset (or deleted and restored) under
   an open transaction, the overlay would be applied to a foreign root. That loses or corrupts data.
2. **Deferred reclaim** (BRANCH-05): tombstones that keep their roots until they are purged, plus
   deletion metadata.
3. **Multiple logical PostgreSQL databases per branch** (RFC 001 §9): the record needs a place for
   more than one database tree, and adding it later must not break the format.
4. **Typed snapshots**: user, reset-recovery and inherited snapshots, each with a creation commit.
5. **A byte-exact, versioned value format** that storage's `RootResolver` contract can remap.

## Proposed change

`BranchRecord` gains these fields: `updated_unix_ms`, `generation` (starts at 1; +1 on reset,
delete and restore; commits compare it), `deleted_at_seq`, `deleted_unix_ms`, `roots_retained`,
`snapshots: Vec<NamedSnapshot{label, root, created_seq, created_unix_ms, kind}>` and
`extra_databases: Vec<DatabaseRoot{db_oid, root}>`.

- `data_root` is the tree of database oid 5 (`postgres`). In v1 that tree also holds the
  cluster-shared catalog rows (roles, pg_database).
- The binary v1 layout is specified in `record.rs`: version byte 1, little-endian, and no trailing
  bytes.
- Directory metadata keys are `Mformat` (u32 = 1, required), `Mdefault_branch` and `Morigin` (the
  JSON export provenance).
- Branch names follow §6 exactly.

## Affected crates

- **pgarbor-core**:
  - The commit path calls `ops::advance_head(.., expected_generation, db_oid, new_root)`.
  - Sessions bind to (branch id, database oid).
- **pgarbor-storage**: none. It interacts only through `BranchRootResolver`.
- **pgarbor / pgarbor-cli**: list and inspect output shows the new fields. The `restore` and
  `purge` subcommands are new.
- **ARCHITECTURE.md §6**: replace the field list with a reference to
  `contracts/crates/pgarbor-branches.md`.
