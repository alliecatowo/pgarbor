//! Serializable Snapshot Isolation (TXN-11). Lease CORE-16 (opus).
//!
//! Implements the Cahill/Ports-Grittner algorithm as in PostgreSQL's
//! `src/backend/storage/lmgr/predicate.c` (README-SSI), adapted to PGArbor's key-value model:
//! - "SIREAD locks" are key RANGES read by a SERIALIZABLE transaction (a point get is a one-key
//!   range, a scan is its bounds), tracked per (branch, database). Unlike PG there is no
//!   granularity promotion except a hull collapse beyond [`MAX_READ_RANGES`] ranges per
//!   transaction (more false positives, never false negatives).
//! - rw-antidependency `R -> W` exists when R read a key range that W (concurrent with R) wrote.
//! - A dangerous structure `T1 -> T2 -> T3` where T3 is the first to commit (and, when T1 is
//!   READ ONLY, T3 committed before T1's snapshot) aborts one participant with 40001
//!   `could not serialize access due to read/write dependencies among transactions`. The pivot
//!   T2 is preferred as the victim when still active, else T1 (predicate.c
//!   `OnConflict_CheckForSerializationFailure` / `PreCommit_CheckForSerializationFailure`).
//! - A victim that is not the calling transaction is "doomed" and fails at its next hook call.
//!
//! # Time model
//! Snapshot ordering uses the container's durable commit sequence (`Txn::snapshot().commit_seq`).
//! A committing transaction records `commit_lo` (container seq just before its commit; its real
//! seq is `> commit_lo`) at `pre_commit` and `commit_hi` (container seq after) at `on_finish`.
//! "Committed before snapshot S" is only claimed when certain (`commit_hi <= S`), "after S" only
//! when certain (`commit_lo >= S`); uncertainty is resolved toward reporting a conflict. The
//! relative commit ORDER of transactions uses an internal counter assigned at `pre_commit`.
//!
//! # Wiring contract (for txn_kv.rs / commit.rs owners)
//! For SERIALIZABLE transactions call `on_read` for every key/range read (after the snapshot is
//! taken), `on_write` for every put/delete, `pre_commit` before `begin_write`, and `on_finish`
//! exactly once on commit AND rollback (including read-only commits that skip the write path).
//! All hooks are no-ops for transactions that are not SERIALIZABLE. Committed transactions are
//! retained until every tracked active transaction can see them (plus a small grace window for
//! transactions whose first hook call comes after their snapshot was taken).
//!
//! # Not implemented
//! READ ONLY DEFERRABLE "safe snapshot" waiting needs a begin-of-transaction hook that the
//! contract does not have; deferrable read-only transactions are tracked like ordinary read-only
//! SERIALIZABLE transactions (correct, but may be aborted instead of waiting).
use crate::txn::Txn;
use crate::types::IsolationLevel;
use pgarbor_common::{BranchId, Oid, PgError, Result, SqlState, TxnId};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Bound;
use std::sync::Mutex;

/// Beyond this many distinct read ranges a transaction's reads collapse into their hull.
const MAX_READ_RANGES: usize = 1024;
/// Newest committed transactions always retained (see module docs, late registration).
const DEFAULT_GRACE: usize = 64;
const NEVER: u64 = u64::MAX;

type Scope = (BranchId, Oid);

const MESSAGE: &str = "could not serialize access due to read/write dependencies among transactions";

fn failure(reason: &str) -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, MESSAGE)
        .with_detail(format!("Reason code: {reason}."))
        .with_hint("The transaction might succeed if retried.")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Read,
    Write,
}

