# Amendment request pgarbor-core-01: non-transactional OID / row-id allocation

Status: PROPOSED (architect, pgarbor-core). Implemented in the skeleton as
`pgarbor_core::alloc::{next_oid, next_rowid, next_rowids, IdAllocator}` and `TxnKv::put_fresh`.

## Problem

ARCHITECTURE §4 says the next-OID counters live in space 3 and are written through the
transaction's `KvWrite`. §7 says every write takes an exclusive key lock first. Together, two
concurrent `CREATE TABLE`s (or two concurrent `INSERT`s that allocate row ids from a counter row)
block on the counter key until the first one commits. PostgreSQL does not block here. Its OID
counter and heap TIDs are non-transactional. The result would be wrong lock timing and spurious
deadlocks, plus one lock entry per inserted row.

## Proposed change

1. pgarbor-core owns in-memory monotonic counters per (branch, database[, relid]).
   - Each counter is seeded lazily from the latest committed head: max row id + 1 per table, and a
     caller-supplied OID seed per database.
   - Counters are never persisted. Gaps are allowed and values never repeat.
   - Reset and delete clear the branch's counters.
2. Row keys built from a freshly allocated row id are inserted with `TxnKv::put_fresh`, which takes
   no key lock. Unique-index keys and every UPDATE/DELETE still lock.
3. pgarbor-catalog cannot depend on pgarbor-core. Two options:
   - (a) catalog APIs that create objects take the OID as a parameter, and pgarbor-exec obtains it
     from `pgarbor_core::alloc::next_oid(txn, catalog::max_oid_seed)`;
   - (b) add `trait IdSource { fn next_oid(&mut self) -> Result<Oid>; }` to `pgarbor-common::kv`,
     and pass `&mut dyn IdSource` next to `&mut dyn KvWrite`.
   Recommend (a) for v1. It needs no change to the common crate. Catalog must expose
   `fn oid_seed(&dyn KvRead) -> Result<Oid>`: the smallest OID >= 16384 above every catalog row.
4. ARCHITECTURE §4: space 3 no longer holds a next-OID counter in v1. It stays reserved for
   branch-local metadata.

## Affected crates

pgarbor-catalog (OID parameter or seed function), pgarbor-exec (uses `alloc` and `put_fresh`),
ARCHITECTURE §4 and §7 wording.
