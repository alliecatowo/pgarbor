//! Source text map: converts sqlparser `Location { line, column }` (1-based, counted in
//! characters; a `\n` starts a new line) into PostgreSQL error positions (1-based CHARACTER offsets
//! into the whole query string, `ErrorResponse` field `P`) and into byte offsets for slicing.
//! Fully implemented, frozen (v1).
//!
//! One `SourceMap` is built per submitted query string (which may contain several statements):
//! PG reports positions relative to the full string, so a statement in the middle of a script
//! carries a position that includes everything before it.
use sqlparser::tokenizer::{Location, Span};

/// Line index of a query string.
// CONTRACT
#[derive(Clone, Debug)]
pub struct SourceMap {
    text: String,
    /// For each line: (character index of its first char, byte index of its first char).
    lines: Vec<(usize, usize)>,
}

impl SourceMap {
    pub fn new(text: &str) -> SourceMap {
        let mut lines = vec![(0usize, 0usize)];
        let mut chars = 0usize;
        for (byte, ch) in text.char_indices() {
            chars += 1;
            if ch == '\n' {
                lines.push((chars, byte + ch.len_utf8()));
            }
        }
        SourceMap { text: text.to_string(), lines }
    }

    /// The full query text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// PG error position (1-based character offset) of a location; `None` for the empty
    /// location (`line == 0`) or a line past the end.
    pub fn char_pos(&self, loc: Location) -> Option<u32> {
        if loc.line == 0 || loc.column == 0 {
            return None;
        }
        let (char_start, _) = *self.lines.get(loc.line as usize - 1)?;
        Some((char_start as u64 + loc.column) as u32)
    }

    /// Position of the start of a span.
    pub fn pos_of_span(&self, span: Span) -> Option<u32> {
        self.char_pos(span.start)
    }

    /// Byte offset of a location (clamped to the text length), `None` for the empty location.
    pub fn byte_offset(&self, loc: Location) -> Option<usize> {
        if loc.line == 0 || loc.column == 0 {
            return None;
        }
        let (_, byte_start) = *self.lines.get(loc.line as usize - 1)?;
        let rest = &self.text[byte_start..];
        let mut off = 0usize;
        let mut col = 1u64;
        for ch in rest.chars() {
            if col == loc.column {
                return Some(byte_start + off);
            }
            if ch == '\n' {
                break;
            }
            off += ch.len_utf8();
            col += 1;
        }
        Some(byte_start + off)
    }

    /// Position one past the last character (for `syntax error at end of input`).
    pub fn end_pos(&self) -> u32 {
        (self.text.chars().count() + 1) as u32
    }

    /// Convert a byte offset into a 1-based character position.
    pub fn pos_of_byte(&self, byte: usize) -> u32 {
        let byte = byte.min(self.text.len());
        (self.text[..byte].chars().count() + 1) as u32
    }

    /// Slice by byte offsets (clamped, char-boundary safe).
    pub fn slice(&self, start: usize, end: usize) -> &str {
        let mut s = start.min(self.text.len());
        let mut e = end.min(self.text.len());
        while s > 0 && !self.text.is_char_boundary(s) {
            s -= 1;
        }
        while e < self.text.len() && !self.text.is_char_boundary(e) {
            e += 1;
        }
        &self.text[s..e.max(s)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(line: u64, column: u64) -> Location {
        Location { line, column }
    }

    #[test]
    fn positions_across_lines() {
        let m = SourceMap::new("select 1;\nselect é, 2");
        assert_eq!(m.char_pos(loc(1, 1)), Some(1));
        assert_eq!(m.char_pos(loc(2, 1)), Some(11));
        // 'é' is one character but two bytes.
        assert_eq!(m.char_pos(loc(2, 10)), Some(20));
        assert_eq!(m.byte_offset(loc(2, 9)), Some(10 + 7 + 2));
        assert_eq!(m.char_pos(loc(0, 0)), None);
        assert_eq!(m.end_pos(), 22);
    }

    #[test]
    fn slice_is_boundary_safe() {
        let m = SourceMap::new("aé;");
        assert_eq!(m.slice(0, 2), "aé");
    }
}
