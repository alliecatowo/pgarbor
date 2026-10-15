//! JSON set-returning and record functions (TYPE-08). Lease TYP-36.
//!
//! PG18 reference: `utils/adt/jsonfuncs.c` (`each_worker`, `elements_worker`, `json_object_keys`,
//! `populate_record_worker`, `populate_recordset_worker`, `to_record`).
//! Owned procs (ImplModule::JsonSrf), all `FuncImpl::Set` except the populate_record/to_record scalars:
//! - `json_each jsonb_each` → rows `Datum::Record([key Text, value Json|Jsonb])`; `json_each_text
//!   jsonb_each_text` → `[key, value Text]` (strings unescaped; JSON null → SQL NULL).
//! - `json_array_elements(_text) jsonb_array_elements(_text)` → one value per element.
//! - `json_object_keys jsonb_object_keys` → key Text per member (json: input order incl. duplicates;
//!   jsonb: storage order).
//! - Wrong container kind, all 22023 (oracle-verified): `cannot call jsonb_each on a non-object`;
//!   json_each on array `cannot deconstruct an array as an object`, on scalar `cannot deconstruct a scalar`;
//!   jsonb_array_elements on object `cannot extract elements from an object`, on scalar
//!   `cannot extract elements from a scalar`; json_array_elements: `cannot call json_array_elements on a
//!   non-array` / `... on a scalar`; `cannot call jsonb_object_keys on an array`, `cannot call
//!   json_object_keys on a scalar`.
//! - `json(b)_populate_record(base anyelement, from_json)`, `json(b)_populate_recordset`,
//!   `json(b)_to_record`, `json(b)_to_recordset`: the target row type is `ctx.result_type` (composite:
//!   fields via `ctx.env.composite_fields`; for `to_record` the binder registers the column definition
//!   list as an anonymous record type and passes it the same way). Without one the BINDER raises 42601
//!   `a column definition list is required for functions returning "record"`. Field values are produced
//!   with the column type's input function applied to the JSON text (strings unescaped).
//! Oracle: `select * from jsonb_each('{"a":1,"b":[2]}')` → (a, 1), (b, [2]).
//!
//! Implementation notes (PG parity):
//! - json sources keep the verbatim token text of nested values (`json_each` value `[2, 3]` keeps the
//!   input spacing). Object keys are decoded, duplicates kept in input order. The private `Node` scanner
//!   below walks the already-validated text (`Datum::Json` is validated at input) and records raw spans.
//! - jsonb sources are rendered with `format_jsonb` first, so their nested values are canonical text and
//!   their members are in storage order. Jsonb values are rebuilt with `parse_jsonb`.
//! - Populate (`populate_record_worker`): a NULL json argument returns the base record (or NULL); a missing
//!   key keeps the base field value (or NULL without a base); an empty object with a base returns the base;
//!   with duplicate keys the last one wins; a JSON null gives SQL NULL; a JSON string into an array or
//!   composite column goes through that type's input function; JSON arrays and objects are converted
//!   recursively into array literals and row values.
//! - Set functions are evaluated eagerly (`Vec` then `into_iter`), so every error is raised by the call.
use crate::catalog_data::BuiltinProc;
use crate::context::{CompositeField, FuncCtx};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::{FuncImpl, SetIter};
use crate::io;
use crate::json::io::format_jsonb;
use crate::json::parse::{escape_json, parse_jsonb};
use crate::oids;
use crate::registry;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::borrow::Cow;
use std::sync::Arc;

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    Some(match proc.prosrc {
        "json_each" => FuncImpl::Set(json_each),
        "jsonb_each" => FuncImpl::Set(jsonb_each),
        "json_each_text" => FuncImpl::Set(json_each_text),
        "jsonb_each_text" => FuncImpl::Set(jsonb_each_text),
        "json_array_elements" => FuncImpl::Set(json_array_elements),
        "jsonb_array_elements" => FuncImpl::Set(jsonb_array_elements),
        "json_array_elements_text" => FuncImpl::Set(json_array_elements_text),
        "jsonb_array_elements_text" => FuncImpl::Set(jsonb_array_elements_text),
        "json_object_keys" => FuncImpl::Set(json_object_keys),
        "jsonb_object_keys" => FuncImpl::Set(jsonb_object_keys),
        "json_populate_record" => FuncImpl::Scalar(json_populate_record),
        "jsonb_populate_record" => FuncImpl::Scalar(jsonb_populate_record),
        "jsonb_populate_record_valid" => FuncImpl::Scalar(jsonb_populate_record_valid),
        "json_populate_recordset" => FuncImpl::Set(json_populate_recordset),
        "jsonb_populate_recordset" => FuncImpl::Set(jsonb_populate_recordset),
        "json_to_record" => FuncImpl::Scalar(json_to_record),
        "jsonb_to_record" => FuncImpl::Scalar(jsonb_to_record),
        "json_to_recordset" => FuncImpl::Set(json_to_recordset),
        "jsonb_to_recordset" => FuncImpl::Set(jsonb_to_recordset),
        _ => return None,
    })
}

