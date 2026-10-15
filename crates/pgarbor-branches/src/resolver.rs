//! `BranchRootResolver`: tells storage which tree roots the directory keeps alive (amendment
//! pgarbor-storage-01). Lease BR-05.
//!
//! Used by `Container::compact`, `Container::verify`, `Container::space_report`,
//! `ExportBuilder::copy_directory` and `gc::collect_roots`. A wrong resolver silently destroys
//! branch data during compaction, so this file is small, total and heavily tested.
use crate::error;
use crate::keys::{classify, DirKey};
use crate::record::BranchRecord;
use pgarbor_common::{BranchId, PageId, PgError, Result};
use pgarbor_storage::{NamedRoot, RootResolver};

// CONTRACT
/// Resolver for the v1 directory format.
#[derive(Clone, Copy, Debug, Default)]
pub struct BranchRootResolver;

/// Decode a `B` value and verify that the record's id matches the id in its key.
fn decode_checked(key_id: &BranchId, value: &[u8]) -> Result<BranchRecord> {
    let record = BranchRecord::decode(value)?;
    if record.id != *key_id {
        return Err(error::directory_corrupt(format!("branch record {key_id} has id {}", record.id)));
    }
    Ok(record)
}

/// Apply `map` to one live root. `NONE` is never passed to `map`; a mapping that yields `NONE`
/// for a live root would silently drop data, so it is reported as an internal error.
fn remap_root(root: &mut PageId, map: &mut dyn FnMut(PageId) -> Result<PageId>) -> Result<()> {
    if root.is_none() {
        return Ok(());
    }
    let new = map(*root)?;
    if new.is_none() {
        return Err(PgError::internal(format!("root remap of page {} produced NONE", root.0)));
    }
    *root = new;
    Ok(())
}

impl RootResolver for BranchRootResolver {
    /// For `B` keys (`keys::classify == DirKey::Branch`): decode the record (`BranchRecord::decode`,
    /// errors propagate as XX001) and append `record.live_roots()` as `NamedRoot{label, root}` in
    /// that order. A `B` key whose record id differs from the key id → XX001 (same message as
    /// `directory`). `N`, `M` and unknown keys embed nothing (`Ok(())`).
    fn roots(&self, key: &[u8], value: &[u8], out: &mut Vec<NamedRoot>) -> Result<()> {
        if let DirKey::Branch(key_id) = classify(key) {
            let record = decode_checked(&key_id, value)?;
            for (label, root) in record.live_roots() {
                out.push(NamedRoot { label, root });
            }
        }
        Ok(())
    }

    /// For `B` keys: decode, replace `data_root`, every `extra_databases[i].root` and every
    /// `snapshots[i].root` that is not NONE with `map(root)` — but ONLY for records whose roots are
    /// live (Active, or Deleted with `roots_retained`); a dropped tombstone embeds no roots →
    /// `Ok(None)` (copied unchanged). Re-encode and return `Some(bytes)`. Other keys → `Ok(None)`.
    /// `map` must be called exactly once per live non-NONE root, in `live_roots()` order (the
    /// copier memoizes, so shared roots map to the same new id).
    fn remap(
        &self,
        key: &[u8],
        value: &[u8],
        map: &mut dyn FnMut(PageId) -> Result<PageId>,
    ) -> Result<Option<Vec<u8>>> {
        let key_id = match classify(key) {
            DirKey::Branch(id) => id,
            _ => return Ok(None),
        };
        let mut record = decode_checked(&key_id, value)?;
        if !record.is_active() && !record.roots_retained {
            return Ok(None);
        }
        // Same order as `live_roots()`: data tree, extra databases, snapshots.
        remap_root(&mut record.data_root, map)?;
        for d in record.extra_databases.iter_mut() {
            remap_root(&mut d.root, map)?;
        }
        for s in record.snapshots.iter_mut() {
            remap_root(&mut s.root, map)?;
        }
        record.encode().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{branch_key, meta_key, name_key};
    use crate::record::{BranchState, NamedSnapshot, SnapshotKind};
    use pgarbor_common::CommitSeq;

    fn snap(label: &str, root: u64) -> NamedSnapshot {
        NamedSnapshot {
            label: label.to_string(),
            root: PageId(root),
            created_seq: CommitSeq(1),
            created_unix_ms: 1,
            kind: SnapshotKind::User,
        }
    }

    fn active(id: BranchId, name: &str, data_root: u64, snaps: Vec<NamedSnapshot>) -> BranchRecord {
        let mut r = BranchRecord::new_active(id, name, None, PageId(data_root), CommitSeq(1), 1);
        r.snapshots = snaps;
        r
    }

    fn deleted(id: BranchId, name: &str, retained: bool, data_root: u64, snaps: Vec<NamedSnapshot>) -> BranchRecord {
        let mut r = active(id, name, data_root, snaps);
        r.state = BranchState::Deleted;
        r.roots_retained = retained;
        r.deleted_at_seq = CommitSeq(9);
        r.deleted_unix_ms = 9;
        r
    }

    fn roots_of(key: &[u8], value: &[u8]) -> Result<Vec<NamedRoot>> {
        let mut out = Vec::new();
        BranchRootResolver.roots(key, value, &mut out)?;
        Ok(out)
    }

    #[test]
    fn active_roots_skip_none() {
        let id = BranchId([1; 16]);
        let rec = active(id, "main", 5, vec![snap("s1", 7), snap("s2", 0)]);
        let v = rec.encode().unwrap();
        let roots = roots_of(&branch_key(&id), &v).unwrap();
        let labels: Vec<&str> = roots.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["branch main", "branch main snapshot s1"]);
        assert_eq!(roots[0].root, PageId(5));
        assert_eq!(roots[1].root, PageId(7));
    }

