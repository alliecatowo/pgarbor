//! PGArbor PostgreSQL v3 wire-protocol server over `std::net` (ARCHITECTURE section 12, seed tasks
//! WIRE-01..13). A thin protocol translator over the embedded API (`pgarbor::Database` /
//! `pgarbor::Session`): it contains NO SQL semantics. Thread per connection; no async runtime.
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor-wire.md` and `contracts/leases/pgarbor-wire.json` (WIR-01..).
//!
//! ```ignore
//! use pgarbor::{Database, OpenOptions};
//! use pgarbor_wire::{ServerConfig, Server};
//! let db = Database::open_or_create("dev.pga", OpenOptions::default())?;
//! let cfg = ServerConfig::loopback("main");                 // 127.0.0.1:0, branch "main"
//! let server = Server::start(db, cfg)?;                     // listener threads running
//! println!("{}", server.url("postgres", "postgres"));      // postgres://postgres@127.0.0.1:PORT/postgres?sslmode=disable
//! server.shutdown();                                        // stop accepting, terminate sessions, join
//! ```
//!
//! ## Module map (layering: lower never imports higher)
//! | layer | modules |
//! |---|---|
//! | primitives | [`consts`], [`error`], [`buf`], [`transport`] |
//! | message codec | [`frontend`], [`backend`], [`startup_packet`], [`error_fields`] |
//! | I/O | [`framing`], [`transcript`], [`conn_io`] |
//! | connection setup | [`config`], [`options_parse`], [`tls`], [`cancel`], [`startup_flow`], [`scram`], [`auth`] |
//! | statement protocols | [`param_report`], [`result_encode`], [`simple_query`], [`extended`], [`copy`], [`notify_out`], [`session_loop`] |
//! | server | [`shared`], [`limits`], [`registry`], [`connection`], [`listener`], [`server`] |
//! | diagnostics / tests | [`client`] |
//!
//! ## One connection, end to end (`connection::serve_connection`)
//! 1. `startup_flow::negotiate`: SSLRequest/GSSENCRequest -> `N`; CancelRequest -> `cancel::handle` and
//!    close; StartupMessage -> `StartupInfo` (user, database, protocol minor, startup GUCs).
//! 2. connection-limit gate (`limits`), `auth::authenticate` (trust / cleartext / md5 / SCRAM-SHA-256).
//! 3. `Database::connect(Some(branch), SessionOptions{..})` on THIS thread (`Session` is `!Send`).
//! 4. AuthenticationOk, ParameterStatus*, BackendKeyData, ReadyForQuery('I').
//! 5. `session_loop::run`: Query / Parse / Bind / Describe / Execute / Close / Sync / Flush / Terminate,
//!    idle polling for NOTIFY and shutdown, idle timeouts.
//! 6. cleanup: session closed (rolls back), registry/gate released, socket closed.
//!
//! ## Cross-cutting rules every file obeys
//! * Never panic on client bytes: bound every length BEFORE allocating, validate UTF-8, no `unwrap`.
//! * Protocol-level problems are `08P01` (`PgError::new(SqlState::PROTOCOL_VIOLATION, ..)`) with PG's
//!   wording; startup/auth errors are FATAL (`PgError::with_severity(Severity::Fatal)`) and followed by
//!   closing the socket (no ReadyForQuery).
//! * Flush discipline: output goes to a buffer; `flush()` runs after ReadyForQuery, after Flush ('H'),
//!   before every blocking read when the output buffer is non-empty, after CopyInResponse, and after
//!   every authentication request. Forgetting a flush before a blocking read hangs the client.
//! * `TCP_NODELAY` on every accepted TCP stream.
#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

pub mod auth;
pub mod backend;
pub mod buf;
pub mod cancel;
pub mod client;
pub mod config;
pub mod conn_io;
pub mod connection;
pub mod consts;
pub mod copy;
pub mod error;
pub mod error_fields;
pub mod extended;
pub mod framing;
pub mod frontend;
pub mod limits;
pub mod listener;
pub mod notify_out;
pub mod options_parse;
pub mod param_report;
pub mod registry;
pub mod result_encode;
pub mod scram;
pub mod server;
pub mod session_loop;
pub mod shared;
pub mod simple_query;
pub mod startup_flow;
pub mod startup_packet;
pub mod tls;
pub mod transcript;
pub mod transport;

pub use backend::Backend;
pub use config::{AuthPolicy, ListenAddr, ServerConfig, TlsPolicy};
pub use error::{WireError, WireResult};
pub use frontend::Frontend;
pub use server::{Server, ServerHandle};
pub use transcript::{TranscriptDirection, TranscriptEntry, TranscriptSink};