/// 0A000 for a JsonSrf-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-36", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Source handling and the raw-span tree
// ---------------------------------------------------------------------------------------------

/// Which JSON type a function reads. Decides how nested values are rendered and re-parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Src {
    Json,
    Jsonb,
}

/// Maximum nesting depth (matches the JSON parser contract).
const MAX_DEPTH: u32 = 6400;

/// A parsed JSON value that remembers the raw text of containers and numbers.
#[derive(Debug)]
enum Node<'a> {
    Null,
    Bool(bool),
    /// Number exactly as spelled in the source text.
    Number(&'a str),
    /// Raw text between the quotes (escape sequences not yet decoded).
    Str(&'a str),
    Array { raw: &'a str, items: Vec<Node<'a>> },
    /// Keys decoded; duplicates kept in input order.
    Object { raw: &'a str, members: Vec<(String, Node<'a>)> },
}

impl<'a> Node<'a> {
    fn is_null(&self) -> bool {
        matches!(self, Node::Null)
    }

    /// Token text as PG passes it on: quoted for strings, verbatim for containers and numbers.
    fn raw(&self) -> Cow<'a, str> {
        match self {
            Node::Null => Cow::Borrowed("null"),
            Node::Bool(true) => Cow::Borrowed("true"),
            Node::Bool(false) => Cow::Borrowed("false"),
            Node::Number(s) => Cow::Borrowed(*s),
            Node::Str(s) => Cow::Owned(format!("\"{s}\"")),
            Node::Array { raw, .. } | Node::Object { raw, .. } => Cow::Borrowed(*raw),
        }
    }

    /// Text form for the `_text` variants: SQL NULL for JSON null, decoded text for strings.
    fn text_value(&self) -> Result<Option<String>> {
        match self {
            Node::Null => Ok(None),
            Node::Str(s) => decode_json_string(s).map(Some),
            other => Ok(Some(other.raw().into_owned())),
        }
    }
}

fn bad_arg(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg)
}

fn bad_json() -> PgError {
    PgError::internal("malformed JSON text in a validated value")
}

fn unicode_nul() -> PgError {
    PgError::new(SqlState::UNTRANSLATABLE_CHARACTER, "unsupported Unicode escape sequence")
        .with_detail("\\u0000 cannot be converted to text.")
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("missing function argument {i}")))
}

fn into_set(rows: Vec<Datum>) -> Result<SetIter> {
    Ok(Box::new(rows.into_iter().map(|d| Ok::<Datum, PgError>(d))))
}

/// Text of a json/jsonb argument that the scanner can walk.
fn source_text(d: &Datum, src: Src) -> Result<Cow<'_, str>> {
    match (d, src) {
        (Datum::Json(s), Src::Json) => Ok(Cow::Borrowed(s.as_str())),
        (Datum::Jsonb(j), Src::Jsonb) => Ok(Cow::Owned(format_jsonb(j)?)),
        _ => Err(type_mismatch(if src == Src::Json { "json" } else { "jsonb" }, d)),
    }
}

/// Decode a JSON string body (the text between the quotes): escapes and surrogate pairs.
/// `\u0000` is rejected with 22P05 because text cannot hold NUL.
fn decode_json_string(raw: &str) -> Result<String> {
    if !raw.contains('\\') {
        return Ok(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hi = hex4(&mut it)?;
                let cp = if (0xD800..0xDC00).contains(&hi) {
                    if it.next() != Some('\\') || it.next() != Some('u') {
                        return Err(bad_json());
                    }
                    let lo = hex4(&mut it)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err(bad_json());
                    }
                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else {
                    hi
                };
                if cp == 0 {
                    return Err(unicode_nul());
                }
                let ch = char::from_u32(cp).ok_or_else(bad_json)?;
                out.push(ch);
            }
            _ => return Err(bad_json()),
        }
    }
    Ok(out)
}

