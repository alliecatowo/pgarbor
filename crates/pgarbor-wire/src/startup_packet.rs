//! The first packet of a connection (lease WIR-03). PG: `postmaster/postmaster.c`
//! `ProcessStartupPacket`, `protocol.sgml` "StartupMessage", "SSLRequest", "GSSENCRequest",
//! "CancelRequest".
//!
//! The packet has NO tag byte: `Int32 length` (counts itself), then `Int32 code`, then a
//! code-specific payload. [`crate::framing::FrameReader::poll_startup`] reads the length-prefixed
//! packet and returns the bytes AFTER the length field (`code` first); [`parse_startup_packet`] decodes
//! them.
//!
//! | `code` | meaning | payload after the code |
//! |---|---|---|
//! | `80877103` | SSLRequest | none |
//! | `80877104` | GSSENCRequest | none |
//! | `80877102` | CancelRequest | `Int32 pid`, then the secret key: all remaining bytes, 4..=256 |
//! | `major << 16 \| minor` (e.g. `196608` = 3.0, `196610` = 3.2) | StartupMessage | repeated `String name`, `String value`, ended by a single `0x00` |
//!
//! ## Length rules (checked by the framing layer using these constants)
//! * total length < 8 or > [`crate::consts::MAX_STARTUP_PACKET_LEN`] (10000): FATAL `08P01`
//!   `invalid length of startup packet`;
//! * CancelRequest total length must be `16..=264`, otherwise the same error.
//!
//! ## StartupMessage rules ([`parse_startup_packet`])
//! * the parameter list must end with an empty name (`0x00`) which must be the LAST byte of the
//!   packet; otherwise FATAL `08P01` `invalid startup packet layout: expected terminator as last byte`;
//! * a name with a missing value terminator -> the same error; names/values must be UTF-8
//!   (`invalid startup packet layout: expected terminator as last byte` is reused: PG reports
//!   invalid encodings later as `invalid byte sequence`);
//! * duplicate names are kept in order (later GUC settings override earlier ones, `user`: last wins);
//! * `major` is NOT validated here (`startup_flow` rejects `major != 3`).
//!
//! All errors carry `Severity::Fatal`.
use pgarbor_common::{PgError, Result, Severity, SqlState};
use crate::consts::{CANCEL_REQUEST_CODE, GSSENC_REQUEST_CODE, SSL_REQUEST_CODE};

/// A decoded first packet.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupPacket {
    /// StartupMessage: protocol `major.minor` and the raw parameter list in arrival order.
    Startup { major: u16, minor: u16, params: Vec<(String, String)> },
    SslRequest,
    GssEncRequest,
    /// CancelRequest: backend pid and secret key (4..=256 bytes).
    Cancel { pid: i32, secret: Vec<u8> },
}

