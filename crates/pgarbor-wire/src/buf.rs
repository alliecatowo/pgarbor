//! Byte cursors for message bodies. Fully implemented, frozen (v1).
//!
//! [`MsgReader`] mirrors PG's `pq_getmsg*` family (`libpq/pqformat.c`) including its error wording:
//! every failure is `08P01` (`PROTOCOL_VIOLATION`). [`MsgWriter`] builds one framed message
//! (`tag` + Int32 length that counts itself but not the tag).
use pgarbor_common::{PgError, Result, SqlState};

fn pv(msg: &str) -> PgError {
    PgError::new(SqlState::PROTOCOL_VIOLATION, msg)
}

/// Cursor over a message body. Never panics; never allocates more than the body holds.
// CONTRACT
#[derive(Debug, Clone)]
pub struct MsgReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> MsgReader<'a> {
    pub fn new(data: &'a [u8]) -> MsgReader<'a> {
        MsgReader { data, pos: 0 }
    }
    /// Bytes not consumed yet.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    /// Take `n` raw bytes. Error: `insufficient data left in message`.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(pv("insufficient data left in message"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn read_i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    pub fn read_i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// NUL-terminated string. Errors: no terminator -> `invalid string in message`; not UTF-8 ->
    /// `invalid byte sequence for encoding "UTF8"` is the PG text for bad client strings, but we use
    /// `invalid string in message` for both (client_encoding conversion is a non-goal).
    pub fn read_cstr(&mut self) -> Result<&'a str> {
        let rest = &self.data[self.pos..];
        let nul = rest.iter().position(|b| *b == 0).ok_or_else(|| pv("invalid string in message"))?;
        let s = std::str::from_utf8(&rest[..nul]).map_err(|_| pv("invalid string in message"))?;
        self.pos += nul + 1;
        Ok(s)
    }
    /// Everything left (consumes it).
    pub fn read_rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos..];
        self.pos = self.data.len();
        s
    }
    /// Require the body to be fully consumed (PG `pq_getmsgend`): `invalid message format`.
    pub fn finish(&self) -> Result<()> {
        if self.pos != self.data.len() {
            return Err(pv("invalid message format"));
        }
        Ok(())
    }
}

/// Builds one backend message into a caller-owned buffer. `MsgWriter::begin` appends the tag and a
/// length placeholder; `finish()` patches the length.
// CONTRACT
#[derive(Debug)]
pub struct MsgWriter<'a> {
    out: &'a mut Vec<u8>,
    start: usize,
}

impl<'a> MsgWriter<'a> {
    /// Append a new message with `tag` to `out`.
    pub fn begin(out: &'a mut Vec<u8>, tag: u8) -> MsgWriter<'a> {
        let start = out.len();
        out.push(tag);
        out.extend_from_slice(&[0, 0, 0, 0]);
        MsgWriter { out, start }
    }
    pub fn put_u8(&mut self, v: u8) {
        self.out.push(v);
    }
    pub fn put_i16(&mut self, v: i16) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }
    pub fn put_i32(&mut self, v: i32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }
    pub fn put_u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }
    pub fn put_bytes(&mut self, v: &[u8]) {
        self.out.extend_from_slice(v);
    }
    /// String followed by NUL. Interior NULs are replaced by `?` (a NUL would corrupt framing).
    pub fn put_cstr(&mut self, s: &str) {
        for b in s.bytes() {
            self.out.push(if b == 0 { b'?' } else { b });
        }
        self.out.push(0);
    }
    /// Patch the length field (counts itself, not the tag). Saturates at `i32::MAX`.
    pub fn finish(self) {
        let len = (self.out.len() - self.start - 1).min(i32::MAX as usize) as i32;
        let at = self.start + 1;
        self.out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_roundtrip_and_errors() {
        let mut r = MsgReader::new(b"ab\0\x00\x01\xff\xff");
        assert_eq!(r.read_cstr().unwrap(), "ab");
        assert_eq!(r.read_i16().unwrap(), 1);
        assert_eq!(r.read_i16().unwrap(), -1);
        assert!(r.finish().is_ok());
        assert_eq!(r.read_u8().unwrap_err().sqlstate.code(), "08P01");
        let mut r = MsgReader::new(b"abc");
        assert_eq!(r.read_cstr().unwrap_err().message, "invalid string in message");
    }

    #[test]
    fn writer_patches_length() {
        let mut out = Vec::new();
        let mut w = MsgWriter::begin(&mut out, b'Z');
        w.put_u8(b'I');
        w.finish();
        assert_eq!(out, vec![b'Z', 0, 0, 0, 5, b'I']);
    }
}
