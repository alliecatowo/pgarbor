//! JSON output schemas of every command (`--json`). FROZEN (v1): field names are a contract for
//! agents and CI. Fully implemented data types, no lease. Reports that already have a frozen
//! `Serialize` form in a lower crate are emitted as-is (`pgarbor_branches::CompareReport`,
//! `DetachReport`, `ResetReport`, `pgarbor_storage::{DoctorReport, SpaceReport, CompactReport,
//! PageInspection}`).
//!
//! Output conventions: success -> ONE JSON document on stdout (pretty-printed; `serve` and
//! `branch exec` emit single-line events instead). Errors -> one JSON object on stderr
//! (`ctx::CliError::render`). Timestamps are unix milliseconds, ids are hyphenated lowercase UUIDs,
//! sizes are bytes, absent values are `null`.
use serde::{Deserialize, Serialize};

/// `pgarbor init`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    pub path: String,
    pub db_uuid: String,
    pub branch: String,
    pub page_size: u32,
    /// False when `--if-not-exists` found a valid existing file.
    pub created: bool,
}

/// One listener of a running `serve` process (also the discovery-file element).
// CONTRACT
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListenerInfo {
    pub branch: String,
    pub host: String,
    pub port: u16,
    /// Unix socket directory, if any.
    pub socket_dir: Option<String>,
    /// True for listeners started on demand by `branch exec` (not the primary `serve --branch`).
    pub on_demand: bool,
}

/// Where the control socket of a serving process lives.
// CONTRACT
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlEndpoint {
    /// Unix-domain socket path (newline-delimited JSON protocol, see `control.rs`).
    pub unix_path: String,
}

/// The `<db>.serve.json` discovery file written by `pgarbor serve` and read by `status`,
/// `branch exec` and lock-error diagnostics.
// CONTRACT
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServeInfo {
    pub format: String,
    pub pid: u32,
    pub db_path: String,
    pub db_uuid: String,
    pub started_unix_ms: u64,
    pub listeners: Vec<ListenerInfo>,
    pub control: Option<ControlEndpoint>,
}

/// `serve` start event (also the `--json` replacement of the human line).
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ListeningEvent {
    /// Always `"listening"`.
    pub event: &'static str,
    pub host: String,
    pub port: u16,
    pub url: String,
    pub socket_dir: Option<String>,
    pub branch: String,
    pub pid: u32,
    pub db_path: String,
}

/// `serve` stop event (`--json` only).
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct StoppedEvent {
    /// Always `"stopped"`.
    pub event: &'static str,
    pub signal: Option<i32>,
    pub connections_closed: usize,
}

/// `pgarbor status`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub path: String,
    pub exists: bool,
    pub file_len: Option<u64>,
    /// False when the file could not be opened for a read-only look (corrupt, wrong format, locked
    /// exclusively and no fallback).
    pub readable: bool,
    pub db_uuid: Option<String>,
    pub format_version: Option<u32>,
    pub page_size: Option<u32>,
    pub commit_seq: Option<u64>,
    pub default_branch: Option<String>,
    pub branches: Option<usize>,
    pub deleted_branches: Option<usize>,
    /// Another process holds the file; its discovery info when available.
    pub held_by: Option<ServeInfo>,
    pub problems: Vec<String>,
}

/// Branch row used by `branch list`, `create`, `rename`, `delete`, ...
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct BranchRow {
    pub name: String,
    pub id: String,
    pub parent: Option<String>,
    pub is_default: bool,
    /// `"active"` or `"deleted"`.
    pub state: String,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub head_seq: u64,
    pub snapshots: Vec<String>,
    /// Present with `branch list --space`.
    pub space: Option<BranchSpaceRow>,
}

// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct BranchSpaceRow {
    pub pages: u64,
    pub exclusive_pages: u64,
    pub bytes: u64,
    pub exclusive_bytes: u64,
}

/// `branch list`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct BranchListReport {
    pub db_path: String,
    pub branches: Vec<BranchRow>,
}