fn hex4(it: &mut std::str::Chars<'_>) -> Result<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        let d = it.next().and_then(|c| c.to_digit(16)).ok_or_else(bad_json)?;
        v = v * 16 + d;
    }
    Ok(v)
}

/// Span-recording scanner over validated JSON text.
struct Scan<'a> {
    src: &'a str,
    pos: usize,
    depth: u32,
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn slice(&self, start: usize) -> &'a str {
        let src = self.src;
        src.get(start..self.pos).unwrap_or_default()
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(PgError::new(SqlState(*b"54001"), "stack depth limit exceeded"));
        }
        Ok(())
    }

    fn value(&mut self) -> Result<Node<'a>> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string_raw().map(Node::Str),
            Some(b't') => self.literal("true", Node::Bool(true)),
            Some(b'f') => self.literal("false", Node::Bool(false)),
            Some(b'n') => self.literal("null", Node::Null),
            Some(_) => self.number(),
            None => Err(bad_json()),
        }
    }

    fn literal(&mut self, word: &str, node: Node<'a>) -> Result<Node<'a>> {
        let src = self.src;
        if src.get(self.pos..).is_some_and(|rest| rest.starts_with(word)) {
            self.pos += word.len();
            Ok(node)
        } else {
            Err(bad_json())
        }
    }

    fn number(&mut self) -> Result<Node<'a>> {
        let start = self.pos;
        let src = self.src;
        let bytes = src.as_bytes();
        while let Some(c) = bytes.get(self.pos) {
            if matches!(*c, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(bad_json());
        }
        Ok(Node::Number(self.slice(start)))
    }

    /// Scan a string token starting at the opening quote; returns the raw body.
    fn string_raw(&mut self) -> Result<&'a str> {
        let src = self.src;
        let bytes = src.as_bytes();
        let start = self.pos + 1;
        let mut i = start;
        loop {
            match bytes.get(i) {
                None => return Err(bad_json()),
                Some(b'\\') => i += 2,
                Some(b'"') => break,
                Some(_) => i += 1,
            }
        }
        self.pos = i + 1;
        src.get(start..i).ok_or_else(bad_json)
    }

    fn array(&mut self) -> Result<Node<'a>> {
        let start = self.pos;
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                items.push(self.value()?);
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(bad_json()),
                }
            }
        }
        self.depth -= 1;
        Ok(Node::Array { raw: self.slice(start), items })
    }

    fn object(&mut self) -> Result<Node<'a>> {
        let start = self.pos;
        self.enter()?;
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                self.skip_ws();
                if self.peek() != Some(b'"') {
                    return Err(bad_json());
                }
                let key = decode_json_string(self.string_raw()?)?;
                self.skip_ws();
                if self.peek() != Some(b':') {
                    return Err(bad_json());
                }
                self.pos += 1;
                let value = self.value()?;
                members.push((key, value));
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(bad_json()),
                }
            }
        }
        self.depth -= 1;
        Ok(Node::Object { raw: self.slice(start), members })
    }
}

/// Parse a whole JSON text (leading and trailing whitespace allowed).
fn parse_root(text: &str) -> Result<Node<'_>> {
    let mut sc = Scan { src: text, pos: 0, depth: 0 };
    let node = sc.value()?;
    sc.skip_ws();
    if sc.pos != text.len() {
        return Err(bad_json());
    }
    Ok(node)
}

/// Datum for one element or value: json keeps the raw token text, jsonb re-parses it.
fn value_datum(node: &Node<'_>, src: Src, as_text: bool) -> Result<Datum> {
    if as_text {
        return Ok(match node.text_value()? {
            Some(s) => Datum::Text(s),
            None => Datum::Null,
        });
    }
    match src {
        Src::Json => Ok(Datum::Json(node.raw().into_owned())),
        Src::Jsonb => Ok(Datum::Jsonb(parse_jsonb(&node.raw())?)),
    }
}

// ---------------------------------------------------------------------------------------------
// json_each / json_each_text / jsonb_each / jsonb_each_text
// ---------------------------------------------------------------------------------------------

