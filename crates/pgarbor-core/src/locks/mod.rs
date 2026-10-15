//! Lock manager: one PostgreSQL-style lock table for row/key locks, relation locks and advisory
//! locks, with FIFO wait queues, a wait-for-graph deadlock detector, `lock_timeout`,
//! `statement_timeout` and cancel handling (TXN-09, TXN-10, ARCHITECTURE §7).
//!
//! Ownership model (as in PG): a lock is held by a SESSION (backend) with a SCOPE —
//! `Transaction{txn, subxact}` (released at commit/abort, or at ROLLBACK TO for subxacts >= the
//! savepoint's) or `Session` (advisory session locks, released by unlock or disconnect).
//! Locks held by the same session never conflict with each other. Deadlock graph nodes are
//! sessions.

pub mod advisory;
pub mod deadlock;
pub mod manager;
pub mod mode;
pub mod table;
pub mod tag;

pub use advisory::{AdvisoryKey, AdvisoryMode, AdvisoryScope, UnlockResult};
pub use manager::LockManager;
pub use mode::LockMode;
pub use table::{LockInfo, LockScope};
pub use tag::LockTag;
