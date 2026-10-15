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
use pgarbor_common::{CommitSeq, KvRead, PageId, PgError, Result};
use pgarbor_storage::Container;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

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
    pub fn check_recovered(&mut self, actual: &RootsState, actual_seq: CommitSeq) -> Result<Verdict> {
        let matched = if actual == &self.acked {
            Some(Verdict::MatchesAcked)
        } else if self.inflight.as_ref() == Some(actual) {
            Some(Verdict::MatchesInflight)
        } else {
            None
        };
        let verdict = match matched {
            None => {
                let diff = diff_summary(&self.acked, actual)?;
                return Ok(Verdict::Violation(format!(
                    "recovered state matches neither acked nor inflight: {diff}; recovered commit_seq {}, acked commit_seq {}",
                    actual_seq.0, self.acked_seq.0
                )));
            }
            Some(_) if actual_seq < self.acked_seq => {
                return Ok(Verdict::Violation(format!(
                    "recovered commit_seq {} is below acked commit_seq {}",
                    actual_seq.0, self.acked_seq.0
                )));
            }
            Some(v) => v,
        };
        self.acked = actual.clone();
        self.acked_seq = actual_seq;
        self.inflight = None;
        self.in_txn = false;
        self.working = self.acked.clone();
        Ok(verdict)
    }
}

// CONTRACT
/// Human-readable first difference between two states, e.g.
/// `root "main": key 6b31 expected 7631 (2 bytes) found <missing>` or `root "feature" missing`.
/// Values longer than 16 bytes are shown as length + first 8 bytes hex. Empty string if equal.
pub fn diff_summary(expected: &RootsState, actual: &RootsState) -> Result<String> {
    let names: BTreeSet<&String> = expected.keys().chain(actual.keys()).collect();
    for name in names {
        match (expected.get(name), actual.get(name)) {
            (Some(_), None) => return Ok(format!("root {name:?} missing")),
            (None, Some(_)) => return Ok(format!("root {name:?} unexpected")),
            (Some(e), Some(a)) => {
                if let Some(msg) = diff_tree(name, e, a) {
                    return Ok(msg);
                }
            }
            (None, None) => {}
        }
    }
    Ok(String::new())
}

/// First differing key of one root, in bytewise key order.
fn diff_tree(name: &str, expected: &TreeState, actual: &TreeState) -> Option<String> {
    let keys: BTreeSet<&Vec<u8>> = expected.keys().chain(actual.keys()).collect();
    for key in keys {
        let (ev, av) = (expected.get(key.as_slice()), actual.get(key.as_slice()));
        if ev != av {
            return Some(format!(
                "root {name:?}: key {} expected {} found {}",
                hex(key),
                show_value(ev),
                show_value(av)
            ));
        }
    }
    None
}