fn each_set(d: &Datum, src: Src, as_text: bool, name: &str) -> Result<SetIter> {
    let text = source_text(d, src)?;
    let members = match parse_root(&text)? {
        Node::Object { members, .. } => members,
        other => {
            let msg = match (src, other) {
                (Src::Json, Node::Array { .. }) => "cannot deconstruct an array as an object".to_string(),
                (Src::Json, _) => "cannot deconstruct a scalar".to_string(),
                (Src::Jsonb, _) => format!("cannot call {name} on a non-object"),
            };
            return Err(bad_arg(msg));
        }
    };
    let mut rows = Vec::with_capacity(members.len());
    for (key, value) in members {
        let v = value_datum(&value, src, as_text)?;
        rows.push(Datum::Record(vec![Datum::Text(key), v]));
    }
    into_set(rows)
}

fn json_each(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    each_set(arg(args, 0)?, Src::Json, false, "json_each")
}

fn jsonb_each(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    each_set(arg(args, 0)?, Src::Jsonb, false, "jsonb_each")
}

fn json_each_text(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    each_set(arg(args, 0)?, Src::Json, true, "json_each_text")
}

fn jsonb_each_text(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    each_set(arg(args, 0)?, Src::Jsonb, true, "jsonb_each_text")
}

// ---------------------------------------------------------------------------------------------
// json(b)_array_elements(_text)
// ---------------------------------------------------------------------------------------------

fn elements_set(d: &Datum, src: Src, as_text: bool, name: &str) -> Result<SetIter> {
    let text = source_text(d, src)?;
    let items = match parse_root(&text)? {
        Node::Array { items, .. } => items,
        Node::Object { .. } => {
            return Err(bad_arg(match src {
                Src::Json => format!("cannot call {name} on a non-array"),
                Src::Jsonb => "cannot extract elements from an object".to_string(),
            }))
        }
        _ => {
            return Err(bad_arg(match src {
                Src::Json => format!("cannot call {name} on a scalar"),
                Src::Jsonb => "cannot extract elements from a scalar".to_string(),
            }))
        }
    };
    let mut rows = Vec::with_capacity(items.len());
    for item in &items {
        rows.push(value_datum(item, src, as_text)?);
    }
    into_set(rows)
}

fn json_array_elements(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    elements_set(arg(args, 0)?, Src::Json, false, "json_array_elements")
}

fn jsonb_array_elements(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    elements_set(arg(args, 0)?, Src::Jsonb, false, "jsonb_array_elements")
}

fn json_array_elements_text(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    elements_set(arg(args, 0)?, Src::Json, true, "json_array_elements_text")
}

fn jsonb_array_elements_text(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    elements_set(arg(args, 0)?, Src::Jsonb, true, "jsonb_array_elements_text")
}

// ---------------------------------------------------------------------------------------------
// json(b)_object_keys
// ---------------------------------------------------------------------------------------------

fn object_keys_set(d: &Datum, src: Src, name: &str) -> Result<SetIter> {
    let text = source_text(d, src)?;
    let members = match parse_root(&text)? {
        Node::Object { members, .. } => members,
        Node::Array { .. } => return Err(bad_arg(format!("cannot call {name} on an array"))),
        _ => return Err(bad_arg(format!("cannot call {name} on a scalar"))),
    };
    into_set(members.into_iter().map(|(k, _)| Datum::Text(k)).collect())
}

fn json_object_keys(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    object_keys_set(arg(args, 0)?, Src::Json, "json_object_keys")
}

fn jsonb_object_keys(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    object_keys_set(arg(args, 0)?, Src::Jsonb, "jsonb_object_keys")
}

// ---------------------------------------------------------------------------------------------
// populate_record / populate_recordset / to_record / to_recordset
// ---------------------------------------------------------------------------------------------

/// Composite fields of the row type. `to_*` functions without a column definition list get the
/// PG parser message; the other functions get PG's `could not determine row type` error.
fn row_fields(ctx: &FuncCtx, row_ty: TypeId, name: &str) -> Result<Arc<[CompositeField]>> {
    let oid = registry::base_type(ctx.env, row_ty.oid);
    if let Some(fields) = ctx.env.composite_fields(oid) {
        return Ok(fields);
    }
    if name.ends_with("_to_record") || name.ends_with("_to_recordset") {
        return Err(PgError::new(
            SqlState::SYNTAX_ERROR,
            "a column definition list is required for functions returning \"record\"",
        ));
    }
    Err(PgError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("could not determine row type for result of {name}"),
    )
    .with_hint("Provide a non-null record argument, or call the function in the FROM clause using a column definition list."))
}

