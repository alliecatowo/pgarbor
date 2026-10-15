//! Diagnostics rows for `pg_stat_activity`, `pg_locks`, `pg_blocking_pids` and branch session
//! listings (TXN-12). Lease CORE-15. pgarbor-catalog/exec turn these into virtual relation rows;
//! all times are unix milliseconds (exec converts to timestamptz).
//!
//! Column semantics follow PG18 (`contracts/reference/pg18/catalog_attributes.csv` lists the exact
//! columns): fields we cannot know are None (rendered NULL).
use crate::engine::Engine;
use crate::session::SessionShared;
use crate::types::SessionId;
use pgarbor_common::{Result, TxnId};

/// `pg_authid` OID of the bootstrap superuser `postgres` (every v1 session runs as it).
const BOOTSTRAP_SUPERUSER_OID: u32 = 10;

/// Whether a session belongs to the viewer's branch (`None` = no filter, every branch).
fn in_scope(s: &SessionShared, branch_filter: Option<pgarbor_common::BranchId>) -> bool {
    branch_filter.map_or(true, |b| s.branch == b)
}

/// Current transaction of a session, read under its activity mutex (no clone of the query text).
fn current_txn(s: &SessionShared) -> Option<TxnId> {
    s.activity.lock().unwrap_or_else(|e| e.into_inner()).txn
}