fn show_value(v: Option<&Vec<u8>>) -> String {
    match v {
        None => "<missing>".to_string(),
        Some(v) if v.is_empty() => "<empty> (0 bytes)".to_string(),
        Some(v) if v.len() <= 16 => format!("{} ({} bytes)", hex(v), v.len()),
        Some(v) => format!("{}... ({} bytes)", hex(&v[..8]), v.len()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// CONTRACT
/// Read the full state of a container: `snapshot()`, scan the directory for keys starting with
/// `prefix`, decode each 8-byte LE root, read the whole tree via `snap.tree(root)?.scan(..)`.
/// Root name = UTF-8 of the key suffix (lossy). Returns (state, snapshot commit_seq).
pub fn read_state(c: &Container, prefix: &[u8]) -> Result<(RootsState, CommitSeq)> {
    let snap = c.snapshot();
    let seq = snap.commit_seq();
    let mut state = RootsState::new();
    if snap.directory_root().is_none() {
        return Ok((state, seq));
    }
    let dir = snap.directory();
    for item in dir.scan_prefix(prefix)? {
        let (key, value) = item?;
        let suffix = key.get(prefix.len()..).ok_or_else(|| {
            PgError::corruption(format!("directory key {} does not start with the root prefix", hex(&key)))
        })?;
        let name = String::from_utf8_lossy(suffix).into_owned();
        let bytes = match <[u8; 8]>::try_from(value.as_slice()) {
            Ok(b) => b,
            Err(_) => {
                return Err(PgError::corruption(format!(
                    "directory entry for root {name:?} holds {} bytes, expected 8",
                    value.len()
                )))
            }
        };
        let root = PageId(u64::from_le_bytes(bytes));
        let mut tree_state = TreeState::new();
        if !root.is_none() {
            let tree = snap.tree(root)?;
            for kv in tree.scan(Bound::Unbounded, Bound::Unbounded)? {
                let (k, v) = kv?;
                tree_state.insert(k, v);
            }
        }
        state.insert(name, tree_state);
    }
    Ok((state, seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::ROOT_PREFIX;
    use pgarbor_storage::{CreateOptions, MemVfs, Vfs};
    use std::path::Path;
    use std::sync::Arc;

    fn tree(kvs: &[(&str, &str)]) -> TreeState {
        kvs.iter().map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec())).collect()
    }

    fn roots(list: &[(&str, TreeState)]) -> RootsState {
        list.iter().map(|(n, t)| (n.to_string(), t.clone())).collect()
    }

    fn model(acked: RootsState, acked_seq: u64, inflight: Option<RootsState>) -> RefModel {
        RefModel { acked, acked_seq: CommitSeq(acked_seq), inflight, ..RefModel::default() }
    }

    #[test]
    fn recovered_acked_state_matches_and_adopts_seq() {
        let acked = roots(&[("main", tree(&[("k1", "v1")]))]);
        let mut m = model(acked.clone(), 5, None);
        m.begin();
        let v = m.check_recovered(&acked, CommitSeq(7)).unwrap();
        assert_eq!(v, Verdict::MatchesAcked);
        assert!(v.ok());
        assert_eq!(m.acked_seq, CommitSeq(7));
        assert!(m.inflight.is_none());
        assert!(!m.in_txn);
    }

    #[test]
    fn recovered_inflight_state_matches_and_becomes_acked() {
        let acked = roots(&[("main", tree(&[("k1", "v1")]))]);
        let inflight = roots(&[("main", tree(&[("k1", "v1"), ("k2", "v2")]))]);
        let mut m = model(acked, 5, Some(inflight.clone()));
        let v = m.check_recovered(&inflight, CommitSeq(6)).unwrap();
        assert_eq!(v, Verdict::MatchesInflight);
        assert!(v.ok());
        assert_eq!(m.acked, inflight);
        assert_eq!(m.acked_seq, CommitSeq(6));
        assert!(m.inflight.is_none());
    }

    #[test]
    fn mixed_state_is_a_violation_and_leaves_model_unchanged() {
        let acked = roots(&[("a", tree(&[("k", "1")])), ("b", tree(&[("k", "1")]))]);
        let inflight = roots(&[("a", tree(&[("k", "2")])), ("b", tree(&[("k", "2")]))]);
        let mixed = roots(&[("a", tree(&[("k", "2")])), ("b", tree(&[("k", "1")]))]);
        let mut m = model(acked.clone(), 5, Some(inflight.clone()));
        let v = m.check_recovered(&mixed, CommitSeq(6)).unwrap();
        assert!(!v.ok());
        let Verdict::Violation(msg) = v else { panic!("expected a violation") };
        assert!(msg.contains("root \"a\": key 6b expected 31 (1 bytes) found 32 (1 bytes)"), "{msg}");
        assert!(msg.contains("recovered commit_seq 6"), "{msg}");
        assert!(msg.contains("acked commit_seq 5"), "{msg}");
        assert_eq!(m.acked, acked);
        assert_eq!(m.acked_seq, CommitSeq(5));
        assert_eq!(m.inflight, Some(inflight));
    }

    #[test]
    fn seq_regression_is_a_violation_even_with_matching_state() {
        let acked = roots(&[("main", tree(&[("k", "v")]))]);
        let mut m = model(acked.clone(), 5, None);
        let v = m.check_recovered(&acked, CommitSeq(4)).unwrap();
        let Verdict::Violation(msg) = v else { panic!("expected a violation") };
        assert_eq!(msg, "recovered commit_seq 4 is below acked commit_seq 5");
        assert_eq!(m.acked_seq, CommitSeq(5));
    }

    #[test]
    fn diff_summary_formats() {
        let exp = roots(&[("main", tree(&[("k1", "v1")]))]);
        assert_eq!(diff_summary(&exp, &exp).unwrap(), "");

        let missing_root = roots(&[]);
        assert_eq!(diff_summary(&exp, &missing_root).unwrap(), "root \"main\" missing");

        let unexpected_root = roots(&[("feature", TreeState::new())]);
        assert_eq!(diff_summary(&RootsState::new(), &unexpected_root).unwrap(), "root \"feature\" unexpected");

        let missing_key = roots(&[("main", TreeState::new())]);
        assert_eq!(
            diff_summary(&exp, &missing_key).unwrap(),
            "root \"main\": key 6b31 expected 7631 (2 bytes) found <missing>"
        );

        let extra_key = roots(&[("main", tree(&[("k1", "v1")]))]);
        assert_eq!(
            diff_summary(&missing_key, &extra_key).unwrap(),
            "root \"main\": key 6b31 expected <missing> found 7631 (2 bytes)"
        );

        let changed = roots(&[("main", tree(&[("k1", "v2")]))]);
        assert_eq!(
            diff_summary(&exp, &changed).unwrap(),
            "root \"main\": key 6b31 expected 7631 (2 bytes) found 7632 (2 bytes)"
        );

        let long = "A".repeat(20);
        let long_exp = roots(&[("main", tree(&[("k1", long.as_str())]))]);
        assert_eq!(
            diff_summary(&long_exp, &missing_key).unwrap(),
            "root \"main\": key 6b31 expected 4141414141414141... (20 bytes) found <missing>"
        );
    }

    #[test]
    fn read_state_of_written_container() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
        let c = Container::create_with(vfs, Path::new("state.pga"), &CreateOptions::default()).unwrap();
        let seq = {
            let mut w = c.begin_write().unwrap();
            let mut main = PageId::NONE;
            main = w.put(main, b"k1", b"v1").unwrap().root;
            main = w.put(main, b"k2", b"v2").unwrap().root;
            let mut dir = PageId::NONE;
            dir = w.put(dir, &[ROOT_PREFIX, &b"main"[..]].concat(), &main.0.to_le_bytes()).unwrap().root;
            w.set_directory_root(dir);
            w.commit().unwrap()
        };
        let (state, got_seq) = read_state(&c, ROOT_PREFIX).unwrap();
        assert_eq!(got_seq, seq);
        assert_eq!(state, roots(&[("main", tree(&[("k1", "v1"), ("k2", "v2")]))]));
    }
}
