//! Branch-local catalog metadata in space 3 (keys in `persist::keys`): the per-branch OID
//! allocator, the catalog version token, and catalog bootstrap. Lease CAT-01.
//!
//! ## OID allocation (amendment `pgarbor-catalog-02-oid-allocation`, consistent with core amendment 01)
//! A DDL statement needs ONE object that can both write the catalog and hand out OIDs: a
//! [`CatalogStore`] (`KvWrite + OidSource`). Why one object: `pgarbor_core::alloc::next_oid` needs
//! `&Txn` while `TxnKv` holds `&mut Txn`, so a separate `OidSource` borrowed next to the `TxnKv` could
//! never call it. In production the `pgarbor` crate wraps the transaction's `TxnKv` in a small struct
//! that forwards `KvRead`/`KvWrite` to it and implements `OidSource::next_oid` through
//! `alloc::next_oid(self.kv.txn(), &oid_seed)` (non-transactional, lock-free; the catalog never key-locks
//! an OID counter). Ready-made stores here:
//! - `impl OidSource for MemKv` (below): the KV counter `cat:next_oid` (tests, standalone tools). It
//!   key-locks nothing in a `MemKv`; on a real `TxnKv` it WOULD serialize writers, so never use it there;
//! - [`WithOids`]: adapter combining any `&mut dyn KvWrite` with any [`OidSource`] ([`MemoryOids`] in-memory
//!   counter seeded with [`oid_seed`], or [`RangeOids`], a pre-reserved range);
//! - the free function `allocate_oid`, the implementation behind `impl OidSource for MemKv`.
//! `allocate_oid` semantics (PG `GetNewOidWithIndex`, simplified):
//! - Counter key `cat:next_oid` (u32 BE). Absent -> `oid_seed(kv)` (>= 16384, above every OID in use).
//! - Returns the counter value and stores value+1. On reaching `u32::MAX` wraps to 16384. OID 0 is
//!   never returned.
//! - Skips OIDs for which `in_use` returns true (like PG's retry loop); after 2^32 - 16384
//!   consecutive hits -> 54000 `out of OIDs` (never loops forever).
//! - OIDs are per branch; two branches forked from one state allocate the same numbers independently.
//!   OIDs are NOT expected to equal PG's; tests must not compare user OIDs to the oracle.
//!
//! `oid_seed(kv)` = `max(FIRST_NORMAL_OID - 1, load_data(kv)?.max_oid(), cat:next_oid - 1 if present) + 1`:
//! the smallest OID above every OID used anywhere in the catalog (object keys AND OIDs embedded in
//! values: rowtype, array types, attrdef and enum-label OIDs; see `CatalogData::max_oid`).
//!
//! ## Catalog version
//! `cat:version` = `counter u64 BE ‖ stamp u64 BE`. Every catalog-writing STATEMENT bumps it once
//! (`CatalogTxn::finish_statement`): `counter + 1` and a caller-provided `stamp` that must be unique
//! within the process lifetime AND across processes with high probability (pgarbor-core passes
//! `random_u64() ^ txn_id`; see `crate::cache`). The pair (not the counter alone) identifies catalog
//! contents: two branches forked at counter 5 can each produce a different counter 6, and a branch
//! reset can revisit counter 6 with different contents.
//!
//! ## Bootstrap
//! `bootstrap(kv)` initializes an empty branch catalog (called by pgarbor once when creating a
//! container's `main` branch; branches created later inherit it via copy-on-write):
//! 1. if `cat:bootstrap` exists → return `Ok(false)` (idempotent);
//! 2. write `Namespace{2200, "public", owner 6171 (pg_database_owner), acl
//!    [{grantee 6171, grantor 6171, USAGE|CREATE}, {grantee 0 (PUBLIC), grantor 6171, USAGE}]}`
//!    (prints `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}`);
//! 3. write `Role{10, "postgres", all attributes true, conn_limit -1, password None}`;
//! 4. write `Database{5, "postgres", owner 10, encoding 6, locale_provider b'b', is_template false,
//!    allow_conn true, conn_limit -1, collate/ctype "C.UTF-8", locale Some("C.UTF-8"), acl None}`
//!    (values from `builtin::DATABASES`);
//! 5. write the description row `COMMENT ON SCHEMA public IS 'standard public schema'`
//!    (PG ships it; classoid 2615) and for database postgres
//!    `'default administrative connection database'` (classoid 1262; pg_shdescription in PG);
//! 6. write `cat:next_oid` = 16384, `cat:version` = (1, 0), `cat:bootstrap` = 1 (u32 BE);
//! 7. return `Ok(true)`.
use super::codec;
use super::keys;
use crate::model::*;
use pgarbor_common::{KvRead, KvWrite, Oid, PgError, Result, FIRST_NORMAL_OID};

