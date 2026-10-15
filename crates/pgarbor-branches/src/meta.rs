//! Container metadata stored under directory keys `M` + name. CONTRACT. Fully implemented.
//!
//! | key | value |
//! |---|---|
//! | `Mformat` | directory format version, u32 little-endian (= 1) — REQUIRED in a non-empty directory |
//! | `Mdefault_branch` | 16-byte branch id of the default branch (what `serve` / `Database::session` use when no branch is named) |
//! | `Morigin` | JSON [`ExportOrigin`] (only in files produced by export/detach) |
//!
//! Unknown `M` keys are preserved by every operation and ignored by readers (forward compatible
//! *optional* metadata). A required change must bump `DIRECTORY_FORMAT_VERSION`.
use crate::error;
use pgarbor_common::{BranchId, Result};
#[cfg(test)]
use pgarbor_common::DatabaseUuid;

/// Directory format understood by this build. Readers refuse (XX001) any other value.
pub const DIRECTORY_FORMAT_VERSION: u32 = 1;

pub const META_FORMAT: &str = "format";
pub const META_DEFAULT_BRANCH: &str = "default_branch";
pub const META_ORIGIN: &str = "origin";

pub fn encode_format(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}

/// Errors: wrong length → XX001 `branch directory is corrupt: bad format marker`.
pub fn decode_format(value: &[u8]) -> Result<u32> {
    let b: [u8; 4] = value.try_into().map_err(|_| error::directory_corrupt("bad format marker"))?;
    Ok(u32::from_le_bytes(b))
}

pub fn encode_default_branch(id: &BranchId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// Errors: wrong length → XX001 `branch directory is corrupt: bad default branch marker`.
pub fn decode_default_branch(value: &[u8]) -> Result<BranchId> {
    crate::keys::decode_id_value(value).ok_or_else(|| error::directory_corrupt("bad default branch marker"))
}

// CONTRACT
/// Provenance of a file produced by `export`/`detach` (RFC 001 §4.3 "documented branch-identity
/// mapping"). Serialized as compact JSON with exactly these field names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportOrigin {
    /// Hyphenated lowercase UUID of the source container.
    pub source_db_uuid: String,
    /// Hyphenated lowercase UUID of the source branch.
    pub source_branch_id: String,
    pub source_branch_name: String,
    /// Source commit the export reflects.
    pub source_commit_seq: u64,
    pub exported_unix_ms: u64,
    /// "copy" or "reflink".
    pub strategy: String,
}

impl ExportOrigin {
    pub fn to_json(&self) -> Vec<u8> {
        // Serializing plain data structs into a Vec cannot fail.
        serde_json::to_vec(self).unwrap_or_default()
    }
    /// Errors: invalid JSON → XX001 `branch directory is corrupt: bad origin metadata: <e>`.
    pub fn from_json(bytes: &[u8]) -> Result<ExportOrigin> {
        serde_json::from_slice(bytes).map_err(|e| error::directory_corrupt(format!("bad origin metadata: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meta_roundtrip() {
        assert_eq!(decode_format(&encode_format(1)).unwrap(), 1);
        assert!(decode_format(b"xx").is_err());
        let o = ExportOrigin {
            source_db_uuid: DatabaseUuid([1; 16]).to_hyphenated(),
            source_branch_id: BranchId([2; 16]).to_hyphenated(),
            source_branch_name: "feature".into(),
            source_commit_seq: 9,
            exported_unix_ms: 5,
            strategy: "copy".into(),
        };
        assert_eq!(ExportOrigin::from_json(&o.to_json()).unwrap(), o);
    }
}
