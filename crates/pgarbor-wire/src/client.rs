//! Minimal PG v3 CLIENT for tests, diagnostics and the harness-independent smoke checks of the CLI
//! (lease WIR-21). It is NOT libpq: it implements exactly what is needed to drive the server in
//! integration tests without external drivers.
//!
//! ```ignore
//! let mut c = WireClient::connect_tcp(addr, "postgres", "postgres", None)?;
//! let r = c.simple_query("select 1 as x; select 'a'")?;
//! assert_eq!(r.results[0].rows[0][0].as_deref(), Some(&b"1"[..]));
//! let r = c.extended_query("select $1::int + 1", &[Some("41")])?;
//! ```
//!
//! ## Behavior
//! * `connect_*`: send StartupMessage 3.0 (`user`, `database`, `client_encoding=UTF8`), handle
//!   `AuthenticationOk`, cleartext, md5 (needs `password`) and SCRAM-SHA-256 (via `scram::ScramClient`),
//!   `NegotiateProtocolVersion` (recorded), collect ParameterStatus, BackendKeyData, stop at the first
//!   ReadyForQuery. ErrorResponse before that -> `Err(PgError)` rebuilt from the fields (code, message,
//!   severity).
//! * `simple_query`: send `Q`; read until ReadyForQuery; build a [`QueryResponse`].
//! * `extended_query`: unnamed Parse (no param oids), Bind with text parameters (`None` = NULL, all
//!   result columns text), Describe portal, Execute (0 rows), Sync; same response shape. Errors skip to
//!   ReadyForQuery.
//! * `send_raw` / `read_message` for byte-level protocol tests.
//! * `cancel()` opens a second connection and sends a CancelRequest with this client's key.
//! * Reads use a 10 s socket timeout by default (`set_timeout`).
use std::net::SocketAddr;
use std::io::{Read, Write};
use crate::buf::{MsgReader, MsgWriter};
use std::path::Path;
use std::time::Duration;

use pgarbor_common::{PgError, Result};

use crate::error_fields::ErrorFields;
use crate::transport::Stream;

/// One statement's result inside a [`QueryResponse`].
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    /// `(name, type_oid, format)` per column from RowDescription.
    pub columns: Vec<(String, u32, i16)>,
    /// Raw DataRow bytes (`None` = NULL).
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    /// CommandComplete tag (`None` for EmptyQueryResponse).
    pub tag: Option<String>,
}

/// Everything received until ReadyForQuery.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResponse {
    pub results: Vec<ResultSet>,
    pub notices: Vec<ErrorFields>,
    pub error: Option<ErrorFields>,
    pub notifications: Vec<(i32, String, String)>,
    pub parameter_status: Vec<(String, String)>,
    /// Status byte of the final ReadyForQuery (`b'I'`, `b'T'`, `b'E'`).
    pub status: u8,
}

/// The test client (module docs).
pub struct WireClient {
    pub(crate) stream: Stream,
    pub(crate) buf: Vec<u8>,
    pub(crate) params: Vec<(String, String)>,
    pub(crate) key: Option<(i32, Vec<u8>)>,
    pub(crate) status: u8,
    pub(crate) peer: Option<SocketAddr>,
}

impl WireClient {
    /// Connect over TCP and complete the startup (module docs).
    #[allow(unused_variables)]
    pub fn connect_tcp(addr: SocketAddr, user: &str, database: &str, password: Option<&str>) -> Result<WireClient> {
        let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(client_io)?;
        Self::startup(Stream::Tcp(stream), Some(addr), user, database, password)
    }

    /// Connect to a Unix socket file and complete the startup.
    #[allow(unused_variables)]
    pub fn connect_unix(path: &Path, user: &str, database: &str, password: Option<&str>) -> Result<WireClient> {
        #[cfg(unix)] { let stream = std::os::unix::net::UnixStream::connect(path).map_err(client_io)?; Self::startup(Stream::Unix(stream), None, user, database, password) }
        #[cfg(not(unix))] { Err(PgError::unimplemented("WIR-21", "Unix sockets on this platform")) }
    }

