//! Character type I/O: text, varchar(n), bpchar(n), name, "char", unknown, cstring, pg_node_tree
//! (TYPE-01, TYPE-15). Lease TYP-13.
//!
//! PG18 reference: `utils/adt/varlena.c` (textin/out/recv/send), `varchar.c` (bpcharin, varcharin,
//! `bpchar_input`, `varchar_input`), `name.c` (namein), `char.c` (charin/charout).
//!
//! All string Datums are `Datum::Text(String)` (valid UTF-8). Rules:
//! - Any input containing U+0000 → 22021 `invalid byte sequence for encoding "UTF8": 0x00`.
//!   Binary input that is not valid UTF-8 → 22021 `invalid byte sequence for encoding "UTF8": 0xNN 0xNN`
//!   (PG prints the offending bytes, up to the remaining length of that sequence, as `0x%02x` joined by spaces).
//! - text, unknown, cstring, pg_node_tree: stored verbatim.
//! - varchar(n) (typmod n+4): length counted in CHARACTERS. If longer than n: when every excess
//!   character is a space, silently truncate to n; otherwise 22001
//!   `value too long for type character varying(n)`. No typmod → no limit.
//! - bpchar(n): same length check (message `value too long for type character(n)`), then right-pad
//!   with spaces to n characters. The STORED value keeps its padding; output prints it padded.
//!   bpchar without typmod (`'ab  '::bpchar`) is stored as given (no padding, no trimming).
//!   SQL `char` with no length is `bpchar(1)` — that is the binder's job (`names`), not this file's.
//! - name: truncated (never an error) to at most 63 BYTES on a UTF-8 character boundary.
//! - "char" (`Datum::Char(u8)`): input = first BYTE of the string (multi-byte UTF-8 → its first byte),
//!   empty string → 0, except the 4-char form `\ooo` (backslash + 3 octal digits) which gives that byte.
//!   Output: 0 → ``, byte >= 0x80 → `\ooo` (3-digit octal), otherwise the ASCII char.
//!   Binary: exactly 1 byte (recv of 0 bytes → value 0 is NOT allowed: 22P03).
//! - Binary I/O for text/varchar/bpchar/name/unknown: the raw UTF-8 bytes (recv applies the same
//!   typmod rules as input).
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::io::invalid_binary;
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};

/// `NAMEDATALEN - 1`: the longest `name` in bytes.
const NAME_MAX_BYTES: usize = 63;
/// `NAMEDATALEN`: a binary `name` of this many bytes or more is rejected by recv (42622).
const NAMEDATALEN: usize = 64;

/// Apply varchar/bpchar length rules for typmod `typmod` (no-op when < 0).
/// `is_bpchar` selects padding + the `character(n)` message. `explicit` = explicit cast semantics
/// (`'abcd'::varchar(3)` silently truncates to `abc` instead of erroring; oracle-verified).
// CONTRACT
#[allow(unused_variables)]
pub fn apply_char_typmod(s: String, typmod: i32, is_bpchar: bool, explicit: bool) -> Result<String> {
    if typmod < 4 {
        return Ok(s);
    }
    let max = (typmod - 4) as usize;
    let mut s = s;
    // Byte offset of the first character beyond the limit, if the string is too long.
    let cut = s.char_indices().nth(max).map(|(i, _)| i);
    if let Some(cut) = cut {
        if !explicit && s.as_bytes()[cut..].iter().any(|&b| b != b' ') {
            let type_name = if is_bpchar { "character" } else { "character varying" };
            return Err(PgError::new(
                SqlState::STRING_DATA_RIGHT_TRUNCATION,
                format!("value too long for type {type_name}({max})"),
            ));
        }
        s.truncate(cut);
    }
    if is_bpchar {
        let len = s.chars().count();
        if len < max {
            s.extend(std::iter::repeat(' ').take(max - len));
        }
    }
    Ok(s)
}

/// Truncate to a valid `name` (≤ 63 bytes on a char boundary).
// CONTRACT
#[allow(unused_variables)]
pub fn truncate_name(s: &str) -> Result<String> {
    check_no_nul(s)?;
    Ok(clip_to_bytes(s, NAME_MAX_BYTES).to_string())
}

/// bpchar semantic value: the string without trailing spaces (used by comparison, length, casts to text).
pub fn bpchar_trimmed(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// Text input for this family (see module docs).
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    check_no_nul(s)?;
    match ty.oid {
        oids::CHAR => Ok(Datum::Char(char_from_str(s))),
        oids::NAME => Ok(Datum::Text(clip_to_bytes(s, NAME_MAX_BYTES).to_string())),
        oids::VARCHAR => Ok(Datum::Text(apply_char_typmod(s.to_string(), ty.typmod, false, false)?)),
        oids::BPCHAR => Ok(Datum::Text(apply_char_typmod(s.to_string(), ty.typmod, true, false)?)),
        // text, unknown, cstring, pg_node_tree and every other text-input type: verbatim.
        _ => Ok(Datum::Text(s.to_string())),
    }
}

