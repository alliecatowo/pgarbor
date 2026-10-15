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
use crate::buf::MsgReader;
use pgarbor_common::{PgError, Result, SqlState};

fn invalid_format<T>() -> Result<T> {
    Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid message format"))
}

fn read_codes(r: &mut MsgReader<'_>) -> Result<Vec<i16>> {
    let n = r.read_i16()?;
    if n < 0 { return invalid_format(); }
    let n = n as usize;
    if n > r.remaining() / 2 { return invalid_format(); }
    let mut values = Vec::new();
    for _ in 0..n { values.push(r.read_i16()?); }
    Ok(values)
}

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
pub fn decode_frontend(tag: u8, body: &[u8]) -> Result<Frontend> {
    let mut r = MsgReader::new(body);
    let message = match tag {
        b'Q' => Frontend::Query(r.read_cstr()?.to_owned()),
        b'P' => {
            let name = r.read_cstr()?.to_owned();
            let query = r.read_cstr()?.to_owned();
            let n = r.read_i16()?;
            if n < 0 { return invalid_format(); }
            let n = n as usize;
            if n > r.remaining() / 4 { return invalid_format(); }
            let mut param_oids = Vec::new();
            for _ in 0..n { param_oids.push(r.read_u32()?); }
            Frontend::Parse { name, query, param_oids }
        }
        b'B' => {
            let portal = r.read_cstr()?.to_owned();
            let statement = r.read_cstr()?.to_owned();
            let param_formats = read_codes(&mut r)?;
            let n = r.read_i16()?;
            if n < 0 { return invalid_format(); }
            let n = n as usize;
            if n > r.remaining() / 4 { return invalid_format(); }
            let mut params = Vec::new();
            for _ in 0..n {
                let len = r.read_i32()?;
                if len == -1 { params.push(None); }
                else if len < 0 || len as usize > r.remaining() { return invalid_format(); }
                else { params.push(Some(r.take(len as usize)?.to_vec())); }
            }
            let result_formats = read_codes(&mut r)?;
            Frontend::Bind { portal, statement, param_formats, params, result_formats }
        }
        b'E' => Frontend::Execute { portal: r.read_cstr()?.to_owned(), max_rows: r.read_i32()? },
        b'D' | b'C' => {
            let subtype = r.read_u8()?;
            let kind = match subtype {
                b'S' => TargetKind::Statement,
                b'P' => TargetKind::Portal,
                other => return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, format!("invalid {} message subtype {}", if tag == b'D' { "DESCRIBE" } else { "CLOSE" }, other))),
            };
            let name = r.read_cstr()?.to_owned();
            if tag == b'D' { Frontend::Describe { kind, name } } else { Frontend::Close { kind, name } }
        }
        b'S' => Frontend::Sync,
        b'H' => Frontend::Flush,
        b'X' => Frontend::Terminate,
        b'd' => Frontend::CopyData(r.read_rest().to_vec()),
        b'c' => Frontend::CopyDone,
        b'f' => Frontend::CopyFail(r.read_cstr()?.to_owned()),
        b'p' => Frontend::Password(r.read_rest().to_vec()),
        b'F' => Frontend::FunctionCall(r.read_rest().to_vec()),
        other => return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, format!("invalid frontend message type {other}"))),
    };
    r.finish()?;
    Ok(message)
}

/// Interpret a `p` body as PasswordMessage: one NUL-terminated UTF-8 `String password` filling the
/// whole body. Missing NUL or invalid UTF-8 -> `08P01 invalid string in message`; bytes after the NUL
/// -> `08P01 invalid message format`.
pub fn parse_password(body: &[u8]) -> Result<String> {
    let mut r = MsgReader::new(body);
    let password = r.read_cstr()?.to_owned();
    r.finish()?;
    Ok(password)
}

/// Interpret a `p` body as SASLInitialResponse: `String mechanism`, `Int32 len` (-1 = no initial
/// response), `len bytes`. Returns `(mechanism, Some(data) | None)`. `len < -1`, `len` larger than the
/// remaining bytes, or trailing bytes after the data -> `08P01 invalid message format`.
pub fn parse_sasl_initial(body: &[u8]) -> Result<(String, Option<Vec<u8>>)> {
    let mut r = MsgReader::new(body);
    let mechanism = r.read_cstr()?.to_owned();
    let len = r.read_i32()?;
    let data = if len == -1 { None }
        else if len < -1 || len as usize > r.remaining() { return invalid_format(); }
        else { Some(r.take(len as usize)?.to_vec()) };
    r.finish()?;
    Ok((mechanism, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_frontend_message_shapes() {
        assert_eq!(decode_frontend(b'Q', b"select 1\0").unwrap(), Frontend::Query("select 1".into()));
        assert_eq!(decode_frontend(b'S', b"").unwrap(), Frontend::Sync);
        assert_eq!(decode_frontend(b'E', b"\0\0\0\0\0").unwrap(), Frontend::Execute { portal: String::new(), max_rows: 0 });
        assert_eq!(decode_frontend(b'p', b"raw").unwrap(), Frontend::Password(b"raw".to_vec()));
        assert_eq!(parse_password(b"pw\0").unwrap(), "pw");
        assert_eq!(parse_sasl_initial(b"SCRAM-SHA-256\0\0\0\0\x03abc").unwrap(), ("SCRAM-SHA-256".into(), Some(b"abc".to_vec())));
    }

    #[test]
    fn bind_prefixes_and_bad_tags_are_protocol_errors() {
        let body = b"\0\0\0\0\0\0\0\0"; // empty portal/statement and three zero counts
        for end in 0..body.len() {
            assert_eq!(decode_frontend(b'B', &body[..end]).unwrap_err().sqlstate.code(), "08P01");
        }
        let err = decode_frontend(b'Z', b"").unwrap_err();
        assert_eq!(err.message, "invalid frontend message type 90");
        assert_eq!(err.sqlstate.code(), "08P01");
    }
}
