//! Test helpers for lease workers and downstream crates' unit tests (not part of the stable API).
//!
//! Version discipline in tests: `CatalogTxn::begin` requires `read_version(store) ==
//! env.snapshot().version()`. An env built with `CatalogSnapshot::build(CatalogVersion::ZERO, ..)` over a
//! KV that was bootstrapped therefore fails with 40001. Use [`bootstrapped`] for the first env and
//! [`next_env`] with the previous statement's `DdlResult::snapshot` for later statements.
use crate::env::CatalogEnv;
use crate::persist;
use crate::snapshot::CatalogSnapshot;
use crate::txn::DdlResult;
use pgarbor_common::{MemKv, Result};

/// A bootstrapped in-memory catalog store and an env over it (default search path `pg_catalog, public`,
/// current user `postgres` 10). `MemKv` is a `CatalogStore` (OIDs from the `cat:next_oid` counter).
pub fn bootstrapped() -> Result<(MemKv, CatalogEnv)> {
    let mut kv = MemKv::default();
    persist::bootstrap(&mut kv)?;
    let snap = CatalogSnapshot::load(&kv)?;
    Ok((kv, CatalogEnv::with_default_path(snap)))
}

/// The env for the next statement after `res` (same session context, snapshot after the DDL).
pub fn next_env(prev: &CatalogEnv, res: &DdlResult) -> CatalogEnv {
    prev.with_snapshot(res.snapshot.clone())
}

/// Run one DDL statement against `kv` with a fresh stamp and return the result plus the next env:
/// `let (res, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;`
pub fn run<F>(kv: &mut MemKv, env: &CatalogEnv, f: F) -> Result<(DdlResult, CatalogEnv)>
where
    F: FnOnce(&mut crate::txn::CatalogTxn) -> Result<()>,
{
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAMP: AtomicU64 = AtomicU64::new(1);
    let mut t = crate::txn::CatalogTxn::begin(kv, env)?;
    f(&mut t)?;
    let res = t.finish_statement(STAMP.fetch_add(1, Ordering::Relaxed))?;
    let next = next_env(env, &res);
    Ok((res, next))
}
