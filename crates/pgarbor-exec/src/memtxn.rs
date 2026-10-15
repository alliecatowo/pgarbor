//! [`MemTxn`]: an [`ExecTxn`] over an in-memory `MemKv` for unit tests (no engine, no locks, no
//! concurrency). Fully implemented, frozen (v1). Workers use it for the unit tests they must write
//! in their own file:
//!
//! ```ignore
//! let mut txn = MemTxn::new();
//! let rows = txn.alloc_rowids(16400, 3)?;           // 1..4
//! txn.put_fresh(&pgarbor_core::alloc::row_key(16400, rows.start), &bytes)?;
//! ```
//! Semantics: `lock_*` always succeed (nothing else runs), `lock_and_read_latest` returns the
//! current value with `changed = false`, `alloc_oid` counts up from the seed, sequences keep their
//! state in the same map under `seq_key` (so `seq_state` is visible to `get`), and everything else
//! is a thin wrapper. It is NOT a model of isolation; concurrency behavior is verified centrally
//! against real transactions.
use std::collections::HashMap;
use std::ops::{Bound, Range};

use pgarbor_common::{KvIter, KvRead, KvWrite, MemKv, Oid, PgError, RelId, Result};
use pgarbor_core::locks::{AdvisoryKey, AdvisoryMode, AdvisoryScope, LockMode, UnlockResult};
use pgarbor_core::sequence::state as seqstate;
use pgarbor_core::sequence::{SequenceParams, SequenceState};
use pgarbor_core::{IsolationLevel, WaitPolicy};

use crate::txn::{ExecTxn, LatestRead};

/// Test transaction (module docs).
#[derive(Debug, Default)]
pub struct MemTxn {
    pub kv: MemKv,
    pub read_only: bool,
    next_rowid: HashMap<RelId, u64>,
    next_oid: Option<Oid>,
    last_val: HashMap<RelId, i64>,
    last_seq: Option<RelId>,
    /// Advisory locks held, for `advisory_unlock` results: (key, shared) -> count.
    advisory: HashMap<(AdvisoryKey, bool), u32>,
    /// Number of `lock_and_read_latest` calls (tests assert the hook is used).
    pub lock_reads: u64,
}

impl MemTxn {
    pub fn new() -> MemTxn {
        MemTxn::default()
    }

    fn max_rowid(&self, relid: RelId) -> Result<u64> {
        let prefix = relid.to_be_bytes();
        let mut max = 0u64;
        for item in self.kv.scan_prefix(&prefix)? {
            let (k, _) = item?;
            if k.len() == 12 {
                let mut b = [0u8; 8];
                b.copy_from_slice(&k[4..12]);
                max = max.max(u64::from_be_bytes(b));
            }
        }
        Ok(max)
    }
}

impl KvRead for MemTxn {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.kv.get(key)
    }
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        self.kv.scan(lower, upper)
    }
}

impl KvWrite for MemTxn {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_writable("write")?;
        self.kv.put(key, value)
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        self.check_writable("write")?;
        self.kv.delete(key)
    }
}

