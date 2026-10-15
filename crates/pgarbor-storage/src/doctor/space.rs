//! Space accounting and page inspection (STORE-15 accounting, STORE-19). Lease STO-19.
use crate::btree::node::{internal_span_key, internal_spans, leaf_spans, span_key};
use crate::container::snapshot::Snapshot;
use crate::doctor::report::{PageInspection, SpaceReport};
use crate::error;
use crate::format::consts::{PageKind, PAGES_OFFSET};
use crate::format::page::{decode_page, PageHeader};
use crate::gc::trace::{collect_roots, trace};
use crate::pager::PageSource;
use crate::roots::RootResolver;
use pgarbor_common::{PageId, PgError, Result};

// CONTRACT
/// Build a `SpaceReport` for `snap`: `collect_roots` (directory + resolver roots), `trace(..,
/// per_root = true)`, file length from `snap.pager().file_len()`, arithmetic per the field docs
/// of `SpaceReport` (all subtraction saturating). `pins` is left empty here (`Container` callers
/// may fill it from `Container::pins().list()`).
///
/// Example: fresh container after one commit writing a 1-leaf data tree and a 1-leaf directory:
/// page_count 2, reachable 2, garbage 0, roots [directory (1 page, exclusive 1), data (1, 1)].
#[allow(unused_variables)]
pub fn space_report(snap: &Snapshot, resolver: &dyn RootResolver) -> Result<SpaceReport> {
    let page_size = snap.page_size();
    let page_size_u64 = u64::from(page_size);
    let page_count = snap.page_count();

    let roots = collect_roots(snap, snap.directory_root(), resolver)?;
    let traced = trace(snap, page_count, &roots, true)?;
    let file_bytes = snap.pager().file_len()?;

    let committed_bytes = PAGES_OFFSET.saturating_add(page_count.saturating_mul(page_size_u64));
    let tail_bytes = file_bytes.saturating_sub(committed_bytes);

    let reachable_pages = traced.reachable.len();
    let reachable_bytes = reachable_pages.saturating_mul(page_size_u64);
    let garbage_pages = page_count.saturating_sub(reachable_pages);
    let garbage_bytes = garbage_pages.saturating_mul(page_size_u64);
    let reclaimable_bytes = file_bytes.saturating_sub(PAGES_OFFSET.saturating_add(reachable_bytes));

    Ok(SpaceReport {
        page_size,
        commit_seq: snap.commit_seq(),
        file_bytes,
        superblock_bytes: PAGES_OFFSET,
        page_count,
        committed_bytes,
        tail_bytes,
        reachable_pages,
        reachable_bytes,
        garbage_pages,
        garbage_bytes,
        reclaimable_bytes,
        kinds: traced.kinds,
        roots: traced.roots,
        pins: Vec::new(),
    })
}

// CONTRACT
/// Inspect one page without failing on damage: `id` outside `1..=page_count` → Err
/// `error::page_out_of_range`; otherwise read raw bytes (`pager.read_raw`), parse the header
/// (`PageHeader::parse`), try `decode_page`; on failure fill `error` with the message and return
/// Ok. For valid node pages fill level/entries/first/last key hex (first 64 bytes of the key,
/// lowercase hex) and children; for overflow pages `children = [next]` when next != 0.
#[allow(unused_variables)]
pub fn inspect_page(snap: &Snapshot, id: PageId) -> Result<PageInspection> {
    let page_count = snap.page_count();
    if id.is_none() || id.0 > page_count {
        return Err(error::page_out_of_range(id, page_count));
    }
    let raw = snap.pager().read_raw(id)?;
    Ok(inspect_raw(snap.page_size(), id, &raw))
}

