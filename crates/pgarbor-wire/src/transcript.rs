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
pub fn to_jsonl(entry: &TranscriptEntry) -> pgarbor_common::Result<String> {
    let capacity = 96usize.saturating_add(entry.name.len()).saturating_add(entry.body.len().saturating_mul(2));
    let mut out = String::with_capacity(capacity);
    use std::fmt::Write as _;
    write!(out, "{{\"conn\":{},\"seq\":{},\"dir\":\"{}\",\"tag\":", entry.conn_id, entry.seq, entry.direction.as_str())
        .map_err(|_| pgarbor_common::PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR, "failed to format transcript"))?;
    match entry.tag {
        None => out.push_str("null"),
        Some(tag) => {
            out.push('"');
            if tag.is_ascii_graphic() || tag == b' ' {
                if tag == b'"' || tag == b'\\' { out.push('\\'); }
                out.push(tag as char);
            } else {
                write!(out, "\\u00{:02x}", tag).map_err(|_| pgarbor_common::PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR, "failed to format transcript"))?;
            }
            out.push('"');
        }
    }
    write!(out, ",\"name\":\"{}\",\"len\":{},\"body_hex\":\"", json_escape(&entry.name), entry.body.len().saturating_add(4))
        .map_err(|_| pgarbor_common::PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR, "failed to format transcript"))?;
    for byte in &entry.body { write!(out, "{:02x}", byte).map_err(|_| pgarbor_common::PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR, "failed to format transcript"))?; }
    write!(out, "\",\"us\":{}}}", entry.elapsed_us)
        .map_err(|_| pgarbor_common::PgError::new(pgarbor_common::SqlState::INTERNAL_ERROR, "failed to format transcript"))?;
    Ok(out)
}

/// Human one-liner for diagnostics, e.g. `"#3 S2C Z ReadyForQuery status=I"`,
/// `"#1 C2S Q Query 'select 1'"`, `"#7 S2C D DataRow cols=2"`, `"#2 S2C E ErrorResponse 42P01 relation \"t\" does not exist"`.
/// Decodes just enough of the body per message name: Query text (truncated to 80 chars with `...`),
/// ReadyForQuery status byte, ErrorResponse code `C` + message `M`, CommandComplete tag,
/// ParameterStatus `name=value`, DataRow/RowDescription column counts; other messages show only the
/// name and body length (`len=N`).
pub fn summarize(entry: &TranscriptEntry) -> pgarbor_common::Result<String> {
    use crate::buf::MsgReader;
    let fallback = || format!("#{} {} {} len={}", entry.seq, entry.direction.as_str(), entry.name, entry.body.len());
    let summary = match entry.name.as_str() {
        "Query" => {
            let mut r = MsgReader::new(&entry.body);
            match r.read_cstr() {
                Ok(q) => {
                    let clipped: String = q.chars().take(80).collect();
                    let text = if q.chars().count() > 80 { format!("{}...", clipped) } else { clipped };
                    Some(format!("#{} {} {} '{}'", entry.seq, entry.direction.as_str(), entry.name, text))
                }
                Err(_) => None,
            }
        }
        "ReadyForQuery" => {
            let mut r = MsgReader::new(&entry.body);
            r.read_u8().ok().map(|b| format!("#{} {} {} status={}", entry.seq, entry.direction.as_str(), entry.name, char::from(b)))
        }
        "ErrorResponse" => {
            let mut r = MsgReader::new(&entry.body);
            let mut code = None;
            let mut message = None;
            let mut valid = true;
            let mut terminated = false;
            while r.remaining() > 0 {
                let field = match r.read_u8() { Ok(0) => { terminated = true; break; }, Ok(v) => v, Err(_) => { valid = false; break; } };
                match r.read_cstr() {
                    Ok(v) => { if field == b'C' { code = Some(v); } else if field == b'M' { message = Some(v); } }
                    Err(_) => { valid = false; break; }
                }
            }
            if valid && terminated { match (code, message) { (Some(c), Some(m)) => Some(format!("#{} {} {} {} {}", entry.seq, entry.direction.as_str(), entry.name, c, m)), _ => None } } else { None }
        }
        "CommandComplete" => {
            let mut r = MsgReader::new(&entry.body);
            r.read_cstr().ok().map(|tag| format!("#{} {} {} {}", entry.seq, entry.direction.as_str(), entry.name, tag))
        }
        "ParameterStatus" => {
            let mut r = MsgReader::new(&entry.body);
            match (r.read_cstr(), r.read_cstr()) { (Ok(n), Ok(v)) => Some(format!("#{} {} {} {}={}", entry.seq, entry.direction.as_str(), entry.name, n, v)), _ => None }
        }
        "DataRow" | "RowDescription" => {
            let mut r = MsgReader::new(&entry.body);
            r.read_i16().ok().filter(|n| *n >= 0).map(|n| format!("#{} {} {} cols={}", entry.seq, entry.direction.as_str(), entry.name, n))
        }
        _ => None,
    };
    Ok(summary.unwrap_or_else(fallback))
}

