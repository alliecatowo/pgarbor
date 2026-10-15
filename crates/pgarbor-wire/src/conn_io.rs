//! Typed, transcript-aware connection I/O (lease WIR-06): the object every protocol handler
//! receives. Wraps a [`FrameReader`] and a [`FrameWriter`] over two clones of one socket.
//!
//! ## Responsibilities
//! * `send(&Backend)`: transcript record (S2C) + `FrameWriter::push`.
//! * `flush()`: write buffered output.
//! * `poll(timeout)` / `recv()`: read one frame, record it (C2S), decode with
//!   `frontend::decode_frontend`. **Before blocking on a read, any pending output is flushed**
//!   (the classic hang otherwise). A decode error is returned as `WireError::Pg` with the original
//!   `08P01` error (non-fatal: the frame boundary is intact, so the caller reports it and continues).
//! * startup-phase helpers: `poll_startup` (record the raw packet), `write_ssl_answer(b'N')`
//!   (single byte, flushed immediately, recorded as `SSLResponse`).
//! * `shutdown()`: best-effort flush then `shutdown(Both)` on the socket.
//!
//! The transcript `seq` counter is per connection and shared by both directions; `elapsed_us` is
//! measured from `ConnIo::new`.
use std::time::{Duration, Instant};

use crate::backend::{encode_backend, Backend};
use crate::config::ServerConfig;
use crate::consts::{CANCEL_REQUEST_CODE, GSSENC_REQUEST_CODE, MAX_STARTUP_PACKET_LEN, SSL_REQUEST_CODE};
use crate::error::{WireError, WireResult};
use crate::framing::{Frame, FrameReader, FrameWriter, Polled};
use crate::frontend::Frontend;
use crate::transcript::{TranscriptDirection, TranscriptEntry, TranscriptHandle};
use crate::transport::{PeerInfo, Stream};

/// Connection I/O (module docs).
pub struct ConnIo {
    pub(crate) reader: FrameReader,
    pub(crate) writer: FrameWriter,
    pub(crate) peer: PeerInfo,
    pub(crate) conn_id: u64,
    pub(crate) seq: u64,
    pub(crate) started: Instant,
    pub(crate) transcript: Option<TranscriptHandle>,
}

impl ConnIo {
    /// Wrap `stream`: `try_clone` for the writer half, apply `tcp_nodelay`, capture the peer.
    /// `cfg.max_frame_bytes` bounds frames, `cfg.transcript` enables recording.
    pub fn new(stream: Stream, conn_id: u64, cfg: &ServerConfig) -> WireResult<ConnIo> {
        let peer = stream.peer_info();
        stream.set_nodelay(cfg.tcp_nodelay)?;
        let writer_stream = stream.try_clone()?;
        Ok(ConnIo {
            reader: FrameReader::new(stream, cfg.max_frame_bytes),
            writer: FrameWriter::new(writer_stream),
            peer,
            conn_id,
            seq: 0,
            started: Instant::now(),
            transcript: cfg.transcript.clone(),
        })
    }

    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Queue a backend message (module docs). Encode with `backend::encode_backend` into a scratch
    /// buffer first so the transcript can record the exact body (`bytes[5..]`), then
    /// `FrameWriter::push_raw`-style append and auto-flush past `AUTO_FLUSH_BYTES`.
    pub fn send(&mut self, msg: &Backend) -> WireResult<()> {
        let mut bytes = Vec::new();
        encode_backend(msg, &mut bytes)?;
        self.record(TranscriptDirection::ServerToClient, Some(msg.tag()), msg.name(), &bytes[5..]);
        self.writer.push_raw(&bytes);
        if self.writer.pending() > crate::framing::AUTO_FLUSH_BYTES {
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Flush buffered output.
    pub fn flush(&mut self) -> WireResult<()> {
        self.writer.flush()
    }

    /// Send + flush (used for auth requests and fatal errors).
    pub fn send_flush(&mut self, msg: &Backend) -> WireResult<()> {
        self.send(msg)?;
        self.flush()
    }

    /// Wait up to `timeout` for one decoded frontend message (module docs). `Eof` = clean close.
    pub fn poll(&mut self, timeout: Option<Duration>) -> WireResult<Polled<Frontend>> {
        match self.poll_raw(timeout)? {
            Polled::Ready(frame) => crate::frontend::decode_frontend(frame.tag, &frame.body)
                .map(Polled::Ready).map_err(WireError::Pg),
            Polled::TimedOut => Ok(Polled::TimedOut),
            Polled::Eof => Ok(Polled::Eof),
        }
    }

    /// Blocking receive; EOF -> `WireError::Closed`.
    pub fn recv(&mut self) -> WireResult<Frontend> {
        match self.poll(None)? {
            Polled::Ready(m) => Ok(m),
            Polled::Eof => Err(WireError::Closed),
            Polled::TimedOut => Err(WireError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))),
        }
    }

