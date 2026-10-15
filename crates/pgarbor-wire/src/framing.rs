//! Message framing over a [`Stream`] (lease WIR-05): a resumable frame reader and a buffered writer.
//!
//! ## Why a custom reader
//! Connection threads must wake periodically while idle (server shutdown, NOTIFY delivery, idle
//! timeouts) but a read timeout in the middle of a message would lose the bytes already consumed
//! with `read_exact`. [`FrameReader`] therefore owns an accumulation buffer: `poll_frame` reads
//! whatever is available (honoring the socket read timeout) and returns a frame only when a COMPLETE
//! one is buffered; partial data stays buffered across timeouts.
//!
//! ## Frame format
//! Regular message: `tag: u8`, `len: Int32` (includes the 4 length bytes, excludes the tag), body of
//! `len - 4` bytes. Startup-phase packet: `len: Int32` (includes itself), body of `len - 4` bytes, no tag.
//!
//! ## Errors
//! * EOF before the first byte of a frame: [`Polled::Eof`] (clean close). EOF in the middle of a
//!   frame: `WireError::Io(UnexpectedEof)`.
//! * `len < 4`: FATAL `08P01` `invalid message length`.
//! * `len - 4 > max_body`: FATAL `08P01` `invalid message length` (the body is NOT read or
//!   allocated; the connection is closed).
//! * Startup packet: `len` outside `8..=max_startup` -> FATAL `08P01` `invalid length of startup packet`.
//! * `ErrorKind::WouldBlock` / `TimedOut` from the socket = timeout (not an error); `Interrupted` is
//!   retried.
//!
//! ## Writer
//! [`FrameWriter`] appends encoded messages to an in-memory buffer; `flush` writes it all
//! (`write_all`). It auto-flushes when the buffer exceeds [`AUTO_FLUSH_BYTES`] so a huge result set
//! streams instead of accumulating. Write errors (`BrokenPipe`, `ConnectionReset`) are returned as
//! `WireError::Io`.
use std::io::{Read, Write};
use std::time::Duration;

use pgarbor_common::{PgError, SqlState};

use crate::backend::Backend;
use crate::error::{WireError, WireResult};
use crate::transport::Stream;

/// Flush the output buffer once it holds this many bytes.
pub const AUTO_FLUSH_BYTES: usize = 64 * 1024;

/// One tagged frontend frame.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub tag: u8,
    /// Bytes after the length field.
    pub body: Vec<u8>,
}

/// Outcome of a timed read.
// CONTRACT
#[derive(Debug, PartialEq, Eq)]
pub enum Polled<T> {
    Ready(T),
    /// No complete frame arrived within the timeout; partial bytes stay buffered.
    TimedOut,
    /// Peer closed the connection at a frame boundary.
    Eof,
}

/// Resumable frame reader (module docs).
#[derive(Debug)]
pub struct FrameReader {
    stream: Stream,
    buf: Vec<u8>,
    /// Start of unconsumed data in `buf`.
    pos: usize,
    max_body: usize,
}

impl FrameReader {
    /// `max_body` bounds one message body (`ServerConfig::max_frame_bytes`).
    pub fn new(stream: Stream, max_body: usize) -> FrameReader {
        FrameReader { stream, buf: Vec::new(), pos: 0, max_body }
    }

    /// True when unconsumed bytes are buffered (a pipelined client sent more than one message).
    pub fn has_buffered_data(&self) -> bool {
        self.pos < self.buf.len()
    }

