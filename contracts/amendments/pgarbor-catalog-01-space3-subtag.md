# Amendment request pgarbor-catalog-01: catalog metadata keys inside KV space 3

Status: PROPOSED (architect, pgarbor-catalog). Implemented in `persist::keys` (frozen on-disk v1).

## Problem

ARCHITECTURE §4 reserves space `0x0000_0003` for "branch-local metadata (next OID counters, etc.)" without
a key layout. pgarbor-core also writes metadata there (or may), so two crates need a collision-free
convention. pgarbor-core-01 (non-transactional id allocation) removes the OID counter from space 3 for
the engine, but the catalog still needs a durable catalog VERSION token and a bootstrap marker that
travel with the branch tree (copy-on-write, rolled back with the transaction).

## Proposed change

pgarbor-catalog owns ONLY keys that start with the 8-byte prefix `00 00 00 03` `"cat:"`:

| key suffix | value | meaning |
|---|---|---|
| `cat:version` | `counter u64 BE ‖ stamp u64 BE` (16 bytes; absent = 0‖0) | catalog content identity; bumped once per catalog-writing statement in the SAME transaction as the rows |
| `cat:bootstrap` | `u32 BE` catalog format version (1) | set by `persist::bootstrap`; absent = branch not bootstrapped |
| `cat:next_oid` | `u32 BE` | STANDALONE use only (`impl OidSource for MemKv`); the engine never writes it (see amendment 02) |

Other crates must not write keys with this prefix and may use any other `00 00 00 03` prefix freely.
The `stamp` must be process-unique and effectively random across processes (pgarbor-core passes
`random_u64() ^ txn_id`); equality of the full (counter, stamp) pair identifies catalog content across
branches, resets and rollbacks (`cache::CatalogCache`).

## Affected crates

pgarbor-core (must not use the `cat:` prefix; `TxnKv` locks these keys like any other: the version key
doubles as the DDL serialization lock, see contracts/crates/pgarbor-catalog.md), ARCHITECTURE §4 wording.
