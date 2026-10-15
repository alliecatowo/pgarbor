//! Protocol constants (PostgreSQL 18 `protocol.sgml`, `libpq/pqcomm.h`, `libpq/protocol.h`).
//! Fully implemented, frozen (v1).

/// Protocol 3.0 as sent in a StartupMessage (`3 << 16`).
// CONTRACT
pub const PROTOCOL_3_0: u32 = 196_608;
/// Protocol 3.2 (PG18): variable-length cancel secret.
// CONTRACT
pub const PROTOCOL_3_2: u32 = 196_610;
/// Highest minor version this server speaks.
// CONTRACT
pub const NEWEST_MINOR: u32 = 2;
/// Request codes that take the place of a protocol version in the first packet.
// CONTRACT
pub const CANCEL_REQUEST_CODE: u32 = 80_877_102;
// CONTRACT
pub const SSL_REQUEST_CODE: u32 = 80_877_103;
// CONTRACT
pub const GSSENC_REQUEST_CODE: u32 = 80_877_104;

/// PG `MAX_STARTUP_PACKET_LENGTH`: larger startup packets are rejected (FATAL 08P01
/// `invalid length of startup packet`).
// CONTRACT
pub const MAX_STARTUP_PACKET_LEN: usize = 10_000;
/// CancelRequest packet length bounds: `len(4) + code(4) + pid(4) + secret(4..=256)`.
// CONTRACT
pub const CANCEL_PACKET_MIN_LEN: usize = 16;
// CONTRACT
pub const CANCEL_PACKET_MAX_LEN: usize = 264;
/// Cancel secret lengths (bytes) for protocol 3.0 and 3.2.
// CONTRACT
pub const CANCEL_KEY_LEN_3_0: usize = 4;
// CONTRACT
pub const CANCEL_KEY_LEN_3_2: usize = 32;

/// Default cap for one frontend message body (PG allows up to 1 GiB - 1; we default lower and
/// make it configurable: `ServerConfig::max_frame_bytes`).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

// ---- frontend message tags -------------------------------------------------------------------
pub const F_QUERY: u8 = b'Q';
pub const F_PARSE: u8 = b'P';
pub const F_BIND: u8 = b'B';
pub const F_EXECUTE: u8 = b'E';
pub const F_DESCRIBE: u8 = b'D';
pub const F_CLOSE: u8 = b'C';
pub const F_SYNC: u8 = b'S';
pub const F_FLUSH: u8 = b'H';
pub const F_TERMINATE: u8 = b'X';
pub const F_COPY_DATA: u8 = b'd';
pub const F_COPY_DONE: u8 = b'c';
pub const F_COPY_FAIL: u8 = b'f';
/// PasswordMessage / SASLInitialResponse / SASLResponse share tag `p` (decoded by context).
pub const F_PASSWORD: u8 = b'p';
pub const F_FUNCTION_CALL: u8 = b'F';

// ---- backend message tags --------------------------------------------------------------------
pub const B_AUTH: u8 = b'R';
pub const B_BACKEND_KEY_DATA: u8 = b'K';
pub const B_BIND_COMPLETE: u8 = b'2';
pub const B_CLOSE_COMPLETE: u8 = b'3';
pub const B_COMMAND_COMPLETE: u8 = b'C';
pub const B_COPY_DATA: u8 = b'd';
pub const B_COPY_DONE: u8 = b'c';
pub const B_COPY_IN_RESPONSE: u8 = b'G';
pub const B_COPY_OUT_RESPONSE: u8 = b'H';
pub const B_COPY_BOTH_RESPONSE: u8 = b'W';
pub const B_DATA_ROW: u8 = b'D';
pub const B_EMPTY_QUERY: u8 = b'I';
pub const B_ERROR: u8 = b'E';
pub const B_FUNCTION_CALL_RESPONSE: u8 = b'V';
pub const B_NEGOTIATE_PROTOCOL_VERSION: u8 = b'v';
pub const B_NO_DATA: u8 = b'n';
pub const B_NOTICE: u8 = b'N';
pub const B_NOTIFICATION: u8 = b'A';
pub const B_PARAMETER_DESCRIPTION: u8 = b't';
pub const B_PARAMETER_STATUS: u8 = b'S';
pub const B_PARSE_COMPLETE: u8 = b'1';
pub const B_PORTAL_SUSPENDED: u8 = b's';
pub const B_READY_FOR_QUERY: u8 = b'Z';
pub const B_ROW_DESCRIPTION: u8 = b'T';

// ---- Authentication request codes ('R' message, Int32 code) -----------------------------------
pub const AUTH_OK: i32 = 0;
pub const AUTH_CLEARTEXT_PASSWORD: i32 = 3;
pub const AUTH_MD5_PASSWORD: i32 = 5;
pub const AUTH_SASL: i32 = 10;
pub const AUTH_SASL_CONTINUE: i32 = 11;
pub const AUTH_SASL_FINAL: i32 = 12;

/// The only SASL mechanism offered (no channel binding: there is no TLS).
pub const SASL_SCRAM_SHA_256: &str = "SCRAM-SHA-256";
/// `server_version` reported in ParameterStatus (PG 18.6 reference).
pub const SERVER_VERSION: &str = "18.6";
/// Idle-poll granularity of connection threads (stop flag / NOTIFY checks).
pub const DEFAULT_IDLE_POLL_MS: u64 = 100;