/// Core of [`inspect_page`]: never fails, reports damage in `error`.
fn inspect_raw(page_size: u32, id: PageId, raw: &[u8]) -> PageInspection {
    let mut out = PageInspection {
        id,
        header: None,
        error: None,
        kind: None,
        level: None,
        entries: None,
        first_key_hex: None,
        last_key_hex: None,
        children: Vec::new(),
        payload_len: None,
    };

    match PageHeader::parse(raw) {
        Ok(h) => {
            out.kind = PageKind::from_u8(h.kind).map(|k| k.name().to_string());
            out.payload_len = Some(h.payload_len);
            out.header = Some(h);
        }
        Err(e) => {
            out.error = Some(message_of(&e));
            return out;
        }
    }

    let page = match decode_page(page_size, id, raw) {
        Ok(p) => p,
        Err(e) => {
            out.error = Some(message_of(&e));
            return out;
        }
    };
    out.kind = Some(page.kind.name().to_string());
    out.payload_len = Some(page.payload.len() as u32);

    match page.kind {
        PageKind::Leaf => match leaf_spans(id, &page.payload) {
            Ok(spans) => {
                out.level = Some(0);
                out.entries = Some(spans.len());
                if let (Some(first), Some(last)) = (spans.first(), spans.last()) {
                    out.first_key_hex = Some(hex_prefix(span_key(&page.payload, first)));
                    out.last_key_hex = Some(hex_prefix(span_key(&page.payload, last)));
                }
            }
            Err(e) => out.error = Some(message_of(&e)),
        },
        PageKind::Internal => match internal_spans(id, &page.payload) {
            Ok((level, first_child, spans)) => {
                out.level = Some(level);
                out.entries = Some(spans.len());
                if let (Some(first), Some(last)) = (spans.first(), spans.last()) {
                    out.first_key_hex = Some(hex_prefix(internal_span_key(&page.payload, first)));
                    out.last_key_hex = Some(hex_prefix(internal_span_key(&page.payload, last)));
                }
                out.children.reserve(spans.len() + 1);
                out.children.push(first_child);
                out.children.extend(spans.iter().map(|s| s.child));
            }
            Err(e) => out.error = Some(message_of(&e)),
        },
        PageKind::Overflow => {
            if page.payload.len() >= 8 {
                let mut b = [0u8; 8];
                b.copy_from_slice(&page.payload[..8]);
                let next = PageId(u64::from_le_bytes(b));
                if !next.is_none() {
                    out.children.push(next);
                }
            } else {
                out.error = Some(format!(
                    "overflow page payload is {} bytes, expected at least 8",
                    page.payload.len()
                ));
            }
        }
        PageKind::FreeList => {}
    }
    out
}

/// Message for a damage report: the error message, plus its detail when present.
fn message_of(e: &PgError) -> String {
    match &e.detail {
        Some(d) => format!("{}: {}", e.message, d),
        None => e.message.clone(),
    }
}

