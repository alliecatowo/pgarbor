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
pub fn encode_backend(msg: &Backend, out: &mut Vec<u8>) -> Result<()> {
    fn limit(message: &'static str) -> PgError {
        PgError::new(pgarbor_common::SqlState::PROGRAM_LIMIT_EXCEEDED, message)
    }

    match msg {
        Backend::RowDescription(fields) if fields.len() > i16::MAX as usize => {
            return Err(limit("too many columns"));
        }
        Backend::DataRow(values) if values.len() > i16::MAX as usize => {
            return Err(limit("too many columns"));
        }
        Backend::ParameterDescription(params) if params.len() > i16::MAX as usize => {
            return Err(limit("too many parameters"));
        }
        Backend::CopyInResponse(response)
        | Backend::CopyOutResponse(response)
        | Backend::CopyBothResponse(response)
            if response.column_formats.len() > i16::MAX as usize =>
        {
            return Err(limit("too many columns"));
        }
        Backend::NegotiateProtocolVersion { unrecognized, .. }
            if unrecognized.len() > u32::MAX as usize =>
        {
            return Err(limit("too many unrecognized startup parameters"));
        }
        Backend::DataRow(values) if values.iter().flatten().any(|v| v.len() > i32::MAX as usize) => {
            return Err(limit("value too long for wire format"));
        }
        Backend::BackendKeyData { secret, .. } if secret.len() > i32::MAX as usize - 8 => {
            return Err(limit("message is too long"));
        }
        Backend::CopyData(data) if data.len() > i32::MAX as usize - 4 => {
            return Err(limit("message is too long"));
        }
        Backend::AuthenticationSaslContinue { data } | Backend::AuthenticationSaslFinal { data }
            if data.len() > i32::MAX as usize - 8 =>
        {
            return Err(limit("message is too long"));
        }
        _ => {}
    }

    let original_len = out.len();
    let result = (|| {
        let mut w = MsgWriter::begin(out, msg.tag());
        match msg {
            Backend::AuthenticationOk => w.put_i32(AUTH_OK),
            Backend::AuthenticationCleartextPassword => w.put_i32(AUTH_CLEARTEXT_PASSWORD),
            Backend::AuthenticationMd5Password { salt } => {
                w.put_i32(AUTH_MD5_PASSWORD);
                w.put_bytes(salt);
            }
            Backend::AuthenticationSasl { mechanisms } => {
                w.put_i32(AUTH_SASL);
                for mechanism in mechanisms {
                    w.put_cstr(mechanism);
                }
                w.put_u8(0);
            }
            Backend::AuthenticationSaslContinue { data } => {
                w.put_i32(AUTH_SASL_CONTINUE);
                w.put_bytes(data);
            }
            Backend::AuthenticationSaslFinal { data } => {
                w.put_i32(AUTH_SASL_FINAL);
                w.put_bytes(data);
            }
            Backend::BackendKeyData { pid, secret } => {
                w.put_i32(*pid);
                w.put_bytes(secret);
            }
            Backend::ParameterStatus { name, value } => {
                w.put_cstr(name);
                w.put_cstr(value);
            }
            Backend::ReadyForQuery(status) => w.put_u8(status.byte()),
            Backend::RowDescription(fields) => {
                w.put_i16(fields.len() as i16);
                for field in fields {
                    w.put_cstr(&field.name);
                    w.put_u32(field.table_oid);
                    w.put_i16(field.column_attnum);
                    w.put_u32(field.type_oid);
                    w.put_i16(field.type_len);
                    w.put_i32(field.type_mod);
                    w.put_i16(field.format);
                }
            }
            Backend::DataRow(values) => {
                w.put_i16(values.len() as i16);
                for value in values {
                    match value {
                        Some(bytes) => {
                            w.put_i32(bytes.len() as i32);
                            w.put_bytes(bytes);
                        }
                        None => w.put_i32(-1),
                    }
                }
            }
            Backend::CommandComplete { tag } => w.put_cstr(tag),
            Backend::EmptyQueryResponse
            | Backend::ParseComplete
            | Backend::BindComplete
            | Backend::CloseComplete
            | Backend::NoData
            | Backend::PortalSuspended
            | Backend::CopyDone => {}
            Backend::ErrorResponse(fields) | Backend::NoticeResponse(fields) => {
                for (code, value) in &fields.fields {
                    w.put_u8(*code);
                    w.put_cstr(value);
                }
                w.put_u8(0);
            }
            Backend::NotificationResponse { pid, channel, payload } => {
                w.put_i32(*pid);
                w.put_cstr(channel);
                w.put_cstr(payload);
            }
            Backend::ParameterDescription(params) => {
                w.put_i16(params.len() as i16);
                for oid in params {
                    w.put_u32(*oid);
                }
            }
            Backend::CopyInResponse(response)
            | Backend::CopyOutResponse(response)
            | Backend::CopyBothResponse(response) => {
                w.put_u8(response.overall_format);
                w.put_i16(response.column_formats.len() as i16);
                for format in &response.column_formats {
                    w.put_i16(*format);
                }
            }
            Backend::CopyData(data) => w.put_bytes(data),
            Backend::NegotiateProtocolVersion { newest_minor, unrecognized } => {
                w.put_u32(*newest_minor);
                w.put_u32(unrecognized.len() as u32);
                for option in unrecognized {
                    w.put_cstr(option);
                }
            }
        }
        w.finish();
        Ok(())
    })();
    if result.is_err() {
        out.truncate(original_len);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(msg: Backend) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(encode_backend(&msg, &mut out).is_ok());
        out
    }

    #[test]
    fn documented_wire_examples() {
        assert_eq!(encoded(Backend::ReadyForQuery(TxnStatus::Idle)), b"Z\0\0\0\x05I");
        assert_eq!(
            encoded(Backend::complete("SELECT 1")),
            b"C\0\0\0\x0dSELECT 1\0"
        );
        assert_eq!(
            encoded(Backend::DataRow(vec![Some(b"1".to_vec()), None])),
            b"D\0\0\0\x0f\0\x02\0\0\0\x011\xff\xff\xff\xff"
        );
        assert_eq!(
            encoded(Backend::ParameterStatus { name: "a".into(), value: "b".into() }),
            b"S\0\0\0\x08a\0b\0"
        );
        assert_eq!(
            encoded(Backend::AuthenticationSasl { mechanisms: vec!["SCRAM-SHA-256".into()] }),
            b"R\0\0\0\x17\0\0\0\x0aSCRAM-SHA-256\0\0"
        );
    }

    #[test]
    fn row_description_and_data_row_layouts() {
        let row_description = encoded(Backend::RowDescription(vec![RowField {
            name: "x".into(), table_oid: 42, column_attnum: 1, type_oid: 23,
            type_len: 4, type_mod: -1, format: 0,
        }]));
        let mut r = crate::buf::MsgReader::new(&row_description[5..]);
        assert_eq!(r.read_i16().ok(), Some(1));
        assert_eq!(r.read_cstr().ok(), Some("x"));
        assert_eq!(r.read_u32().ok(), Some(42));
        assert_eq!(r.read_i16().ok(), Some(1));
        assert_eq!(r.read_u32().ok(), Some(23));
        assert_eq!(r.read_i16().ok(), Some(4));
        assert_eq!(r.read_i32().ok(), Some(-1));
        assert_eq!(r.read_i16().ok(), Some(0));
        assert!(r.finish().is_ok());

        let data = encoded(Backend::DataRow(vec![Some(b"1".to_vec()), None]));
        let mut r = crate::buf::MsgReader::new(&data[5..]);
        assert_eq!(r.read_i16().ok(), Some(2));
        assert_eq!(r.read_i32().ok(), Some(1));
        assert_eq!(r.take(1).ok(), Some(&b"1"[..]));
        assert_eq!(r.read_i32().ok(), Some(-1));
        assert!(r.finish().is_ok());
    }

    #[test]
    fn count_limit_preserves_existing_buffer() {
        let mut out = vec![7, 8, 9];
        let err = encode_backend(&Backend::DataRow(vec![None; i16::MAX as usize + 1]), &mut out)
            .err();
        assert_eq!(err.as_ref().map(|e| e.sqlstate.code()), Some("54000"));
        assert_eq!(out, vec![7, 8, 9]);
        let err = encode_backend(&Backend::ParameterDescription(vec![0; i16::MAX as usize + 1]), &mut out)
            .err();
        assert_eq!(err.map(|e| e.message), Some("too many parameters".into()));
        assert_eq!(out, vec![7, 8, 9]);
    }
}
