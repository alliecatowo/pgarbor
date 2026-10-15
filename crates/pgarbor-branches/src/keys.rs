//! Directory-tree key encoding (ARCHITECTURE §6). CONTRACT. Fully implemented.
//!
//! The directory is the storage tree whose root lives in the superblock. Its keys:
//!
//! | key | value | owner |
//! |---|---|---|
//! | `b"B"` + branch_id (16 raw bytes) | `BranchRecord` (record.rs binary format) | this crate |
//! | `b"N"` + name (UTF-8 bytes, no terminator) | branch_id (16 raw bytes) | this crate |
//! | `b"M"` + meta name (ASCII) | metadata value (meta.rs) | this crate |
//!
//! Only ACTIVE branches have an `N` entry; deleted (tombstoned) branches keep their `B` record.
//! Any other first byte is reserved: readers ignore it, `doctor` reports it as a warning.
use pgarbor_common::BranchId;

pub const BRANCH_PREFIX: u8 = b'B';
pub const NAME_PREFIX: u8 = b'N';
pub const META_PREFIX: u8 = b'M';

/// `B` + id.
pub fn branch_key(id: &BranchId) -> Vec<u8> {
    let mut k = Vec::with_capacity(17);
    k.push(BRANCH_PREFIX);
    k.extend_from_slice(id.as_bytes());
    k
}

/// `N` + name bytes.
pub fn name_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + name.len());
    k.push(NAME_PREFIX);
    k.extend_from_slice(name.as_bytes());
    k
}

/// `M` + meta name bytes.
pub fn meta_key(meta: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + meta.len());
    k.push(META_PREFIX);
    k.extend_from_slice(meta.as_bytes());
    k
}

/// Classified directory key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirKey {
    Branch(BranchId),
    Name(String),
    Meta(String),
    /// Malformed `B` key (wrong length) or non-UTF-8 `N`/`M` key, or an unknown prefix.
    Unknown,
}

/// Classify a directory key.
pub fn classify(key: &[u8]) -> DirKey {
    match key.split_first() {
        Some((&BRANCH_PREFIX, rest)) => match <[u8; 16]>::try_from(rest) {
            Ok(b) => DirKey::Branch(BranchId(b)),
            Err(_) => DirKey::Unknown,
        },
        Some((&NAME_PREFIX, rest)) => match std::str::from_utf8(rest) {
            Ok(s) => DirKey::Name(s.to_string()),
            Err(_) => DirKey::Unknown,
        },
        Some((&META_PREFIX, rest)) => match std::str::from_utf8(rest) {
            Ok(s) => DirKey::Meta(s.to_string()),
            Err(_) => DirKey::Unknown,
        },
        _ => DirKey::Unknown,
    }
}

/// Decode a 16-byte `N` value into a branch id (`None` if the length is wrong).
pub fn decode_id_value(value: &[u8]) -> Option<BranchId> {
    <[u8; 16]>::try_from(value).ok().map(BranchId)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let id = BranchId([7; 16]);
        assert_eq!(classify(&branch_key(&id)), DirKey::Branch(id));
        assert_eq!(classify(&name_key("main")), DirKey::Name("main".into()));
        assert_eq!(classify(&meta_key("format")), DirKey::Meta("format".into()));
        assert_eq!(classify(b"Bshort"), DirKey::Unknown);
        assert_eq!(classify(b"Zx"), DirKey::Unknown);
        assert!(branch_key(&id) < meta_key("a") && meta_key("z") < name_key("a"));
    }
}