/// `<pid>/<txn>` virtual transaction id; a session with no open transaction reports local id 0
/// (PG's `InvalidLocalTransactionId`), which is what session-level advisory locks show.
fn vxid_of(session: SessionId, txn: Option<TxnId>) -> String {
    format!("{}/{}", session.0, txn.map_or(0, |t| t.0))
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActivityRow {
    pub datid: u32,
    pub datname: String,
    pub pid: i32,
    /// Always None in v1 (no parallel workers).
    pub leader_pid: Option<i32>,
    /// Role OID of `usename` (10 for postgres; resolved by the caller when other roles exist).
    pub usesysid: u32,
    pub usename: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub client_hostname: Option<String>,
    /// -1 for Unix-socket/embedded, like PG.
    pub client_port: Option<i32>,
    pub backend_start_ms: u64,
    pub xact_start_ms: Option<u64>,
    pub query_start_ms: Option<u64>,
    pub state_change_ms: Option<u64>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    /// `BackendState::as_pg_str`.
    pub state: Option<String>,
    pub backend_xid: Option<u64>,
    pub backend_xmin: Option<u64>,
    pub query_id: Option<i64>,
    pub query: String,
    /// `client backend`.
    pub backend_type: String,
    /// PGArbor extension column (not in pg_stat_activity; exposed by `pgarbor_stat_branch_sessions`).
    pub branch: String,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LockRow {
    pub locktype: String,
    pub database: Option<u32>,
    pub relation: Option<u32>,
    pub page: Option<i32>,
    pub tuple: Option<i16>,
    pub virtualxid: Option<String>,
    pub transactionid: Option<u64>,
    pub classid: Option<u32>,
    pub objid: Option<u32>,
    pub objsubid: Option<i16>,
    /// `<pid>/<local txn id>` like PG's `3/45` (we use `<pid>/<txn id>`).
    pub virtualtransaction: Option<String>,
    pub pid: Option<i32>,
    pub mode: String,
    pub granted: bool,
    pub fastpath: bool,
    pub waitstart_ms: Option<u64>,
}

/// One row per registered session (`SessionRegistry::all`), visible to every session of the
/// engine (v1 has no privilege filtering; PG hides `query` of other users from non-superusers —
/// documented gap). wait_event_type/wait_event from `Activity::wait`; `backend_xid` from activity.
/// Rows for sessions of OTHER branches are included in `branch_sessions` but EXCLUDED from
/// `pg_stat_activity` (a branch is its own "cluster" from the client's point of view) — so this
/// function takes the viewing session's branch filter.
pub fn activity_rows(engine: &Engine, branch_filter: Option<pgarbor_common::BranchId>) -> Result<Vec<ActivityRow>> {
    let mut rows = Vec::new();
    // `all()` is sorted by pid already.
    for s in engine.sessions().all() {
        if !in_scope(&s, branch_filter) {
            continue;
        }
        let a = s.activity();
        rows.push(ActivityRow {
            datid: s.database_oid,
            datname: s.database_name.clone(),
            pid: s.id.0,
            leader_pid: None,
            usesysid: BOOTSTRAP_SUPERUSER_OID,
            usename: s.user.clone(),
            application_name: s.application_name.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            client_addr: s.client_addr.clone(),
            client_hostname: None,
            client_port: s.client_port,
            backend_start_ms: s.backend_start_ms,
            xact_start_ms: a.xact_start_ms,
            query_start_ms: a.query_start_ms,
            state_change_ms: Some(a.state_change_ms),
            wait_event_type: a.wait.map(|w| w.event_type.to_string()),
            wait_event: a.wait.map(|w| w.event.to_string()),
            state: Some(a.state.as_pg_str().to_string()),
            backend_xid: a.backend_xid.map(|x| x.0),
            backend_xmin: None,
            query_id: None,
            query: a.query,
            backend_type: "client backend".to_string(),
            branch: s.branch_name(),
        });
    }
    Ok(rows)
}

/// `pg_locks` rows from `LockManager::snapshot()` (filtered to `branch_filter`'s tags):
/// relation → locktype `relation`, database, relation; key → locktype `tuple`, database, relation
/// = `LockTag::key_relation()`, page/tuple None; advisory → `advisory`, database, classid = key1,
/// objid = key2, objsubid = field4; object → `object`, classid/objid/objsubid. Plus, like PG, one
/// `virtualxid` ExclusiveLock row per session with an open transaction (virtualxid =
/// virtualtransaction = `<pid>/<txn>`), and one `transactionid` ExclusiveLock row per transaction
/// that has written (`backend_xid`).
pub fn lock_rows(engine: &Engine, branch_filter: Option<pgarbor_common::BranchId>) -> Result<Vec<LockRow>> {
    let sessions = engine.sessions();
    let exclusive = crate::locks::LockMode::Exclusive.pg_name()?;
    let mut rows = Vec::new();

    // Lock table entries. LockInfo carries no branch, so the branch comes from the owning session
    // (a lock's tag always belongs to its session's branch).
    for info in engine.locks().snapshot()? {
        let owner = sessions.get(info.session);
        if let Some(f) = branch_filter {
            match &owner {
                Some(s) if s.branch == f => {}
                _ => continue,
            }
        }
        let pid = info.session.0;
        let txn = owner.as_deref().and_then(current_txn);
        let (database, relation, classid, objid, objsubid) = match info.tag.locktype {
            "relation" | "tuple" => (Some(info.tag.database), info.tag.relation, None, None, None),
            "advisory" | "object" => (
                Some(info.tag.database),
                None,
                info.tag.classid,
                info.tag.objid,
                // PG stores objsubid as uint16 and reports it through int2: truncate like PG.
                info.tag.objsubid.map(|v| v as i16),
            ),
            _ => (None, None, None, None, None),
        };
        let vxid = vxid_of(info.session, txn);
        rows.push(LockRow {
            locktype: info.tag.locktype.to_string(),
            database,
            relation,
            page: None,
            tuple: None,
            virtualxid: None,
            transactionid: None,
            classid,
            objid,
            objsubid,
            virtualtransaction: Some(vxid),
            pid: Some(pid),
            mode: info.mode.pg_name()?.to_string(),
            granted: info.granted,
            fastpath: false,
            waitstart_ms: if info.granted { None } else { info.wait_start_unix_ms },
        });
    }

    // Per session: a virtualxid row while a transaction is open, and a transactionid row once it
    // has written (PG holds both as ExclusiveLock for the transaction's lifetime).
    for s in sessions.all() {
        if !in_scope(&s, branch_filter) {
            continue;
        }
        let a = s.activity();
        if let Some(txn) = a.txn {
            let vxid = vxid_of(s.id, Some(txn));
            rows.push(LockRow {
                locktype: "virtualxid".to_string(),
                database: None,
                relation: None,
                page: None,
                tuple: None,
                virtualxid: Some(vxid.clone()),
                transactionid: None,
                classid: None,
                objid: None,
                objsubid: None,
                virtualtransaction: Some(vxid),
                pid: Some(s.id.0),
                mode: exclusive.to_string(),
                granted: true,
                fastpath: false,
                waitstart_ms: None,
            });
        }
        if let Some(xid) = a.backend_xid {
            rows.push(LockRow {
                locktype: "transactionid".to_string(),
                database: None,
                relation: None,
                page: None,
                tuple: None,
                virtualxid: None,
                transactionid: Some(xid.0),
                classid: None,
                objid: None,
                objsubid: None,
                virtualtransaction: Some(vxid_of(s.id, a.txn)),
                pid: Some(s.id.0),
                mode: exclusive.to_string(),
                granted: true,
                fastpath: false,
                waitstart_ms: None,
            });
        }
    }

    // Stable sort: equal (pid, locktype) keep snapshot order.
    rows.sort_by(|x, y| (x.pid, x.locktype.as_str()).cmp(&(y.pid, y.locktype.as_str())));
    Ok(rows)
}

/// `pg_blocking_pids(pid)` → `LockManager::blocking_sessions` as i32 pids.
pub fn blocking_pids(engine: &Engine, pid: SessionId) -> Result<Vec<i32>> {
    Ok(engine.locks().blocking_sessions(pid)?.into_iter().map(|s| s.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locks::advisory::{self, AdvisoryKey, AdvisoryMode, AdvisoryScope};
    use crate::options::{EngineOptions, SessionRequest};
    use crate::types::TxnOptions;
    use pgarbor_common::BranchId;
    use pgarbor_storage::io::MemVfs;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn engine() -> Arc<Engine> {
        let opts = EngineOptions { vfs: Some(Arc::new(MemVfs::new())), ..EngineOptions::default() };
        Engine::create(Path::new("/diag.pga"), &opts).expect("create engine")
    }

    #[test]
    fn advisory_wait_is_visible_in_activity_locks_and_blocking() {
        let engine = engine();
        let holder = engine.attach_session(&SessionRequest::embedded("main")).expect("holder");
        let waiter = engine.attach_session(&SessionRequest::embedded("main")).expect("waiter");
        let holder_id = holder.id();
        let waiter_id = waiter.id();
        let waiter_shared = Arc::clone(waiter.shared());
        let key = AdvisoryKey::Single(42);

        let h_txn = engine.begin(&holder, &TxnOptions::default()).expect("begin holder");
        assert!(advisory::lock(&h_txn, key, AdvisoryMode::Exclusive, AdvisoryScope::Session, true).expect("hold"));

        let waiter_engine = Arc::clone(&engine);
        let handle = std::thread::spawn(move || -> Result<bool> {
            let mut w_txn = waiter_engine.begin(&waiter, &TxnOptions::default())?;
            w_txn.begin_statement()?;
            advisory::lock(&w_txn, key, AdvisoryMode::Exclusive, AdvisoryScope::Session, true)
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while waiter_shared.activity().wait.is_none() {
            assert!(Instant::now() < deadline, "waiter never blocked");
            std::thread::sleep(Duration::from_millis(5));
        }

        let acts = activity_rows(&engine, Some(holder.branch())).expect("activity");
        let w = acts.iter().find(|r| r.pid == waiter_id.0).expect("waiter activity row");
        assert_eq!(w.wait_event_type.as_deref(), Some("Lock"));
        assert_eq!(w.wait_event.as_deref(), Some("advisory"));
        assert_eq!(w.state.as_deref(), Some("active"));
        assert_eq!(w.backend_type, "client backend");
        assert_eq!(w.usesysid, 10);

        let locks = lock_rows(&engine, None).expect("lock rows");
        let adv: Vec<&LockRow> = locks.iter().filter(|r| r.locktype == "advisory").collect();
        assert_eq!(adv.len(), 2);
        let granted: Vec<&&LockRow> = adv.iter().filter(|r| r.granted).collect();
        let waiting: Vec<&&LockRow> = adv.iter().filter(|r| !r.granted).collect();
        assert_eq!(granted.len(), 1);
        assert_eq!(waiting.len(), 1);
        assert_eq!(granted[0].pid, Some(holder_id.0));
        assert_eq!(waiting[0].pid, Some(waiter_id.0));
        assert!(waiting[0].waitstart_ms.is_some());
        for r in adv.iter() {
            assert_eq!(r.classid, Some(0));
            assert_eq!(r.objid, Some(42));
            assert_eq!(r.objsubid, Some(1));
            assert_eq!(r.mode, "ExclusiveLock");
        }

        assert_eq!(blocking_pids(&engine, waiter_id).expect("blocking"), vec![holder_id.0]);

        assert!(advisory::unlock(&h_txn, key, AdvisoryMode::Exclusive).expect("unlock").released);
        assert!(handle.join().expect("join").expect("waiter acquired"));
    }

    #[test]
    fn branch_filter_excludes_other_branches() {
        let engine = engine();
        let s = engine.attach_session(&SessionRequest::embedded("main")).expect("attach");
        let own = s.branch();
        assert_eq!(activity_rows(&engine, Some(own)).expect("own").len(), 1);
        assert_eq!(activity_rows(&engine, None).expect("all").len(), 1);
        let other = BranchId([0xee; 16]);
        assert!(activity_rows(&engine, Some(other)).expect("other").is_empty());
        assert!(lock_rows(&engine, Some(other)).expect("other locks").is_empty());
    }
}
