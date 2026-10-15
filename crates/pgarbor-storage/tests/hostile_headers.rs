use std::{path::Path, sync::Arc};
use pgarbor_common::{CommitSeq, DatabaseUuid};
use pgarbor_storage::{Container, MemVfs, OpenOptions, Superblock};
use pgarbor_storage::format::consts::max_page_count;

fn header(field: usize, value: u64) -> Vec<u8> {
    let mut bytes = Superblock::new_empty(8192, DatabaseUuid([7;16]), 0).encode().unwrap();
    bytes[field..field+8].copy_from_slice(&value.to_le_bytes());
    let crc = crc32c::crc32c(&bytes[..88]);
    bytes[88..92].copy_from_slice(&crc.to_le_bytes());
    bytes.resize(8192, 0);
    bytes
}

#[test]
fn checksummed_hostile_page_counts_rejected_through_open() {
    for pages in [1 << 51, u64::MAX, max_page_count(8192), max_page_count(8192)+1] {
        let vfs = Arc::new(MemVfs::new());
        vfs.write_bytes(Path::new("bad.pga"), header(64, pages));
        let e = Container::open_with(vfs, Path::new("bad.pga"), &OpenOptions::default()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.detail.unwrap_or_default().contains("page count exceeds"));
    }
}

#[test]
fn max_commit_sequence_rejected_through_open() {
    let vfs = Arc::new(MemVfs::new());
    vfs.write_bytes(Path::new("bad.pga"), header(48, u64::MAX));
    let e = Container::open_with(vfs, Path::new("bad.pga"), &Default::default()).unwrap_err();
    assert_eq!(e.sqlstate.code(), "XX001");
    assert!(e.detail.unwrap_or_default().contains("commit sequence exhausted"));
}

#[test]
fn encode_refuses_unaddressable_counts_and_sequence() {
    let mut sb = Superblock::new_empty(8192, DatabaseUuid([7;16]), 0);
    sb.page_count = 1 << 51;
    assert_eq!(sb.encode().unwrap_err().sqlstate.code(), "XX001");
    sb.page_count = 0;
    sb.commit_seq = CommitSeq(u64::MAX);
    assert_eq!(sb.encode().unwrap_err().sqlstate.code(), "XX001");
}

#[test]
fn last_accepted_sequence_is_readable_but_cannot_advance() {
    let vfs = Arc::new(MemVfs::new());
    vfs.write_bytes(Path::new("last.pga"), header(48, u64::MAX-1));
    let c = Container::open_with(vfs, Path::new("last.pga"), &Default::default()).unwrap();
    assert_eq!(c.begin_write().unwrap_err().sqlstate.code(), "XX001");
    assert_eq!(c.compact(&pgarbor_storage::NoNestedRoots, &Default::default()).unwrap_err().sqlstate.code(), "XX001");
}

#[test]
fn representable_large_count_is_truncation_error_and_valid_slot_still_recovers() {
    let vfs = Arc::new(MemVfs::new());
    vfs.write_bytes(Path::new("bad.pga"), header(64, max_page_count(8192)-1));
    let e = Container::open_with(vfs.clone(), Path::new("bad.pga"), &Default::default()).unwrap_err();
    assert!(e.message.contains("is truncated"));
    let mut bytes = header(64, 1 << 51);
    let good = Superblock::new_empty(8192, DatabaseUuid([7;16]), 0).encode().unwrap();
    bytes[4096..8192].copy_from_slice(&good);
    vfs.write_bytes(Path::new("bad.pga"), bytes);
    assert_eq!(Container::open_with(vfs, Path::new("bad.pga"), &Default::default()).unwrap().commit_seq(), CommitSeq(1));
}