    #[test]
    fn remap_shifts_roots_and_preserves_none() {
        let id = BranchId([2; 16]);
        let rec = active(id, "main", 5, vec![snap("s1", 7), snap("s2", 0)]);
        let v = rec.encode().unwrap();
        let mut calls = Vec::new();
        let mut f = |p: PageId| -> Result<PageId> {
            calls.push(p.0);
            Ok(PageId(p.0 + 100))
        };
        let out = BranchRootResolver.remap(&branch_key(&id), &v, &mut f).unwrap().expect("record remapped");
        assert_eq!(calls, vec![5, 7]);
        let back = BranchRecord::decode(&out).unwrap();
        assert_eq!(back.data_root, PageId(105));
        assert_eq!(back.snapshots[0].root, PageId(107));
        assert_eq!(back.snapshots[1].root, PageId::NONE);
        assert_eq!(back.name, "main");
        assert_eq!(back.id, id);
    }

    #[test]
    fn retained_tombstone_uses_deleted_prefix() {
        let id = BranchId([3; 16]);
        let rec = deleted(id, "old", true, 11, vec![snap("keep", 12)]);
        let v = rec.encode().unwrap();
        let roots = roots_of(&branch_key(&id), &v).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| r.label.starts_with("deleted branch old (")));
        assert!(roots[1].label.ends_with("snapshot keep"));
    }

    #[test]
    fn dropped_tombstone_embeds_nothing() {
        let id = BranchId([4; 16]);
        let rec = deleted(id, "gone", false, 0, Vec::new());
        let v = rec.encode().unwrap();
        assert!(roots_of(&branch_key(&id), &v).unwrap().is_empty());
        let mut f = |_: PageId| -> Result<PageId> { panic!("map must not be called") };
        assert_eq!(BranchRootResolver.remap(&branch_key(&id), &v, &mut f).unwrap(), None);
    }

    #[test]
    fn non_branch_keys_embed_nothing() {
        let id = BranchId([5; 16]);
        let rec = active(id, "main", 5, Vec::new());
        let v = rec.encode().unwrap();
        assert!(roots_of(&name_key("main"), &id.0).unwrap().is_empty());
        assert!(roots_of(&meta_key("format"), b"1").unwrap().is_empty());
        assert!(roots_of(b"Zunknown", &v).unwrap().is_empty());
        let mut f = |p: PageId| -> Result<PageId> { Ok(p) };
        assert_eq!(BranchRootResolver.remap(&name_key("main"), &id.0, &mut f).unwrap(), None);
        assert_eq!(BranchRootResolver.remap(&meta_key("format"), b"1", &mut f).unwrap(), None);
        assert_eq!(BranchRootResolver.remap(b"Zunknown", &v, &mut f).unwrap(), None);
    }

    #[test]
    fn garbage_branch_value_is_xx001() {
        let id = BranchId([6; 16]);
        let key = branch_key(&id);
        let e = roots_of(&key, b"garbage").unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        let mut f = |p: PageId| -> Result<PageId> { Ok(p) };
        let e = BranchRootResolver.remap(&key, b"garbage", &mut f).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
    }

    #[test]
    fn id_mismatch_is_xx001() {
        let key_id = BranchId([7; 16]);
        let rec_id = BranchId([8; 16]);
        let v = active(rec_id, "main", 5, Vec::new()).encode().unwrap();
        let e = roots_of(&branch_key(&key_id), &v).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert!(e.message.contains(&format!("branch record {key_id} has id {rec_id}")));
    }
}
