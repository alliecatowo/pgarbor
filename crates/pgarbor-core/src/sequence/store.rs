//! Engine-wide sequence state cache and the non-transactional "mini-commit" (TXN-13).
//! Lease CORE-13 (sonnet).
//!
//! # Where state lives
//! - Durable: key `seq_key(relid)` in the sequence's database tree of the branch head.
//! - In memory: `SequenceStore.entries[(branch, db, relid)]` = the visible `SequenceState`
//!   (what PG keeps in the shared buffer). Loaded on first use from the LATEST branch head
//!   (`engine.branch_head`), never from a transaction snapshot (sequences are non-transactional).
//!
//! # nextval(txn, relid, params)
//! 1. `txn.check_not_failed()`; read-only txn → `error::read_only_transaction("nextval()")`
//!    (oracle: 25006 `cannot execute nextval() in a read-only transaction`).
//! 2. Session CACHE range non-empty → pop front, record currval/lastval, return (no shared state).
//! 3. Created-in-this-transaction case: the txn's overlay holds `Some(state)` for `seq_key(relid)`
//!    and the latest head has no such key → advance that overlay state and write it back with
//!    `overlay.put_unlogged` (the sequence vanishes on rollback; nextval survives ROLLBACK TO).
//! 4. Else lock `entries` (one Mutex; mini-commits for different sequences serialize — acceptable
//!    in v1), load the entry if absent (`force_log = true` on first load in this process),
//!    `state::advance(..)`; if `persist` is Some → MINI-COMMIT before returning the value:
//!    `w = container.begin_write()`; read the branch record (`directory::load_record`) — deleted →
//!    `branch_deleted`; `new_root = w.apply(rec.database_root(db), {seq_key → persist.encode()})`;
//!    `ops::advance_head(.., rec.generation, db, new_root)` (the generation of the CURRENT record:
//!    sequence writes are valid on whatever the head is); `w.commit()`. Commit failure → error,
//!    in-memory state unchanged (no value handed out).
//! 5. Update the entry, push extras into the session CACHE range, record currval/lastval.
//!
//! # setval(txn, relid, params, value, is_called)
//! Read-only → 25006 `cannot execute setval() in a read-only transaction`. `validate_setval`;
//! persist immediately via mini-commit (PG WAL-logs every setval); clear this session's CACHE range
//! for the sequence; record currval/lastval (PG sets currval to the setval value).
//!
//! # Transactional DDL helpers (write into the txn overlay through `TxnKv`, NOT mini-commits)
//! `create(txn, relid, params)` → put `initial(start)`; `restart(txn, relid, value)` (ALTER SEQUENCE
//! RESTART) → put `initial(value)`; `drop(txn, relid)` → delete key. On COMMIT, `commit.rs` calls
//! `invalidate_keys` so cached entries reload from the new head.
//!
//! # Lock order (CONTRACT, deadlock-free)
//! `SequenceStore.entries` mutex BEFORE the container writer (`begin_write`). nextval/setval
//! mini-commits take `entries` then `begin_write`; `Engine::create_branch` takes
//! `lock_entries()` FIRST, then `begin_write`, then calls `flush_branch` with the guard. Never call
//! `begin_write` and then lock `entries`. `forget_branch`/`invalidate_keys` lock `entries` alone
//! and must not be called while holding a `WriteTxn` (commit.rs calls `invalidate_keys` after
//! `w.commit()` consumed the writer).
//!
//! # Branch operations
//! `flush_branch(entries, w, rec)`: before forking a branch, write every cached entry of `rec.id` as its
//! VISIBLE state (not the logged-ahead image) into `rec`'s database trees inside the caller's write
//! txn and drop those entries, so the child starts exactly at the parent's last value (RFC 001 §9
//! "fork copies state at the branch snapshot"). Next nextval on the parent reloads with
//! `force_log`, re-reserving ahead — never reissuing a value.
//! `forget_branch(id)`: drop entries after reset/delete. `flush_all(engine)`: clean shutdown writes
//! visible states of all branches in one commit (PG's shutdown checkpoint), so a clean reopen shows
//! `log_cnt` as before.
use crate::engine::Engine;
use crate::error as core_error;
use crate::sequence::state::{self, seq_key, Advance, SequenceParams, SequenceState};
use crate::txn::Txn;
use pgarbor_branches::ops::{self, OpCtx};
use pgarbor_branches::{directory, error as branch_error, BranchRecord, BranchState};
use pgarbor_common::{BranchId, CommitSeq, KvRead, KvWrite, Oid, PageId, PgError, RelId, Result};
use pgarbor_storage::format::superblock::now_unix_ms;
use pgarbor_storage::{WriteBatch, WriteTxn};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

