//! json / jsonb text and binary I/O (TYPE-08, TYPE-15). Lease TYP-33.
//!
//! PG18 reference: `utils/adt/json.c` (`json_in/out/recv/send`), `jsonb.c` (`jsonb_in`, `jsonb_out`
//! via `JsonbToCString`, `jsonb_recv`/`jsonb_send` with version byte), `jsonb_util.c` (normalization).
//!
//! - json input: `parse::validate_json`, store the original text verbatim (`Datum::Json`).
//!   Output: verbatim. Binary send/recv: raw UTF-8 text (no version byte).
//! - jsonb input: `parse::parse_jsonb` → `Datum::Jsonb`. Output (`format_jsonb`): canonical form
//!   `{"a": 1, "b": [1, 2]}` (", " and ": " separators; strings via `parse::escape_json`; numbers via
//!   `numeric::io::format_numeric` (so `1.50` stays `1.50`, `1e3` prints `1000`)).
//!   Oracle: `'{"b":1,"aa":[1,2.50,{"c":null}],"a":true, "b":2}'::jsonb` →
//!   `{"a": true, "b": 2, "aa": [1, 2.50, {"c": null}]}`; `'1.000e-2'::jsonb` → `0.01000`; `'-0'::jsonb` → `0`.
//! - jsonb binary: send = version byte 0x01 followed by the canonical text; recv requires version 1
//!   (else XX000 `unsupported jsonb version number N`, as PG's elog) then parses the text.
//! - `jsonb_pretty` format (used by json::build): 4-space indentation, `{` newline, `"k": v`, items
//!   separated by `,\n`, closing bracket on its own line. Follows PG18's `JsonbToCStringWorker` exactly,
//!   which means an EMPTY container prints as `{` + newline + indent + `}` (so `'{}'` → `"{\n}"`,
//!   `'[]'` → `"[\n]"`) rather than the `{}` / `[]` the older doc comment suggested.
use super::{parse, Jsonb};
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use crate::numeric::io::format_numeric;
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};
use std::str::Utf8Error;

/// jsonb binary format version byte (`JSONB_VERSION` in PG `jsonb.h`).
const JSONB_VERSION: u8 = 1;

/// Canonical jsonb text (`jsonb_out`).
// CONTRACT
#[allow(unused_variables)]
pub fn format_jsonb(j: &Jsonb) -> Result<String> {
    let mut out = String::new();
    write_value(&mut out, j, 0, false, false)?;
    Ok(out)
}

/// Indented jsonb text (`jsonb_pretty`).
// CONTRACT
#[allow(unused_variables)]
pub fn format_jsonb_pretty(j: &Jsonb) -> Result<String> {
    let mut out = String::new();
    write_value(&mut out, j, 0, true, false)?;
    Ok(out)
}

/// Newline followed by `level * 4` spaces (PG `add_indent`).
fn push_indent(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("    ");
    }
}