    #[allow(unused_variables)]
    pub fn simple_query(&mut self, sql: &str) -> Result<QueryResponse> {
        let mut data = Vec::new(); let mut w = MsgWriter::begin(&mut data, b'Q'); w.put_cstr(sql); w.finish(); self.send_raw(&data)?; self.read_response()
    }

    #[allow(unused_variables)]
    pub fn extended_query(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResponse> {
        let count = i16::try_from(params.len()).map_err(|_| protocol("too many parameters"))?;
        let mut data = Vec::new();
        let mut w = MsgWriter::begin(&mut data, b'P'); w.put_cstr(""); w.put_cstr(sql); w.put_i16(0); w.finish();
        let mut w = MsgWriter::begin(&mut data, b'B'); w.put_cstr(""); w.put_cstr(""); w.put_i16(0); w.put_i16(count);
        for value in params { match value { None => w.put_i32(-1), Some(v) => { w.put_i32(i32::try_from(v.len()).map_err(|_| protocol("parameter too large"))?); w.put_bytes(v.as_bytes()); } } }
        w.put_i16(0); w.finish();
        let mut w = MsgWriter::begin(&mut data, b'D'); w.put_u8(b'P'); w.put_cstr(""); w.finish();
        let mut w = MsgWriter::begin(&mut data, b'E'); w.put_cstr(""); w.put_i32(0); w.finish();
        MsgWriter::begin(&mut data, b'S').finish(); self.send_raw(&data)?; self.read_response()
    }

    /// Write raw bytes (already framed) to the socket.
    #[allow(unused_variables)]
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).map_err(client_io)?; self.stream.flush().map_err(client_io)
    }

    /// Read one tagged backend message `(tag, body)`; `Ok(None)` on EOF.
    pub fn read_message(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        read_frame(&mut self.stream)
    }

    /// Send CancelRequest for this connection's key on a NEW connection to the same address.
    pub fn cancel(&self) -> Result<()> {
        let addr = self.peer.ok_or_else(|| PgError::unimplemented("WIR-21", "cancel over Unix socket"))?;
        let (pid, key) = self.backend_key().ok_or_else(|| protocol("no backend key"))?;
        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(10)).map_err(client_io)?;
        let mut data = Vec::new(); data.extend_from_slice(&((12 + key.len()) as u32).to_be_bytes()); data.extend_from_slice(&80_877_102u32.to_be_bytes()); data.extend_from_slice(&pid.to_be_bytes()); data.extend_from_slice(key); stream.write_all(&data).map_err(client_io)
    }

    #[allow(unused_variables)]
    pub fn set_timeout(&self, d: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(d).map_err(client_io)?; self.stream.set_write_timeout(d).map_err(client_io)
    }

    /// Value of a ParameterStatus seen so far.
    pub fn parameter_status(&self, name: &str) -> Option<&str> {
        self.params.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
    /// `(pid, secret)` from BackendKeyData.
    pub fn backend_key(&self) -> Option<(i32, &[u8])> {
        self.key.as_ref().map(|(p, k)| (*p, k.as_slice()))
    }
    /// Last ReadyForQuery status byte.
    pub fn txn_status(&self) -> u8 {
        self.status
    }

    /// Send Terminate and close.
    pub fn terminate(self) -> Result<()> {
        let mut client = self; client.send_raw(&[b'X', 0, 0, 0, 4])?; client.stream.shutdown_both().map_err(client_io)
    }
}

fn client_io(e: std::io::Error) -> PgError { PgError::io("wire client", &e) }
fn protocol(message: &str) -> PgError { PgError::new(pgarbor_common::SqlState::PROTOCOL_VIOLATION, message) }