/// `branch snapshot list`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRow {
    pub label: String,
    /// `"user"`, `"reset_recovery"` or `"system"` (Debug name of `SnapshotKind` lowercased).
    pub kind: String,
    pub created_seq: u64,
    pub created_unix_ms: u64,
}

/// `branch link`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct LinkReport {
    pub handle_path: String,
    pub db_path: String,
    pub db_uuid: String,
    pub branch: String,
    pub branch_id: String,
}

/// `branch resolve`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ResolveReport {
    pub handle_path: String,
    /// Where the master database was found.
    pub db_path: String,
    pub db_uuid: String,
    pub branch_id: String,
    /// Current name of the branch (may differ from the handle's hint after a rename).
    pub branch_name: Option<String>,
    pub branch_exists: bool,
    /// The master was not at the handle's hint.
    pub relocated: bool,
    pub hint_rewritten: bool,
}

/// `branch delete`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct DeleteReport {
    pub name: String,
    pub id: String,
    /// Data retained for `branch restore` (default) or dropped.
    pub retained: bool,
}

/// `branch purge`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct PurgeReport {
    pub purged: usize,
}

/// One statement of `exec` / `shell -c`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ExecStatementJson {
    pub index: usize,
    pub sql: String,
    /// Command tag (`SELECT 3`, `INSERT 0 1`, ...); `null` for an empty statement.
    pub tag: Option<String>,
    pub columns: Option<Vec<ExecColumnJson>>,
    /// JSON rows: bool -> boolean, int2/4/8 and finite float4/8 -> number, json/jsonb -> embedded
    /// value, numeric and everything else -> string (PG text output), NULL -> null.
    pub rows: Option<Vec<Vec<serde_json::Value>>>,
    pub notices: Vec<String>,
    pub error: Option<ErrorJson>,
}

// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ExecColumnJson {
    pub name: String,
    pub type_oid: u32,
}

// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ErrorJson {
    pub sqlstate: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<u32>,
}

/// `exec --json`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ExecReport {
    pub branch: String,
    pub statements: Vec<ExecStatementJson>,
    pub ok: bool,
}

/// `import`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub branch: String,
    pub statements_run: u64,
    pub copy_rows: u64,
    pub errors: Vec<ScriptErrorJson>,
    pub committed: bool,
}

// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ScriptErrorJson {
    /// 1-based statement number in the script.
    pub statement: u64,
    /// 1-based line where the statement starts.
    pub line: u64,
    pub sqlstate: String,
    pub message: String,
}

/// One `doctor` check.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    /// `"ok"`, `"warn"`, `"fail"` or `"skip"`.
    pub status: String,
    pub message: String,
    pub hint: Option<String>,
}

/// `doctor --json`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct DoctorOutput {
    pub path: Option<String>,
    pub ok: bool,
    pub checks: Vec<DoctorCheck>,
}

/// `version`.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct VersionReport {
    pub name: &'static str,
    pub version: &'static str,
    pub postgres_compat: &'static str,
    pub wire_protocol: &'static str,
    pub container_format_version: u32,
    pub os: &'static str,
    pub arch: &'static str,
}

/// `inspect` (the space/page parts reuse the storage reports).
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct InspectOutput {
    pub path: String,
    pub db_uuid: Option<String>,
    pub format_version: Option<u32>,
    pub page_size: Option<u32>,
    pub commit_seq: Option<u64>,
    pub file_len: u64,
    pub page_count: Option<u64>,
    pub default_branch: Option<String>,
    pub branches: Vec<BranchRow>,
    pub space: Option<pgarbor_storage::SpaceReport>,
    pub page: Option<pgarbor_storage::doctor::PageInspection>,
}

/// `branch exec` summary event on stderr (`--json`) after the child exits.
// CONTRACT
#[derive(Debug, Clone, Serialize)]
pub struct ExecExitEvent {
    /// Always `"exited"`.
    pub event: &'static str,
    pub branch: String,
    pub exit_code: u8,
    pub signal: Option<i32>,
    /// A listener started and stopped by this invocation (false when an existing one was reused).
    pub owned_listener: bool,
}