fn record_of(base: Option<&Datum>) -> Option<&[Datum]> {
    match base {
        Some(Datum::Record(v)) => Some(v.as_slice()),
        _ => None,
    }
}

/// `populate_record_worker` for one object: a row of field values in attribute order.
fn populate_row(
    ctx: &FuncCtx,
    fields: &[CompositeField],
    base: Option<&[Datum]>,
    obj: &[(String, Node<'_>)],
    colname: Option<&str>,
) -> Result<Vec<Datum>> {
    if let Some(b) = base {
        if obj.is_empty() {
            return Ok(b.to_vec());
        }
    }
    let mut out = Vec::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        if f.dropped {
            out.push(Datum::Null);
            continue;
        }
        let bval = base.and_then(|b| b.get(i));
        let found = obj.iter().rev().find(|(k, _)| k.as_str() == f.name.as_str()).map(|(_, v)| v);
        match found {
            Some(node) => out.push(populate_field(ctx, f.type_id, node, bval, Some(f.name.as_str()))?),
            None => out.push(bval.cloned().unwrap_or(Datum::Null)),
        }
    }
    Ok(out)
}

fn is_json_type(oid: Oid) -> bool {
    oid == oids::JSON || oid == oids::JSONB
}

fn expected_json_array(colname: Option<&str>) -> PgError {
    let e = PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "expected JSON array");
    match colname {
        Some(c) => e.with_hint(format!("See the value of key \"{c}\".")),
        None => e,
    }
}

/// `populate_record_field`: one JSON value into a column of type `ty`.
fn populate_field(
    ctx: &FuncCtx,
    ty: TypeId,
    node: &Node<'_>,
    base: Option<&Datum>,
    colname: Option<&str>,
) -> Result<Datum> {
    if node.is_null() {
        return Ok(Datum::Null);
    }
    let env = ctx.env;
    let base_oid = registry::base_type(env, ty.oid);
    // A JSON string goes through the column type's input function instead of the structural path.
    if !matches!(node, Node::Str(_)) {
        if let Some(fields) = env.composite_fields(base_oid) {
            return match node {
                Node::Object { members, .. } => {
                    Ok(Datum::Record(populate_row(ctx, &fields, record_of(base), members, colname)?))
                }
                Node::Array { .. } => Err(bad_arg("cannot call populate_composite on an array")),
                _ => Err(bad_arg("cannot call populate_composite on a scalar")),
            };
        }
        if registry::is_array_type(env, base_oid) {
            return match node {
                Node::Array { items, .. } => {
                    let elem = registry::element_type_of(env, base_oid)
                        .ok_or_else(|| PgError::internal("array type without an element type"))?;
                    let literal = array_literal(ctx, elem, items, colname)?;
                    io::input(&ctx.io_ctx(), ty, &literal)
                }
                _ => Err(expected_json_array(colname)),
            };
        }
    }
    let text: String = match node {
        Node::Str(raw) => {
            let s = decode_json_string(raw)?;
            if is_json_type(base_oid) {
                escape_json(&s)
            } else {
                s
            }
        }
        other => other.raw().into_owned(),
    };
    io::input(&ctx.io_ctx(), ty, &text)
}

/// PG array literal for a JSON array whose elements have element type `elem`.
fn array_literal(ctx: &FuncCtx, elem: Oid, items: &[Node<'_>], colname: Option<&str>) -> Result<String> {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            Node::Null => out.push_str("NULL"),
            Node::Array { items: sub, .. } => out.push_str(&array_literal(ctx, elem, sub, colname)?),
            _ => {
                let ety = TypeId::of(elem);
                let d = populate_field(ctx, ety, item, None, colname)?;
                if d.is_null() {
                    out.push_str("NULL");
                } else {
                    let s = io::output(&ctx.io_ctx(), ety, &d)?;
                    push_quoted(&mut out, &s);
                }
            }
        }
    }
    out.push('}');
    Ok(out)
}

fn push_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// `populate_record_worker`: `base` is None for `*_to_record`; `row_ty` is the row type.
fn populate_one(
    base: Option<&Datum>,
    json: &Datum,
    ctx: &FuncCtx,
    row_ty: TypeId,
    src: Src,
    name: &str,
) -> Result<Datum> {
    let fields = row_fields(ctx, row_ty, name)?;
    if json.is_null() {
        return Ok(base.cloned().unwrap_or(Datum::Null));
    }
    let text = source_text(json, src)?;
    match parse_root(&text)? {
        Node::Object { members, .. } => {
            Ok(Datum::Record(populate_row(ctx, &fields, record_of(base), &members, None)?))
        }
        Node::Array { .. } => Err(bad_arg("cannot call populate_composite on an array")),
        _ => Err(bad_arg("cannot call populate_composite on a scalar")),
    }
}

