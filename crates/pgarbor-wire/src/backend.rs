//! Backend (server -> client) messages (lease WIR-02). PG: `protocol.sgml` "Message Formats",
//! `libpq/pqformat.c`, `access/common/printtup.c` (RowDescription/DataRow), `utils/error/elog.c`
//! (`send_message_to_frontend`).
//!
//! [`Backend`] is the typed form; [`encode_backend`] appends the exact bytes to a buffer (tag, Int32
//! length counting itself but not the tag, body). [`Backend::tag`] gives the type byte.
//!
//! ## Body layouts (big-endian)
//! | message | tag | body |
//! |---|---|---|
//! | AuthenticationOk | `R` | `Int32 0` |
//! | AuthenticationCleartextPassword | `R` | `Int32 3` |
//! | AuthenticationMD5Password | `R` | `Int32 5`, `Byte4 salt` |
//! | AuthenticationSASL | `R` | `Int32 10`, then each mechanism as NUL-terminated string, then one extra `0x00` terminator |
//! | AuthenticationSASLContinue | `R` | `Int32 11`, `data` (rest of body) |
//! | AuthenticationSASLFinal | `R` | `Int32 12`, `data` (rest of body) |
//! | BackendKeyData | `K` | `Int32 pid`, `secret` bytes (rest of body; 4 bytes for 3.0, 32 for 3.2) |
//! | ParameterStatus | `S` | `String name`, `String value` |
//! | ReadyForQuery | `Z` | `Byte1` `I`/`T`/`E` |
//! | RowDescription | `T` | `Int16 n`, per field: `String name`, `Int32 table_oid`, `Int16 attnum`, `Int32 type_oid`, `Int16 typlen`, `Int32 typmod`, `Int16 format` |
//! | DataRow | `D` | `Int16 n`, per column `Int32 len` (-1 = NULL, no bytes follow) + bytes |
//! | CommandComplete | `C` | `String tag` |
//! | EmptyQueryResponse | `I` | empty |
//! | ErrorResponse / NoticeResponse | `E` / `N` | repeated `Byte1 field_code` + `String value`, ended by one `0x00` byte (see `error_fields`) |
//! | NotificationResponse | `A` | `Int32 pid`, `String channel`, `String payload` |
//! | ParseComplete / BindComplete / CloseComplete | `1` / `2` / `3` | empty |
//! | ParameterDescription | `t` | `Int16 n`, `n x Int32 type_oid` |
//! | NoData | `n` | empty |
//! | PortalSuspended | `s` | empty |
//! | CopyInResponse / CopyOutResponse / CopyBothResponse | `G` / `H` / `W` | `Int8 overall_format` (0 text, 1 binary), `Int16 n`, `n x Int16 column_format` (all 0 when overall is 0) |
//! | CopyData | `d` | raw bytes |
//! | CopyDone | `c` | empty |
//! | NegotiateProtocolVersion | `v` | `Int32 newest_minor_supported`, `Int32 n_unrecognized`, `n x String option_name` |
//!
//! ## Rules
//! * Strings are emitted with `MsgWriter::put_cstr` (interior NULs replaced by `?`).
//! * Counts are `Int16`: a `RowDescription`/`DataRow`/`ParameterDescription` with more than
//!   `i16::MAX` (32767) entries cannot be encoded: `encode_backend` returns `54000`
//!   `program_limit_exceeded` with message `too many columns` (RowDescription/DataRow) or
//!   `too many parameters` (ParameterDescription) and appends NOTHING to `out`.
//! * A DataRow column longer than `i32::MAX` bytes: `54000 value too long for wire format`.
//! * On any error the buffer is left exactly as it was on entry (truncate back).
use pgarbor::{FormatCode, TxnStatus};
use pgarbor_common::{PgError, Result};

use crate::buf::MsgWriter;
use crate::consts::*;
use crate::error_fields::ErrorFields;