/// The cache map type guarded by `SequenceStore::entries`.
pub type SeqEntries = HashMap<SeqCacheKey, SeqEntry>;

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeqCacheKey {
    pub branch: BranchId,
    pub database: Oid,
    pub relid: RelId,
}

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqEntry {
    /// Visible state.
    pub state: SequenceState,
}

// CONTRACT
#[derive(Debug, Default)]
pub struct SequenceStore {
    pub(crate) entries: Mutex<SeqEntries>,
}

impl SequenceStore {
    pub fn new() -> SequenceStore {
        SequenceStore::default()
    }

    /// Lock the cache (first step of the lock order in the module docs).
    pub fn lock_entries(&self) -> MutexGuard<'_, SeqEntries> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// See module docs. `entries` is the guard from `lock_entries()`, taken BEFORE `w` was begun.
    pub fn flush_branch(&self, entries: &mut SeqEntries, w: &mut WriteTxn<'_>, rec: &mut BranchRecord) -> Result<usize> {
        let (roots, written) = flush_roots(entries, w, rec)?;
        for (db, root) in roots {
            rec.set_database_root(db, root);
        }
        entries.retain(|k, _| k.branch != rec.id);
        Ok(written)
    }

    /// Drop all cached entries of a branch.
    pub fn forget_branch(&self, branch: &BranchId) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| k.branch != *branch);
    }

    /// Drop cached entries whose `seq_key` appears in `keys` (committed DDL on sequences).
    pub fn invalidate_keys(&self, branch: &BranchId, database: Oid, keys: &[RelId]) {
        let mut m = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for relid in keys {
            m.remove(&SeqCacheKey { branch: *branch, database, relid: *relid });
        }
    }

    /// Clean-shutdown flush (module docs).
    pub fn flush_all(&self, engine: &Engine) -> Result<()> {
        if engine.is_read_only() {
            return Ok(());
        }
        let mut entries = self.lock_entries();
        if entries.is_empty() {
            return Ok(());
        }
        let mut branches: Vec<BranchId> = Vec::new();
        for k in entries.keys() {
            if !branches.contains(&k.branch) {
                branches.push(k.branch);
            }
        }
        let mut w = engine.container().begin_write()?;
        let mut any = false;
        for id in branches {
            // The directory root moves with every head advance, so re-read it per branch.
            let dir = w.directory_root();
            let loaded = directory::load_record(&w.tree(dir), &id)?;
            let rec = match loaded {
                Some(r) if r.state == BranchState::Active => r,
                _ => continue,
            };
            let (roots, _) = flush_roots(&entries, &mut w, &rec)?;
            if roots.is_empty() {
                continue;
            }
            let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };
            let new_dir = {
                let mut t = w.tree(dir);
                for (db, root) in roots {
                    ops::advance_head(&mut t, &ctx, &id, rec.generation, db, root)?;
                }
                t.root()
            };
            w.set_directory_root(new_dir);
            any = true;
        }
        if any {
            w.commit()?;
        } else {
            w.abort();
        }
        entries.clear();
        Ok(())
    }
}

fn missing_state(relid: RelId) -> PgError {
    PgError::internal(format!("sequence state for relation {relid} is missing"))
}

fn lock_session(txn: &Txn) -> MutexGuard<'_, state::SessionSequences> {
    txn.session.sequences.lock().unwrap_or_else(|e| e.into_inner())
}

/// Load a sequence's durable state from the LATEST head of the branch (never a txn snapshot).
fn load_from_head(engine: &Engine, branch: &BranchId, db: Oid, relid: RelId) -> Result<SequenceState> {
    let head = engine.branch_head(branch, db)?;
    match head.tree.get(&seq_key(relid))? {
        Some(bytes) => SequenceState::decode(relid, &bytes),
        None => Err(missing_state(relid)),
    }
}

/// State of `relid` as held by the transaction's own overlay, if the overlay mentions it.
/// `Ok(None)` = overlay silent; a tombstone means the sequence was dropped in this transaction.
fn overlay_state(txn: &Txn, relid: RelId) -> Result<Option<SequenceState>> {
    match txn.overlay.get(&seq_key(relid)) {
        None => Ok(None),
        Some(None) => Err(missing_state(relid)),
        Some(Some(bytes)) => SequenceState::decode(relid, bytes).map(Some),
    }
}

