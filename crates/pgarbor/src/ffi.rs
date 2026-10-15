//! FFI-friendly synchronous surface (lease API-15). A flat, handle-based API using only integers,
//! strings and byte vectors so that thin `extern "C"` shims (C), PyO3 (Python), napi-rs (Node) and
//! wasm-bindgen (WASM) can wrap it without touching engine types. The shims themselves are separate
//! crates (not part of this contract); this module is the stable, documented seam.
//!
//! ## Model
//! * Handles are non-zero `u64`s into a thread-local table (sessions are `!Send`): a handle is only valid
//!   on the thread that created it. Language bindings that need cross-thread use run a dedicated worker
//!   thread per database.
//! * Every function returns `Result<_, FfiError>`; `FfiError{ code, sqlstate, message, detail, hint,
//!   position }` where `code` is the numeric severity (`ERROR=1, FATAL=2, PANIC=3`) and `sqlstate` the
//!   5-character string. No panics cross the boundary: every entry point wraps its body in
//!   `std::panic::catch_unwind` and maps a panic to `XX000 internal error`.
//! * Text everywhere: values are rendered in PG text format (`io::output`); NULL = `None`.
//!
//! ## Functions
//! * `db_open(path, flags) -> db handle` (`flags`: bit0 create-if-missing, bit1 read-only, bit2 in-memory);
//!   `db_close(db)`.
//! * `session_open(db, branch, user, database) -> session handle`; `session_close(session)`.
//! * `exec(session, sql) -> Vec<FfiResult>`: simple-query semantics; `FfiResult{ columns: Vec<String>,
//!   rows: Vec<Vec<Option<String>>>, tag: String, notices: Vec<String> }` (one per statement); the first
//!   error aborts and is returned as `Err` after rolling back per PG rules (earlier results are lost for
//!   FFI simplicity; use `exec_partial` to keep them).
//! * `exec_params(session, sql, params: Vec<Option<String>>) -> FfiResult`: one statement with text
//!   parameters (types inferred by the server, `Parse`/`Bind`/`Execute`).
//! * `exec_json(session, sql) -> String`: results of `exec` as a JSON document
//!   `[{"columns":[{"name":..,"type_oid":..}],"rows":[[..]],"tag":".."}]` produced by a small hand-written
//!   serializer (this crate has no serde); strings escaped per RFC 8259, NULL as `null`.
//! * `txn_status(session) -> u8` (`I`/`T`/`E`), `cancel(session)`, `branch_create(db, name, from)`,
//!   `branch_list(db) -> Vec<String>`, `branch_delete(db, name)`, `version() -> &'static str` (`18.6`).
//! Handles are released by the matching `*_close`; leaking one leaks only that object.
use pgarbor_common::{PgError, Result};

/// Error crossing the FFI boundary.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfiError {
    pub code: i32,
    pub sqlstate: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<u32>,
}

impl From<PgError> for FfiError {
    fn from(e: PgError) -> FfiError {
        FfiError {
            code: match e.severity {
                pgarbor_common::Severity::Fatal => 2,
                pgarbor_common::Severity::Panic => 3,
                _ => 1,
            },
            sqlstate: e.sqlstate.code().to_string(),
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            position: e.position,
        }
    }
}

/// One statement result in text form.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FfiResult {
    pub columns: Vec<String>,
    pub column_type_oids: Vec<u32>,
    pub rows: Vec<Vec<Option<String>>>,
    pub tag: String,
    pub notices: Vec<String>,
}

/// Opaque handle.
pub type Handle = u64;

pub const FLAG_CREATE: u32 = 1;
pub const FLAG_READ_ONLY: u32 = 2;
pub const FLAG_IN_MEMORY: u32 = 4;

#[allow(unused_variables)]
pub fn db_open(path: &str, flags: u32) -> std::result::Result<Handle, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi db_open").into())
}

#[allow(unused_variables)]
pub fn db_close(db: Handle) -> std::result::Result<(), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi db_close").into())
}

#[allow(unused_variables)]
pub fn session_open(db: Handle, branch: &str, user: &str, database: &str) -> std::result::Result<Handle, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi session_open").into())
}

#[allow(unused_variables)]
pub fn session_close(session: Handle) -> std::result::Result<(), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi session_close").into())
}

#[allow(unused_variables)]
pub fn exec(session: Handle, sql: &str) -> std::result::Result<Vec<FfiResult>, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi exec").into())
}

/// Like [`exec`] but keeps the results of the statements that completed before an error:
/// `(results, error)`.
#[allow(unused_variables)]
pub fn exec_partial(session: Handle, sql: &str) -> std::result::Result<(Vec<FfiResult>, Option<FfiError>), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi exec_partial").into())
}

#[allow(unused_variables)]
pub fn exec_params(session: Handle, sql: &str, params: &[Option<String>]) -> std::result::Result<FfiResult, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi exec_params").into())
}

#[allow(unused_variables)]
pub fn exec_json(session: Handle, sql: &str) -> std::result::Result<String, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi exec_json").into())
}

#[allow(unused_variables)]
pub fn txn_status(session: Handle) -> std::result::Result<u8, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi txn_status").into())
}

#[allow(unused_variables)]
pub fn cancel(session: Handle) -> std::result::Result<(), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi cancel").into())
}

#[allow(unused_variables)]
pub fn branch_create(db: Handle, name: &str, from: &str) -> std::result::Result<(), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi branch_create").into())
}

#[allow(unused_variables)]
pub fn branch_list(db: Handle) -> std::result::Result<Vec<String>, FfiError> {
    Err(PgError::unimplemented("API-15", "ffi branch_list").into())
}

#[allow(unused_variables)]
pub fn branch_delete(db: Handle, name: &str) -> std::result::Result<(), FfiError> {
    Err(PgError::unimplemented("API-15", "ffi branch_delete").into())
}

/// Server version string reported by the engine (`SHOW server_version`).
pub fn version() -> &'static str {
    "18.6"
}

#[allow(dead_code)]
fn _result_alias(_: Result<()>) {}
