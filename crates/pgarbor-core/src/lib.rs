//! PGArbor engine core: engine handle, sessions, transactions (snapshots, overlays, savepoints,
//! commit), lock manager (key/relation/advisory locks, deadlock detection), sequences and id
//! allocation (ARCHITECTURE §7, TXN-01..13).
//!
//! CONTRACT: every `pub` item re-exported here is frozen at `contracts-v1`
//! (see `contracts/crates/pgarbor-core.md`). Changes need an amendment under
//! `contracts/amendments/`.
//!
//! Synchronous, no async runtime, no background threads: waits use `std::sync::Condvar`.
//!
//! North-star usage (what the pgarbor crate's `Session` does):
//!
//! ```ignore
//! use pgarbor_core::{Engine, EngineOptions, SessionRequest, TxnOptions, TxnKind};
//! let engine = Engine::create("dev.pga".as_ref(), &EngineOptions::default())?;   // branch "main"
//! let session = engine.attach_session(&SessionRequest::embedded("main"))?;
//! let mut txn = engine.begin(&session, &TxnOptions { kind: TxnKind::Explicit, ..Default::default() })?;
//! txn.begin_statement()?;                                  // snapshot
//! { let mut kv = txn.kv("INSERT"); kv.put(b"...", b"...")?; }  // locks key, writes overlay
//! txn.commit()?;                                           // durable, then locks released
//! engine.create_branch("feature", &pgarbor_branches::CreateSource::branch("main"), &Default::default())?;
//! ```

pub mod alloc;
pub mod commit;
pub mod diag;
pub mod engine;
pub mod error;
pub mod locks;
pub mod options;
pub mod overlay;
pub mod savepoint;
pub mod sequence;
pub mod session;
pub mod ssi;
pub mod txn;
pub mod txn_kv;
pub mod types;

pub use alloc::IdAllocator;
pub use diag::{ActivityRow, LockRow};
pub use engine::{BranchAdminOptions, BranchHead, Engine};
pub use locks::{AdvisoryKey, AdvisoryMode, AdvisoryScope, LockManager, LockMode, LockTag, UnlockResult};
pub use options::{EngineOptions, SessionRequest};
pub use overlay::Overlay;
pub use savepoint::Savepoint;
pub use sequence::{SequenceParams, SequenceState, SequenceStore};
pub use session::{Activity, BackendState, Interrupts, SessionHandle, SessionRegistry, SessionShared};
pub use txn::Txn;
pub use txn_kv::TxnKv;
pub use types::{
    AcquireResult, CommitOutcome, IsolationLevel, LockOutcome, SessionId, SubXactId, TxnKind, TxnOptions, TxnState,
    WaitLimits, WaitPolicy,
};
