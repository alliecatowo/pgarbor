//! Public value types of the embedded API (CLI-08, ARCHITECTURE §11). Fully implemented, frozen (v1).
//! Everything the wire layer (`pgarbor-wire`, which sees only `pgarbor-common`, `pgarbor-types` and
//! this crate) needs is defined or re-exported here.
use std::sync::Arc;

pub use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
pub use pgarbor_storage::{MemVfs, OsVfs, SyncMode, VerifyLevel, Vfs};
pub use pgarbor_types::{Datum, TypeId};

/// Transaction status byte of ReadyForQuery.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// `I`: not in a transaction block.
    Idle,
    /// `T`: in a transaction block.
    InTransaction,
    /// `E`: in a failed transaction block (only ROLLBACK/COMMIT/ROLLBACK TO accepted).
    Failed,
}

impl TxnStatus {
    pub fn byte(self) -> u8 {
        match self {
            TxnStatus::Idle => b'I',
            TxnStatus::InTransaction => b'T',
            TxnStatus::Failed => b'E',
        }
    }
}

/// Wire format code of a parameter or result column.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatCode {
    Text,
    Binary,
}

impl FormatCode {
    /// Parse a wire format code (0 text, 1 binary); anything else is 08P01 `unsupported format code: N`.
    pub fn from_i16(code: i16) -> Result<FormatCode> {
        match code {
            0 => Ok(FormatCode::Text),
            1 => Ok(FormatCode::Binary),
            n => Err(PgError::new(SqlState::PROTOCOL_VIOLATION, format!("unsupported format code: {n}"))),
        }
    }
    pub fn as_i16(self) -> i16 {
        match self {
            FormatCode::Text => 0,
            FormatCode::Binary => 1,
        }
    }
}

/// One result column as sent in RowDescription (the format code is chosen at Bind time).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_oid: Oid,
    /// `atttypmod`, -1 when none.
    pub type_mod: i32,
    /// `pg_type.typlen` (-1 varlena, -2 cstring) - RowDescription "data type size".
    pub type_len: i16,
    /// OID of the source table or 0.
    pub table_oid: Oid,
    /// Attribute number in `table_oid` or 0.
    pub column_attnum: i16,
}

/// Result of one statement.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum StatementResult {
    /// Empty query string / `;`: EmptyQueryResponse.
    Empty,
    /// CommandComplete only (`CREATE TABLE`, `INSERT 0 3`, `BEGIN`, ...).
    Command { tag: String },
    /// RowDescription + DataRows + CommandComplete (`SELECT 5`, `SHOW`, RETURNING, `EXPLAIN`).
    Rows { columns: Vec<ColumnInfo>, rows: Vec<Vec<Datum>>, tag: String },
}

impl StatementResult {
    /// Command tag (`""` for `Empty`).
    pub fn tag(&self) -> &str {
        match self {
            StatementResult::Empty => "",
            StatementResult::Command { tag } | StatementResult::Rows { tag, .. } => tag,
        }
    }
    /// Rows of a `Rows` result (empty slice otherwise).
    pub fn rows(&self) -> &[Vec<Datum>] {
        match self {
            StatementResult::Rows { rows, .. } => rows,
            _ => &[],
        }
    }
}

/// One statement's result plus the side messages that go with it, in wire order: NoticeResponses
/// (raised before the statement's rows/CommandComplete), then the result, then ParameterStatus
/// changes.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct StatementOutput {
    pub result: StatementResult,
    pub notices: Vec<PgError>,
    /// `(name, value)` of GUC_REPORT parameters that changed during the statement.
    pub param_status: Vec<(String, String)>,
}

/// Outcome of a simple Query message: the results of the statements that completed, then optionally
/// the error that stopped the rest.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SimpleQueryOutput {
    pub statements: Vec<StatementOutput>,
    /// The failing statement's error, if any (remaining statements were NOT run).
    pub error: Option<PgError>,
    /// Notices raised by the failing statement before it failed (sent before the ErrorResponse).
    pub error_notices: Vec<PgError>,
    /// ParameterStatus changes caused by the rollback after an error.
    pub error_param_status: Vec<(String, String)>,
    /// Status byte for ReadyForQuery.
    pub status: TxnStatus,
}

impl SimpleQueryOutput {
    /// `Ok(statement outputs)` or the error.
    pub fn into_result(self) -> Result<Vec<StatementOutput>> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.statements),
        }
    }
}

/// Describe(Statement) answer.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct StatementDescription {
    /// ParameterDescription type OIDs.
    pub param_oids: Vec<Oid>,
    /// RowDescription columns, `None` = NoData.
    pub columns: Option<Vec<ColumnInfo>>,
}

/// One DataRow: per column the encoded bytes (`None` = NULL) in the portal's result format.
pub type EncodedRow = Vec<Option<Vec<u8>>>;

/// How an Execute ended.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortalStatus {
    /// Row limit reached: PortalSuspended.
    Suspended,
    /// CommandComplete with this tag.
    Complete { tag: String },
    /// EmptyQueryResponse.
    Empty,
}