/// `populate_recordset_worker`: one row per element of the top-level array.
fn populate_recordset_one(
    base: Option<&Datum>,
    json: &Datum,
    ctx: &FuncCtx,
    row_ty: TypeId,
    src: Src,
    name: &str,
) -> Result<SetIter> {
    let fields = row_fields(ctx, row_ty, name)?;
    if json.is_null() {
        return into_set(Vec::new());
    }
    let text = source_text(json, src)?;
    let items = match parse_root(&text)? {
        Node::Array { items, .. } => items,
        Node::Object { .. } => {
            return Err(bad_arg(match src {
                Src::Json => format!("cannot call {name} on an object"),
                Src::Jsonb => format!("cannot call {name} on a non-array"),
            }))
        }
        _ => {
            return Err(bad_arg(match src {
                Src::Json => format!("cannot call {name} on a scalar"),
                Src::Jsonb => format!("cannot call {name} on a non-array"),
            }))
        }
    };
    let mut rows = Vec::with_capacity(items.len());
    for item in &items {
        match item {
            Node::Object { members, .. } => {
                rows.push(Datum::Record(populate_row(ctx, &fields, record_of(base), members, None)?))
            }
            _ => return Err(bad_arg(format!("argument of {name} must be an array of objects"))),
        }
    }
    into_set(rows)
}

fn json_populate_record(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    populate_one(Some(arg(args, 0)?), arg(args, 1)?, ctx, ctx.result_type, Src::Json, "json_populate_record")
}

fn jsonb_populate_record(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    populate_one(Some(arg(args, 0)?), arg(args, 1)?, ctx, ctx.result_type, Src::Jsonb, "jsonb_populate_record")
}

/// Returns false when populating the record would raise a data error (soft-error context in PG).
fn jsonb_populate_record_valid(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let row_ty = ctx.arg_type(0);
    match populate_one(Some(arg(args, 0)?), arg(args, 1)?, ctx, row_ty, Src::Jsonb, "jsonb_populate_record") {
        Ok(_) => Ok(Datum::Bool(true)),
        Err(e) if e.sqlstate.0[0] == b'2' && e.sqlstate.0[1] == b'2' => Ok(Datum::Bool(false)),
        Err(e) => Err(e),
    }
}

fn json_populate_recordset(args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    populate_recordset_one(
        Some(arg(args, 0)?),
        arg(args, 1)?,
        ctx,
        ctx.result_type,
        Src::Json,
        "json_populate_recordset",
    )
}

fn jsonb_populate_recordset(args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    populate_recordset_one(
        Some(arg(args, 0)?),
        arg(args, 1)?,
        ctx,
        ctx.result_type,
        Src::Jsonb,
        "jsonb_populate_recordset",
    )
}

fn json_to_record(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    populate_one(None, arg(args, 0)?, ctx, ctx.result_type, Src::Json, "json_to_record")
}

fn jsonb_to_record(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    populate_one(None, arg(args, 0)?, ctx, ctx.result_type, Src::Jsonb, "jsonb_to_record")
}

fn json_to_recordset(args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    populate_recordset_one(None, arg(args, 0)?, ctx, ctx.result_type, Src::Json, "json_to_recordset")
}