impl Phase {
    fn pivot_ctx(self) -> &'static str {
        match self {
            Phase::Read => "conflict out checking",
            Phase::Write => "write",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Phase::Read => "read",
            Phase::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Active,
    /// `pre_commit` passed: can no longer be chosen as a victim.
    Committing,
    Committed,
}

/// An owned key range (SIREAD lock).
#[derive(Clone, Debug)]
struct KeyRange {
    lo: Bound<Vec<u8>>,
    hi: Bound<Vec<u8>>,
}

fn own(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match b {
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

impl KeyRange {
    fn is_empty(&self) -> bool {
        match (&self.lo, &self.hi) {
            (Bound::Included(a), Bound::Included(b)) => a > b,
            (Bound::Included(a), Bound::Excluded(b))
            | (Bound::Excluded(a), Bound::Included(b))
            | (Bound::Excluded(a), Bound::Excluded(b)) => a >= b,
            _ => false,
        }
    }

    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        let lo = match &self.lo {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let hi = match &self.hi {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        };
        (lo, hi)
    }

    fn contains(&self, key: &[u8]) -> bool {
        let lo_ok = match &self.lo {
            Bound::Unbounded => true,
            Bound::Included(a) => key >= a.as_slice(),
            Bound::Excluded(a) => key > a.as_slice(),
        };
        let hi_ok = match &self.hi {
            Bound::Unbounded => true,
            Bound::Included(a) => key <= a.as_slice(),
            Bound::Excluded(a) => key < a.as_slice(),
        };
        lo_ok && hi_ok
    }

    /// Orderable lower bound: `None` (unbounded) sorts first; included before excluded.
    fn lo_key(&self) -> Option<(&[u8], u8)> {
        match &self.lo {
            Bound::Unbounded => None,
            Bound::Included(a) => Some((a.as_slice(), 0u8)),
            Bound::Excluded(a) => Some((a.as_slice(), 1u8)),
        }
    }

    /// Orderable upper bound: unbounded sorts last; excluded before included.
    fn hi_key(&self) -> (u8, &[u8], u8) {
        match &self.hi {
            Bound::Unbounded => (1u8, &b""[..], 0u8),
            Bound::Excluded(a) => (0u8, a.as_slice(), 0u8),
            Bound::Included(a) => (0u8, a.as_slice(), 1u8),
        }
    }

    fn covers(&self, other: &KeyRange) -> bool {
        self.lo_key() <= other.lo_key() && self.hi_key() >= other.hi_key()
    }

    fn hits_any(&self, keys: &BTreeSet<Vec<u8>>) -> bool {
        if keys.is_empty() || self.is_empty() {
            return false;
        }
        keys.range::<[u8], _>(self.bounds()).next().is_some()
    }
}

#[derive(Debug)]
struct Xact {
    scope: Scope,
    snap_seq: u64,
    read_only: bool,
    status: Status,
    reads: Vec<KeyRange>,
    writes: BTreeSet<Vec<u8>>,
    /// Readers R with `R -> self`.
    ins: HashSet<TxnId>,
    /// Writers W with `self -> W`.
    outs: HashSet<TxnId>,
    doomed: Option<String>,
    /// Internal commit order; `NEVER` until `pre_commit`.
    order: u64,
    commit_lo: u64,
    commit_hi: u64,
}

impl Xact {
    /// Could this transaction's effects be invisible to a snapshot taken at `snap`?
    fn concurrent_with(&self, snap: u64) -> bool {
        self.status != Status::Committed || self.commit_hi > snap
    }

    fn add_read(&mut self, range: KeyRange) {
        if self.reads.iter().any(|r| r.covers(&range)) {
            return;
        }
        self.reads.push(range);
        if self.reads.len() > MAX_READ_RANGES {
            let mut hull = self.reads[0].clone();
            for r in &self.reads[1..] {
                if r.lo_key() < hull.lo_key() {
                    hull.lo = r.lo.clone();
                }
                if r.hi_key() > hull.hi_key() {
                    hull.hi = r.hi.clone();
                }
            }
            self.reads.clear();
            self.reads.push(hull);
        }
    }
}

#[derive(Debug)]
struct Inner {
    xacts: HashMap<TxnId, Xact>,
    next_order: u64,
    /// Committed transactions in commit order.
    finished: VecDeque<TxnId>,
    grace: usize,
}

impl Default for Inner {
    fn default() -> Inner {
        Inner::new(DEFAULT_GRACE)
    }
}

impl Inner {
    fn new(grace: usize) -> Inner {
        Inner { xacts: HashMap::new(), next_order: 0, finished: VecDeque::new(), grace }
    }

    fn register(&mut self, id: TxnId, scope: Scope, snap_seq: u64, read_only: bool) {
        self.xacts.entry(id).or_insert_with(|| Xact {
            scope,
            snap_seq,
            read_only,
            status: Status::Active,
            reads: Vec::new(),
            writes: BTreeSet::new(),
            ins: HashSet::new(),
            outs: HashSet::new(),
            doomed: None,
            order: NEVER,
            commit_lo: 0,
            commit_hi: NEVER,
        });
    }

    fn is_active(&self, id: TxnId) -> bool {
        self.xacts.get(&id).is_some_and(|x| x.status == Status::Active)
    }

    /// Is `T1 -> T2 -> T3` a dangerous structure (T3 committed first, read-only T1 rule)?
    fn dangerous(&self, t1: TxnId, t2: TxnId, t3: TxnId) -> bool {
        if t1 == t2 || t2 == t3 {
            return false;
        }
        let (Some(x1), Some(x2), Some(x3)) = (self.xacts.get(&t1), self.xacts.get(&t2), self.xacts.get(&t3)) else {
            return false;
        };
        if x3.status == Status::Active {
            return false;
        }
        if x3.order >= x2.order || x3.order > x1.order {
            return false;
        }
        // A read-only T1 only matters if T3 may be visible to its snapshot.
        if x1.read_only && x3.commit_lo >= x1.snap_seq {
            return false;
        }
        true
    }

    /// A new edge `a -> b` was added; look for dangerous structures it completes. `me` is the
    /// calling transaction.
    fn check_new_edge(&mut self, a: TxnId, b: TxnId, me: TxnId, phase: Phase) -> Result<()> {
        let mut structs: Vec<(TxnId, TxnId, TxnId)> = Vec::new();
        if let Some(xb) = self.xacts.get(&b) {
            for &t3 in &xb.outs {
                structs.push((a, b, t3));
            }
        }
        if let Some(xa) = self.xacts.get(&a) {
            for &t1 in &xa.ins {
                structs.push((t1, a, b));
            }
        }
        for (t1, t2, t3) in structs {
            if !self.dangerous(t1, t2, t3) {
                continue;
            }
            let (victim, pivot) = if self.is_active(t2) {
                (t2, true)
            } else if self.is_active(t1) {
                (t1, false)
            } else {
                continue;
            };
            let reason = if pivot {
                format!("Canceled on identification as a pivot, during {}", phase.pivot_ctx())
            } else {
                format!("Canceled on conflict out to pivot {}, during {}", t2.0, phase.verb())
            };
            if let Some(x) = self.xacts.get_mut(&victim) {
                if x.doomed.is_none() {
                    x.doomed = Some(reason.clone());
                }
            }
            if victim == me {
                return Err(failure(&reason));
            }
        }
        Ok(())
    }

    fn doomed_error(&self, id: TxnId) -> Result<()> {
        match self.xacts.get(&id).and_then(|x| x.doomed.as_deref()) {
            Some(r) => Err(failure(r)),
            None => Ok(()),
        }
    }

    fn read(&mut self, id: TxnId, range: KeyRange) -> Result<()> {
        let (scope, snap) = match self.xacts.get(&id) {
            Some(x) => (x.scope, x.snap_seq),
            None => return Ok(()),
        };
        self.doomed_error(id)?;
        if range.is_empty() {
            return Ok(());
        }
        let targets: Vec<TxnId> = self
            .xacts
            .iter()
            .filter(|(oid, x)| **oid != id && x.scope == scope && x.concurrent_with(snap) && range.hits_any(&x.writes))
            .map(|(oid, _)| *oid)
            .collect();
        if let Some(me) = self.xacts.get_mut(&id) {
            me.add_read(range);
        }
        for w in targets {
            let fresh = self.xacts.get_mut(&id).is_some_and(|m| m.outs.insert(w));
            if let Some(xw) = self.xacts.get_mut(&w) {
                xw.ins.insert(id);
            }
            if fresh {
                self.check_new_edge(id, w, id, Phase::Read)?;
            }
        }
        Ok(())
    }

    fn write(&mut self, id: TxnId, key: &[u8]) -> Result<()> {
        let (scope, snap) = match self.xacts.get(&id) {
            Some(x) => (x.scope, x.snap_seq),
            None => return Ok(()),
        };
        self.doomed_error(id)?;
        let targets: Vec<TxnId> = self
            .xacts
            .iter()
            .filter(|(oid, x)| {
                **oid != id && x.scope == scope && x.concurrent_with(snap) && x.reads.iter().any(|r| r.contains(key))
            })
            .map(|(oid, _)| *oid)
            .collect();
        if let Some(me) = self.xacts.get_mut(&id) {
            if !me.writes.contains(key) {
                me.writes.insert(key.to_vec());
            }
        }
        for r in targets {
            let fresh = self.xacts.get_mut(&r).is_some_and(|x| x.outs.insert(id));
            if let Some(me) = self.xacts.get_mut(&id) {
                me.ins.insert(r);
            }
            if fresh {
                self.check_new_edge(r, id, id, Phase::Write)?;
            }
        }
        Ok(())
    }

    fn revert_committing(&mut self, id: TxnId, reason: &str) {
        if let Some(x) = self.xacts.get_mut(&id) {
            x.status = Status::Active;
            x.order = NEVER;
            x.commit_lo = 0;
            x.doomed = Some(reason.to_string());
        }
    }

    fn pre_commit(&mut self, id: TxnId, seq_now: u64) -> Result<()> {
        if !self.xacts.contains_key(&id) {
            return Ok(());
        }
        self.doomed_error(id)?;
        let order = self.next_order;
        self.next_order += 1;
        let (ins, outs): (Vec<TxnId>, Vec<TxnId>) = match self.xacts.get_mut(&id) {
            Some(x) => {
                x.status = Status::Committing;
                x.order = order;
                x.commit_lo = seq_now;
                (x.ins.iter().copied().collect(), x.outs.iter().copied().collect())
            }
            None => return Ok(()),
        };
        const PIVOT: &str = "Canceled on identification as a pivot, during commit attempt";
        const PREPARED: &str = "Canceled on commit attempt with conflict in from prepared pivot";
        // 1. We are the pivot: T1 -> me -> T3 with T3 already committed first.
        for &t1 in &ins {
            for &t3 in &outs {
                if self.dangerous(t1, id, t3) {
                    self.revert_committing(id, PIVOT);
                    return Err(failure(PIVOT));
                }
            }
        }
        // 2. We are T3 (first committer): every pivot P with P -> me and some T1 -> P is dangerous.
        let mut to_doom: Vec<TxnId> = Vec::new();
        for &p in &ins {
            let t1s: Vec<TxnId> = match self.xacts.get(&p) {
                Some(xp) => xp.ins.iter().copied().collect(),
                None => continue,
            };
            for t1 in t1s {
                if !self.dangerous(t1, p, id) {
                    continue;
                }
                if self.is_active(p) {
                    to_doom.push(p);
                    break;
                }
                // Pivot already committing/committed: we cannot commit first after all.
                self.revert_committing(id, PREPARED);
                return Err(failure(PREPARED));
            }
        }
        for p in to_doom {
            if let Some(x) = self.xacts.get_mut(&p) {
                if x.doomed.is_none() {
                    x.doomed = Some(PIVOT.to_string());
                }
            }
        }
        Ok(())
    }

    fn remove_xact(&mut self, id: TxnId) {
        if let Some(x) = self.xacts.remove(&id) {
            for o in x.outs {
                if let Some(y) = self.xacts.get_mut(&o) {
                    y.ins.remove(&id);
                }
            }
            for i in x.ins {
                if let Some(y) = self.xacts.get_mut(&i) {
                    y.outs.remove(&id);
                }
            }
        }
    }

    fn finish(&mut self, id: TxnId, committed: bool, seq_now: u64) {
        if !committed {
            self.remove_xact(id);
            self.finished.retain(|f| *f != id);
        } else if let Some(x) = self.xacts.get_mut(&id) {
            if x.order == NEVER {
                x.order = self.next_order;
                self.next_order += 1;
                x.commit_lo = seq_now;
            }
            x.status = Status::Committed;
            x.commit_hi = seq_now.max(x.commit_lo);
            self.finished.push_back(id);
        }
        self.cleanup();
    }

    /// Drop committed transactions that every remaining active transaction can see.
    fn cleanup(&mut self) {
        let min_active = self
            .xacts
            .values()
            .filter(|x| x.status != Status::Committed)
            .map(|x| x.snap_seq)
            .min()
            .unwrap_or(NEVER);
        while self.finished.len() > self.grace {
            let Some(&front) = self.finished.front() else { break };
            let removable = match self.xacts.get(&front) {
                Some(x) => x.commit_hi <= min_active,
                None => true,
            };
            if !removable {
                break;
            }
            self.finished.pop_front();
            self.remove_xact(front);
        }
    }
}

// CONTRACT
#[derive(Debug, Default)]
pub struct SsiTracker {
    inner: Mutex<Inner>,
}

impl SsiTracker {
    pub fn new() -> SsiTracker {
        SsiTracker { inner: Mutex::new(Inner::default()) }
    }

    /// True: all hooks are implemented (module docs). The hooks are no-ops for non-SERIALIZABLE
    /// transactions, so callers may invoke them unconditionally once wired.
    pub fn is_active(&self) -> bool {
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn applies(txn: &Txn) -> bool {
        txn.isolation() == IsolationLevel::Serializable
    }

    fn seq_now(txn: &Txn) -> u64 {
        txn.engine().container().commit_seq().0
    }

    fn register(inner: &mut Inner, txn: &Txn) {
        let snap = match txn.snapshot() {
            Some(s) => s.commit_seq.0,
            None => Self::seq_now(txn),
        };
        inner.register(txn.id(), (txn.branch(), txn.database()), snap, txn.is_read_only());
    }

    /// Record a SIREAD range for a SERIALIZABLE txn.
    pub fn on_read(&self, txn: &Txn, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<()> {
        if !Self::applies(txn) {
            return Ok(());
        }
        let mut g = self.lock();
        Self::register(&mut g, txn);
        g.read(txn.id(), KeyRange { lo: own(lower), hi: own(upper) })
    }

    /// Record a write and create rw-conflict edges.
    pub fn on_write(&self, txn: &Txn, key: &[u8]) -> Result<()> {
        if !Self::applies(txn) {
            return Ok(());
        }
        let mut g = self.lock();
        Self::register(&mut g, txn);
        g.write(txn.id(), key)
    }

    /// Dangerous-structure check at commit.
    pub fn pre_commit(&self, txn: &Txn) -> Result<()> {
        if !Self::applies(txn) {
            return Ok(());
        }
        let seq = Self::seq_now(txn);
        self.lock().pre_commit(txn.id(), seq)
    }

    /// Transaction finished (commit or abort): release / age out its SIREAD state.
    pub fn on_finish(&self, txn: &Txn, committed: bool) -> Result<()> {
        if !Self::applies(txn) {
            return Ok(());
        }
        let seq = Self::seq_now(txn);
        self.lock().finish(txn.id(), committed, seq);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: Scope = (BranchId([1; 16]), 5);

    fn t(n: u64) -> TxnId {
        TxnId(n)
    }

    fn reg(i: &mut Inner, id: u64, snap: u64, ro: bool) {
        i.register(t(id), SC, snap, ro);
    }

    fn rd(i: &mut Inner, id: u64, lo: &[u8], hi: &[u8]) -> Result<()> {
        i.read(t(id), KeyRange { lo: Bound::Included(lo.to_vec()), hi: Bound::Included(hi.to_vec()) })
    }

    fn is_ssi(e: &PgError) -> bool {
        e.sqlstate == SqlState::SERIALIZATION_FAILURE
            && e.message == MESSAGE
            && e.hint.as_deref() == Some("The transaction might succeed if retried.")
            && e.detail.as_deref().is_some_and(|d| d.starts_with("Reason code: Canceled on "))
    }

    #[test]
    fn write_skew_two_doctors_aborts_one() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        rd(&mut i, 1, b"doc1", b"doc2").unwrap();
        rd(&mut i, 2, b"doc1", b"doc2").unwrap();
        i.write(t(1), b"doc1").unwrap();
        i.write(t(2), b"doc2").unwrap();
        i.pre_commit(t(1), 1).unwrap();
        i.finish(t(1), true, 2);
        let e = i.pre_commit(t(2), 2).unwrap_err();
        assert!(is_ssi(&e), "{e:?}");
        assert_eq!(
            e.detail.as_deref(),
            Some("Reason code: Canceled on identification as a pivot, during commit attempt.")
        );
        i.finish(t(2), false, 2);
        assert!(i.xacts.is_empty());
    }

    #[test]
    fn write_skew_second_committer_fails_at_write() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        rd(&mut i, 1, b"doc1", b"doc2").unwrap();
        rd(&mut i, 2, b"doc1", b"doc2").unwrap();
        i.write(t(1), b"doc1").unwrap();
        i.pre_commit(t(1), 1).unwrap();
        i.finish(t(1), true, 2);
        let e = i.write(t(2), b"doc2").unwrap_err();
        assert!(is_ssi(&e), "{e:?}");
    }

    #[test]
    fn pivot_aborted_at_own_commit_when_not_yet_doomed() {
        // T1 -> T2 -> T3, T3 commits first; T2 (pivot) must fail.
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        reg(&mut i, 3, 1, false);
        rd(&mut i, 2, b"x", b"x").unwrap(); // T2 -> T3 (T3 writes x)
        i.write(t(3), b"x").unwrap();
        rd(&mut i, 1, b"y", b"y").unwrap(); // T1 -> T2 (T2 writes y)
        i.write(t(2), b"y").unwrap();
        i.pre_commit(t(3), 1).unwrap();
        i.finish(t(3), true, 2);
        let e = i.pre_commit(t(2), 2).unwrap_err();
        assert!(is_ssi(&e));
    }

    #[test]
    fn disjoint_ranges_never_conflict() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        rd(&mut i, 1, b"a1", b"a9").unwrap();
        rd(&mut i, 2, b"b1", b"b9").unwrap();
        i.write(t(1), b"a5").unwrap();
        i.write(t(2), b"b5").unwrap();
        i.pre_commit(t(1), 1).unwrap();
        i.finish(t(1), true, 2);
        i.pre_commit(t(2), 2).unwrap();
        i.finish(t(2), true, 3);
        assert!(i.xacts.is_empty());
    }

    #[test]
    fn adjacent_exclusive_bounds_do_not_touch() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        i.read(t(1), KeyRange { lo: Bound::Included(b"a".to_vec()), hi: Bound::Excluded(b"m".to_vec()) }).unwrap();
        i.write(t(2), b"m").unwrap();
        assert!(i.xacts[&t(1)].outs.is_empty());
        i.write(t(2), b"l").unwrap();
        assert!(i.xacts[&t(1)].outs.contains(&t(2)));
    }

    #[test]
    fn empty_and_inverted_ranges_are_ignored() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        i.write(t(2), b"k").unwrap();
        rd(&mut i, 1, b"z", b"a").unwrap();
        i.read(t(1), KeyRange { lo: Bound::Excluded(b"k".to_vec()), hi: Bound::Excluded(b"k".to_vec()) }).unwrap();
        assert!(i.xacts[&t(1)].outs.is_empty());
    }

    #[test]
    fn visible_committed_writer_is_not_concurrent() {
        let mut i = Inner::new(10);
        reg(&mut i, 1, 1, false);
        i.write(t(1), b"k").unwrap();
        i.pre_commit(t(1), 2).unwrap();
        i.finish(t(1), true, 3);
        reg(&mut i, 2, 3, false); // snapshot includes seq 3
        rd(&mut i, 2, b"k", b"k").unwrap();
        assert!(i.xacts[&t(2)].outs.is_empty());
        reg(&mut i, 3, 2, false); // older snapshot: concurrent
        rd(&mut i, 3, b"k", b"k").unwrap();
        assert!(i.xacts[&t(3)].outs.contains(&t(1)));
    }

    /// PG docs / README-SSI read-only anomaly: T2 (receipt) reads batch, T3 (close batch) commits,
    /// T1 (report, READ ONLY) snapshots after T3 and reads receipts T2 is about to add.
    #[test]
    fn read_only_anomaly_aborts_pivot() {
        let mut i = Inner::new(0);
        reg(&mut i, 2, 5, false);
        rd(&mut i, 2, b"batch", b"batch").unwrap();
        reg(&mut i, 3, 5, false);
        i.write(t(3), b"batch").unwrap(); // T2 -> T3
        i.pre_commit(t(3), 5).unwrap();
        i.finish(t(3), true, 6);
        i.write(t(2), b"receipt").unwrap();
        reg(&mut i, 1, 6, true); // report snapshot sees T3
        rd(&mut i, 1, b"batch", b"batch").unwrap();
        rd(&mut i, 1, b"receipt", b"receipt").unwrap(); // T1 -> T2: dangerous
        let e = i.pre_commit(t(2), 6).unwrap_err();
        assert!(is_ssi(&e));
        i.finish(t(2), false, 6);
        i.pre_commit(t(1), 6).unwrap();
        i.finish(t(1), true, 6);
    }

    #[test]
    fn read_only_before_t3_commit_is_safe() {
        let mut i = Inner::new(0);
        reg(&mut i, 2, 5, false);
        rd(&mut i, 2, b"batch", b"batch").unwrap();
        reg(&mut i, 1, 5, true); // report snapshots BEFORE T3 commits
        reg(&mut i, 3, 5, false);
        i.write(t(3), b"batch").unwrap();
        i.pre_commit(t(3), 5).unwrap();
        i.finish(t(3), true, 6);
        i.write(t(2), b"receipt").unwrap();
        rd(&mut i, 1, b"receipt", b"receipt").unwrap();
        i.pre_commit(t(2), 6).unwrap();
        i.finish(t(2), true, 7);
        i.pre_commit(t(1), 7).unwrap();
        i.finish(t(1), true, 7);
    }

    #[test]
    fn rolled_back_transactions_leave_no_edges() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        rd(&mut i, 1, b"a", b"z").unwrap();
        i.write(t(2), b"k").unwrap();
        assert!(i.xacts[&t(1)].outs.contains(&t(2)));
        i.finish(t(2), false, 1);
        assert!(i.xacts[&t(1)].outs.is_empty());
        i.finish(t(1), true, 1);
        assert!(i.xacts.is_empty());
    }