/// One field of a RowDescription.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowField {
    pub name: String,
    /// OID of the source table or 0.
    pub table_oid: u32,
    /// Attribute number in that table or 0.
    pub column_attnum: i16,
    pub type_oid: u32,
    /// `pg_type.typlen`.
    pub type_len: i16,
    pub type_mod: i32,
    /// 0 text, 1 binary.
    pub format: i16,
}

/// Payload of CopyIn/Out/BothResponse.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyResponse {
    /// 0 = textual (text and CSV COPY), 1 = binary.
    pub overall_format: u8,
    /// Per-column format codes; PG sends one entry per COPY column, all 0 when `overall_format` is 0.
    pub column_formats: Vec<i16>,
}

/// A backend message (module docs). `ErrorResponse`/`NoticeResponse` carry pre-built fields
/// ([`crate::error_fields::fields_from_pg_error`]).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    AuthenticationOk,
    AuthenticationCleartextPassword,
    AuthenticationMd5Password { salt: [u8; 4] },
    AuthenticationSasl { mechanisms: Vec<String> },
    AuthenticationSaslContinue { data: Vec<u8> },
    AuthenticationSaslFinal { data: Vec<u8> },
    BackendKeyData { pid: i32, secret: Vec<u8> },
    ParameterStatus { name: String, value: String },
    ReadyForQuery(TxnStatus),
    RowDescription(Vec<RowField>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete { tag: String },
    EmptyQueryResponse,
    ErrorResponse(ErrorFields),
    NoticeResponse(ErrorFields),
    NotificationResponse { pid: i32, channel: String, payload: String },
    ParseComplete,
    BindComplete,
    CloseComplete,
    ParameterDescription(Vec<u32>),
    NoData,
    PortalSuspended,
    CopyInResponse(CopyResponse),
    CopyOutResponse(CopyResponse),
    CopyBothResponse(CopyResponse),
    CopyData(Vec<u8>),
    CopyDone,
    NegotiateProtocolVersion { newest_minor: u32, unrecognized: Vec<String> },
}

impl Backend {
    /// The type byte of the message.
    pub fn tag(&self) -> u8 {
        match self {
            Backend::AuthenticationOk
            | Backend::AuthenticationCleartextPassword
            | Backend::AuthenticationMd5Password { .. }
            | Backend::AuthenticationSasl { .. }
            | Backend::AuthenticationSaslContinue { .. }
            | Backend::AuthenticationSaslFinal { .. } => B_AUTH,
            Backend::BackendKeyData { .. } => B_BACKEND_KEY_DATA,
            Backend::ParameterStatus { .. } => B_PARAMETER_STATUS,
            Backend::ReadyForQuery(_) => B_READY_FOR_QUERY,
            Backend::RowDescription(_) => B_ROW_DESCRIPTION,
            Backend::DataRow(_) => B_DATA_ROW,
            Backend::CommandComplete { .. } => B_COMMAND_COMPLETE,
            Backend::EmptyQueryResponse => B_EMPTY_QUERY,
            Backend::ErrorResponse(_) => B_ERROR,
            Backend::NoticeResponse(_) => B_NOTICE,
            Backend::NotificationResponse { .. } => B_NOTIFICATION,
            Backend::ParseComplete => B_PARSE_COMPLETE,
            Backend::BindComplete => B_BIND_COMPLETE,
            Backend::CloseComplete => B_CLOSE_COMPLETE,
            Backend::ParameterDescription(_) => B_PARAMETER_DESCRIPTION,
            Backend::NoData => B_NO_DATA,
            Backend::PortalSuspended => B_PORTAL_SUSPENDED,
            Backend::CopyInResponse(_) => B_COPY_IN_RESPONSE,
            Backend::CopyOutResponse(_) => B_COPY_OUT_RESPONSE,
            Backend::CopyBothResponse(_) => B_COPY_BOTH_RESPONSE,
            Backend::CopyData(_) => B_COPY_DATA,
            Backend::CopyDone => B_COPY_DONE,
            Backend::NegotiateProtocolVersion { .. } => B_NEGOTIATE_PROTOCOL_VERSION,
        }
    }