/// Durably write the given sequence states in one storage commit onto the CURRENT head of
/// `branch` (whatever its generation is). Caller holds the `entries` lock.
fn mini_commit(engine: &Engine, branch: &BranchId, db: Oid, writes: &[(RelId, SequenceState)]) -> Result<CommitSeq> {
    let mut w = engine.container().begin_write()?;
    let dir = w.directory_root();
    let rec = directory::load_record(&w.tree(dir), branch)?.ok_or_else(|| branch_error::branch_id_not_found(branch))?;
    if rec.state != BranchState::Active {
        return Err(branch_error::branch_deleted(&rec.name, &rec.id));
    }
    let root = rec.database_root(db).ok_or_else(|| core_error::database_not_in_branch(db, &rec.name))?;
    let mut batch = WriteBatch::new();
    for (relid, st) in writes {
        batch.put(&seq_key(*relid), &st.encode()?);
    }
    let new_root = w.apply(root, &batch)?;
    let ctx = OpCtx { commit_seq: w.commit_seq(), now_unix_ms: now_unix_ms() };
    let new_dir = {
        let mut t = w.tree(dir);
        ops::advance_head(&mut t, &ctx, branch, rec.generation, db, new_root)?;
        t.root()
    };
    w.set_directory_root(new_dir);
    w.commit()
}

/// Write the visible state of every cached entry of `rec.id` into the database trees inside `w`.
/// Returns the new root per database that changed, and the number of states written. Entries
/// whose key no longer exists (dropped sequence) or whose stored bytes already match are skipped.
fn flush_roots(entries: &SeqEntries, w: &mut WriteTxn<'_>, rec: &BranchRecord) -> Result<(Vec<(Oid, PageId)>, usize)> {
    let mut by_db: BTreeMap<Oid, Vec<(RelId, SequenceState)>> = BTreeMap::new();
    for (k, e) in entries.iter() {
        if k.branch == rec.id {
            by_db.entry(k.database).or_default().push((k.relid, e.state));
        }
    }
    let mut roots = Vec::new();
    let mut written = 0usize;
    for (db, items) in by_db {
        let root = match rec.database_root(db) {
            Some(r) => r,
            None => continue,
        };
        let mut batch = WriteBatch::new();
        for (relid, st) in items {
            let key = seq_key(relid);
            let encoded = st.encode()?;
            match w.get(root, &key)? {
                None => continue,
                Some(existing) if existing == encoded => continue,
                Some(_) => {}
            }
            batch.put(&key, &encoded);
            written += 1;
        }
        if batch.is_empty() {
            continue;
        }
        roots.push((db, w.apply(root, &batch)?));
    }
    Ok((roots, written))
}

/// Record the result of a nextval in the session memory (currval / lastval / CACHE range).
fn record_advance(txn: &Txn, skey: (Oid, RelId), adv: &Advance) {
    let mut guard = lock_session(txn);
    let s = &mut *guard;
    if !adv.cached.is_empty() {
        s.cached.entry(skey).or_default().extend(adv.cached.iter().copied());
    }
    s.last.insert(skey, adv.value);
    s.lastval = Some(skey);
}

/// `nextval(regclass)`.
pub fn nextval(txn: &mut Txn, relid: RelId, params: &SequenceParams) -> Result<i64> {
    txn.check_not_failed()?;
    txn.check_writable("nextval()")?;
    let skey = (txn.database, relid);

    // Session CACHE range first: no shared state involved.
    {
        let mut guard = lock_session(txn);
        let s = &mut *guard;
        let popped = s.cached.get_mut(&skey).and_then(|q| q.pop_front());
        if let Some(v) = popped {
            s.last.insert(skey, v);
            s.lastval = Some(skey);
            return Ok(v);
        }
    }

    // Sequence created or restarted by this very transaction: its state lives in the overlay.
    if let Some(cur) = overlay_state(txn, relid)? {
        let adv = state::advance(&cur, params, false)?;
        txn.overlay.put_unlogged(&seq_key(relid), &adv.state.encode()?)?;
        record_advance(txn, skey, &adv);
        return Ok(adv.value);
    }

    let engine = Arc::clone(&txn.engine);
    let ckey = SeqCacheKey { branch: txn.branch, database: txn.database, relid };
    let adv = {
        let mut entries = engine.sequences().lock_entries();
        let (cur, force_log) = match entries.get(&ckey) {
            Some(e) => (e.state, false),
            None => (load_from_head(&engine, &ckey.branch, ckey.database, relid)?, true),
        };
        let adv = state::advance(&cur, params, force_log)?;
        if let Some(p) = adv.persist {
            // Durable before the value is handed out; on failure nothing changes in memory.
            mini_commit(&engine, &ckey.branch, ckey.database, &[(relid, p)])?;
        }
        entries.insert(ckey, SeqEntry { state: adv.state });
        adv
    };
    record_advance(txn, skey, &adv);
    Ok(adv.value)
}

