# Amendment request pgarbor-catalog-02: OID allocation is injected, not stored in space 3

Status: ACCEPTED (2026-10-15, lead engineering review).

## Problem

The first catalog skeleton allocated OIDs from a counter key written through the transaction's
`KvWrite` (`cat:next_oid`). Every catalog-writing transaction would then key-lock that counter and
serialize until commit, which is the problem pgarbor-core-01 describes. ARCHITECTURE §9 also says the
per-branch allocator lives in space 3.

## Proposed change

1. `CatalogTxn::begin(store: &mut dyn CatalogStore, env)` where `CatalogStore: KvWrite + OidSource` (blanket
   impl). ONE object does both jobs because `pgarbor_core::alloc::next_oid(txn: &Txn, ..)` needs `&Txn`
   while `TxnKv<'t>` holds `&'t mut Txn`: a separate `OidSource` borrowed next to the `TxnKv` could never
   call it. The `pgarbor` crate defines a small wrapper owning the `TxnKv` that forwards `KvRead`/`KvWrite`
   and implements `OidSource::next_oid` as `alloc::next_oid(self.kv.txn(), &|kv| persist::oid_seed(kv))`
   (non-transactional, lock-free). (Alternative if the wrapper proves awkward: a core amendment exposing
   OID allocation without `&Txn`.)
   `persist::oid_seed(&dyn KvRead) -> Result<Oid>` returns the smallest OID above EVERY OID in the
   catalog, including OIDs embedded in row values (rowtype, array types, attrdef and enum-label OIDs,
   `CatalogData::max_oid`), clamped to >= 16384.
2. `impl OidSource for MemKv` (key `cat:next_oid`, seeded by `oid_seed` when absent) and the adapter
   `WithOids { kv, oids }` keep tests and standalone tools simple; they must not be used on a real `TxnKv`
   (the counter key would be key-locked).
3. ARCHITECTURE §9: "per-branch OID allocator stored in space 3" becomes "per-branch OID counters live in
   pgarbor-core (seeded by `persist::oid_seed`); the catalog only reads them through `OidSource`".
4. `CatalogTxn` double-checks every OID against the working data (`CatalogData::oid_in_use`), so a
   misbehaving source can never corrupt the catalog.

## Affected crates

pgarbor-core (unchanged: already provides `alloc::next_oid` with a seed callback), pgarbor (implements
`OidSource`), ARCHITECTURE §9 wording.


## Steward decision (2026-10-15)

Accept the implemented `CatalogStore: KvWrite + OidSource`, standalone `WithOids`/MemKv sources, and `oid_seed` boundary. Engine integration must use the core allocator; assembled API adapters are not an end-to-end verification claim. Architecture §9 records allocation ownership.
