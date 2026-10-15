//! Superblock codec and dual-slot selection (STORE-01, STORE-02). Lease STO-01.
//!
//! # Byte layout of one slot (CONTRACT, little-endian, slot = 4096 bytes)
//! ```text
//! off  len  field
//!   0    8  magic            = b"PGARBOR\0"
//!   8    4  format_version   = 1
//!  12    4  reserved0        = 0
//!  16    8  feature_flags    (bits 32..63 required, 0..31 compatible)
//!  24    4  page_size        (power of two, 4096..=65536)
//!  28    4  reserved1        = 0
//!  32   16  db_uuid          (raw 16 bytes, RFC 4122 byte order)
//!  48    8  commit_seq       (>= 1)
//!  56    8  directory_root   (PageId, 0 = empty directory tree)
//!  64    8  page_count       (highest allocated PageId; 0 = no pages)
//!  72    8  freelist_root    (PageId, always 0 in format v1)
//!  80    8  created_unix_ms
//!  88    4  crc32c           = crc32c(bytes[0..88])
//!  92 4004  zero padding (written as zeros, NOT checked on read)
//! ```
//! Slot 0 is at file offset 0, slot 1 at 4096. A commit always writes the slot that does NOT
//! hold the current superblock, with `commit_seq + 1` (ARCHITECTURE §5).
#[allow(unused_imports)]
use crate::error;
use crate::format::consts::*;
use pgarbor_common::{CommitSeq, DatabaseUuid, PageId, PgError, Result};
use serde::Serialize;
use std::path::Path;

// CONTRACT
/// Decoded superblock (ARCHITECTURE §5 field list).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Superblock {
    pub format_version: u32,
    pub feature_flags: u64,
    pub page_size: u32,
    pub db_uuid: DatabaseUuid,
    pub commit_seq: CommitSeq,
    pub directory_root: PageId,
    pub page_count: u64,
    pub freelist_root: PageId,
    pub created_unix_ms: u64,
}

impl Superblock {
    /// Superblock of a brand-new container: version 1, no features, commit_seq 1, empty
    /// directory, zero pages, no freelist.
    pub fn new_empty(page_size: u32, db_uuid: DatabaseUuid, created_unix_ms: u64) -> Superblock {
        Superblock {
            format_version: FORMAT_VERSION,
            feature_flags: 0,
            page_size,
            db_uuid,
            commit_seq: CommitSeq(1),
            directory_root: PageId::NONE,
            page_count: 0,
            freelist_root: PageId::NONE,
            created_unix_ms,
        }
    }

    /// Encode into a full 4096-byte slot image per the layout above (padding zeroed, crc set).
    ///
    /// Errors: `PgError::internal` if `page_size` is invalid or `commit_seq == 0` (never write a
    /// slot that `decode` would reject).
    ///
    /// Example: `Superblock::new_empty(8192, uuid, 0).encode()?[0..8] == *b"PGARBOR\0"`, and
    /// `decode(&encode(sb)?)? == SlotStatus::Valid(sb)`.
    #[allow(unused_variables)]
    pub fn encode(&self) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("STO-01", "superblock encoding"))
    }

    /// Decode a slot image (`bytes.len()` must be >= SUPERBLOCK_ENCODED_LEN; callers pass 4096,
    /// zero-filled where the file was shorter). Never returns `Err` for bad data — bad data is a
    /// [`SlotStatus`] variant; `Err` only for an internal precondition (`bytes` too short).
    ///
    /// Check order (first failure wins):
    /// 1. all of `bytes[0..SUPERBLOCK_ENCODED_LEN]` zero → `Empty`
    /// 2. magic != `SUPERBLOCK_MAGIC` → `BadMagic`
    /// 3. crc mismatch → `BadChecksum { stored, computed }`
    /// 4. format_version != FORMAT_VERSION → `UnsupportedVersion(v)`
    /// 5. `feature_flags & FEATURE_REQUIRED_MASK & !KNOWN_FEATURES != 0` → `UnsupportedFeatures(those bits)`
    /// 6. invalid page_size, commit_seq == 0, reserved0/reserved1 != 0, freelist_root != 0,
    ///    or directory_root > page_count → `Invalid(reason)` (reason is a short English phrase,
    ///    e.g. "invalid page size 1000", "directory root 9 beyond page count 4")
    /// 7. otherwise `Valid(sb)`.
    #[allow(unused_variables)]
    pub fn decode(bytes: &[u8]) -> Result<SlotStatus> {
        Err(PgError::unimplemented("STO-01", "superblock decoding"))
    }

    /// Committed file length implied by this superblock.
    pub fn committed_len(&self) -> u64 {
        file_len_for(self.page_size, self.page_count)
    }
}

