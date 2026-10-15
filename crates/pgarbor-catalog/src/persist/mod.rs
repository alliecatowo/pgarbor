//! Catalog persistence over the KV traits (ARCHITECTURE §4, §9): key layout ([`keys`]), row
//! codec and whole-catalog load ([`codec`]), and space-3 metadata: OID allocator, catalog version,
//! bootstrap ([`meta`]).
pub mod codec;
pub mod keys;
pub mod meta;

pub use codec::{delete_description, delete_object, load_data, put_description, put_object};
pub use meta::{
    allocate_oid, bootstrap, bump_version, is_bootstrapped, oid_seed, read_version, CatalogVersion, MemoryOids,
    OidSource, RangeOids, CatalogStore, WithOids,
};