/// Text output for this family.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    match (ty.oid, d) {
        (oids::CHAR, Datum::Char(b)) => Ok(char_to_string(*b)),
        (_, Datum::Text(s)) if ty.oid != oids::CHAR => Ok(s.clone()),
        _ => Err(unexpected_datum("character output", ty)),
    }
}

/// Binary input for this family.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    if ty.oid == oids::CHAR {
        return match buf {
            [b] => Ok(Datum::Char(*b)),
            _ => Err(invalid_binary("")),
        };
    }
    let s = verify_utf8(buf)?;
    match ty.oid {
        oids::NAME => {
            if s.len() >= NAMEDATALEN {
                return Err(PgError::new(SqlState::NAME_TOO_LONG, "identifier too long").with_detail(format!(
                    "Identifier must be less than {NAMEDATALEN} characters."
                )));
            }
            Ok(Datum::Text(s.to_string()))
        }
        oids::VARCHAR => Ok(Datum::Text(apply_char_typmod(s.to_string(), ty.typmod, false, false)?)),
        oids::BPCHAR => Ok(Datum::Text(apply_char_typmod(s.to_string(), ty.typmod, true, false)?)),
        _ => Ok(Datum::Text(s.to_string())),
    }
}

/// Binary output for this family.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match (ty.oid, d) {
        (oids::CHAR, Datum::Char(b)) => Ok(vec![*b]),
        (_, Datum::Text(s)) if ty.oid != oids::CHAR => Ok(s.as_bytes().to_vec()),
        _ => Err(unexpected_datum("character binary output", ty)),
    }
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

fn unexpected_datum(what: &str, ty: TypeId) -> PgError {
    PgError::internal(format!("{what} for type oid {} received an unexpected datum", ty.oid))
}

/// 22021 `invalid byte sequence for encoding "UTF8": 0x..` for the given offending bytes.
fn encoding_error(bytes: &[u8]) -> PgError {
    let shown: Vec<String> = bytes.iter().map(|b| format!("0x{b:02x}")).collect();
    PgError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": {}", shown.join(" ")),
    )
}

/// Reject U+0000, which PostgreSQL text can never contain.
fn check_no_nul(s: &str) -> Result<()> {
    if s.as_bytes().contains(&0) { Err(encoding_error(&[0])) } else { Ok(()) }
}