/// Catalog version token (see module docs). Ordered by `counter` first; equality needs both.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CatalogVersion {
    pub counter: u64,
    pub stamp: u64,
}

impl CatalogVersion {
    pub const ZERO: CatalogVersion = CatalogVersion { counter: 0, stamp: 0 };

    /// 16-byte stored form.
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.counter.to_be_bytes());
        b[8..].copy_from_slice(&self.stamp.to_be_bytes());
        b
    }

    /// Parse the stored form; wrong length → XX001 `invalid catalog version value`.
    pub fn decode(b: &[u8]) -> Result<CatalogVersion> {
        if b.len() != 16 {
            return Err(PgError::corruption("invalid catalog version value"));
        }
        let mut c = [0u8; 8];
        let mut s = [0u8; 8];
        c.copy_from_slice(&b[..8]);
        s.copy_from_slice(&b[8..]);
        Ok(CatalogVersion { counter: u64::from_be_bytes(c), stamp: u64::from_be_bytes(s) })
    }
}

/// Read the catalog version (absent → `CatalogVersion::ZERO`). Cheap: one point read. The engine
/// calls this at every statement start to decide whether its cached snapshot is stale.
pub fn read_version(kv: &dyn KvRead) -> Result<CatalogVersion> {
    match kv.get(&keys::version_key())? {
        Some(b) => CatalogVersion::decode(&b),
        None => Ok(CatalogVersion::ZERO),
    }
}

/// Write `counter + 1` with the given stamp and return the new version.
pub fn bump_version(kv: &mut dyn KvWrite, stamp: u64) -> Result<CatalogVersion> {
    let cur = read_version(kv)?;
    let next = CatalogVersion { counter: cur.counter.wrapping_add(1), stamp };
    kv.put(&keys::version_key(), &next.encode())?;
    Ok(next)
}

/// True once `bootstrap` has run on this branch state.
pub fn is_bootstrapped(kv: &dyn KvRead) -> Result<bool> {
    Ok(kv.get(&keys::bootstrap_key())?.is_some())
}

/// A source of fresh OIDs for DDL (see module docs). Must never return an OID twice for the
/// same branch and must return OIDs >= `FIRST_NORMAL_OID`.
// CONTRACT
pub trait OidSource {
    fn next_oid(&mut self) -> Result<Oid>;
}

/// What `CatalogTxn::begin` needs: catalog writes plus OID allocation in one object (module docs).
// CONTRACT
pub trait CatalogStore: KvWrite + OidSource {}

impl<T: KvWrite + OidSource + ?Sized> CatalogStore for T {}

/// `MemKv` is a complete `CatalogStore` through the `cat:next_oid` counter (module docs).
impl OidSource for pgarbor_common::MemKv {
    fn next_oid(&mut self) -> Result<Oid> {
        allocate_oid(self, &|_| false)
    }
}

/// Combine a key/value writer with an OID source into a [`CatalogStore`].
// CONTRACT
pub struct WithOids<'a> {
    pub kv: &'a mut dyn KvWrite,
    pub oids: &'a mut dyn OidSource,
}

impl pgarbor_common::KvRead for WithOids<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.kv.get(key)
    }
    fn scan<'b>(
        &'b self,
        lower: std::ops::Bound<&[u8]>,
        upper: std::ops::Bound<&[u8]>,
    ) -> Result<pgarbor_common::KvIter<'b>> {
        self.kv.scan(lower, upper)
    }
}

