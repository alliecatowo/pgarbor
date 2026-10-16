//! The catalog write protocol for ONE DDL statement (catalog contract, "Transactions, concurrency and
//! invalidation"). Fully implemented, frozen (v1).
//!
//! [`run_ddl`] does, in order:
//! 1. `cx.txn.lock_ddl()` - the per-branch catalog write lock; on a concurrent catalog change it fails
//!    with `error::catalog_retry()` (the SESSION retries the whole statement after re-snapshotting under
//!    READ COMMITTED; step 2 of the protocol);
//! 2. `CatalogTxn::begin(&mut ExecCatalogStore, &cx.se.env)` (re-verifies the catalog version);
//! 3. runs the caller's closure with the `CatalogTxn` (the closure performs one or more catalog
//!    operations; it may call `ddl.env()`/`ddl.snapshot()` to see its own earlier changes);
//! 4. `finish_statement(stamp)` with `stamp = random u64 XOR txn id` (unique per call);
//! 5. `cx.set_catalog_snapshot(res.snapshot)` (so later steps and statements see the change);
//! 6. forwards `outcome.notices` to the client (`cx.notice`);
//! and returns the closure's value plus the [`DdlOutcome`]. The caller then performs the data-level
//! `outcome.follow_ups` with `ddl_followup::run_followups`.
//! On any error nothing is cleaned up here: the surrounding transaction discards the statement's
//! writes (implicit txn) or becomes failed (explicit txn).
use pgarbor_catalog::{CatalogTxn, DdlOutcome};
use pgarbor_common::Result;

use crate::context::ExecContext;
use crate::txn::ExecCatalogStore;

/// Unique-per-call stamp for `finish_statement` (module docs, step 4).
pub fn next_stamp(txn_id: u64) -> u64 {
    let r = pgarbor_core::session::random_bytes(8)
        .map(|b| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[..8]);
            u64::from_le_bytes(a)
        })
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    r ^ txn_id
}

/// Run one catalog statement (module docs).
pub fn run_ddl<T>(
    cx: &mut ExecContext,
    f: impl FnOnce(&mut CatalogTxn) -> Result<T>,
) -> Result<(T, DdlOutcome)> {
    cx.txn.lock_ddl()?;
    let stamp = next_stamp(cx.txn.txn_id());
    let env = cx.se.env.clone();
    let (value, res) = {
        let mut store = ExecCatalogStore::new(&mut *cx.txn);
        let mut ddl = CatalogTxn::begin(&mut store, &env)?;
        let value = match f(&mut ddl) {
            Ok(v) => v,
            Err(e) => {
                for n in ddl.take_notices() {
                    cx.se.notice(n);
                }
                return Err(e);
            }
        };
        let res = ddl.finish_statement(stamp)?;
        (value, res)
    };
    cx.set_catalog_snapshot(res.snapshot.clone());
    for n in &res.outcome.notices {
        cx.notice(n.clone());
    }
    Ok((value, res.outcome))
}