    /// Wait up to `timeout` (`None` = forever) for one complete tagged frame.
    ///
    /// Algorithm: loop { if `buf[pos..]` holds a full frame (>= 5 bytes and `5 + len - 4` total),
    /// validate the length (module docs), copy the body out, advance `pos`, compact the buffer
    /// when `pos` passes half of `buf.len()`, and return it; otherwise `set_read_timeout(timeout)`
    /// on the stream and `read` into a 8 KiB scratch buffer, appending to `buf`. A read of 0 bytes
    /// is EOF: `Eof` if nothing is buffered else `Io(UnexpectedEof)`. A timeout error returns `TimedOut`.}
    /// The timeout applies per `read` call, which is sufficient for idle polling.
    pub fn poll_frame(&mut self, timeout: Option<Duration>) -> WireResult<Polled<Frame>> {
        let mut scratch = [0u8; 8192];
        loop {
            let available = self.buf.len().saturating_sub(self.pos);
            if available >= 5 {
                let start = self.pos;
                let len = u32::from_be_bytes([
                    self.buf[start + 1], self.buf[start + 2], self.buf[start + 3], self.buf[start + 4],
                ]);
                if len < 4 || (len - 4) as u64 > self.max_body as u64 {
                    return Err(WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid message length")));
                }
                let body_len = (len - 4) as usize;
                let total = 5usize.checked_add(body_len).ok_or_else(|| {
                    WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid message length"))
                })?;
                if available >= total {
                    let tag = self.buf[start];
                    let body = self.buf[start + 5..start + total].to_vec();
                    self.pos += total;
                    if self.pos > self.buf.len() / 2 {
                        self.buf.drain(..self.pos);
                        self.pos = 0;
                    }
                    return Ok(Polled::Ready(Frame { tag, body }));
                }
            }
            if self.pos == self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            } else if self.pos > self.buf.len() / 2 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            self.stream.set_read_timeout(timeout)?;
            match self.stream.read(&mut scratch) {
                Ok(0) => {
                    if self.buf.len() == self.pos {
                        return Ok(Polled::Eof);
                    }
                    return Err(WireError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected EOF while reading message")));
                }
                Ok(n) => self.buf.extend_from_slice(&scratch[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => return Ok(Polled::TimedOut),
                Err(e) => return Err(WireError::Io(e)),
            }
        }
    }

    /// Like [`FrameReader::poll_frame`] for the untagged first packet; returns the bytes after the
    /// length field. `max_len` is `MAX_STARTUP_PACKET_LEN`.
    pub fn poll_startup(&mut self, timeout: Option<Duration>, max_len: usize) -> WireResult<Polled<Vec<u8>>> {
        let mut scratch = [0u8; 8192];
        loop {
            let available = self.buf.len().saturating_sub(self.pos);
            if available >= 4 {
                let start = self.pos;
                let len = u32::from_be_bytes([
                    self.buf[start], self.buf[start + 1], self.buf[start + 2], self.buf[start + 3],
                ]);
                if len < 8 || len as u64 > max_len as u64 {
                    return Err(WireError::fatal(PgError::new(SqlState::PROTOCOL_VIOLATION, "invalid length of startup packet")));
                }
                let total = len as usize;
                if available >= total {
                    let body = self.buf[start + 4..start + total].to_vec();
                    self.pos += total;
                    if self.pos > self.buf.len() / 2 {
                        self.buf.drain(..self.pos);
                        self.pos = 0;
                    }
                    return Ok(Polled::Ready(body));
                }
            }
            if self.pos == self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            } else if self.pos > self.buf.len() / 2 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            self.stream.set_read_timeout(timeout)?;
            match self.stream.read(&mut scratch) {
                Ok(0) => {
                    if self.buf.len() == self.pos { return Ok(Polled::Eof); }
                    return Err(WireError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected EOF while reading startup packet")));
                }
                Ok(n) => self.buf.extend_from_slice(&scratch[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => return Ok(Polled::TimedOut),
                Err(e) => return Err(WireError::Io(e)),
            }
        }
    }

    /// Blocking convenience: `poll_frame(None)`; `Eof` becomes `WireError::Closed`.
    pub fn read_frame(&mut self) -> WireResult<Frame> {
        match self.poll_frame(None)? {
            Polled::Ready(f) => Ok(f),
            Polled::Eof => Err(WireError::Closed),
            Polled::TimedOut => Err(WireError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "unexpected timeout while reading a frame",
            ))),
        }
    }

    /// The underlying stream (for `set_read_timeout` / shutdown).
    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}

/// Buffered writer (module docs).
#[derive(Debug)]
pub struct FrameWriter {
    stream: Stream,
    buf: Vec<u8>,
}

impl FrameWriter {
    pub fn new(stream: Stream) -> FrameWriter {
        FrameWriter { stream, buf: Vec::with_capacity(8192) }
    }

    /// Bytes buffered and not yet written.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Encode `msg` (`backend::encode_backend`) into the buffer; auto-flush past [`AUTO_FLUSH_BYTES`].
    /// Encoding errors (`54000`) are returned as `WireError::Pg` and leave the buffer unchanged.
    pub fn push(&mut self, msg: &Backend) -> WireResult<()> {
        let mut encoded = Vec::new();
        crate::backend::encode_backend(msg, &mut encoded)?;
        self.buf.extend_from_slice(&encoded);
        if self.buf.len() > AUTO_FLUSH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// Append raw bytes (already framed).
    pub fn push_raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Write the whole buffer to the socket and flush it; clears the buffer even on error (the
    /// connection is dead then).
    pub fn flush(&mut self) -> WireResult<()> {
        let result = self.stream.write_all(&self.buf).and_then(|()| self.stream.flush());
        self.buf.clear();
        result.map_err(WireError::Io)
    }

    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn connected() -> (Stream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener address");
        let client = TcpStream::connect(addr).expect("connect test client");
        let (server, _) = listener.accept().expect("accept test client");
        (Stream::Tcp(server), client)
    }

    #[test]
    fn partial_frame_survives_timeout() {
        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        peer.write_all(b"Q\0\0").expect("write partial header");
        assert_eq!(reader.poll_frame(Some(Duration::from_millis(20))).expect("poll"), Polled::TimedOut);
        peer.write_all(b"\0\x06a\0").expect("write remainder");
        assert_eq!(reader.poll_frame(Some(Duration::from_secs(1))).expect("poll complete"), Polled::Ready(Frame { tag: b'Q', body: b"a\0".to_vec() }));
    }

    #[test]
    fn pipelined_frames_are_returned_one_at_a_time() {
        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        peer.write_all(b"Q\0\0\0\x05aZ\0\0\0\x05b").expect("write frames");
        assert_eq!(reader.poll_frame(Some(Duration::from_secs(1))).expect("first"), Polled::Ready(Frame { tag: b'Q', body: b"a".to_vec() }));
        assert_eq!(reader.poll_frame(Some(Duration::from_secs(1))).expect("second"), Polled::Ready(Frame { tag: b'Z', body: b"b".to_vec() }));
    }

    #[test]
    fn invalid_length_is_fatal_before_body_read() {
        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 16);
        peer.write_all(b"Q\0\0\0\x03").expect("write invalid header");
        match reader.poll_frame(Some(Duration::from_secs(1))) {
            Err(WireError::Pg(e)) => {
                assert_eq!(e.sqlstate.code(), "08P01");
                assert_eq!(e.message, "invalid message length");
                assert_eq!(e.severity, pgarbor_common::Severity::Fatal);
            }
            _ => panic!("expected fatal protocol error"),
        }
    }

    #[test]
    fn oversized_length_is_rejected_from_header() {
        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 16);
        peer.write_all(b"Q\0\0\0\x15").expect("write oversized header");
        match reader.poll_frame(Some(Duration::from_secs(1))) {
            Err(WireError::Pg(e)) => {
                assert_eq!(e.sqlstate.code(), "08P01");
                assert_eq!(e.message, "invalid message length");
            }
            _ => panic!("expected protocol error"),
        }
    }

    #[test]
    fn startup_packet_is_bounded_and_returns_body() {
        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        peer.write_all(b"\0\0\0\x08\x04\xd2\x16\x2f").expect("write startup packet");
        assert_eq!(reader.poll_startup(Some(Duration::from_secs(1)), 64).expect("startup"), Polled::Ready(vec![4, 210, 22, 47]));

        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        peer.write_all(b"\0\0\0\x40").expect("write oversized startup header");
        match reader.poll_startup(Some(Duration::from_secs(1)), 32) {
            Err(WireError::Pg(e)) => {
                assert_eq!(e.sqlstate.code(), "08P01");
                assert_eq!(e.message, "invalid length of startup packet");
            }
            _ => panic!("expected startup protocol error"),
        }
    }

    #[test]
    fn eof_mid_frame_is_io_error_and_boundary_eof_is_clean() {
        let (stream, peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        drop(peer);
        assert!(matches!(reader.poll_frame(None), Ok(Polled::Eof)));

        let (stream, mut peer) = connected();
        let mut reader = FrameReader::new(stream, 1024);
        peer.write_all(b"Q\0\0").expect("write partial frame");
        peer.shutdown(std::net::Shutdown::Write).expect("close write side");
        assert!(matches!(reader.poll_frame(None), Err(WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof));
    }

    #[test]
    fn writer_push_auto_flushes_large_messages() {
        let (stream, mut peer) = connected();
        peer.set_read_timeout(Some(Duration::from_secs(1))).expect("set test read timeout");
        let drain = thread::spawn(move || {
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).expect("read writer output");
            bytes
        });
        let mut writer = FrameWriter::new(stream);
        let msg = Backend::CopyData(vec![b'x'; AUTO_FLUSH_BYTES]);
        writer.push(&msg).expect("push large message");
        assert_eq!(writer.pending(), 0);
        writer.stream.shutdown_both().expect("close test stream");
        let received = drain.join().expect("join reader");
        assert!(received.len() > AUTO_FLUSH_BYTES);
    }
}
