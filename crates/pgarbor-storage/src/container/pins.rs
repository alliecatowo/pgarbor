//! In-process snapshot pin table (STORE-13). CONTRACT. Fully implemented.
//!
//! Every live `Snapshot` holds a `PinGuard` for its `CommitSeq`. In format v1 (append-only, pages
//! never overwritten in place, compaction writes a new file while old snapshots keep the old
//! `Pager` alive) pins are NOT needed for safety; they exist for introspection (`doctor`, "oldest
//! active reader") and as the hook a future in-place free-space reuse (STORE-15 phase 2) must
//! consult: a page freed at commit `c` may only be reused once `oldest() > c`.
//! Pins are process-local because a container file has exactly one owning process (STORE-12).
use pgarbor_common::CommitSeq;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct PinInner {
    counts: Mutex<BTreeMap<u64, usize>>,
}

// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct PinTable {
    inner: Arc<PinInner>,
}

// CONTRACT
/// Releases its pin on drop.
#[derive(Debug)]
pub struct PinGuard {
    inner: Arc<PinInner>,
    seq: u64,
}

impl PinGuard {
    pub fn commit_seq(&self) -> CommitSeq {
        CommitSeq(self.seq)
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut m = self.inner.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = m.get_mut(&self.seq) {
            *c -= 1;
            if *c == 0 {
                m.remove(&self.seq);
            }
        }
    }
}

impl PinTable {
    pub fn new() -> PinTable {
        PinTable::default()
    }
    pub fn pin(&self, seq: CommitSeq) -> PinGuard {
        let mut m = self.inner.counts.lock().unwrap_or_else(|e| e.into_inner());
        *m.entry(seq.0).or_insert(0) += 1;
        PinGuard { inner: self.inner.clone(), seq: seq.0 }
    }
    /// Oldest pinned commit, if any reader is active.
    pub fn oldest(&self) -> Option<CommitSeq> {
        let m = self.inner.counts.lock().unwrap_or_else(|e| e.into_inner());
        m.keys().next().map(|s| CommitSeq(*s))
    }
    /// Total live pins.
    pub fn active(&self) -> usize {
        let m = self.inner.counts.lock().unwrap_or_else(|e| e.into_inner());
        m.values().sum()
    }
    /// `(commit_seq, count)` ascending.
    pub fn list(&self) -> Vec<(CommitSeq, usize)> {
        let m = self.inner.counts.lock().unwrap_or_else(|e| e.into_inner());
        m.iter().map(|(s, c)| (CommitSeq(*s), *c)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pins_count() {
        let t = PinTable::new();
        let a = t.pin(CommitSeq(3));
        let b = t.pin(CommitSeq(5));
        let c = t.pin(CommitSeq(3));
        assert_eq!(t.oldest(), Some(CommitSeq(3)));
        assert_eq!(t.active(), 3);
        drop(a);
        drop(c);
        assert_eq!(t.oldest(), Some(CommitSeq(5)));
        drop(b);
        assert_eq!(t.oldest(), None);
    }
}
