//! Frontend (client -> server) messages after startup (lease WIR-01). PG: `protocol.sgml`
//! "Message Formats", `tcop/postgres.c` (`SocketBackend`, `exec_*_message`).
//!
//! A message on the wire is `tag: u8`, `len: Int32` (counts itself, NOT the tag; minimum 4), then
//! `len - 4` body bytes. [`crate::framing::FrameReader`] delivers `(tag, body)`; this module turns that
//! pair into a [`Frontend`] value. Decoding is pure (no I/O) and never panics.
//!
//! ## Body layouts (all integers big-endian)
//! | tag | message | body |
//! |---|---|---|
//! | `Q` | Query | `String sql` |
//! | `P` | Parse | `String name`, `String query`, `Int16 n`, `n x Int32 param_type_oid` (0 = unspecified) |
//! | `B` | Bind | `String portal`, `String statement`, `Int16 nf`, `nf x Int16 param_format`, `Int16 np`, `np x (Int32 len or -1 = NULL, len bytes)`, `Int16 nr`, `nr x Int16 result_format` |
//! | `E` | Execute | `String portal`, `Int32 max_rows` (0 or negative = no limit) |
//! | `D` | Describe | `Byte1 'S' or 'P'`, `String name` |
//! | `C` | Close | `Byte1 'S' or 'P'`, `String name` |
//! | `S` | Sync | empty |
//! | `H` | Flush | empty |
//! | `X` | Terminate | empty |
//! | `d` | CopyData | raw bytes |
//! | `c` | CopyDone | empty |
//! | `f` | CopyFail | `String message` |
//! | `p` | PasswordMessage / SASLInitialResponse / SASLResponse | raw body, interpreted by the auth state ([`parse_password`], [`parse_sasl_initial`]) |
//! | `F` | FunctionCall | raw body kept (answered with 0A000, see `session_loop`) |
//!
//! ## Errors (all `08P01`, PG wording from `pqformat.c`, produced by [`crate::buf::MsgReader`])
//! * `insufficient data left in message` - body shorter than the layout needs;
//! * `invalid message format` - trailing bytes after a fully parsed fixed-layout message;
//! * `invalid string in message` - missing NUL / invalid UTF-8;
//! * negative count (`Int16 n < 0`) for formats/params/oids: `invalid message format`;
//! * Bind parameter length < -1 or larger than the remaining body: `invalid message format`;
//! * Describe/Close kind byte not `S`/`P`: `invalid DESCRIBE message subtype N` / `invalid CLOSE
//!   message subtype N` (N = the byte as a decimal number, PG prints `%d`);
//! * unknown tag: `invalid frontend message type N` (decimal), see [`decode_frontend`].
//!
//! Allocation safety: never `Vec::with_capacity(count)` from an unchecked wire count; cap the
//! reservation at `remaining()` of the body (each element takes at least 2 bytes).
use pgarbor_common::{PgError, Result};

/// Describe / Close target.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// `S`: prepared statement.
    Statement,
    /// `P`: portal.
    Portal,
}

/// A decoded frontend message.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frontend {
    Query(String),
    Parse { name: String, query: String, param_oids: Vec<u32> },
    Bind {
        portal: String,
        statement: String,
        /// Raw Int16 format codes as sent (0/1); validated later with `FormatCode::from_i16`.
        param_formats: Vec<i16>,
        /// `None` = SQL NULL.
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    /// `max_rows <= 0` means "all rows".
    Execute { portal: String, max_rows: i32 },
    Describe { kind: TargetKind, name: String },
    Close { kind: TargetKind, name: String },
    Sync,
    Flush,
    Terminate,
    CopyData(Vec<u8>),
    CopyDone,
    CopyFail(String),
    /// Raw body of a `p` message.
    Password(Vec<u8>),
    /// Raw body of an `F` message.
    FunctionCall(Vec<u8>),
}

impl Frontend {
    /// Message name for logs and transcripts (`"Query"`, `"Parse"`, ...).
    pub fn name(&self) -> &'static str {
        match self {
            Frontend::Query(_) => "Query",
            Frontend::Parse { .. } => "Parse",
            Frontend::Bind { .. } => "Bind",
            Frontend::Execute { .. } => "Execute",
            Frontend::Describe { .. } => "Describe",
            Frontend::Close { .. } => "Close",
            Frontend::Sync => "Sync",
            Frontend::Flush => "Flush",
            Frontend::Terminate => "Terminate",
            Frontend::CopyData(_) => "CopyData",
            Frontend::CopyDone => "CopyDone",
            Frontend::CopyFail(_) => "CopyFail",
            Frontend::Password(_) => "PasswordMessage",
            Frontend::FunctionCall(_) => "FunctionCall",
        }
    }
}

/// Decode one frontend message. `tag` is the type byte, `body` the bytes after the length field.
///
/// Fixed-layout messages (everything except CopyData/Password/FunctionCall, whose bodies are raw)
/// must consume the body exactly (`MsgReader::finish`). Empty-body messages (`S`, `H`, `X`, `c`)
/// with a non-empty body -> `invalid message format`.
/// Unknown tag -> `08P01 invalid frontend message type N` (N decimal of the tag byte).
/// Examples: `decode_frontend(b'Q', b"select 1\0")` -> `Query("select 1")`;
/// `decode_frontend(b'S', b"")` -> `Sync`; `decode_frontend(b'E', b"\0\0\0\0\0")` ->
/// `Execute{portal:"", max_rows:0}`; `decode_frontend(b'Z', b"")` -> error `invalid frontend
/// message type 90`.
#[allow(unused_variables)]
pub fn decode_frontend(tag: u8, body: &[u8]) -> Result<Frontend> {
    Err(PgError::unimplemented("WIR-01", "decode a frontend message (frontend::decode_frontend)"))
}

/// Interpret a `p` body as PasswordMessage: one NUL-terminated UTF-8 `String password` filling the
/// whole body. Missing NUL or invalid UTF-8 -> `08P01 invalid string in message`; bytes after the NUL
/// -> `08P01 invalid message format`.
#[allow(unused_variables)]
pub fn parse_password(body: &[u8]) -> Result<String> {
    Err(PgError::unimplemented("WIR-01", "parse PasswordMessage body (frontend::parse_password)"))
}

/// Interpret a `p` body as SASLInitialResponse: `String mechanism`, `Int32 len` (-1 = no initial
/// response), `len bytes`. Returns `(mechanism, Some(data) | None)`. `len < -1`, `len` larger than the
/// remaining bytes, or trailing bytes after the data -> `08P01 invalid message format`.
#[allow(unused_variables)]
pub fn parse_sasl_initial(body: &[u8]) -> Result<(String, Option<Vec<u8>>)> {
    Err(PgError::unimplemented("WIR-01", "parse SASLInitialResponse body (frontend::parse_sasl_initial)"))
}