    #[test]
    fn committed_state_is_garbage_collected() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        reg(&mut i, 2, 1, false);
        i.write(t(1), b"k").unwrap();
        i.pre_commit(t(1), 1).unwrap();
        i.finish(t(1), true, 2);
        assert!(i.xacts.contains_key(&t(1))); // T2 may still not see it
        i.finish(t(2), false, 2);
        assert!(i.xacts.is_empty());
    }

    #[test]
    fn read_ranges_collapse_but_still_conflict() {
        let mut i = Inner::new(0);
        reg(&mut i, 1, 1, false);
        for n in 0..(MAX_READ_RANGES as u32 + 5) {
            let k = n.to_be_bytes();
            i.read(t(1), KeyRange { lo: Bound::Included(k.to_vec()), hi: Bound::Included(k.to_vec()) }).unwrap();
        }
        assert!(i.xacts[&t(1)].reads.len() <= MAX_READ_RANGES);
        reg(&mut i, 2, 1, false);
        i.write(t(2), &3u32.to_be_bytes()).unwrap();
        assert!(i.xacts[&t(1)].outs.contains(&t(2)));
    }

    #[test]
    fn range_cover_ordering() {
        let a = KeyRange { lo: Bound::Unbounded, hi: Bound::Unbounded };
        let b = KeyRange { lo: Bound::Included(b"a".to_vec()), hi: Bound::Excluded(b"b".to_vec()) };
        assert!(a.covers(&b));
        assert!(!b.covers(&a));
        let c = KeyRange { lo: Bound::Excluded(b"a".to_vec()), hi: Bound::Included(b"b".to_vec()) };
        assert!(!b.covers(&c) && !c.covers(&b));
    }
}
