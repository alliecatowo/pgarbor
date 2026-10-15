//! Engine and session options. CONTRACT. Data only, fully implemented.
use pgarbor_branches::BranchRef;
use pgarbor_common::{DatabaseUuid, Oid};
use pgarbor_storage::{SyncMode, VerifyLevel, Vfs};
use std::sync::Arc;

// CONTRACT
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// File system; `None` = the real OS file system (`OsVfs`). Tests pass `MemVfs`/`FaultVfs`.
    pub vfs: Option<Arc<dyn Vfs>>,
    pub sync: SyncMode,
    /// Page cache capacity in pages (default 8192).
    pub cache_pages: usize,
    /// Open read-only (shared lock; every write → 25006 `cannot execute <cmd> in a read-only
    /// transaction`, all branch admin → 25006 storage `read_only`).
    pub read_only: bool,
    /// Verification at open (default `Roots`).
    pub verify: VerifyLevel,
    /// Page size for `create` (default 8192).
    pub page_size: u32,
    /// Fixed database UUID for `create` (tests); `None` = random.
    pub db_uuid: Option<DatabaseUuid>,
    /// Name of the first branch on `create` (default "main").
    pub initial_branch: String,
    /// Maximum concurrent sessions (PG `max_connections`, default 100). Exceeding →
    /// 53300 `sorry, too many clients already` (FATAL).
    pub max_sessions: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            vfs: None,
            sync: SyncMode::Full,
            cache_pages: 8192,
            read_only: false,
            verify: VerifyLevel::Roots,
            page_size: 8192,
            db_uuid: None,
            initial_branch: "main".to_string(),
            max_sessions: 100,
        }
    }
}

// CONTRACT
/// What a client session binds to at startup (RFC 001 §8: a connection binds to ONE branch for its
/// lifetime). `database_oid` comes from the catalog (pgarbor crate resolves the startup `database`
/// name); v1 supports only `pgarbor_branches::POSTGRES_DATABASE_OID` (5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRequest {
    pub branch: BranchRef,
    pub database_oid: Oid,
    pub database_name: String,
    pub user: String,
    pub application_name: String,
    /// `pg_stat_activity.client_addr` (None = local/embedded).
    pub client_addr: Option<String>,
    pub client_port: Option<i32>,
    /// Cancel secret length: 4 for protocol 3.0, up to 256 bytes for 3.2 (PG18 uses 32).
    pub cancel_key_len: usize,
}

impl SessionRequest {
    /// Embedded default: branch `branch`, database "postgres" (oid 5), user "postgres".
    pub fn embedded(branch: &str) -> SessionRequest {
        SessionRequest {
            branch: BranchRef::name(branch),
            database_oid: pgarbor_branches::POSTGRES_DATABASE_OID,
            database_name: "postgres".to_string(),
            user: "postgres".to_string(),
            application_name: String::new(),
            client_addr: None,
            client_port: None,
            cancel_key_len: 4,
        }
    }
}