/// Recursive writer mirroring PG's `JsonbToCStringWorker` state machine.
///
/// `level` is the number of enclosing containers (the depth of `v`). `after_key` is true when `v` is the
/// value of an object key (PG's `last_was_key`), in which case a container opens on the key's line.
/// Non-pretty output uses `", "` between items; pretty output uses `","` followed by a newline and
/// indentation before every item. Top-level containers never get a leading newline.
fn write_value(out: &mut String, v: &Jsonb, level: usize, pretty: bool, after_key: bool) -> Result<()> {
    match v {
        Jsonb::Null => out.push_str("null"),
        Jsonb::Bool(true) => out.push_str("true"),
        Jsonb::Bool(false) => out.push_str("false"),
        Jsonb::Number(n) => out.push_str(&format_numeric(n)?),
        Jsonb::String(s) => out.push_str(&parse::escape_json(s)),
        Jsonb::Array(items) => {
            if pretty && !after_key && level > 0 {
                push_indent(out, level);
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(if pretty { "," } else { ", " });
                }
                // A container item opens its own line via the leading indent in `write_value`
                // (`!after_key && level > 0`); pushing one here too would emit a blank line.
                if pretty && !matches!(item, Jsonb::Array(_) | Jsonb::Object(_)) {
                    push_indent(out, level + 1);
                }
                write_value(out, item, level + 1, pretty, false)?;
            }
            if pretty {
                push_indent(out, level);
            }
            out.push(']');
        }
        Jsonb::Object(pairs) => {
            if pretty && !after_key && level > 0 {
                push_indent(out, level);
            }
            out.push('{');
            for (i, (key, value)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(if pretty { "," } else { ", " });
                }
                if pretty {
                    push_indent(out, level + 1);
                }
                out.push_str(&parse::escape_json(key));
                out.push_str(": ");
                write_value(out, value, level + 1, pretty, true)?;
            }
            if pretty {
                push_indent(out, level);
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Error for a datum that does not belong to the type's I/O routine (internal bug, never user data).
fn mismatch(what: &str, ty: TypeId, d: &Datum) -> PgError {
    PgError::internal(format!("{what} for type oid {} called with datum {:?}", ty.oid, d))
}

/// `json_in` / `jsonb_in` by `ty.oid`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    match ty.oid {
        oids::JSON => {
            parse::validate_json(s)?;
            Ok(Datum::Json(s.to_string()))
        }
        oids::JSONB => Ok(Datum::Jsonb(parse::parse_jsonb(s)?)),
        other => Err(PgError::internal(format!("json input called for type oid {other}"))),
    }
}

/// `json_out` / `jsonb_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    match (ty.oid, d) {
        (oids::JSON, Datum::Json(s)) => Ok(s.clone()),
        (oids::JSONB, Datum::Jsonb(j)) => format_jsonb(j),
        _ => Err(mismatch("json output", ty, d)),
    }
}

/// Byte span of the first invalid UTF-8 sequence (PG reports these bytes).
fn utf8_error_span(buf: &[u8], e: &Utf8Error) -> (usize, usize) {
    let start = e.valid_up_to();
    let end = match e.error_len() {
        Some(n) => start + n,
        None => buf.len(),
    };
    (start, end)
}

/// Client-to-server text conversion for binary input (`pq_getmsgtext` under UTF8): the buffer must be
/// valid UTF-8 with no NUL bytes. Violations are 22021 `invalid byte sequence for encoding "UTF8": 0xNN ...`.
fn utf8_text(buf: &[u8]) -> Result<&str> {
    let utf = std::str::from_utf8(buf);
    let nul = buf.iter().position(|&b| b == 0);
    let (start, end) = match (utf, nul) {
        (Ok(s), None) => return Ok(s),
        (Ok(_), Some(p)) => (p, p + 1),
        (Err(e), None) => utf8_error_span(buf, &e),
        (Err(e), Some(p)) => {
            if p < e.valid_up_to() {
                (p, p + 1)
            } else {
                utf8_error_span(buf, &e)
            }
        }
    };
    let bytes: Vec<String> = buf[start..end].iter().map(|b| format!("0x{:02x}", b)).collect();
    Err(PgError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": {}", bytes.join(" ")),
    ))
}

/// `json_recv` / `jsonb_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    match ty.oid {
        oids::JSON => {
            let s = utf8_text(buf)?;
            parse::validate_json(s)?;
            Ok(Datum::Json(s.to_string()))
        }
        oids::JSONB => {
            let (&version, rest) = buf.split_first().ok_or_else(|| {
                PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message")
            })?;
            if version != JSONB_VERSION {
                return Err(PgError::internal(format!("unsupported jsonb version number {version}")));
            }
            let s = utf8_text(rest)?;
            Ok(Datum::Jsonb(parse::parse_jsonb(s)?))
        }
        other => Err(PgError::internal(format!("json binary input called for type oid {other}"))),
    }
}