impl KvWrite for WithOids<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.kv.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.kv.delete(key)
    }
}

impl OidSource for WithOids<'_> {
    fn next_oid(&mut self) -> Result<Oid> {
        self.oids.next_oid()
    }
}

/// In-memory counter source.
#[derive(Clone, Debug)]
pub struct MemoryOids {
    next: Oid,
}

impl MemoryOids {
    /// Start at an explicit OID (clamped up to `FIRST_NORMAL_OID`).
    pub fn starting_at(next: Oid) -> MemoryOids {
        MemoryOids { next: next.max(FIRST_NORMAL_OID) }
    }
    /// Seed from the catalog visible through `kv` (see [`oid_seed`]).
    pub fn seeded(kv: &dyn KvRead) -> Result<MemoryOids> {
        Ok(MemoryOids::starting_at(oid_seed(kv)?))
    }
    /// The next OID that will be handed out.
    pub fn peek(&self) -> Oid {
        self.next
    }
}

impl OidSource for MemoryOids {
    fn next_oid(&mut self) -> Result<Oid> {
        if self.next == Oid::MAX {
            return Err(PgError::new(pgarbor_common::SqlState::PROGRAM_LIMIT_EXCEEDED, "out of OIDs"));
        }
        let o = self.next;
        self.next += 1;
        Ok(o)
    }
}

/// A pre-reserved half-open range `[next, end)`.
#[derive(Clone, Debug)]
pub struct RangeOids {
    pub next: Oid,
    pub end: Oid,
}

impl OidSource for RangeOids {
    fn next_oid(&mut self) -> Result<Oid> {
        if self.next >= self.end {
            return Err(PgError::internal("reserved OID range exhausted"));
        }
        let o = self.next;
        self.next += 1;
        Ok(o)
    }
}

/// The smallest OID above every OID used in the catalog visible through `kv` (module docs).
pub fn oid_seed(kv: &dyn KvRead) -> Result<Oid> {
    let data = codec::load_data(kv)?;
    let mut hi = data.max_oid().max(FIRST_NORMAL_OID - 1);
    if let Some(b) = kv.get(&keys::next_oid_key())? {
        if b.len() == 4 {
            hi = hi.max(u32::from_be_bytes([b[0], b[1], b[2], b[3]]).saturating_sub(1));
        }
    }
    Ok(hi.saturating_add(1))
}

/// KV-counter allocation (module docs; the body behind `impl OidSource for MemKv`). `in_use` reports OIDs already taken in the working catalog.
// CONTRACT
#[allow(unused_variables)]
pub fn allocate_oid(kv: &mut dyn KvWrite, in_use: &dyn Fn(Oid) -> bool) -> Result<Oid> {
    Err(PgError::unimplemented("CAT-01", "catalog OID allocation"))
}

/// Initialize an empty branch catalog (see module docs). Returns false if already bootstrapped.
// CONTRACT
#[allow(unused_variables)]
pub fn bootstrap(kv: &mut dyn KvWrite) -> Result<bool> {
    Err(PgError::unimplemented("CAT-01", "catalog bootstrap"))
}

#[cfg(test)]
mod oid_tests {
    use super::*;
    use pgarbor_common::MemKv;

    #[test]
    fn memory_and_range_sources() {
        let mut m = MemoryOids::starting_at(5);
        assert_eq!(m.next_oid().unwrap(), FIRST_NORMAL_OID);
        assert_eq!(m.next_oid().unwrap(), FIRST_NORMAL_OID + 1);
        let mut r = RangeOids { next: 20000, end: 20002 };
        assert_eq!(r.next_oid().unwrap(), 20000);
        assert_eq!(r.next_oid().unwrap(), 20001);
        assert!(r.next_oid().is_err());
    }

    #[test]
    fn seed_of_empty_catalog() {
        let kv = MemKv::default();
        assert_eq!(oid_seed(&kv).unwrap(), FIRST_NORMAL_OID);
    }
}
