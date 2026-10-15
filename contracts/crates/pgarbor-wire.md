# pgarbor-wire — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-wire.json` (WIR-01..WIR-21; seed tasks
WIRE-01..WIRE-13). Amendments: `pgarbor-wire-01-copy-channel-and-streaming`, `pgarbor-wire-02-protocol-3-2-native`.
Depends on: pgarbor-common, pgarbor-types, pgarbor (embedded API) + sha2/hmac/base64/md-5/pbkdf2/rand. Consumed by pgarbor-cli.

## Purpose

The PostgreSQL v3 frontend/backend protocol server over `std::net` (TCP, plus Unix-domain sockets on unix), one OS thread per
connection, no async runtime. It is a protocol translator: **no SQL semantics live here**. Every message maps to a method on
`pgarbor::Database` / `pgarbor::Session` (simple query, parse/bind/describe/execute/close/sync, cancel, notifications, COPY channel).
Each listener is bound to exactly ONE branch (`ServerConfig::branch`); the logical SQL database comes from the startup `database`
parameter (v1: only `postgres`). Different branches are served by different listeners (RFC 001 section 7).

## Public API

```rust
let server = pgarbor_wire::Server::start(db, ServerConfig::loopback("main"))?;   // binds + spawns accept threads
let url    = server.url("postgres", "postgres");        // postgres://postgres@127.0.0.1:PORT/postgres?sslmode=disable
let handle = server.handle();                           // ServerHandle: stop(), is_stopping(), local_addr(), connection_count()
server.wait_for(&|| stop_requested())?;                 // blocks, then runs shutdown()
server.shutdown()?;                                     // idempotent graceful stop
```
| Item | Use |
|---|---|
| `ServerConfig`, `ListenAddr`, `AuthPolicy`, `TlsPolicy` | configuration; `validate()` refuses unsafe combinations; `ServerConfig::loopback(branch)` |
| `Server`, `ServerHandle` | lifecycle; `tcp_addr()`, `unix_paths()`, `url()`, `connection_count()` |
| `Frontend`, `Backend`, `RowField`, `CopyResponse`, `ErrorFields` | typed message model; `frontend::decode_frontend`, `backend::encode_backend` are the only (de)serializers |
| `transcript::{TranscriptSink, TranscriptEntry, MemoryTranscript, to_jsonl, summarize}` | WIRE-12 diagnostics hook (no serde in this crate) |
| `client::WireClient` | minimal test/diagnostic client (startup incl. cleartext/md5/SCRAM, simple + extended query, raw frames, cancel) |
| `scram::{ScramServer, ScramClient, make_verifier, parse_verifier}` | SCRAM-SHA-256 building blocks |

## Protocol support (byte-exact per PG 18.6 `protocol.sgml`)

* **Versions:** 3.0 (4-byte cancel secret) and 3.2 (32-byte secret, variable-length CancelRequest 16..=264 bytes) natively; minor > 2 or
  any `_pq_.*` option -> `NegotiateProtocolVersion(newest_minor, unrecognized names)`; major != 3 -> FATAL `0A000`
  `unsupported frontend protocol N.M: server supports 3.0 to 3.2` (amendment 02 deviates from ARCHITECTURE section 12 here).
* **First packet** (untagged): StartupMessage, SSLRequest (80877103) and GSSENCRequest (80877104) -> single byte `N`, CancelRequest
  (80877102) -> `Database::cancel_request(pid, secret)` then close silently. Max 10000 bytes.
* **Startup params:** `user` (required), `database` (default = user), `application_name`, `options` (`-c n=v`, `--n=v`),
  `replication` (rejected unless false), other names forwarded as startup GUCs (the session validates them), `_pq_.*` negotiated away.
* **Auth:** `Auto` (default), `Trust`, `Password` (cleartext), `Md5`, `ScramSha256`. Failures are FATAL `28P01 password authentication
  failed for user "x"` with no detail; unknown roles run a mock SCRAM exchange first. See `auth.rs` for the method table.
* **After auth:** AuthenticationOk, ParameterStatus for `Session::startup_param_status()`, BackendKeyData, ReadyForQuery('I').
* **Simple Query:** `Session::simple_query_raw`; per statement: NoticeResponse*, RowDescription (all text), DataRow*, CommandComplete /
  EmptyQueryResponse; ErrorResponse stops the string; ParameterStatus changes (deduplicated against the last reported value) are
  emitted just before ReadyForQuery, as PG 14+ does (`ReportChangedGUCOptions`).
* **Extended:** Parse/Bind/Describe/Execute/Close/Sync/Flush. After an error everything (including Query, Flush, Terminate) is discarded
  until Sync (`ignore_till_sync`); no ReadyForQuery until then. Describe('S') reports format 0; Describe('P') reports the formats
  recorded at Bind. `max_rows <= 0` = unlimited; PortalSuspended on row-limit.
* **COPY:** CopyIn/CopyOut/CopyData/CopyDone/CopyFail (`copy.rs`), blocked on amendment 01 for installation (see gaps).
* **Notifications:** `NotificationResponse` after ReadyForQuery and on idle polls, only while the session is idle (status `I`).
* **Cancel/terminate:** engine owns the key map; `CancelToken::terminate` implements shutdown; `statement_timeout` is enforced by
  core `Interrupts`; the wire layer enforces `idle_in_transaction_session_timeout` (FATAL `25P03`) and `idle_session_timeout`
  (FATAL `57P05`) while waiting for the next message.
* **Limits:** `max_connections` established sessions (FATAL `53300 sorry, too many clients already`), handler-thread cap
  `2*max+16` (excess sockets are closed unanswered), startup timeout 60 s (FATAL `57014 canceling authentication due to timeout`),
  frame cap 256 MiB (FATAL `08P01 invalid message length`).