/// Byte length of the UTF-8 sequence introduced by `lead` (PG `pg_utf_mblen`).
fn utf8_mblen(lead: u8) -> usize {
    if lead & 0x80 == 0 {
        1
    } else if lead & 0xe0 == 0xc0 {
        2
    } else if lead & 0xf0 == 0xe0 {
        3
    } else if lead & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

/// Validate a binary text payload: UTF-8 and no NUL bytes. The first offending byte wins.
fn verify_utf8(buf: &[u8]) -> Result<&str> {
    let nul = buf.iter().position(|&b| b == 0);
    match std::str::from_utf8(buf) {
        Ok(s) => match nul {
            Some(_) => Err(encoding_error(&[0])),
            None => Ok(s),
        },
        Err(e) => {
            let at = e.valid_up_to();
            // NUL is valid UTF-8, so a NUL before the first invalid byte is reported first.
            if let Some(n) = nul {
                if n < at {
                    return Err(encoding_error(&[0]));
                }
            }
            let len = utf8_mblen(buf[at]).min(buf.len() - at);
            Err(encoding_error(&buf[at..at + len]))
        }
    }
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn clip_to_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// PG `charin`: the 4-char octal form `\ooo` gives its byte (truncated to 8 bits as a C cast does),
/// otherwise the first byte, or 0 for the empty string.
fn char_from_str(s: &str) -> u8 {
    let b = s.as_bytes();
    if b.len() == 4 && b[0] == b'\\' && b[1..].iter().all(|c| (b'0'..=b'7').contains(c)) {
        let v = (((b[1] - b'0') as u32) << 6) | (((b[2] - b'0') as u32) << 3) | ((b[3] - b'0') as u32);
        return v as u8;
    }
    b.first().copied().unwrap_or(0)
}

/// PG `charout`: 0 → empty, high-bit bytes → `\ooo`, otherwise the ASCII character.
fn char_to_string(b: u8) -> String {
    match b {
        0 => String::new(),
        0x80..=0xff => format!("\\{b:03o}"),
        _ => (b as char).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;
    use crate::registry;

    fn ty(oid: u32, typmod: i32) -> TypeId {
        TypeId::new(oid, typmod)
    }

    #[test]
    fn varchar_length_rules() {
        let cfg = IoConfig::default();
        let c = IoCtx::new(registry::builtin_env(), &cfg);
        let e = input(&c, ty(oids::VARCHAR, 7), "abcd").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::STRING_DATA_RIGHT_TRUNCATION);
        assert_eq!(e.message, "value too long for type character varying(3)");
        assert_eq!(input(&c, ty(oids::VARCHAR, 7), "abc   ").unwrap(), Datum::Text("abc".into()));
        assert_eq!(input(&c, ty(oids::VARCHAR, -1), "abcd").unwrap(), Datum::Text("abcd".into()));
        assert_eq!(apply_char_typmod("abcd".into(), 7, false, true).unwrap(), "abc");
    }

    #[test]
    fn bpchar_padding_rules() {
        let cfg = IoConfig::default();
        let c = IoCtx::new(registry::builtin_env(), &cfg);
        assert_eq!(input(&c, ty(oids::BPCHAR, 8), "ab").unwrap(), Datum::Text("ab  ".into()));
        assert_eq!(input(&c, ty(oids::BPCHAR, -1), "ab  ").unwrap(), Datum::Text("ab  ".into()));
        let e = input(&c, ty(oids::BPCHAR, 5), "abcdef").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::STRING_DATA_RIGHT_TRUNCATION);
        assert_eq!(e.message, "value too long for type character(1)");
        assert_eq!(apply_char_typmod("abcd".into(), 6, true, true).unwrap(), "ab");
    }

    #[test]
    fn name_truncates_on_char_boundary() {
        let long: String = std::iter::repeat('é').take(40).collect(); // 80 bytes
        let t = truncate_name(&long).unwrap();
        assert_eq!(t.len(), 62);
        assert!(t.chars().all(|c| c == 'é'));
        assert_eq!(truncate_name("short").unwrap(), "short");
    }

    #[test]
    fn char_type_io() {
        let cfg = IoConfig::default();
        let c = IoCtx::new(registry::builtin_env(), &cfg);
        assert_eq!(input(&c, TypeId::CHAR, "\\101").unwrap(), Datum::Char(65));
        assert_eq!(input(&c, TypeId::CHAR, "A").unwrap(), Datum::Char(65));
        assert_eq!(input(&c, TypeId::CHAR, "").unwrap(), Datum::Char(0));
        assert_eq!(output(&c, TypeId::CHAR, &Datum::Char(200)).unwrap(), "\\310");
        assert_eq!(output(&c, TypeId::CHAR, &Datum::Char(0)).unwrap(), "");
        assert_eq!(output(&c, TypeId::CHAR, &Datum::Char(b'z')).unwrap(), "z");
        assert_eq!(recv(&c, TypeId::CHAR, &[7]).unwrap(), Datum::Char(7));
        assert_eq!(recv(&c, TypeId::CHAR, &[]).unwrap_err().sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        assert_eq!(send(&c, TypeId::CHAR, &Datum::Char(200)).unwrap(), vec![200]);
    }

    #[test]
    fn nul_and_invalid_utf8_rejected() {
        let cfg = IoConfig::default();
        let c = IoCtx::new(registry::builtin_env(), &cfg);
        let e = input(&c, TypeId::TEXT, "a\0b").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::CHARACTER_NOT_IN_REPERTOIRE);
        assert_eq!(e.message, "invalid byte sequence for encoding \"UTF8\": 0x00");
        let e = recv(&c, TypeId::TEXT, &[0xff]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::CHARACTER_NOT_IN_REPERTOIRE);
        assert_eq!(e.message, "invalid byte sequence for encoding \"UTF8\": 0xff");
        let e = recv(&c, TypeId::TEXT, &[b'a', 0xe2, 0x28, 0xa1]).unwrap_err();
        assert_eq!(e.message, "invalid byte sequence for encoding \"UTF8\": 0xe2 0x28 0xa1");
    }

    #[test]
    fn binary_text_roundtrip() {
        let cfg = IoConfig::default();
        let c = IoCtx::new(registry::builtin_env(), &cfg);
        let d = recv(&c, TypeId::VARCHAR, "héllo".as_bytes()).unwrap();
        assert_eq!(d, Datum::Text("héllo".into()));
        assert_eq!(send(&c, TypeId::VARCHAR, &d).unwrap(), "héllo".as_bytes().to_vec());
        let long = "x".repeat(64);
        let e = recv(&c, TypeId::NAME, long.as_bytes()).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::NAME_TOO_LONG);
    }
}
