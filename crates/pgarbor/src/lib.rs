//! PGArbor embedded API: [`Database`], [`Session`], statement dispatch, GUCs, prepared statements and
//! portals (ARCHITECTURE §11, seed task CLI-08). The wire server (`pgarbor-wire`) is a thin protocol
//! translator over this crate: it contains no SQL semantics.
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor.md` and `contracts/leases/pgarbor.json` (API-01..).
//!
//! ```ignore
//! use pgarbor::{Database, OpenOptions, SessionOptions};
//! let db = Database::open_or_create("dev.pga", OpenOptions::default())?;
//! let mut s = db.session("main", SessionOptions::default())?;
//! let out = s.simple_query("CREATE TABLE t(id int primary key); INSERT INTO t VALUES (1); SELECT * FROM t")?;
//! assert_eq!(out[2].result.rows().len(), 1);
//! s.parse("q1", "SELECT * FROM t WHERE id = $1", &[])?;           // extended protocol primitives
//! db.create_branch("feature", "main")?;                           // O(1) copy-on-write fork
//! ```
//! ## Module map
//! | area | modules |
//! |---|---|
//! | public data | [`types`] |
//! | database + branches | [`database`], [`branches`], [`bootstrap`] |
//! | session | [`session`], [`txn_block`], [`simple_query`], [`run_stmt`], [`ctx_build`], [`extended`], [`prepared`], [`portal`], [`session_utility`], [`engine_info`], [`notify`] |
//! | configuration | [`guc_def`], [`guc_table`] (generated), [`guc_value`], [`guc`], [`guc_derive`] |
//! | bindings seam | [`ffi`] |
#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

pub mod bootstrap;
pub mod branches;
pub mod ctx_build;
pub mod database;
pub mod engine_info;
pub mod extended;
pub mod ffi;
pub mod guc;
pub mod guc_def;
pub mod guc_derive;
pub mod guc_table;
pub mod guc_value;
pub mod notify;
pub mod portal;
pub mod prepared;
pub mod run_stmt;
pub mod session;
pub mod session_utility;
pub mod simple_query;
pub mod txn_block;
pub mod types;

pub use database::Database;
pub use pgarbor_exec::copy::{CopyChannel, MemChannel};
pub use guc::{GucScope, GucSource, GucTable};
pub use session::Session;
pub use types::{
    BranchInfo, CancelToken, ColumnInfo, Datum, EncodedRow, FormatCode, Notification, OpenOptions, Oid, PgError, PortalOutput,
    PortalStatus, Result, RoleAuth, SessionOptions, Severity, SimpleQueryOutput, SqlState, StatementDescription,
    StatementOutput, StatementResult, SyncMode, TxnStatus, TypeId, VerifyLevel,
};
