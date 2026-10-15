//! Doctor report types. CONTRACT. Fully implemented (data + JSON).
use crate::container::options::{RecoveryReport, VerifyLevel};
use crate::format::page::PageHeader;
use crate::format::superblock::SlotStatus;
use crate::gc::trace::{KindCounts, RootUsage};
use pgarbor_common::{CommitSeq, DatabaseUuid, PageId};
use serde::Serialize;
use std::path::PathBuf;

// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum IssueSeverity {
    /// Data is unreadable or an invariant is broken (checksum, ordering, dangling pointer).
    Error,
    /// Suspicious but readable (e.g. empty non-root leaf, overflow value small enough to inline).
    Warning,
    /// Informational (e.g. unreachable garbage page that does not verify; uncommitted tail).
    Info,
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub severity: IssueSeverity,
    pub page: Option<PageId>,
    /// Label of the root whose tree contains the page (e.g. "directory", "main").
    pub tree: Option<String>,
    pub message: String,
}

impl Issue {
    pub fn error(page: Option<PageId>, tree: Option<&str>, message: impl Into<String>) -> Issue {
        Issue { severity: IssueSeverity::Error, page, tree: tree.map(str::to_string), message: message.into() }
    }
    pub fn warning(page: Option<PageId>, tree: Option<&str>, message: impl Into<String>) -> Issue {
        Issue { severity: IssueSeverity::Warning, page, tree: tree.map(str::to_string), message: message.into() }
    }
    pub fn info(page: Option<PageId>, tree: Option<&str>, message: impl Into<String>) -> Issue {
        Issue { severity: IssueSeverity::Info, page, tree: tree.map(str::to_string), message: message.into() }
    }
}

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub path: PathBuf,
    pub level: VerifyLevel,
    pub file_len: u64,
    pub slots: [SlotStatus; 2],
    /// Adopted superblock fields (None when no valid superblock exists).
    pub page_size: Option<u32>,
    pub format_version: Option<u32>,
    pub db_uuid: Option<DatabaseUuid>,
    pub commit_seq: Option<CommitSeq>,
    pub directory_root: Option<PageId>,
    pub page_count: Option<u64>,
    /// Recovery outcome when produced from an open container.
    pub recovery: Option<RecoveryReport>,
    /// Pages whose checksum/structure was verified.
    pub pages_checked: u64,
    pub reachable_pages: u64,
    pub unreachable_pages: u64,
    pub kinds: KindCounts,
    pub roots: Vec<RootUsage>,
    pub issues: Vec<Issue>,
}

impl DoctorReport {
    /// No `Error` issues.
    pub fn is_ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == IssueSeverity::Error)
    }
    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.severity == IssueSeverity::Error)
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

// CONTRACT
/// `inspect --space` (RFC 001 §5.3): logical vs physical usage and retention per root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpaceReport {
    pub page_size: u32,
    pub commit_seq: CommitSeq,
    /// Physical file length.
    pub file_bytes: u64,
    /// Always 8192 (two superblock slots).
    pub superblock_bytes: u64,
    pub page_count: u64,
    /// `8192 + page_count * page_size`.
    pub committed_bytes: u64,
    /// `file_bytes - committed_bytes` (uncommitted tail; 0 after a clean open), saturating.
    pub tail_bytes: u64,
    pub reachable_pages: u64,
    pub reachable_bytes: u64,
    /// Committed but unreachable pages (old versions no root needs) = page_count - reachable.
    pub garbage_pages: u64,
    pub garbage_bytes: u64,
    /// Bytes `compact()` would free ≈ `file_bytes - (8192 + reachable_pages * page_size)`.
    pub reclaimable_bytes: u64,
    pub kinds: KindCounts,
    /// Per live root: pages reachable / exclusively retained.
    pub roots: Vec<RootUsage>,
    /// Active snapshot pins `(commit_seq, count)` (process-local).
    pub pins: Vec<(CommitSeq, usize)>,
}

// CONTRACT
/// Decoded view of one page for `pgarbor inspect --page N`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PageInspection {
    pub id: PageId,
    pub header: Option<PageHeader>,
    /// Verification error message, if the page does not verify.
    pub error: Option<String>,
    pub kind: Option<String>,
    pub level: Option<u16>,
    pub entries: Option<usize>,
    /// Lowercase hex of the first/last key (truncated to 64 bytes) for node pages.
    pub first_key_hex: Option<String>,
    pub last_key_hex: Option<String>,
    /// Children (internal) or overflow `next` (overflow pages).
    pub children: Vec<PageId>,
    pub payload_len: Option<u32>,
}