    /// Next RAW frame without decoding (authentication reads `p` messages by context, COPY mode
    /// needs the tag to produce PG's `unexpected message type` error). Records it like `poll`.
    pub fn poll_raw(&mut self, timeout: Option<Duration>) -> WireResult<Polled<Frame>> {
        if self.writer.pending() > 0 {
            self.writer.flush()?;
        }
        match self.reader.poll_frame(timeout)? {
            Polled::Ready(frame) => {
                let name = match crate::frontend::decode_frontend(frame.tag, &frame.body) {
                    Ok(frontend) => frontend.name(),
                    Err(_) => "Invalid",
                };
                self.record(TranscriptDirection::ClientToServer, Some(frame.tag), name, &frame.body);
                Ok(Polled::Ready(frame))
            }
            Polled::TimedOut => Ok(Polled::TimedOut),
            Polled::Eof => Ok(Polled::Eof),
        }
    }

    /// Read the untagged first packet (`FrameReader::poll_startup`) and record it (`tag = None`,
    /// body = the bytes after the length field). The recorded name is derived from the leading
    /// Int32 code: `"SSLRequest"`, `"GSSENCRequest"`, `"CancelRequest"`, otherwise `"StartupMessage"`.
    pub fn poll_startup(&mut self, timeout: Option<Duration>) -> WireResult<Polled<Vec<u8>>> {
        if self.writer.pending() > 0 {
            self.writer.flush()?;
        }
        match self.reader.poll_startup(timeout, MAX_STARTUP_PACKET_LEN)? {
            Polled::Ready(packet) => {
                let name = startup_name(&packet);
                self.record(TranscriptDirection::ClientToServer, None, name, &packet);
                Ok(Polled::Ready(packet))
            }
            Polled::TimedOut => Ok(Polled::TimedOut),
            Polled::Eof => Ok(Polled::Eof),
        }
    }

    /// Answer an SSLRequest / GSSENCRequest: write the single byte (`b'N'`) immediately, flushed,
    /// recorded as `SSLResponse`.
    pub fn write_ssl_answer(&mut self, answer: u8) -> WireResult<()> {
        self.writer.push_raw(&[answer]);
        self.writer.flush()?;
        self.record(TranscriptDirection::ServerToClient, None, "SSLResponse", &[answer]);
        Ok(())
    }

    /// True when the reader holds unconsumed bytes (pipelining).
    pub fn has_buffered_input(&self) -> bool {
        self.reader.has_buffered_data()
    }

    /// Bytes waiting to be flushed.
    pub fn pending_output(&self) -> usize {
        self.writer.pending()
    }

    /// Best-effort flush, then close both directions.
    pub fn shutdown(&mut self) {
        let _ = self.writer.flush();
        let _ = self.writer.stream().shutdown_both();
    }

    /// Record one entry (shared by the methods above).
    pub(crate) fn record(&mut self, dir: TranscriptDirection, tag: Option<u8>, name: &str, body: &[u8]) {
        if let Some(t) = &self.transcript {
            let entry = TranscriptEntry {
                conn_id: self.conn_id,
                seq: self.seq,
                direction: dir,
                tag,
                name: name.to_string(),
                body: body.to_vec(),
                elapsed_us: self.started.elapsed().as_micros() as u64,
            };
            t.0.record(entry);
        }
        self.seq += 1;
    }
}

fn startup_name(packet: &[u8]) -> &'static str {
    match packet.get(..4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_be_bytes) {
        Some(SSL_REQUEST_CODE) => "SSLRequest",
        Some(GSSENC_REQUEST_CODE) => "GSSENCRequest",
        Some(CANCEL_REQUEST_CODE) => "CancelRequest",
        _ => "StartupMessage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_packet_names_use_request_codes() {
        assert_eq!(startup_name(&SSL_REQUEST_CODE.to_be_bytes()), "SSLRequest");
        assert_eq!(startup_name(&GSSENC_REQUEST_CODE.to_be_bytes()), "GSSENCRequest");
        assert_eq!(startup_name(&CANCEL_REQUEST_CODE.to_be_bytes()), "CancelRequest");
        assert_eq!(startup_name(&[0, 3, 0, 0]), "StartupMessage");
        assert_eq!(startup_name(&[1, 2]), "StartupMessage");
    }
}