/// Decode the bytes that follow the length field of the first packet (module docs).
///
/// Examples: `[00 03 00 00]` + `"user\0alice\0database\0db\0\0"` -> `Startup{major:3, minor:0,
/// params:[("user","alice"),("database","db")]}`; `04d2162f` (80877103) -> `SslRequest`;
/// 80877102 + pid 1234 + 4 secret bytes -> `Cancel{pid:1234, secret:[..4 bytes]}`;
/// CancelRequest with fewer than 4 or more than 256 secret bytes -> FATAL `08P01 invalid length of
/// startup packet`.
pub fn parse_startup_packet(body: &[u8]) -> Result<StartupPacket> {
    fn invalid_length() -> PgError {
        PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid length of startup packet")
            .with_severity(Severity::Fatal)
    }
    fn invalid_layout() -> PgError {
        PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            "invalid startup packet layout: expected terminator as last byte",
        )
        .with_severity(Severity::Fatal)
    }
    let Some(code_bytes) = body.get(..4) else {
        return Err(invalid_length());
    };
    let code = u32::from_be_bytes([code_bytes[0], code_bytes[1], code_bytes[2], code_bytes[3]]);
    match code {
        SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => {
            if body.len() != 4 {
                return Err(invalid_length());
            }
            if code == SSL_REQUEST_CODE {
                Ok(StartupPacket::SslRequest)
            } else {
                Ok(StartupPacket::GssEncRequest)
            }
        }
        CANCEL_REQUEST_CODE => {
            let secret_len = body.len().saturating_sub(8);
            if !(4..=256).contains(&secret_len) {
                return Err(invalid_length());
            }
            let pid_bytes = body.get(4..8).ok_or_else(invalid_length)?;
            let pid = i32::from_be_bytes([pid_bytes[0], pid_bytes[1], pid_bytes[2], pid_bytes[3]]);
            let secret = body.get(8..).ok_or_else(invalid_length)?.to_vec();
            Ok(StartupPacket::Cancel { pid, secret })
        }
        _ => {
            let major = (code >> 16) as u16;
            let minor = (code & 0xffff) as u16;
            let mut params = Vec::new();
            let mut pos = 4usize;
            loop {
                let tail = body.get(pos..).ok_or_else(invalid_layout)?;
                let name_end_rel = tail.iter().position(|b| *b == 0).ok_or_else(invalid_layout)?;
                if name_end_rel == 0 {
                    if pos + 1 != body.len() {
                        return Err(invalid_layout());
                    }
                    return Ok(StartupPacket::Startup { major, minor, params });
                }
                let name_bytes = &tail[..name_end_rel];
                let name = std::str::from_utf8(name_bytes).map_err(|_| invalid_layout())?;
                pos += name_end_rel + 1;
                let value_tail = body.get(pos..).ok_or_else(invalid_layout)?;
                let value_end_rel = value_tail.iter().position(|b| *b == 0).ok_or_else(invalid_layout)?;
                let value_bytes = &value_tail[..value_end_rel];
                let value = std::str::from_utf8(value_bytes).map_err(|_| invalid_layout())?;
                params.push((name.to_owned(), value.to_owned()));
                pos += value_end_rel + 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(code: u32, payload: &[u8]) -> Result<StartupPacket> {
        let mut body = code.to_be_bytes().to_vec();
        body.extend_from_slice(payload);
        parse_startup_packet(&body)
    }

    #[test]
    fn parses_request_packets() {
        assert_eq!(parse(SSL_REQUEST_CODE, &[]).unwrap(), StartupPacket::SslRequest);
        assert_eq!(parse(GSSENC_REQUEST_CODE, &[]).unwrap(), StartupPacket::GssEncRequest);
        assert_eq!(parse(SSL_REQUEST_CODE, &[0]).unwrap_err().message, "invalid length of startup packet");
    }

    #[test]
    fn parses_startup_and_preserves_parameter_order() {
        assert_eq!(
            parse(196_608, b"user\0alice\0database\0db\0\0").unwrap(),
            StartupPacket::Startup { major: 3, minor: 0, params: vec![("user".into(), "alice".into()), ("database".into(), "db".into())] }
        );
        assert!(matches!(parse(196_610, b"\0").unwrap(), StartupPacket::Startup { major: 3, minor: 2, .. }));
    }

    #[test]
    fn rejects_bad_startup_terminators() {
        for payload in [b"user\0alice\0".as_slice(), b"\0x", b"user\0alice\0\0x"] {
            let err = parse(196_608, payload).unwrap_err();
            assert_eq!(err.sqlstate, SqlState::PROTOCOL_VIOLATION);
            assert_eq!(err.severity, Severity::Fatal);
            assert_eq!(err.message, "invalid startup packet layout: expected terminator as last byte");
        }
    }

    #[test]
    fn validates_cancel_secret_bounds() {
        let mut valid = 123i32.to_be_bytes().to_vec();
        valid.extend_from_slice(&[7; 4]);
        assert_eq!(parse(CANCEL_REQUEST_CODE, &valid).unwrap(), StartupPacket::Cancel { pid: 123, secret: vec![7; 4] });
        for len in [3usize, 257] {
            let mut payload = 0i32.to_be_bytes().to_vec();
            payload.resize(4 + len, 0);
            assert_eq!(parse(CANCEL_REQUEST_CODE, &payload).unwrap_err().message, "invalid length of startup packet");
        }
    }
}