* **Errors:** `ErrorResponse`/`NoticeResponse` fields `S V C M D H P s t c d n` in that order. `F`/`L`/`R`/`W` are never sent (PgError
  has no such data; fabricating them would break transcript parity).
* **TLS:** NOT implemented (WIRE-13 is "explicit not yet"): SSLRequest/GSSENCRequest are answered `N`; `TlsPolicy::Required` is refused
  by `validate()` with `0A000 TLS is not supported yet`. Non-loopback binds need `allow_nonloopback` and a password policy.

## Threading and state

* `Shared` (Arc): `Database`, config, stop flag, two `ConnGate` counters, `ConnRegistry`, mock-auth secret.
* One thread per accepted socket (`connection::serve_connection`); the `Session` is created ON that thread (it is `!Send`).
* Idle connections poll with a read timeout (`idle_poll`, default 100 ms; 25 ms while LISTENing) so they observe shutdown and notifications;
  partial frames survive timeouts because `FrameReader` owns an accumulation buffer.
* Shutdown: stop flag -> accept loops exit (non-blocking accept, 50 ms poll; SA_RESTART signal handlers cannot interrupt a blocking accept)
  -> `CancelToken::terminate` for every session -> grace wait -> `shutdown(Both)` on remaining sockets -> detach stragglers.
* Output is buffered per connection and flushed after ReadyForQuery / Flush / CopyInResponse / auth challenges and before every blocking read.

## Error conventions

* Protocol violations are `08P01` with PG's wording from `pqformat.c` (`insufficient data left in message`, `invalid message format`,
  `invalid string in message`, `invalid frontend message type N`, ...). Startup/auth errors are FATAL and close the socket without
  ReadyForQuery.
* The address-in-use bind failure message contains the substring `address in use` (the CLI maps it to exit code 2, harness-02).
* `WireError::{Io, Closed, Pg, Shutdown}` separates "socket is gone" (close silently) from "send this error".

## How downstream crates use it

`pgarbor-cli` runs `Server::start` for `serve`, for `branch serve`, and for owned/on-demand listeners of `branch exec` and the control
socket; it never touches protocol internals. The harness (`PgarborTarget`) reaches the same server through `pgarbor serve`.

## Non-goals and known gaps (v1)

* No TLS/GSS, no replication protocol, no fastpath `FunctionCall` (answered `0A000`), no `COPY BOTH`.
* **COPY** installation is blocked on amendment `pgarbor-wire-01`: `Session::set_copy_channel` needs a nameable `CopyChannel` trait
  (re-export from `pgarbor`), a `start(direction, binary, column_count)` hook (CopyIn/OutResponse need column counts/formats) and a
  streaming simple-query API. Until then `copy::install` returns `0A000` and a `COPY` inside a multi-statement string cannot interleave
  correctly.
* Simple Query results are fully materialized by `Session::simple_query_raw` (no row streaming).
* Client encodings other than UTF-8 are not converted.
* A blocked `read` cannot be woken by the NotifyHub waker; notification latency is bounded by the poll interval.
* One listener serves one branch; reaching another branch needs another listener (CLI `branch exec` automates this).

## Lease index
| lease | file | tier | scope |
|---|---|---|---|
| WIR-01 | frontend.rs | haiku | decode all frontend messages |
| WIR-02 | backend.rs | haiku | encode all backend messages |
| WIR-03 | startup_packet.rs | haiku | first-packet parser |
| WIR-04 | error_fields.rs | haiku | PgError -> Error/NoticeResponse fields |
| WIR-05 | framing.rs | sonnet | resumable frame reader + buffered writer |
| WIR-06 | conn_io.rs | haiku | typed I/O with transcript recording |
| WIR-07 | options_parse.rs | haiku | `options` parsing and startup GUC assembly |
| WIR-08 | startup_flow.rs | sonnet | SSL/GSS/cancel/version negotiation/params |
| WIR-09 | scram.rs | sonnet | SCRAM-SHA-256 server + client |
| WIR-10 | auth.rs | sonnet | method choice and exchanges |
| WIR-11 | simple_query.rs | haiku | Query message |
| WIR-12 | extended.rs | sonnet | extended protocol + skip-until-Sync |
| WIR-13 | copy.rs | sonnet | COPY IN/OUT channel (install blocked on amendment 01) |
| WIR-14 | result_encode.rs | haiku | result/error/RFQ message emission |
| WIR-15 | notify_out.rs | haiku | NotificationResponse delivery, poll interval |
| WIR-16 | session_loop.rs | sonnet | command loop, idle timeouts, shutdown |
| WIR-17 | connection.rs | sonnet | connection lifecycle |
| WIR-18 | listener.rs | sonnet | bind + non-blocking accept loops |
| WIR-19 | server.rs (+ tests/wir_19.rs) | sonnet | server lifecycle, URLs, e2e tests |
| WIR-20 | transcript.rs | haiku | JSONL/summary formatting |
| WIR-21 | client.rs | haiku | test/diagnostic client |

Frozen and implemented (not leased): `lib.rs`, `consts.rs`, `error.rs`, `buf.rs`, `transport.rs`, `config.rs`, `tls.rs`, `cancel.rs`,
`param_report.rs`, `limits.rs`, `registry.rs`, `shared.rs`.

## Assembly decisions (2026-10-15)

Steward decisions: wire-02 native protocol 3.2 accepted as the intended representation; wire-01 complete COPY streaming deferred. WIR-13 remains 0A000. Live protocol parity is unverified.