fn read_frame<R: Read>(stream: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0u8];
    loop { match stream.read(&mut tag) { Ok(0) => return Ok(None), Ok(_) => break, Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue, Err(e) => return Err(client_io(e)) } }
    let mut length = [0u8;4]; stream.read_exact(&mut length).map_err(client_io)?;
    let length = u32::from_be_bytes(length);
    if !(4..=268_435_456).contains(&length) { return Err(protocol("invalid backend message length")); }
    let mut body = vec![0u8; (length - 4) as usize]; stream.read_exact(&mut body).map_err(client_io)?;
    Ok(Some((tag[0], body)))
}
fn read_error(r: &mut MsgReader<'_>) -> Result<ErrorFields> {
    let mut fields = Vec::new();
    loop { let code = r.read_u8()?; if code == 0 { break; } fields.push((code, r.read_cstr()?.to_owned())); }
    r.finish()?; Ok(ErrorFields { fields })
}
impl WireClient {
    fn startup(stream: Stream, peer: Option<SocketAddr>, user: &str, database: &str, password: Option<&str>) -> Result<Self> {
        if user.contains('\0') || database.contains('\0') { return Err(protocol("invalid startup parameter")); }
        let mut client = Self { stream, buf: Vec::new(), params: Vec::new(), key: None, status: b'I', peer };
        client.set_timeout(Some(Duration::from_secs(10)))?;
        let mut data = Vec::from(196_608u32.to_be_bytes());
        for (key, value) in [("user",user),("database",database),("client_encoding","UTF8")] { data.extend_from_slice(key.as_bytes()); data.push(0); data.extend_from_slice(value.as_bytes()); data.push(0); }
        data.push(0); let length = u32::try_from(data.len() + 4).map_err(|_| protocol("startup packet too large"))?;
        client.send_raw(&length.to_be_bytes())?; client.send_raw(&data)?;
        let mut scram = None;
        loop {
            let (tag, body) = client.read_message()?.ok_or_else(|| protocol("EOF during startup"))?;
            let mut r = MsgReader::new(&body);
            match tag {
                b'R' => match r.read_i32()? {
                    0 => r.finish()?,
                    3 => { let pass = password.ok_or_else(|| protocol("password required"))?; let mut data = Vec::new(); let mut w = MsgWriter::begin(&mut data,b'p'); w.put_cstr(pass);w.finish();client.send_raw(&data)?; },
                    5 => { use md5::{Digest, Md5}; let pass = password.ok_or_else(|| protocol("password required"))?; let salt = r.take(4)?;r.finish()?; let inner = format!("{:x}", Md5::digest(format!("{pass}{user}").as_bytes())); let mut h = Md5::new();h.update(inner.as_bytes());h.update(salt);let pass = format!("md5{:x}",h.finalize());let mut data = Vec::new();let mut w = MsgWriter::begin(&mut data,b'p');w.put_cstr(&pass);w.finish();client.send_raw(&data)?; },
                    10 => { let mut supported = false;loop { let name = r.read_cstr()?; if name.is_empty() { break; } supported |= name == "SCRAM-SHA-256"; } r.finish()?; if !supported { return Err(protocol("unsupported SASL mechanism")); } let mut c = crate::scram::ScramClient::new(password.ok_or_else(|| protocol("password required"))?);let first = c.client_first()?; let mut data = Vec::new();let mut w = MsgWriter::begin(&mut data,b'p');w.put_cstr("SCRAM-SHA-256");w.put_i32(first.len() as i32);w.put_bytes(first.as_bytes());w.finish();client.send_raw(&data)?;scram = Some(c); },
                    11 => { let text = std::str::from_utf8(r.read_rest()).map_err(|_| protocol("invalid SASL text"))?;let c = scram.as_mut().ok_or_else(|| protocol("unexpected SASL continuation"))?;let final_msg = c.client_final(text)?;let mut data = Vec::new();let mut w = MsgWriter::begin(&mut data,b'p');w.put_bytes(final_msg.as_bytes());w.finish();client.send_raw(&data)?; },
                    12 => { let text = std::str::from_utf8(r.read_rest()).map_err(|_| protocol("invalid SASL text"))?;scram.as_ref().ok_or_else(|| protocol("unexpected SASL final"))?.verify_server_final(text)?; },
                    _ => return Err(PgError::unimplemented("WIR-21", "authentication method")),
                },
                b'S' => { let key = r.read_cstr()?.to_owned(); let value = r.read_cstr()?.to_owned();r.finish()?;client.params.push((key,value)); },
                b'K' => { let pid = r.read_i32()?;let secret = r.read_rest().to_vec();if secret.len() < 4 || secret.len() > 256 { return Err(protocol("invalid backend key")); } client.key=Some((pid,secret)); },
                b'E' => { let e = read_error(&mut r)?;return Err(PgError::new(pgarbor_common::SqlState::from_code(e.sqlstate()).unwrap_or(pgarbor_common::SqlState::INTERNAL_ERROR),e.message())); },
                b'Z' => { client.status = r.read_u8()?;r.finish()?;return Ok(client); },
                b'N' | b'v' => {},
                _ => return Err(protocol("unexpected backend startup message")),
            }
        }
    }
    fn read_response(&mut self) -> Result<QueryResponse> {
        let mut out = QueryResponse::default();let mut current = ResultSet::default();
        loop {
            let (tag,body)=self.read_message()?.ok_or_else(|| protocol("EOF before ReadyForQuery"))?;
            let mut r=MsgReader::new(&body);
            match tag {
                b'T' => { let count=r.read_i16()?;if count < 0 { return Err(protocol("negative field count")); }current=ResultSet::default();for _ in 0..count {let name=r.read_cstr()?.to_owned();r.read_u32()?;r.read_i16()?;let oid=r.read_u32()?;r.read_i16()?;r.read_i32()?;let format=r.read_i16()?;current.columns.push((name,oid,format));}r.finish()?; },
                b'D' => { let count=r.read_i16()?;if count < 0 || count as usize != current.columns.len() {return Err(protocol("invalid DataRow field count"));}let mut row=Vec::new();for _ in 0..count {let n=r.read_i32()?;row.push(if n == -1 {None} else if n < 0 {return Err(protocol("invalid field length"));} else {Some(r.take(n as usize)?.to_vec())});}r.finish()?;current.rows.push(row); },
                b'C' => {current.tag=Some(r.read_cstr()?.to_owned());r.finish()?;out.results.push(std::mem::take(&mut current));},
                b'I' => {r.finish()?;out.results.push(ResultSet::default());},
                b'E' => out.error=Some(read_error(&mut r)?),
                b'N' => out.notices.push(read_error(&mut r)?),
                b'S' => {let key=r.read_cstr()?.to_owned();let value=r.read_cstr()?.to_owned();r.finish()?;self.params.push((key.clone(),value.clone()));out.parameter_status.push((key,value));},
                b'A' => {let pid=r.read_i32()?;let channel=r.read_cstr()?.to_owned();let payload=r.read_cstr()?.to_owned();r.finish()?;out.notifications.push((pid,channel,payload));},
                b'Z' => {self.status=r.read_u8()?;r.finish()?;out.status=self.status;return Ok(out);},
                b'1' | b'2' | b'3' | b'n' => r.finish()?,
                b't' => {let count=r.read_i16()?;if count < 0 {return Err(protocol("invalid parameter count"));}for _ in 0..count {r.read_u32()?;}r.finish()?;},
                _ => return Err(PgError::unimplemented("WIR-21", format!("backend response {}",tag as char))),
            }
        }
    }
}
#[cfg(test)]
mod safety_tests {
    use super::*;
    #[test] fn malformed_frames_are_rejected_before_allocation() {
        for len in [0u32,3,268_435_457,u32::MAX] { let mut bytes=vec![b'D'];bytes.extend_from_slice(&len.to_be_bytes());assert_eq!(read_frame(&mut &bytes[..]).unwrap_err().sqlstate.code(),"08P01"); }
        assert!(read_frame(&mut &b""[..]).unwrap().is_none());
        assert!(read_frame(&mut &b"D\0\0"[..]).is_err());
    }
}