/// Lowercase hex of the first 64 bytes of `key`.
fn hex_prefix(key: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let n = key.len().min(64);
    let mut s = String::with_capacity(n * 2);
    for &b in &key[..n] {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::options::CreateOptions;
    use crate::container::Container;
    use crate::format::consts::PAGE_HEADER_LEN;
    use crate::format::consts::PAGE_MAGIC;
    use crate::format::page::page_checksum;
    use crate::io::{MemVfs, Vfs};
    use crate::roots::PrefixU64Roots;
    use std::path::Path;
    use std::sync::Arc;

    /// Build a full page image with a valid header and checksum.
    fn raw_page(page_size: usize, id: u64, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut h = [0u8; PAGE_HEADER_LEN];
        h[0..4].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
        h[4] = kind;
        h[8..16].copy_from_slice(&id.to_le_bytes());
        h[16..24].copy_from_slice(&1u64.to_le_bytes());
        h[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let crc = page_checksum(&h, payload);
        h[24..28].copy_from_slice(&crc.to_le_bytes());
        let mut out = vec![0u8; page_size];
        out[..PAGE_HEADER_LEN].copy_from_slice(&h);
        out[PAGE_HEADER_LEN..PAGE_HEADER_LEN + payload.len()].copy_from_slice(payload);
        out
    }

    #[test]
    fn hex_prefix_truncates_to_64_bytes() {
        assert_eq!(hex_prefix(b"\x00\xffab"), "00ff6162");
        assert_eq!(hex_prefix(&[0xabu8; 100]).len(), 128);
    }

    #[test]
    fn inspect_valid_leaf() {
        // One cell: key "k" -> inline value "v" (layout documented in btree::node).
        let payload = [1u8, 0, 0, 0, 6, 0, 1, 0, 0, 1, 0, 0, 0, b'k', b'v'];
        let raw = raw_page(4096, 1, 1, &payload);
        let ins = inspect_raw(4096, PageId(1), &raw);
        assert_eq!(ins.error, None);
        assert_eq!(ins.kind.as_deref(), Some("leaf"));
        assert_eq!(ins.level, Some(0));
        assert_eq!(ins.entries, Some(1));
        assert_eq!(ins.first_key_hex.as_deref(), Some("6b"));
        assert_eq!(ins.last_key_hex.as_deref(), Some("6b"));
        assert!(ins.children.is_empty());
        assert_eq!(ins.payload_len, Some(15));
    }

    #[test]
    fn inspect_corrupt_page_is_ok_with_error() {
        let payload = [1u8, 0, 0, 0, 6, 0, 1, 0, 0, 1, 0, 0, 0, b'k', b'v'];
        let mut raw = raw_page(4096, 1, 1, &payload);
        raw[PAGE_HEADER_LEN + 8] ^= 0xff; // damage the payload; checksum no longer matches
        let ins = inspect_raw(4096, PageId(1), &raw);
        assert!(ins.header.is_some());
        assert!(ins.error.is_some());
    }

    #[test]
    fn inspect_zero_page_reports_error() {
        let raw = vec![0u8; 4096];
        let ins = inspect_raw(4096, PageId(3), &raw);
        assert_eq!(ins.id, PageId(3));
        assert!(ins.error.is_some());
        assert_eq!(ins.kind, None);
    }

    #[test]
    fn inspect_overflow_page_reports_next() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u64.to_le_bytes());
        payload.extend_from_slice(b"chunk");
        let raw = raw_page(4096, 2, 3, &payload);
        let ins = inspect_raw(4096, PageId(2), &raw);
        assert_eq!(ins.error, None);
        assert_eq!(ins.kind.as_deref(), Some("overflow"));
        assert_eq!(ins.children, vec![PageId(5)]);
    }

    #[test]
    fn space_report_layout_and_overwrite_garbage() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let c = Container::create_with(vfs, Path::new("space.pga"), &CreateOptions::default()).unwrap();
        let resolver = PrefixU64Roots::new(b"R");

        let fresh = space_report(&c.snapshot(), &resolver).unwrap();
        assert_eq!(fresh.page_count, 0);
        assert_eq!(fresh.file_bytes, PAGES_OFFSET);
        assert_eq!(fresh.reclaimable_bytes, 0);
        assert_eq!(fresh.garbage_pages, 0);

        let mut tx = c.begin_write().unwrap();
        let data = tx.put(PageId::NONE, b"k", b"v").unwrap().root;
        let droot = tx.directory_root();
        let dir = tx.put(droot, b"Rmain", &data.0.to_le_bytes()).unwrap().root;
        tx.set_directory_root(dir);
        tx.commit().unwrap();

        let r1 = space_report(&c.snapshot(), &resolver).unwrap();
        assert_eq!(r1.page_count, 2);
        assert_eq!(r1.reachable_pages, 2);
        assert_eq!(r1.garbage_pages, 0);
        assert_eq!(r1.roots.len(), 2);
        assert_eq!(r1.roots[0].label, "directory");
        assert_eq!(r1.roots[0].exclusive_pages, 1);
        assert_eq!(r1.roots[1].label, "main");
        assert_eq!(r1.roots[1].exclusive_pages, 1);

        // Overwrite the data leaf: the old leaf and the old directory page become garbage.
        let mut tx = c.begin_write().unwrap();
        let droot = tx.directory_root();
        let data2 = tx.put(data, b"k", b"w").unwrap().root;
        let dir2 = tx.put(droot, b"Rmain", &data2.0.to_le_bytes()).unwrap().root;
        tx.set_directory_root(dir2);
        tx.commit().unwrap();

        let r2 = space_report(&c.snapshot(), &resolver).unwrap();
        assert!(r2.garbage_pages > 0);
        assert_eq!(r2.reachable_pages + r2.garbage_pages, r2.page_count);
    }
}