/// Result of one Execute message.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct PortalOutput {
    pub rows: Vec<EncodedRow>,
    pub status: PortalStatus,
    pub notices: Vec<PgError>,
    pub param_status: Vec<(String, String)>,
}

/// An asynchronous notification (LISTEN/NOTIFY) ready for delivery to this session.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub channel: String,
    pub payload: String,
    /// Backend pid of the notifying session.
    pub pid: i32,
}

/// Options for opening/creating a database file.
// CONTRACT
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Shared read-only open: every write fails with 25006.
    pub read_only: bool,
    pub sync: SyncMode,
    /// Page cache capacity in pages.
    pub cache_pages: usize,
    pub verify: VerifyLevel,
    /// File system. `None` = the OS. Use `MemVfs` for in-memory databases and WASM (the host supplies
    /// a `Vfs` over its own storage).
    pub vfs: Option<Arc<dyn Vfs>>,
    /// Page size for `create`.
    pub page_size: u32,
    /// Name of the first branch for `create`.
    pub initial_branch: String,
    /// `max_connections`.
    pub max_sessions: usize,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            read_only: false,
            sync: SyncMode::Full,
            cache_pages: 8192,
            verify: VerifyLevel::Roots,
            vfs: None,
            page_size: 8192,
            initial_branch: "main".to_string(),
            max_sessions: 100,
        }
    }
}

impl OpenOptions {
    /// Options for a throw-away in-memory database: a fresh `MemVfs`, `SyncMode::Off`.
    pub fn in_memory() -> OpenOptions {
        OpenOptions { vfs: Some(Arc::new(MemVfs::new())), sync: SyncMode::Off, ..OpenOptions::default() }
    }

    /// The core engine options these map to.
    pub fn to_engine_options(&self) -> pgarbor_core::EngineOptions {
        pgarbor_core::EngineOptions {
            vfs: self.vfs.clone(),
            sync: self.sync,
            cache_pages: self.cache_pages,
            read_only: self.read_only,
            verify: self.verify,
            page_size: self.page_size,
            db_uuid: None,
            initial_branch: self.initial_branch.clone(),
            max_sessions: self.max_sessions,
        }
    }
}

/// Who connects and how (embedded `Database::session` and the wire server's startup packet).
// CONTRACT
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Role name; must exist and have LOGIN (28000 `role "x" does not exist` otherwise).
    pub user: String,
    /// Logical database name (v1: only `postgres`; others 3D000 `database "x" does not exist`).
    pub database: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub client_port: Option<i32>,
    /// Length of the cancel secret in BackendKeyData (4 for protocol 3.0, 32 for 3.2).
    pub cancel_key_len: usize,
    /// Startup GUCs from the `options` parameter (`-c name=value`) and unrecognized startup
    /// parameters that are valid GUC names, applied in order with session scope.
    pub startup_gucs: Vec<(String, String)>,
}

impl Default for SessionOptions {
    fn default() -> SessionOptions {
        SessionOptions {
            user: "postgres".to_string(),
            database: "postgres".to_string(),
            application_name: String::new(),
            client_addr: None,
            client_port: None,
            cancel_key_len: 4,
            startup_gucs: Vec::new(),
        }
    }
}

/// What the wire layer needs to authenticate a role (SCRAM/MD5/cleartext).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleAuth {
    pub name: String,
    pub oid: Oid,
    pub can_login: bool,
    pub superuser: bool,
    /// Stored verifier (`SCRAM-SHA-256$4096:salt$stored:server` or `md5...`); None = no password.
    pub password_verifier: Option<String>,
    /// `rolvaliduntil` as timestamptz microseconds since 2000-01-01.
    pub valid_until: Option<i64>,
    pub conn_limit: i32,
}

/// Branch summary for listings.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    /// Hyphenated UUID.
    pub id: String,
    pub parent: Option<String>,
    pub is_default: bool,
    pub deleted: bool,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub head_seq: u64,
    /// Named snapshot labels.
    pub snapshots: Vec<String>,
}

/// Handle that cancels the running statement of one session from another thread (the wire layer's
/// CancelRequest, `pg_cancel_backend`). Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct CancelToken {
    pub(crate) shared: Arc<pgarbor_core::SessionShared>,
}

impl CancelToken {
    /// Request cancellation of the current statement (57014 `canceling statement due to user request`).
    pub fn cancel(&self) {
        self.shared.interrupts.request_cancel();
    }
    /// Request termination of the session (FATAL 57P01).
    pub fn terminate(&self) {
        self.shared.interrupts.request_terminate();
    }
    /// Backend pid.
    pub fn pid(&self) -> i32 {
        self.shared.id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bytes_and_format_codes() {
        assert_eq!(TxnStatus::Idle.byte(), b'I');
        assert_eq!(TxnStatus::Failed.byte(), b'E');
        assert_eq!(FormatCode::from_i16(1).unwrap(), FormatCode::Binary);
        assert_eq!(FormatCode::from_i16(7).unwrap_err().sqlstate.code(), "08P01");
    }

    #[test]
    fn in_memory_options() {
        let o = OpenOptions::in_memory();
        assert!(o.vfs.is_some());
        assert_eq!(o.sync, SyncMode::Off);
    }
}