/// `setval(regclass, bigint [, boolean])`; returns `value`.
pub fn setval(txn: &mut Txn, relid: RelId, params: &SequenceParams, value: i64, is_called: bool) -> Result<i64> {
    txn.check_not_failed()?;
    txn.check_writable("setval()")?;
    let new_state = state::validate_setval(params, value, is_called)?;
    let skey = (txn.database, relid);

    if overlay_state(txn, relid)?.is_some() {
        txn.overlay.put_unlogged(&seq_key(relid), &new_state.encode()?)?;
    } else {
        let engine = Arc::clone(&txn.engine);
        let ckey = SeqCacheKey { branch: txn.branch, database: txn.database, relid };
        let mut entries = engine.sequences().lock_entries();
        // The sequence must exist durably before we write its key.
        if !entries.contains_key(&ckey) {
            load_from_head(&engine, &ckey.branch, ckey.database, relid)?;
        }
        mini_commit(&engine, &ckey.branch, ckey.database, &[(relid, new_state)])?;
        entries.insert(ckey, SeqEntry { state: new_state });
    }

    let mut guard = lock_session(txn);
    let s = &mut *guard;
    s.cached.remove(&skey);
    if is_called {
        s.last.insert(skey, value);
    }
    s.lastval = Some(skey);
    Ok(value)
}

/// `currval(regclass)`: session memory only; absent → `state::currval_undefined(name)`.
pub fn currval(txn: &Txn, relid: RelId, name: &str) -> Result<i64> {
    let s = lock_session(txn);
    match s.last.get(&(txn.database, relid)) {
        Some(v) => Ok(*v),
        None => Err(state::currval_undefined(name)),
    }
}

/// `lastval()`: session memory; absent → `state::lastval_undefined()`.
pub fn lastval(txn: &Txn) -> Result<i64> {
    let s = lock_session(txn);
    match s.lastval.and_then(|k| s.last.get(&k).copied()) {
        Some(v) => Ok(v),
        None => Err(state::lastval_undefined()),
    }
}

/// Visible state for `SELECT * FROM seq`: overlay (created in txn) → cached entry → latest head;
/// missing everywhere → XX000 `sequence state for relation <oid> is missing`.
pub fn read_state(txn: &Txn, relid: RelId) -> Result<SequenceState> {
    if let Some(st) = overlay_state(txn, relid)? {
        return Ok(st);
    }
    let ckey = SeqCacheKey { branch: txn.branch, database: txn.database, relid };
    {
        let entries = txn.engine.sequences().lock_entries();
        if let Some(e) = entries.get(&ckey) {
            return Ok(e.state);
        }
    }
    load_from_head(&txn.engine, &ckey.branch, ckey.database, relid)
}

/// CREATE SEQUENCE: transactional initial state (module docs).
pub fn create(txn: &mut Txn, relid: RelId, params: &SequenceParams) -> Result<()> {
    let bytes = SequenceState::initial(params.start).encode()?;
    txn.kv("CREATE SEQUENCE").put(&seq_key(relid), &bytes)
}

/// ALTER SEQUENCE ... RESTART [WITH v]: transactional (module docs).
pub fn restart(txn: &mut Txn, relid: RelId, value: i64) -> Result<()> {
    let bytes = SequenceState::initial(value).encode()?;
    txn.kv("ALTER SEQUENCE").put(&seq_key(relid), &bytes)?;
    // PG forgets this session's unused cached numbers on RESTART.
    let skey = (txn.database, relid);
    lock_session(txn).cached.remove(&skey);
    Ok(())
}

