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
use std::time::Duration;

use pgarbor_common::PgError;

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
    #[allow(unused_variables)]
    pub fn poll_frame(&mut self, timeout: Option<Duration>) -> WireResult<Polled<Frame>> {
        Err(WireError::Pg(PgError::unimplemented("WIR-05", "poll a tagged frame (FrameReader::poll_frame)")))
    }

    /// Like [`FrameReader::poll_frame`] for the untagged first packet; returns the bytes after the
    /// length field. `max_len` is `MAX_STARTUP_PACKET_LEN`.
    #[allow(unused_variables)]
    pub fn poll_startup(&mut self, timeout: Option<Duration>, max_len: usize) -> WireResult<Polled<Vec<u8>>> {
        Err(WireError::Pg(PgError::unimplemented("WIR-05", "poll the startup packet (FrameReader::poll_startup)")))
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
    #[allow(unused_variables)]
    pub fn push(&mut self, msg: &Backend) -> WireResult<()> {
        Err(WireError::Pg(PgError::unimplemented("WIR-05", "buffer a backend message (FrameWriter::push)")))
    }

    /// Append raw bytes (already framed).
    pub fn push_raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Write the whole buffer to the socket and flush it; clears the buffer even on error (the
    /// connection is dead then).
    #[allow(unused_variables)]
    pub fn flush(&mut self) -> WireResult<()> {
        Err(WireError::Pg(PgError::unimplemented("WIR-05", "flush buffered output (FrameWriter::flush)")))
    }

    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}