/// `json_send` / `jsonb_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match (ty.oid, d) {
        (oids::JSON, Datum::Json(s)) => Ok(s.as_bytes().to_vec()),
        (oids::JSONB, Datum::Jsonb(j)) => {
            let text = format_jsonb(j)?;
            let mut out = Vec::with_capacity(text.len() + 1);
            out.push(JSONB_VERSION);
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        _ => Err(mismatch("json binary output", ty, d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::IoConfig;
    use crate::numeric::bigdecimal::BigDecimal;
    use crate::numeric::Numeric;
    use crate::registry;
    use std::str::FromStr;

    fn num(s: &str) -> Jsonb {
        Jsonb::Number(Numeric::from_decimal(BigDecimal::from_str(s).unwrap()))
    }

    fn obj(pairs: Vec<(&str, Jsonb)>) -> Jsonb {
        Jsonb::object_from_pairs(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    #[test]
    fn canonical_object_text() {
        let j = obj(vec![
            ("b", num("1")),
            ("aa", Jsonb::Array(vec![num("1"), num("2.50"), obj(vec![("c", Jsonb::Null)])])),
            ("a", Jsonb::Bool(true)),
            ("b", num("2")),
        ]);
        assert_eq!(format_jsonb(&j).unwrap(), r#"{"a": true, "b": 2, "aa": [1, 2.50, {"c": null}]}"#);
    }

    #[test]
    fn scalar_and_empty_text() {
        assert_eq!(format_jsonb(&num("1000")).unwrap(), "1000");
        assert_eq!(format_jsonb(&Jsonb::Object(vec![])).unwrap(), "{}");
        assert_eq!(format_jsonb(&Jsonb::Array(vec![])).unwrap(), "[]");
        assert_eq!(format_jsonb(&Jsonb::String("a\"b\n".into())).unwrap(), r#""a\"b\n""#);
    }

    #[test]
    fn pretty_layout_matches_pg() {
        let j = obj(vec![("a", Jsonb::Array(vec![num("1"), obj(vec![("b", num("2"))])]))]);
        assert_eq!(
            format_jsonb_pretty(&j).unwrap(),
            "{\n    \"a\": [\n        1,\n        {\n            \"b\": 2\n        }\n    ]\n}"
        );
        assert_eq!(format_jsonb_pretty(&Jsonb::Array(vec![num("1"), num("2")])).unwrap(), "[\n    1,\n    2\n]");
        assert_eq!(format_jsonb_pretty(&obj(vec![("a", Jsonb::Object(vec![]))])).unwrap(), "{\n    \"a\": {\n    }\n}");
        assert_eq!(format_jsonb_pretty(&num("5")).unwrap(), "5");
    }

    #[test]
    fn json_is_verbatim() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let d = input(&ctx, TypeId::of(oids::JSON), "[1,  2]").unwrap();
        assert_eq!(output(&ctx, TypeId::of(oids::JSON), &d).unwrap(), "[1,  2]");
        assert_eq!(send(&ctx, TypeId::of(oids::JSON), &d).unwrap(), b"[1,  2]".to_vec());
    }

    #[test]
    fn jsonb_send_recv_roundtrip() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let ty = TypeId::of(oids::JSONB);
        let d = input(&ctx, ty, r#"{"b":1,"a":[1.50]}"#).unwrap();
        let bytes = send(&ctx, ty, &d).unwrap();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(&bytes[1..], br#"{"a": [1.50], "b": 1}"#);
        assert_eq!(recv(&ctx, ty, &bytes).unwrap(), d);
    }

    #[test]
    fn jsonb_recv_rejects_bad_version_and_empty() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let ty = TypeId::of(oids::JSONB);
        let err = recv(&ctx, ty, &[2, b'1']).unwrap_err();
        assert_eq!(err.sqlstate.code(), "XX000");
        assert_eq!(err.message, "unsupported jsonb version number 2");
        let err = recv(&ctx, ty, &[]).unwrap_err();
        assert_eq!(err.sqlstate.code(), "08P01");
    }

    #[test]
    fn recv_rejects_invalid_utf8_and_nul() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(registry::builtin_env(), &cfg);
        let err = recv(&ctx, TypeId::of(oids::JSON), &[b'"', 0xff, b'"']).unwrap_err();
        assert_eq!(err.sqlstate.code(), "22021");
        assert_eq!(err.message, "invalid byte sequence for encoding \"UTF8\": 0xff");
        let err = recv(&ctx, TypeId::of(oids::JSON), &[b'"', 0, b'"']).unwrap_err();
        assert_eq!(err.message, "invalid byte sequence for encoding \"UTF8\": 0x00");
    }
}
