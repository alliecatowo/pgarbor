//! `TxnKv`: the transaction's `KvRead + KvWrite` view (overlay over snapshot) that pgarbor-exec and
//! pgarbor-catalog read and write through (ARCHITECTURE §4, §7; TXN-03). Lease CORE-06.
//!
//! Reads: overlay entry wins (`Some(None)` = deleted → absent); else the snapshot tree. Requires a
//! snapshot (`Txn::begin_statement` first) — otherwise XX000 `no snapshot: begin_statement was not
//! called`. Scans merge with `overlay::merge_scan`.
//!
//! Writes (`put`/`delete`), in order:
//! 1. `txn.check_not_failed()`; read-only → `error::read_only_transaction(self.command)`.
//! 2. Key lock: `txn.lock_key(key, ForNoKeyUpdate if self.no_key_update else ForUpdate, Block)`;
//!    under RR/SERIALIZABLE a `Changed` outcome raises 40001 (update/delete wording per
//!    `Txn::lock_key_for_write`); under RC the write proceeds (callers that need EvalPlanQual call
//!    `lock_key` first and decide). Keys in space 2 (sequences) skip locking (non-transactional
//!    state, see sequence/store.rs).
//! 3. `overlay.put/delete`; `txn.wrote = true`; session `backend_xid = Some(txn.id)` on first write.
//! `delete` returns whether the key was visible in the merged view before deleting.
//!
//! `put_fresh`: insert of a key whose uniqueness is guaranteed by construction (a table row key
//! built from a rowid just returned by `alloc::next_rowid` for this txn) — skips step 2 (no lock
//! entry, no outcome check), keeping bulk INSERT memory O(rows written), not O(rows * lock state).
use crate::txn::Txn;
use pgarbor_common::{KvIter, KvRead, KvWrite, PgError, Result};
use std::ops::Bound;

// CONTRACT
pub struct TxnKv<'t> {
    pub(crate) txn: &'t mut Txn,
    /// Command name for 25006 messages (`INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, ...).
    pub(crate) command: &'static str,
    /// Use ForNoKeyUpdate instead of ForUpdate for implicit write locks (UPDATE not touching key
    /// columns, PG semantics so FK checks' KEY SHARE locks do not block it).
    pub(crate) no_key_update: bool,
}

impl Txn {
    /// The read/write view for one statement. `command` names the statement for read-only errors.
    pub fn kv(&mut self, command: &'static str) -> TxnKv<'_> {
        TxnKv { txn: self, command, no_key_update: false }
    }
}

impl<'t> TxnKv<'t> {
    pub fn txn(&self) -> &Txn {
        self.txn
    }
    pub fn txn_mut(&mut self) -> &mut Txn {
        self.txn
    }
    pub fn set_no_key_update(&mut self, on: bool) {
        self.no_key_update = on;
    }

    /// Unlocked insert of a freshly allocated row key (module docs).
    #[allow(unused_variables)]
    pub fn put_fresh(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-06", "fresh-key insert"))
    }
}

impl KvRead for TxnKv<'_> {
    #[allow(unused_variables)]
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Err(PgError::unimplemented("CORE-06", "transactional point read"))
    }
    #[allow(unused_variables)]
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        Err(PgError::unimplemented("CORE-06", "transactional range scan"))
    }
}

impl KvWrite for TxnKv<'_> {
    #[allow(unused_variables)]
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Err(PgError::unimplemented("CORE-06", "transactional put"))
    }
    #[allow(unused_variables)]
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        Err(PgError::unimplemented("CORE-06", "transactional delete"))
    }
}
