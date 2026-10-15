//! Byte-exact catalog key layout (ARCHITECTURE §4 / §9). Fully implemented, FROZEN (on-disk v1).
//!
//! ## Space 1 (`0x0000_0001`, catalog rows, owned by pgarbor-catalog)
//!
//! ```text
//! object:       [00 00 00 01] [kind u8] [oid u32 BE]                                  (9 bytes)
//! description:  [00 00 00 01] [b'D']    [classoid u32 BE] [objoid u32 BE] [objsubid u32 BE ^ 0x8000_0000]  (17 bytes)
//! ```
//! Kind bytes: `n` namespace, `r` relation, `i` index, `S` sequence, `t` type, `c` constraint,
//! `f` function, `a` role, `m` role membership, `d` database, `D` description
//! (`model::ObjKind::byte`). `objsubid` is stored sign-flipped so keys sort numerically.
//! Values: see `persist::codec` (`[0x01] + serde_json`).
//!
//! ## Space 3 (`0x0000_0003`, branch-local metadata, SHARED with pgarbor-core)
//!
//! pgarbor-catalog owns only keys under the sub-prefix `b"cat:"` (amendment
//! `contracts/amendments/pgarbor-catalog-01-space3-subtag.md`):
//! ```text
//! [00 00 00 03] b"cat:next_oid"   → u32 BE: next OID to hand out (absent = 16384). STANDALONE USE ONLY
//!                                   (amendment pgarbor-catalog-02: the engine allocates OIDs through
//!                                   `persist::OidSource` / pgarbor-core alloc and never writes this key)
//! [00 00 00 03] b"cat:version"    → u64 BE counter ‖ u64 BE stamp  (16 bytes; absent = 0 ‖ 0)
//! [00 00 00 03] b"cat:bootstrap"  → u32 BE catalog format version written by bootstrap (1)
//! ```
use crate::model::{DescriptionKey, ObjKind};
use pgarbor_common::{Oid, PgError, Result, SpaceId};

/// Space-3 sub-prefix owned by the catalog.
pub const META_SUBTAG: &[u8] = b"cat:";

/// `[0,0,0,1]`.
pub fn catalog_prefix() -> [u8; 4] {
    SpaceId::CATALOG.prefix()
}

/// `[0,0,0,1][kind]`: prefix of every row of one kind.
pub fn kind_prefix(kind: ObjKind) -> Vec<u8> {
    let mut k = catalog_prefix().to_vec();
    k.push(kind.byte());
    k
}

/// Key of an OID-keyed object.
pub fn object_key(kind: ObjKind, oid: Oid) -> Vec<u8> {
    let mut k = kind_prefix(kind);
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

/// Key of a description row.
pub fn description_key(d: &DescriptionKey) -> Vec<u8> {
    let mut k = kind_prefix(ObjKind::Description);
    k.extend_from_slice(&d.classoid.to_be_bytes());
    k.extend_from_slice(&d.objoid.to_be_bytes());
    k.extend_from_slice(&((d.objsubid as u32) ^ 0x8000_0000).to_be_bytes());
    k
}

/// A decoded space-1 key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogKey {
    Object(ObjKind, Oid),
    Description(DescriptionKey),
}

/// Decode a space-1 key. Malformed keys are corruption (XX001
/// `invalid catalog key <hex>`).
pub fn decode_key(key: &[u8]) -> Result<CatalogKey> {
    let bad = || PgError::corruption(format!("invalid catalog key {}", hex(key)));
    if key.len() < 5 || key[..4] != catalog_prefix() {
        return Err(bad());
    }
    let kind = ObjKind::from_byte(key[4]).ok_or_else(bad)?;
    let rd = |i: usize| -> Option<u32> { key.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])) };
    if kind == ObjKind::Description {
        if key.len() != 17 {
            return Err(bad());
        }
        let classoid = rd(5).ok_or_else(bad)?;
        let objoid = rd(9).ok_or_else(bad)?;
        let sub = rd(13).ok_or_else(bad)? ^ 0x8000_0000;
        Ok(CatalogKey::Description(DescriptionKey { objoid, classoid, objsubid: sub as i32 }))
    } else {
        if key.len() != 9 {
            return Err(bad());
        }
        Ok(CatalogKey::Object(kind, rd(5).ok_or_else(bad)?))
    }
}

/// A space-3 catalog metadata key: `[0,0,0,3] b"cat:" name`.
pub fn meta_key(name: &str) -> Vec<u8> {
    let mut k = SpaceId::BRANCH_META.prefix().to_vec();
    k.extend_from_slice(META_SUBTAG);
    k.extend_from_slice(name.as_bytes());
    k
}

/// Key of the next-OID counter.
pub fn next_oid_key() -> Vec<u8> {
    meta_key("next_oid")
}

/// Key of the catalog version.
pub fn version_key() -> Vec<u8> {
    meta_key("version")
}

/// Key of the bootstrap marker.
pub fn bootstrap_key() -> Vec<u8> {
    meta_key("bootstrap")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let k = object_key(ObjKind::Relation, 16384);
        assert_eq!(k, vec![0, 0, 0, 1, b'r', 0, 0, 0x40, 0]);
        assert_eq!(decode_key(&k).unwrap(), CatalogKey::Object(ObjKind::Relation, 16384));
        let d = DescriptionKey { objoid: 16384, classoid: 1259, objsubid: -1 };
        assert_eq!(decode_key(&description_key(&d)).unwrap(), CatalogKey::Description(d));
        let a = description_key(&DescriptionKey { objoid: 1, classoid: 1, objsubid: -1 });
        let b = description_key(&DescriptionKey { objoid: 1, classoid: 1, objsubid: 0 });
        assert!(a < b);
        assert_eq!(&version_key()[..8], b"\0\0\0\x03cat:");
        assert!(decode_key(&[0, 0, 0, 1, b'z', 0, 0, 0, 1]).is_err());
    }
}