// CONTRACT
/// Result of decoding one slot. Serialized into doctor reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SlotStatus {
    Valid(Superblock),
    Empty,
    BadMagic,
    BadChecksum { stored: u32, computed: u32 },
    UnsupportedVersion(u32),
    UnsupportedFeatures(u64),
    Invalid(String),
}

impl SlotStatus {
    pub fn valid(&self) -> Option<&Superblock> {
        match self {
            SlotStatus::Valid(sb) => Some(sb),
            _ => None,
        }
    }
    /// Short description for messages: "valid (commit 7)", "empty", "bad magic",
    /// "bad checksum", "unsupported version 2", "unsupported features 0x..", "invalid: <reason>".
    pub fn describe(&self) -> String {
        match self {
            SlotStatus::Valid(sb) => format!("valid (commit {})", sb.commit_seq.0),
            SlotStatus::Empty => "empty".into(),
            SlotStatus::BadMagic => "bad magic".into(),
            SlotStatus::BadChecksum { .. } => "bad checksum".into(),
            SlotStatus::UnsupportedVersion(v) => format!("unsupported version {v}"),
            SlotStatus::UnsupportedFeatures(f) => format!("unsupported features 0x{f:016x}"),
            SlotStatus::Invalid(r) => format!("invalid: {r}"),
        }
    }
}

// CONTRACT
/// Outcome of [`choose_slot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SlotChoice {
    /// Slot index (0/1) holding the winning superblock.
    pub slot: usize,
    pub superblock: Superblock,
    /// The other slot also held a valid (older) superblock.
    pub other_valid: bool,
}

impl SlotChoice {
    /// Slot the NEXT commit must write.
    pub fn next_slot(&self) -> usize {
        1 - self.slot
    }
}

// CONTRACT
/// Pick the authoritative superblock from both slots (STORE-02 tie rules). `path` is only for
/// error messages.
///
/// Rules, in order:
/// 1. Any slot `UnsupportedVersion(v)` → `error::unsupported_version(path, v, FORMAT_VERSION)`;
///    any `UnsupportedFeatures(f)` → `error::unsupported_features(path, f)`. Fail closed even if
///    the other slot is valid (a newer build may have committed there).
/// 2. No `Valid` slot: if neither slot has the magic (both `Empty`/`BadMagic`) →
///    `error::not_a_database(path, "no valid superblock")`; else
///    `error::corrupt(format!("could not find a valid superblock in database file \"{path}\""))`
///    with detail `slot 0: <describe>; slot 1: <describe>`.
/// 3. Exactly one valid → it wins, `other_valid = false`.
/// 4. Both valid: if `db_uuid`, `page_size` or `created_unix_ms` differ →
///    `error::corrupt("superblock slots of database file \"<path>\" disagree on database identity")`.
///    Higher `commit_seq` wins. Equal `commit_seq`: if the two superblocks are identical, slot 0
///    wins; otherwise corrupt `superblock slots of database file "<path>" have the same commit
///    sequence <n> but different contents`.
///
/// Example: slot0 = Valid(seq 4), slot1 = Valid(seq 5) → `SlotChoice { slot: 1, .. }`,
/// `next_slot() == 0`.
#[allow(unused_variables)]
pub fn choose_slot(path: &Path, slots: &[SlotStatus; 2]) -> Result<SlotChoice> {
    Err(PgError::unimplemented("STO-01", "superblock slot selection"))
}

// CONTRACT
/// A fresh random RFC 4122 version-4 database UUID. Uses OS randomness (`getrandom(2)` via libc
/// on Linux/Android), mixed with std's randomly seeded `RandomState`, the time and the pid, so it
/// also works on targets without libc randomness. Uniqueness, not secrecy, is the requirement.
pub fn random_db_uuid() -> DatabaseUuid {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = [0u8; 16];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: buffer is valid for 16 writable bytes; flags 0 = blocking urandom pool.
        let _ = unsafe { libc::getrandom(out.as_mut_ptr() as *mut libc::c_void, out.len(), 0) };
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for half in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(now);
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        h.write_usize(half);
        let mix = h.finish().to_le_bytes();
        for i in 0..8 {
            out[half * 8 + i] ^= mix[i];
        }
    }
    out[6] = (out[6] & 0x0f) | 0x40; // version 4
    out[8] = (out[8] & 0x3f) | 0x80; // RFC 4122 variant
    DatabaseUuid(out)
}

/// Current wall-clock time in Unix milliseconds (0 if the clock is before 1970).
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uuid_is_v4_and_unique() {
        let a = random_db_uuid();
        let b = random_db_uuid();
        assert_ne!(a, b);
        assert_eq!(a.0[6] >> 4, 4);
        assert_eq!(a.0[8] >> 6, 0b10);
    }
    // STO-01 adds: encode/decode roundtrip; every SlotStatus variant reachable from crafted
    // bytes; torn slot (bytes 40..92 zeroed) → BadChecksum; choose_slot rules 1-4 incl. ties.
}
