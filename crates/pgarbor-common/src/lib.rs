//! Shared contracts for every PGArbor crate (ARCH-04, ARCH-05, ARCH-06).
//!
//! CONTRACT: everything public in this crate is frozen at `contracts-v1`.
//! Changes require an amendment ticket under `contracts/amendments/`.

pub mod error;
pub mod time;
pub mod ids;
pub mod kv;

pub use error::{PgError, Result, Severity, SqlState};
pub use ids::{BranchId, CommitSeq, DatabaseUuid, Oid, PageId, RelId, TxnId, FIRST_NORMAL_OID};
pub use kv::{KvIter, KvRead, KvWrite, MemKv, SpaceId};