fn jsonb_to_recordset(args: &[Datum], ctx: &FuncCtx) -> Result<SetIter> {
    populate_recordset_one(None, arg(args, 0)?, ctx, ctx.result_type, Src::Jsonb, "jsonb_to_recordset")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::registry::builtin_env;

    fn with_ctx<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::RECORD,
            collation: 0,
        };
        f(&ctx)
    }

    fn collect(r: Result<SetIter>) -> Result<Vec<Datum>> {
        r.and_then(|it| it.collect::<Result<Vec<Datum>>>())
    }

    fn show(d: &Datum) -> String {
        match d {
            Datum::Null => "NULL".to_string(),
            Datum::Text(s) | Datum::Json(s) => s.clone(),
            Datum::Jsonb(j) => format_jsonb(j).unwrap_or_else(|e| e.message),
            Datum::Record(v) => format!("({})", v.iter().map(show).collect::<Vec<_>>().join(",")),
            other => format!("{other:?}"),
        }
    }

    fn shown(r: Result<Vec<Datum>>) -> Vec<String> {
        r.expect("set function should succeed").iter().map(show).collect()
    }

    fn jsonv(s: &str) -> Datum {
        Datum::Json(s.to_string())
    }

    fn jb(s: &str) -> Datum {
        Datum::Jsonb(parse_jsonb(s).expect("valid jsonb literal"))
    }

    #[test]
    fn jsonb_each_acceptance() {
        let rows = with_ctx(|c| collect(jsonb_each(&[jb(r#"{"a":1,"b":[2]}"#)], c)));
        assert_eq!(shown(rows), vec!["(a,1)", "(b,[2])"]);
    }

    #[test]
    fn json_each_keeps_raw_text_and_duplicates() {
        let rows = with_ctx(|c| collect(json_each(&[jsonv(r#"{"b": 1, "a" :  [2, 3], "a": "x"}"#)], c)));
        assert_eq!(shown(rows), vec!["(b,1)", "(a,[2, 3])", "(a,\"x\")"]);
    }

    #[test]
    fn json_each_text_nulls_and_unescape() {
        let rows = with_ctx(|c| collect(json_each_text(&[jsonv(r#"{"n":null,"s":"xA"}"#)], c)));
        assert_eq!(shown(rows), vec!["(n,NULL)", "(s,xA)"]);
    }

    #[test]
    fn json_each_scalar_is_22023() {
        let err = with_ctx(|c| collect(json_each(&[jsonv("1")], c))).unwrap_err();
        assert_eq!(err.message, "cannot deconstruct a scalar");
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn jsonb_each_array_message() {
        let err = with_ctx(|c| collect(jsonb_each(&[jb("[1]")], c))).unwrap_err();
        assert_eq!(err.message, "cannot call jsonb_each on a non-object");
    }

    #[test]
    fn array_elements_text_acceptance() {
        let rows = with_ctx(|c| collect(json_array_elements_text(&[jsonv(r#"["a",null]"#)], c)));
        assert_eq!(shown(rows), vec!["a", "NULL"]);
    }

    #[test]
    fn jsonb_array_elements_text_canonical() {
        let rows = with_ctx(|c| collect(jsonb_array_elements_text(&[jb(r#"["x",[1,2]]"#)], c)));
        assert_eq!(shown(rows), vec!["x", "[1, 2]"]);
    }

    #[test]
    fn array_elements_wrong_kind_messages() {
        let e1 = with_ctx(|c| collect(json_array_elements(&[jsonv("1")], c))).unwrap_err();
        assert_eq!(e1.message, "cannot call json_array_elements on a scalar");
        let e2 = with_ctx(|c| collect(json_array_elements(&[jsonv("{}")], c))).unwrap_err();
        assert_eq!(e2.message, "cannot call json_array_elements on a non-array");
        let e3 = with_ctx(|c| collect(jsonb_array_elements(&[jb("{}")], c))).unwrap_err();
        assert_eq!(e3.message, "cannot extract elements from an object");
    }

    #[test]
    fn object_keys_keeps_duplicates_for_json() {
        let rows = with_ctx(|c| collect(json_object_keys(&[jsonv(r#"{"a":1,"a":2}"#)], c)));
        assert_eq!(shown(rows), vec!["a", "a"]);
    }

    #[test]
    fn object_keys_wrong_kind_messages() {
        let e = with_ctx(|c| collect(json_object_keys(&[jsonv("[1]")], c))).unwrap_err();
        assert_eq!(e.message, "cannot call json_object_keys on an array");
        let e = with_ctx(|c| collect(jsonb_object_keys(&[jb("[1]")], c))).unwrap_err();
        assert_eq!(e.message, "cannot call jsonb_object_keys on an array");
    }

    #[test]
    fn unicode_nul_is_rejected() {
        let err = with_ctx(|c| collect(json_each_text(&[jsonv(r#"{"s":"\u0000"}"#)], c))).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::UNTRANSLATABLE_CHARACTER);
        assert_eq!(err.message, "unsupported Unicode escape sequence");
    }

    #[test]
    fn surrogate_pairs_decode() {
        assert_eq!(decode_json_string(r"😀").unwrap(), "\u{1F600}");
    }
}
