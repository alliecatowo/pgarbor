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
    pub fn encode(&self) -> Result<Vec<u8>> {
        // Refuse to write any slot that `decode` would reject.
        if !is_valid_page_size(self.page_size) {
            return Err(PgError::internal(format!("invalid page size {}", self.page_size)));
        }
        if self.commit_seq.0 == 0 {
            return Err(PgError::internal("invalid commit sequence 0"));
        }
        if self.format_version != FORMAT_VERSION {
            return Err(PgError::internal(format!("unsupported format version {}", self.format_version)));
        }
        let unknown_required = self.feature_flags & FEATURE_REQUIRED_MASK & !KNOWN_FEATURES;
        if unknown_required != 0 {
            return Err(PgError::internal(format!("unsupported features 0x{unknown_required:016x}")));
        }
        if !self.freelist_root.is_none() {
            return Err(PgError::internal(format!("freelist root {} must be zero in format v1", self.freelist_root.0)));
        }
        if self.directory_root.0 > self.page_count {
            return Err(PgError::internal(format!(
                "directory root {} beyond page count {}",
                self.directory_root.0, self.page_count
            )));
        }
        let mut out = vec![0u8; SUPERBLOCK_SLOT_SIZE];
        out[0..8].copy_from_slice(&SUPERBLOCK_MAGIC);
        out[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        // 12..16 reserved0 stays zero
        out[16..24].copy_from_slice(&self.feature_flags.to_le_bytes());
        out[24..28].copy_from_slice(&self.page_size.to_le_bytes());
        // 28..32 reserved1 stays zero
        out[32..48].copy_from_slice(&self.db_uuid.0);
        out[48..56].copy_from_slice(&self.commit_seq.0.to_le_bytes());
        out[56..64].copy_from_slice(&self.directory_root.0.to_le_bytes());
        out[64..72].copy_from_slice(&self.page_count.to_le_bytes());
        out[72..80].copy_from_slice(&self.freelist_root.0.to_le_bytes());
        out[80..88].copy_from_slice(&self.created_unix_ms.to_le_bytes());
        let crc = crc32c::crc32c(&out[0..88]);
        out[88..92].copy_from_slice(&crc.to_le_bytes());
        // 92..4096 zero padding
        Ok(out)
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
    pub fn decode(bytes: &[u8]) -> Result<SlotStatus> {
        let b = bytes.get(..SUPERBLOCK_ENCODED_LEN).ok_or_else(|| {
            PgError::internal(format!("superblock slot image too short: {} bytes", bytes.len()))
        })?;
        if b.iter().all(|&x| x == 0) {
            return Ok(SlotStatus::Empty);
        }
        if field::<8>(b, 0)? != SUPERBLOCK_MAGIC {
            return Ok(SlotStatus::BadMagic);
        }
        let stored = u32::from_le_bytes(field::<4>(b, 88)?);
        let computed = crc32c::crc32c(&b[0..88]);
        if stored != computed {
            return Ok(SlotStatus::BadChecksum { stored, computed });
        }
        let format_version = u32::from_le_bytes(field::<4>(b, 8)?);
        if format_version != FORMAT_VERSION {
            return Ok(SlotStatus::UnsupportedVersion(format_version));
        }
        let feature_flags = u64::from_le_bytes(field::<8>(b, 16)?);
        let unknown_required = feature_flags & FEATURE_REQUIRED_MASK & !KNOWN_FEATURES;
        if unknown_required != 0 {
            return Ok(SlotStatus::UnsupportedFeatures(unknown_required));
        }
        let reserved0 = u32::from_le_bytes(field::<4>(b, 12)?);
        let page_size = u32::from_le_bytes(field::<4>(b, 24)?);
        let reserved1 = u32::from_le_bytes(field::<4>(b, 28)?);
        let db_uuid = DatabaseUuid(field::<16>(b, 32)?);
        let commit_seq = u64::from_le_bytes(field::<8>(b, 48)?);
        let directory_root = u64::from_le_bytes(field::<8>(b, 56)?);
        let page_count = u64::from_le_bytes(field::<8>(b, 64)?);
        let freelist_root = u64::from_le_bytes(field::<8>(b, 72)?);
        let created_unix_ms = u64::from_le_bytes(field::<8>(b, 80)?);

        if !is_valid_page_size(page_size) {
            return Ok(SlotStatus::Invalid(format!("invalid page size {page_size}")));
        }
        if commit_seq == 0 {
            return Ok(SlotStatus::Invalid("invalid commit sequence 0".into()));
        }
        if reserved0 != 0 || reserved1 != 0 {
            return Ok(SlotStatus::Invalid("reserved field is nonzero".into()));
        }
        if freelist_root != 0 {
            return Ok(SlotStatus::Invalid(format!("freelist root {freelist_root} must be zero in format v1")));
        }
        if directory_root > page_count {
            return Ok(SlotStatus::Invalid(format!(
                "directory root {directory_root} beyond page count {page_count}"
            )));
        }
        Ok(SlotStatus::Valid(Superblock {
            format_version,
            feature_flags,
            page_size,
            db_uuid,
            commit_seq: CommitSeq(commit_seq),
            directory_root: PageId(directory_root),
            page_count,
            freelist_root: PageId(freelist_root),
            created_unix_ms,
        }))
    }

    /// Committed file length implied by this superblock.
    pub fn committed_len(&self) -> u64 {
        file_len_for(self.page_size, self.page_count)
    }
}

/// Fixed-width little-endian field read at `off`; error (never panic) if out of range.
fn field<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N]> {
    b.get(off..off + N)
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| PgError::internal(format!("superblock field at offset {off} out of range")))
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
pub fn choose_slot(path: &Path, slots: &[SlotStatus; 2]) -> Result<SlotChoice> {
    // Rule 1: fail closed on any slot written by an unknown format or with unknown required features.
    for status in slots {
        match status {
            SlotStatus::UnsupportedVersion(v) => {
                return Err(error::unsupported_version(path, *v, FORMAT_VERSION));
            }
            SlotStatus::UnsupportedFeatures(f) => return Err(error::unsupported_features(path, *f)),
            _ => {}
        }
    }
    match (slots[0].valid(), slots[1].valid()) {
        // Rule 2: no valid superblock at all.
        (None, None) => {
            if slots.iter().all(|s| matches!(s, SlotStatus::Empty | SlotStatus::BadMagic)) {
                Err(error::not_a_database(path, "no valid superblock"))
            } else {
                Err(error::corrupt(format!(
                    "could not find a valid superblock in database file \"{}\"",
                    path.display()
                ))
                .with_detail(format!("slot 0: {}; slot 1: {}", slots[0].describe(), slots[1].describe())))
            }
        }
        // Rule 3: exactly one valid slot wins.
        (Some(a), None) => Ok(SlotChoice { slot: 0, superblock: *a, other_valid: false }),
        (None, Some(b)) => Ok(SlotChoice { slot: 1, superblock: *b, other_valid: false }),
        // Rule 4: both valid.
        (Some(a), Some(b)) => {
            if a.db_uuid != b.db_uuid || a.page_size != b.page_size || a.created_unix_ms != b.created_unix_ms {
                return Err(error::corrupt(format!(
                    "superblock slots of database file \"{}\" disagree on database identity",
                    path.display()
                )));
            }
            let (slot, winner) = if a.commit_seq > b.commit_seq {
                (0, *a)
            } else if b.commit_seq > a.commit_seq {
                (1, *b)
            } else if a == b {
                (0, *a)
            } else {
                return Err(error::corrupt(format!(
                    "superblock slots of database file \"{}\" have the same commit sequence {} but different contents",
                    path.display(),
                    a.commit_seq.0
                )));
            };
            Ok(SlotChoice { slot, superblock: winner, other_valid: true })
        }
    }
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
    use pgarbor_common::SqlState;
    #[test]
    fn uuid_is_v4_and_unique() {
        let a = random_db_uuid();
        let b = random_db_uuid();
        assert_ne!(a, b);
        assert_eq!(a.0[6] >> 4, 4);
        assert_eq!(a.0[8] >> 6, 0b10);
    }

    fn sample() -> Superblock {
        Superblock {
            format_version: FORMAT_VERSION,
            feature_flags: 0,
            page_size: 8192,
            db_uuid: DatabaseUuid([0x5a; 16]),
            commit_seq: CommitSeq(4),
            directory_root: PageId(2),
            page_count: 3,
            freelist_root: PageId::NONE,
            created_unix_ms: 1_700_000_000_000,
        }
    }

    /// Recompute the crc over bytes 0..88 so a patched slot is still "checksum clean".
    fn reseal(b: &mut [u8]) {
        let crc = crc32c::crc32c(&b[0..88]);
        b[88..92].copy_from_slice(&crc.to_le_bytes());
    }

    fn patch(b: &mut [u8], off: usize, bytes: &[u8]) {
        b[off..off + bytes.len()].copy_from_slice(bytes);
    }

    fn valid(sb: Superblock) -> SlotStatus {
        SlotStatus::Valid(sb)
    }

    #[test]
    fn roundtrip() {
        let sb = sample();
        let img = sb.encode().unwrap();
        assert_eq!(img.len(), SUPERBLOCK_SLOT_SIZE);
        assert_eq!(&img[0..8], b"PGARBOR\0");
        assert!(img[92..].iter().all(|&x| x == 0));
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::Valid(sb));
        let fresh = Superblock::new_empty(4096, DatabaseUuid([1; 16]), 0);
        assert_eq!(Superblock::decode(&fresh.encode().unwrap()).unwrap(), valid(fresh));
    }

    #[test]
    fn all_zero_is_empty() {
        assert_eq!(Superblock::decode(&[0u8; SUPERBLOCK_SLOT_SIZE]).unwrap(), SlotStatus::Empty);
    }

    #[test]
    fn short_input_is_internal_error() {
        assert!(Superblock::decode(&[0u8; 91]).is_err());
    }

    #[test]
    fn bad_magic() {
        let mut img = sample().encode().unwrap();
        img[0] = b'X';
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::BadMagic);
    }

    #[test]
    fn bad_checksum_on_flipped_field() {
        let mut img = sample().encode().unwrap();
        img[40] ^= 0x01;
        match Superblock::decode(&img).unwrap() {
            SlotStatus::BadChecksum { stored, computed } => assert_ne!(stored, computed),
            other => panic!("expected BadChecksum, got {other:?}"),
        }
    }

    #[test]
    fn any_flipped_byte_in_header_is_not_valid() {
        for i in 0..SUPERBLOCK_ENCODED_LEN {
            let mut img = sample().encode().unwrap();
            img[i] ^= 0xff;
            let st = Superblock::decode(&img).unwrap();
            assert!(!matches!(st, SlotStatus::Valid(_) | SlotStatus::Empty), "byte {i} gave {st:?}");
        }
    }

    #[test]
    fn torn_slot_is_bad_checksum() {
        let mut img = sample().encode().unwrap();
        for b in &mut img[40..92] {
            *b = 0;
        }
        assert!(matches!(Superblock::decode(&img).unwrap(), SlotStatus::BadChecksum { .. }));
    }

    #[test]
    fn unsupported_version() {
        let mut img = sample().encode().unwrap();
        patch(&mut img, 8, &2u32.to_le_bytes());
        reseal(&mut img);
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::UnsupportedVersion(2));
    }

    #[test]
    fn unsupported_required_feature() {
        let mut img = sample().encode().unwrap();
        patch(&mut img, 16, &(1u64 << 40).to_le_bytes());
        reseal(&mut img);
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::UnsupportedFeatures(1u64 << 40));
    }

    #[test]
    fn unknown_compatible_feature_is_ignored() {
        let mut img = sample().encode().unwrap();
        patch(&mut img, 16, &1u64.to_le_bytes());
        reseal(&mut img);
        let mut expect = sample();
        expect.feature_flags = 1;
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::Valid(expect));
    }

    #[test]
    fn invalid_fields() {
        let mut img = sample().encode().unwrap();
        patch(&mut img, 24, &1000u32.to_le_bytes());
        reseal(&mut img);
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::Invalid("invalid page size 1000".into()));

        let mut img = sample().encode().unwrap();
        patch(&mut img, 48, &0u64.to_le_bytes());
        reseal(&mut img);
        assert_eq!(Superblock::decode(&img).unwrap(), SlotStatus::Invalid("invalid commit sequence 0".into()));

        let mut img = sample().encode().unwrap();
        patch(&mut img, 56, &9u64.to_le_bytes());
        reseal(&mut img);
        assert_eq!(
            Superblock::decode(&img).unwrap(),
            SlotStatus::Invalid("directory root 9 beyond page count 3".into())
        );

        let mut img = sample().encode().unwrap();
        patch(&mut img, 12, &1u32.to_le_bytes());
        reseal(&mut img);
        assert!(matches!(Superblock::decode(&img).unwrap(), SlotStatus::Invalid(_)));
    }

    #[test]
    fn encode_refuses_invalid_superblocks() {
        let mut sb = sample();
        sb.page_size = 1000;
        assert!(sb.encode().is_err());
        let mut sb = sample();
        sb.commit_seq = CommitSeq(0);
        assert!(sb.encode().is_err());
    }

    #[test]
    fn choose_slot_rules() {
        let p = Path::new("t.pga");
        let mut newer = sample();
        newer.commit_seq = CommitSeq(5);
        let mut older = sample();
        older.commit_seq = CommitSeq(4);
        let mut other_db = sample();
        other_db.db_uuid = DatabaseUuid([9; 16]);
        other_db.commit_seq = CommitSeq(6);
        let bad = SlotStatus::BadChecksum { stored: 1, computed: 2 };

        let c = choose_slot(p, &[valid(older), valid(newer)]).unwrap();
        assert_eq!(c.slot, 1);
        assert_eq!(c.next_slot(), 0);
        assert!(c.other_valid);

        let c = choose_slot(p, &[valid(newer), SlotStatus::Empty]).unwrap();
        assert_eq!((c.slot, c.other_valid), (0, false));

        let c = choose_slot(p, &[bad.clone(), valid(sample())]).unwrap();
        assert_eq!(c.slot, 1);
        assert!(!c.other_valid);

        let e = choose_slot(p, &[SlotStatus::Empty, SlotStatus::Empty]).unwrap_err();
        assert!(e.message.contains("is not a PGArbor database"));
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);

        let e = choose_slot(p, &[bad.clone(), bad.clone()]).unwrap_err();
        assert!(e.message.contains("could not find a valid superblock"));
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);

        let e = choose_slot(p, &[valid(sample()), SlotStatus::UnsupportedVersion(2)]).unwrap_err();
        assert!(e.message.contains("unsupported format version 2"));
        let e = choose_slot(p, &[SlotStatus::UnsupportedVersion(2), valid(sample())]).unwrap_err();
        assert!(e.message.contains("unsupported format version 2"));

        let e = choose_slot(p, &[valid(sample()), SlotStatus::UnsupportedFeatures(1 << 40)]).unwrap_err();
        assert!(e.message.contains("requires unsupported features"));

        let e = choose_slot(p, &[valid(older), valid(other_db)]).unwrap_err();
        assert!(e.message.contains("disagree on database identity"));

        let c = choose_slot(p, &[valid(sample()), valid(sample())]).unwrap();
        assert_eq!(c.slot, 0);
        assert!(c.other_valid);

        let mut differ = sample();
        differ.page_count = 2;
        let e = choose_slot(p, &[valid(sample()), valid(differ)]).unwrap_err();
        assert!(e.message.contains("same commit sequence 4 but different contents"));
    }
}