fn json_escape(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' => { let _ = write!(out, "\\u00{:02x}", c as u32); }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, tag: Option<u8>, body: &[u8]) -> TranscriptEntry {
        TranscriptEntry { conn_id: 1, seq: 3, direction: TranscriptDirection::ServerToClient,
            tag, name: name.into(), body: body.into(), elapsed_us: 123 }
    }

    #[test]
    fn jsonl_ready_for_query_exact() {
        assert_eq!(to_jsonl(&entry("ReadyForQuery", Some(b'Z'), b"I")).unwrap(),
            "{\"conn\":1,\"seq\":3,\"dir\":\"S2C\",\"tag\":\"Z\",\"name\":\"ReadyForQuery\",\"len\":5,\"body_hex\":\"49\",\"us\":123}");
    }

    #[test]
    fn jsonl_escapes_quotes_and_controls() {
        let e = entry("a\"\\\n", Some(0), b"");
        let json = to_jsonl(&e).unwrap();
        assert!(json.contains("a\\\"\\\\\\u000a"));
        assert!(json.contains("\\u0000"));
    }

    #[test]
    fn message_summaries_decode_expected_fields() {
        assert_eq!(summarize(&entry("Query", Some(b'Q'), b"select 1\0")).unwrap(), "#3 S2C Query 'select 1'");
        assert_eq!(summarize(&entry("ErrorResponse", Some(b'E'), b"C42P01\0Mrelation \"t\" does not exist\0\0")).unwrap(), "#3 S2C ErrorResponse 42P01 relation \"t\" does not exist");
        assert_eq!(summarize(&entry("ReadyForQuery", Some(b'Z'), b"I")).unwrap(), "#3 S2C ReadyForQuery status=I");
        assert_eq!(summarize(&entry("DataRow", Some(b'D'), b"\0\x02")).unwrap(), "#3 S2C DataRow cols=2");
        assert_eq!(summarize(&entry("RowDescription", Some(b'T'), b"\0\x02")).unwrap(), "#3 S2C RowDescription cols=2");
        assert_eq!(summarize(&entry("CommandComplete", Some(b'C'), b"SELECT 1\0")).unwrap(), "#3 S2C CommandComplete SELECT 1");
        assert_eq!(summarize(&entry("ParameterStatus", Some(b'S'), b"client_encoding\0UTF8\0")).unwrap(), "#3 S2C ParameterStatus client_encoding=UTF8");
    }

    #[test]
    fn query_summary_truncates_after_80_characters() {
        let query = format!("{}\0", "x".repeat(81));
        assert_eq!(summarize(&entry("Query", Some(b'Q'), query.as_bytes())).unwrap(), format!("#3 S2C Query '{}'", format!("{}...", "x".repeat(80))));
    }

    #[test]
    fn malformed_summary_falls_back_without_panicking() {
        assert_eq!(summarize(&entry("DataRow", Some(b'D'), b"x")).unwrap(), "#3 S2C DataRow len=1");
    }
}