    /// Message name for logs/transcripts (`"ReadyForQuery"`, ...).
    pub fn name(&self) -> &'static str {
        match self {
            Backend::AuthenticationOk => "AuthenticationOk",
            Backend::AuthenticationCleartextPassword => "AuthenticationCleartextPassword",
            Backend::AuthenticationMd5Password { .. } => "AuthenticationMD5Password",
            Backend::AuthenticationSasl { .. } => "AuthenticationSASL",
            Backend::AuthenticationSaslContinue { .. } => "AuthenticationSASLContinue",
            Backend::AuthenticationSaslFinal { .. } => "AuthenticationSASLFinal",
            Backend::BackendKeyData { .. } => "BackendKeyData",
            Backend::ParameterStatus { .. } => "ParameterStatus",
            Backend::ReadyForQuery(_) => "ReadyForQuery",
            Backend::RowDescription(_) => "RowDescription",
            Backend::DataRow(_) => "DataRow",
            Backend::CommandComplete { .. } => "CommandComplete",
            Backend::EmptyQueryResponse => "EmptyQueryResponse",
            Backend::ErrorResponse(_) => "ErrorResponse",
            Backend::NoticeResponse(_) => "NoticeResponse",
            Backend::NotificationResponse { .. } => "NotificationResponse",
            Backend::ParseComplete => "ParseComplete",
            Backend::BindComplete => "BindComplete",
            Backend::CloseComplete => "CloseComplete",
            Backend::ParameterDescription(_) => "ParameterDescription",
            Backend::NoData => "NoData",
            Backend::PortalSuspended => "PortalSuspended",
            Backend::CopyInResponse(_) => "CopyInResponse",
            Backend::CopyOutResponse(_) => "CopyOutResponse",
            Backend::CopyBothResponse(_) => "CopyBothResponse",
            Backend::CopyData(_) => "CopyData",
            Backend::CopyDone => "CopyDone",
            Backend::NegotiateProtocolVersion { .. } => "NegotiateProtocolVersion",
        }
    }

    /// Convenience: `ReadyForQuery` from a session status.
    pub fn ready(status: TxnStatus) -> Backend {
        Backend::ReadyForQuery(status)
    }

    /// Convenience: `CommandComplete`.
    pub fn complete(tag: impl Into<String>) -> Backend {
        Backend::CommandComplete { tag: tag.into() }
    }
}

/// Result-format codes for a RowDescription column under PG's rule (`exec_bind_message`): an empty
/// list means all text, a single entry applies to every column, otherwise one entry per column.
/// Used by `result_encode` and `extended` (Describe portal).
pub fn format_for_column(formats: &[FormatCode], column: usize) -> FormatCode {
    match formats.len() {
        0 => FormatCode::Text,
        1 => formats[0],
        _ => formats.get(column).copied().unwrap_or(FormatCode::Text),
    }
}

/// Append the exact wire bytes of `msg` to `out` (module docs: layouts and limits). On error `out`
/// is restored to its entry length.
///
/// Examples (hex): `ReadyForQuery(Idle)` -> `5a 00000005 49`; `CommandComplete{"SELECT 1"}` ->
/// `43 0000000d 53454c4543542031 00`; `DataRow([Some(b"1"), None])` -> `44 0000000f 0002
/// 00000001 31 ffffffff`; `ParameterStatus{"a","b"}` -> `53 00000008 6100 6200`;
/// `AuthenticationSasl{["SCRAM-SHA-256"]}` -> `52 len 0000000a "SCRAM-SHA-256\0" 00`.
#[allow(unused_variables)]
pub fn encode_backend(msg: &Backend, out: &mut Vec<u8>) -> Result<()> {
    let _ = MsgWriter::begin; // used by the implementation
    Err(PgError::unimplemented("WIR-02", "encode a backend message (backend::encode_backend)"))
}
