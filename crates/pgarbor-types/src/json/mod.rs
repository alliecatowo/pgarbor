//! PostgreSQL `json` and `jsonb` (TYPE-08).
//!
//! - `json` values are stored as `Datum::Json(String)`: the exact input text after validation
//!   (whitespace, key order and duplicate keys preserved; output = input verbatim).
//! - `jsonb` values are stored as `Datum::Jsonb(Jsonb)`: a normalized tree.
//!
//! jsonb normalization (PG18 `utils/adt/jsonb_util.c`), enforced by `Jsonb::object_from_pairs`
//! and by every worker that builds objects:
//! 1. Object keys are unique; for duplicates the LAST value wins (`'{"a":1,"a":2}'::jsonb` → `{"a": 2}`).
//! 2. Object keys are sorted by (byte length, then bytewise memcmp): `{"b":1,"aa":2,"a":3}` →
//!    `{"a": 3, "b": 1, "aa": 2}`.
//! 3. Numbers are `Numeric` and keep their numeric display scale: `1.50` stays `1.50`, `1e3` becomes `1000`,
//!    `-0` becomes `0`, `1.000e-2` becomes `0.01000`.
//! 4. Strings are Rust `String`s (valid UTF-8, never containing NUL: `\u0000` is rejected with 22P05
//!    "unsupported Unicode escape sequence", DETAIL "\u0000 cannot be converted to text.").
//!
//! Text output (`jsonb_out`): `{"a": 1, "b": [1, 2]}` — `", "` between items and `": "` after keys,
//! strings escaped per `escape_json` (`"` `\\` `\b` `\f` `\n` `\r` `\t`, other control chars < 0x20 as
//! `\u00XX` lowercase hex), non-ASCII emitted raw.
//!
//! Files: `parse.rs` (lexer/parser with PG error messages), `io.rs` (in/out/send/recv),
//! `ops.rs` (operators, comparison), `build.rs` (constructors/conversions/mutation functions),
//! `srf.rs` (set-returning functions).
use crate::numeric::Numeric;
use std::cmp::Ordering;

pub mod build;
pub mod io;
pub mod ops;
pub mod parse;
pub mod srf;

/// A normalized jsonb value.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum Jsonb {
    Null,
    Bool(bool),
    /// Never NaN or ±Infinity (JSON has no such numbers): `to_jsonb('NaN'::numeric)` and
    /// `to_jsonb('inf'::float8)` produce the STRINGS `"NaN"` / `"Infinity"` (oracle-verified).
    Number(Numeric),
    String(String),
    Array(Vec<Jsonb>),
    /// INVARIANT: keys unique and sorted by `jsonb_key_cmp`.
    Object(Vec<(String, Jsonb)>),
}

/// jsonb key ordering: shorter keys first, then bytewise (PG `lengthCompareJsonbString`).
pub fn jsonb_key_cmp(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

impl Jsonb {
    /// Build a normalized object from pairs in input order (later duplicates win, keys sorted).
    pub fn object_from_pairs(pairs: Vec<(String, Jsonb)>) -> Jsonb {
        let mut out: Vec<(String, Jsonb)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            match out.binary_search_by(|(ek, _)| jsonb_key_cmp(ek, &k)) {
                Ok(i) => out[i].1 = v,
                Err(i) => out.insert(i, (k, v)),
            }
        }
        Jsonb::Object(out)
    }
    /// Look up an object key (None for non-objects / missing keys).
    pub fn get_key(&self, key: &str) -> Option<&Jsonb> {
        match self {
            Jsonb::Object(pairs) => {
                pairs.binary_search_by(|(k, _)| jsonb_key_cmp(k, key)).ok().map(|i| &pairs[i].1)
            }
            _ => None,
        }
    }
    /// `jsonb_typeof` name: "object", "array", "string", "number", "boolean", "null".
    pub fn type_name(&self) -> &'static str {
        match self {
            Jsonb::Null => "null",
            Jsonb::Bool(_) => "boolean",
            Jsonb::Number(_) => "number",
            Jsonb::String(_) => "string",
            Jsonb::Array(_) => "array",
            Jsonb::Object(_) => "object",
        }
    }
    /// True for scalars (PG wraps a top-level scalar in a "raw scalar" pseudo array; we do not).
    pub fn is_scalar(&self) -> bool {
        !matches!(self, Jsonb::Array(_) | Jsonb::Object(_))
    }
}
