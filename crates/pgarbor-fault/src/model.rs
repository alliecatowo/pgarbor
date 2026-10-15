//! Reference model of committed state per named root. Lease FLT-05.
//!
//! Workloads use the `PrefixU64Roots` directory convention: directory key
//! `ROOT_PREFIX + name` → 8-byte LE PageId of that root's data tree. The model tracks, per root
//! name, the expected key/value contents.
//!
//! Oracle invariant (CONTRACT, RFC 001 §11.2): after any crash and successful recovery the
//! container's state equals EITHER the last acknowledged commit (`acked`) OR the in-flight commit
//! whose `commit()` had not returned Ok (`inflight`) — nothing else, never a mix. Without a crash,
//! the live state always equals `acked` (or `inflight` when its outcome was ambiguous and the
//! container got poisoned). Recovered commit_seq >= last acked commit_seq.
use pgarbor_common::{CommitSeq, PgError, Result};
use pgarbor_storage::Container;
use std::collections::BTreeMap;

// CONTRACT
pub type TreeState = BTreeMap<Vec<u8>, Vec<u8>>;
// CONTRACT
pub type RootsState = BTreeMap<String, TreeState>;

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    MatchesAcked,
    MatchesInflight,
    /// Invariant violated; message names the first differing root/key (hex keys) and both seqs.
    Violation(String),
}

impl Verdict {
    pub fn ok(&self) -> bool {
        !matches!(self, Verdict::Violation(_))
    }
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefModel {
    pub acked: RootsState,
    pub acked_seq: CommitSeq,
    /// Commit attempted but not acknowledged and possibly durable.
    pub inflight: Option<RootsState>,
    /// The open transaction's view (starts as a copy of `acked` at `begin`).
    pub working: RootsState,
    pub in_txn: bool,
}

impl RefModel {
    pub fn new() -> RefModel {
        RefModel::default()
    }
    /// Start a txn: `working = acked.clone()`, `in_txn = true` (no-op if already in a txn).
    pub fn begin(&mut self) {
        if !self.in_txn {
            self.working = self.acked.clone();
            self.in_txn = true;
        }
    }
    pub fn put(&mut self, root: &str, key: &[u8], value: &[u8]) {
        self.begin();
        self.working.entry(root.to_string()).or_default().insert(key.to_vec(), value.to_vec());
    }
    pub fn delete(&mut self, root: &str, key: &[u8]) {
        self.begin();
        if let Some(t) = self.working.get_mut(root) {
            t.remove(key);
        }
    }
    /// New root (empty, or a copy of `from` = O(1) branch). Existing name → replaced.
    pub fn create_root(&mut self, name: &str, from: Option<&str>) {
        self.begin();
        let t = from.and_then(|f| self.working.get(f).cloned()).unwrap_or_default();
        self.working.insert(name.to_string(), t);
    }
    pub fn drop_root(&mut self, name: &str) {
        self.begin();
        self.working.remove(name);
    }
    pub fn abort(&mut self) {
        self.working = self.acked.clone();
        self.in_txn = false;
    }
    /// Commit about to run: `inflight = Some(working)`.
    pub fn commit_started(&mut self) {
        self.inflight = Some(self.working.clone());
    }
    /// Commit returned Ok(seq): `acked = inflight.take()`, `acked_seq = seq`, txn closed.
    pub fn commit_acked(&mut self, seq: CommitSeq) {
        if let Some(s) = self.inflight.take() {
            self.acked = s;
        }
        self.acked_seq = seq;
        self.in_txn = false;
        self.working = self.acked.clone();
    }
    /// Commit returned Err. `maybe_durable` = the container reports poisoned (superblock write
    /// may have landed) → keep `inflight`; else the commit definitely did not happen → drop it.
    pub fn commit_failed(&mut self, maybe_durable: bool) {
        if !maybe_durable {
            self.inflight = None;
        }
        self.in_txn = false;
        self.working = self.acked.clone();
    }

    /// Compare a recovered state. MatchesAcked if `actual == acked` (checked first), else
    /// MatchesInflight if equal to `inflight`, else Violation (via `diff_summary` against acked).
    /// Also a Violation if `actual_seq < acked_seq`. On a match: adopt it as the new `acked`
    /// (`acked_seq = actual_seq`), clear `inflight`, close any txn.
    #[allow(unused_variables)]
    pub fn check_recovered(&mut self, actual: &RootsState, actual_seq: CommitSeq) -> Result<Verdict> {
        Err(PgError::unimplemented("FLT-05", "recovered state check"))
    }
}

// CONTRACT
/// Human-readable first difference between two states, e.g.
/// `root "main": key 6b31 expected 7631 (2 bytes) found <missing>` or `root "feature" missing`.
/// Values longer than 16 bytes are shown as length + first 8 bytes hex. Empty string if equal.
#[allow(unused_variables)]
pub fn diff_summary(expected: &RootsState, actual: &RootsState) -> Result<String> {
    Err(PgError::unimplemented("FLT-05", "state diff"))
}

// CONTRACT
/// Read the full state of a container: `snapshot()`, scan the directory for keys starting with
/// `prefix`, decode each 8-byte LE root, read the whole tree via `snap.tree(root)?.scan(..)`.
/// Root name = UTF-8 of the key suffix (lossy). Returns (state, snapshot commit_seq).
#[allow(unused_variables)]
pub fn read_state(c: &Container, prefix: &[u8]) -> Result<(RootsState, CommitSeq)> {
    Err(PgError::unimplemented("FLT-05", "container state read"))
}
