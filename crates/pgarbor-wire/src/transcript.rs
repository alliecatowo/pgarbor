//! Wire transcript recording hook for WIRE-12 diagnostics (lease WIR-20 for the formatting helpers).
//!
//! The server can tee every frontend and backend message of every connection into a
//! [`TranscriptSink`]. The harness compares PGArbor transcripts with transcripts recorded from the
//! PG18 oracle (startup response and prepared-statement frame sequences). The wire crate has no serde,
//! so the JSON Lines form is hand-written by [`to_jsonl`]; the CLI (`serve --transcript FILE`) owns
//! the file.
//!
//! ## What is recorded
//! * startup phase: the raw first packet (`tag = None`, `name = "StartupMessage"` /
//!   `"SSLRequest"` / `"GSSENCRequest"` / `"CancelRequest"`), the one-byte SSL answer
//!   (`tag = None`, name `"SSLResponse"`, body `N`);
//! * every later tagged message in both directions with its exact body bytes.
//! * entries are recorded at the point the bytes are decoded/encoded, not when the TCP segment
//!   moves; `seq` is per connection, starting at 0, shared by both directions.
use std::sync::{Arc, Mutex};

/// Direction of a recorded message.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptDirection {
    ClientToServer,
    ServerToClient,
}

impl TranscriptDirection {
    /// `"C2S"` or `"S2C"`.
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptDirection::ClientToServer => "C2S",
            TranscriptDirection::ServerToClient => "S2C",
        }
    }
}

/// One recorded message.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub conn_id: u64,
    pub seq: u64,
    pub direction: TranscriptDirection,
    /// Type byte; `None` for untagged startup-phase packets.
    pub tag: Option<u8>,
    /// Message name (`"Query"`, `"RowDescription"`, ...).
    pub name: String,
    /// Body bytes after the length field (for backend messages: exactly what was encoded).
    pub body: Vec<u8>,
    /// Microseconds since the connection was accepted.
    pub elapsed_us: u64,
}

/// Receiver of transcript entries; called from connection threads, so it must be thread-safe and fast.
// CONTRACT
pub trait TranscriptSink: Send + Sync {
    fn record(&self, entry: TranscriptEntry);
}

/// Cloneable, debuggable holder used inside `ServerConfig`.
#[derive(Clone)]
pub struct TranscriptHandle(pub Arc<dyn TranscriptSink>);

impl std::fmt::Debug for TranscriptHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TranscriptHandle(..)")
    }
}

/// In-memory sink for tests.
#[derive(Debug, Default)]
pub struct MemoryTranscript {
    entries: Mutex<Vec<TranscriptEntry>>,
}

impl MemoryTranscript {
    pub fn new() -> MemoryTranscript {
        MemoryTranscript::default()
    }
    /// Copy of everything recorded so far.
    pub fn entries(&self) -> Vec<TranscriptEntry> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Names of recorded messages in order, e.g. `["StartupMessage", "AuthenticationOk", ...]`.
    pub fn names(&self) -> Vec<String> {
        self.entries().into_iter().map(|e| e.name).collect()
    }
}

impl TranscriptSink for MemoryTranscript {
    fn record(&self, entry: TranscriptEntry) {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
    }
}

/// One JSON object per entry, no trailing newline, keys in this order:
/// `{"conn":1,"seq":3,"dir":"S2C","tag":"Z","name":"ReadyForQuery","len":5,"body_hex":"49","us":123}`
/// where `tag` is `null` for untagged packets, `len` is the wire length field value (`body.len()+4`),
/// `body_hex` is lowercase hex. Strings are JSON-escaped (`"` `\\` and control characters as `\u00XX`).
/// Non-printable tag bytes are written as `\u00XX`.
#[allow(unused_variables)]
pub fn to_jsonl(entry: &TranscriptEntry) -> pgarbor_common::Result<String> {
    Err(pgarbor_common::PgError::unimplemented("WIR-20", "transcript JSON line (transcript::to_jsonl)"))
}

/// Human one-liner for diagnostics, e.g. `"#3 S2C Z ReadyForQuery status=I"`,
/// `"#1 C2S Q Query 'select 1'"`, `"#7 S2C D DataRow cols=2"`, `"#2 S2C E ErrorResponse 42P01 relation \"t\" does not exist"`.
/// Decodes just enough of the body per message name: Query text (truncated to 80 chars with `...`),
/// ReadyForQuery status byte, ErrorResponse code `C` + message `M`, CommandComplete tag,
/// ParameterStatus `name=value`, DataRow/RowDescription column counts; other messages show only the
/// name and body length (`len=N`).
#[allow(unused_variables)]
pub fn summarize(entry: &TranscriptEntry) -> pgarbor_common::Result<String> {
    Err(pgarbor_common::PgError::unimplemented("WIR-20", "transcript summary line (transcript::summarize)"))
}