impl ExecTxn for MemTxn {
    fn txn_id(&self) -> u64 {
        3
    }
    fn isolation(&self) -> IsolationLevel {
        IsolationLevel::ReadCommitted
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn check_interrupts(&self) -> Result<()> {
        Ok(())
    }
    fn check_writable(&self, command: &str) -> Result<()> {
        if self.read_only {
            return Err(pgarbor_core::error::read_only_transaction(command));
        }
        Ok(())
    }

    fn alloc_rowids(&mut self, relid: RelId, n: u64) -> Result<Range<u64>> {
        if n == 0 {
            return Ok(0..0);
        }
        let start = match self.next_rowid.get(&relid) {
            Some(v) => *v,
            None => self.max_rowid(relid)? + 1,
        };
        self.next_rowid.insert(relid, start + n);
        Ok(start..start + n)
    }
    fn put_fresh(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put(key, value)
    }

    fn lock_and_read_latest(&mut self, key: &[u8], _mode: LockMode, _policy: WaitPolicy) -> Result<LatestRead> {
        self.lock_reads += 1;
        let value = self.kv.get(key)?;
        Ok(LatestRead::Locked { value, changed: false })
    }
    fn lock_relation(&mut self, _relid: RelId, _mode: LockMode, _policy: WaitPolicy) -> Result<bool> {
        Ok(true)
    }
    fn lock_ddl(&mut self) -> Result<()> {
        Ok(())
    }
    fn advisory_lock(&mut self, key: AdvisoryKey, mode: AdvisoryMode, _scope: AdvisoryScope, _wait: bool) -> Result<bool> {
        let shared = matches!(mode, AdvisoryMode::Shared);
        *self.advisory.entry((key, shared)).or_insert(0) += 1;
        Ok(true)
    }
    fn advisory_unlock(&mut self, key: AdvisoryKey, mode: AdvisoryMode) -> Result<UnlockResult> {
        let shared = matches!(mode, AdvisoryMode::Shared);
        match self.advisory.get_mut(&(key, shared)) {
            Some(c) if *c > 0 => {
                *c -= 1;
                Ok(UnlockResult { released: true, warning: None })
            }
            _ => Ok(UnlockResult {
                released: false,
                warning: Some(pgarbor_core::error::advisory_not_owned(if shared { "ShareLock" } else { "ExclusiveLock" })),
            }),
        }
    }
    fn advisory_unlock_all(&mut self) -> Result<()> {
        self.advisory.clear();
        Ok(())
    }

    fn alloc_oid(&mut self, seed: &dyn Fn(&dyn KvRead) -> Result<Oid>) -> Result<Oid> {
        let cur = match self.next_oid {
            Some(v) => v,
            None => seed(&self.kv)?.max(pgarbor_common::FIRST_NORMAL_OID),
        };
        self.next_oid = Some(cur + 1);
        Ok(cur)
    }

    fn seq_nextval(&mut self, relid: RelId, params: &SequenceParams) -> Result<i64> {
        self.check_writable("nextval()")?;
        let cur = self.seq_state(relid)?;
        let adv = seqstate::advance(&cur, params, false)?;
        self.kv.put(&seqstate::seq_key(relid), &adv.state.encode()?)?;
        self.last_val.insert(relid, adv.value);
        self.last_seq = Some(relid);
        Ok(adv.value)
    }
    fn seq_setval(&mut self, relid: RelId, params: &SequenceParams, value: i64, is_called: bool) -> Result<i64> {
        self.check_writable("setval()")?;
        let st = seqstate::validate_setval(params, value, is_called)?;
        self.kv.put(&seqstate::seq_key(relid), &st.encode()?)?;
        if is_called {
            self.last_val.insert(relid, value);
        }
        self.last_seq = Some(relid);
        Ok(value)
    }
    fn seq_currval(&self, relid: RelId, name: &str) -> Result<i64> {
        self.last_val.get(&relid).copied().ok_or_else(|| seqstate::currval_undefined(name))
    }
    fn seq_lastval(&self) -> Result<i64> {
        self.last_seq
            .and_then(|r| self.last_val.get(&r).copied())
            .ok_or_else(seqstate::lastval_undefined)
    }
    fn seq_state(&self, relid: RelId) -> Result<SequenceState> {
        match self.kv.get(&seqstate::seq_key(relid))? {
            Some(b) => SequenceState::decode(relid, &b),
            None => Err(PgError::internal(format!("sequence state for relation {relid} is missing"))),
        }
    }
    fn seq_create(&mut self, relid: RelId, params: &SequenceParams) -> Result<()> {
        let bytes = SequenceState::initial(params.start).encode()?;
        self.kv.put(&seqstate::seq_key(relid), &bytes)
    }
    fn seq_restart(&mut self, relid: RelId, value: i64) -> Result<()> {
        let bytes = SequenceState::initial(value).encode()?;
        self.kv.put(&seqstate::seq_key(relid), &bytes)
    }
    fn seq_drop(&mut self, relid: RelId) -> Result<()> {
        self.kv.delete(&seqstate::seq_key(relid)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rowids_and_sequences() {
        let mut t = MemTxn::new();
        assert_eq!(t.alloc_rowids(16400, 3).unwrap(), 1..4);
        assert_eq!(t.alloc_rowids(16400, 1).unwrap(), 4..5);
        let p = SequenceParams { name: "s".into(), increment: 1, min_value: 1, max_value: 10, start: 1, cache: 1, cycle: false };
        t.seq_create(20000, &p).unwrap();
        assert_eq!(t.seq_nextval(20000, &p).unwrap(), 1);
        assert_eq!(t.seq_nextval(20000, &p).unwrap(), 2);
        assert_eq!(t.seq_currval(20000, "s").unwrap(), 2);
        assert!(t.seq_currval(20001, "z").is_err());
    }

    #[test]
    fn read_only_blocks_writes() {
        let mut t = MemTxn::new();
        t.read_only = true;
        assert_eq!(t.put(b"k", b"v").unwrap_err().sqlstate.code(), "25006");
    }
}