/// DROP SEQUENCE: transactional delete of the state key.
pub fn drop(txn: &mut Txn, relid: RelId) -> Result<()> {
    txn.kv("DROP SEQUENCE").delete(&seq_key(relid)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{EngineOptions, SessionRequest};
    use crate::session::SessionHandle;
    use crate::types::{CommitOutcome, TxnOptions};
    use pgarbor_branches::{CreateOptions, CreateSource};
    use pgarbor_storage::{MemVfs, Vfs};
    use std::path::Path;

    const REL: RelId = 20000;

    fn opts(vfs: &Arc<MemVfs>) -> EngineOptions {
        EngineOptions { vfs: Some(vfs.clone() as Arc<dyn Vfs>), ..Default::default() }
    }

    fn params() -> SequenceParams {
        SequenceParams { name: "s".into(), increment: 1, min_value: 1, max_value: i64::MAX, start: 1, cache: 1, cycle: false }
    }

    fn setup() -> (Arc<MemVfs>, Arc<Engine>) {
        let vfs = Arc::new(MemVfs::new());
        let engine = Engine::create(Path::new("seq.pga"), &opts(&vfs)).unwrap();
        let session = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let mut t = engine.begin(&session, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        create(&mut t, REL, &params()).unwrap();
        assert!(matches!(t.commit().unwrap(), CommitOutcome::Committed { .. }));
        (vfs, engine)
    }

    fn next(engine: &Arc<Engine>, session: &SessionHandle) -> i64 {
        let mut t = engine.begin(session, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        let v = nextval(&mut t, REL, &params()).unwrap();
        t.commit().unwrap();
        v
    }

    #[test]
    fn interleaved_sessions_get_distinct_increasing_values() {
        let (_vfs, engine) = setup();
        let a = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let b = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        assert_eq!(next(&engine, &a), 1);
        assert_eq!(next(&engine, &b), 2);
        assert_eq!(next(&engine, &a), 3);
        let mut t = engine.begin(&a, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        assert_eq!(read_state(&t, REL).unwrap(), SequenceState { last_value: 3, log_cnt: 30, is_called: true });
        assert_eq!(currval(&t, REL, "s").unwrap(), 3);
        assert_eq!(lastval(&t).unwrap(), 3);
    }

    #[test]
    fn currval_before_nextval_and_read_only() {
        let (_vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let mut t = engine.begin(&s, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        assert_eq!(currval(&t, REL, "s").unwrap_err().message, "currval of sequence \"s\" is not yet defined in this session");
        assert_eq!(lastval(&t).unwrap_err().message, "lastval is not yet defined in this session");
        t.rollback().unwrap();
        let mut ro = engine.begin(&s, &TxnOptions { read_only: true, ..Default::default() }).unwrap();
        ro.begin_statement().unwrap();
        let e = nextval(&mut ro, REL, &params()).unwrap_err();
        assert_eq!(e.message, "cannot execute nextval() in a read-only transaction");
    }

    #[test]
    fn setval_then_currval() {
        let (_vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let mut t = engine.begin(&s, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        assert_eq!(setval(&mut t, REL, &params(), 100, true).unwrap(), 100);
        assert_eq!(currval(&t, REL, "s").unwrap(), 100);
        assert_eq!(read_state(&t, REL).unwrap(), SequenceState { last_value: 100, log_cnt: 0, is_called: true });
        t.commit().unwrap();
        assert_eq!(next(&engine, &s), 101);
    }

    #[test]
    fn sequence_created_in_rolled_back_txn_disappears() {
        let (_vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let mut t = engine.begin(&s, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        create(&mut t, 20001, &params()).unwrap();
        assert_eq!(nextval(&mut t, 20001, &params()).unwrap(), 1);
        assert_eq!(nextval(&mut t, 20001, &params()).unwrap(), 2);
        t.rollback().unwrap();
        let mut t2 = engine.begin(&s, &TxnOptions::default()).unwrap();
        t2.begin_statement().unwrap();
        assert!(read_state(&t2, 20001).is_err());
    }

    #[test]
    fn crash_reopen_skips_and_clean_close_continues() {
        let (vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        for want in 1..=3 {
            assert_eq!(next(&engine, &s), want);
        }
        std::mem::drop(s);
        std::mem::drop(engine);
        let engine = Engine::open(Path::new("seq.pga"), &opts(&vfs)).unwrap();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        assert_eq!(next(&engine, &s), 34);
        std::mem::drop(s);
        std::mem::drop(engine);

        let (vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        for want in 1..=3 {
            assert_eq!(next(&engine, &s), want);
        }
        std::mem::drop(s);
        engine.close().unwrap();
        std::mem::drop(engine);
        let engine = Engine::open(Path::new("seq.pga"), &opts(&vfs)).unwrap();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        let mut t = engine.begin(&s, &TxnOptions::default()).unwrap();
        t.begin_statement().unwrap();
        assert_eq!(read_state(&t, REL).unwrap().log_cnt, 30);
        t.rollback().unwrap();
        assert_eq!(next(&engine, &s), 4);
    }

    #[test]
    fn forked_branch_continues_from_parent_visible_value() {
        let (_vfs, engine) = setup();
        let s = engine.attach_session(&SessionRequest::embedded("main")).unwrap();
        for want in 1..=3 {
            assert_eq!(next(&engine, &s), want);
        }
        engine.create_branch("child", &CreateSource::branch("main"), &CreateOptions::default()).unwrap();
        let c = engine.attach_session(&SessionRequest::embedded("child")).unwrap();
        assert_eq!(next(&engine, &c), 4);
        assert_eq!(next(&engine, &s), 4);
    }
}
