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
use pgarbor_common::{PgError, Result};

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
#[allow(unused_variables)]
pub fn parse_startup_packet(body: &[u8]) -> Result<StartupPacket> {
    Err(PgError::unimplemented("WIR-03", "parse the first packet (startup_packet::parse_startup_packet)"))
}
