//! Record / composite values: I/O and row comparison (TYPE-01, TYPE-15). Lease TYP-40.
//!
//! PG18 reference: `src/backend/utils/adt/rowtypes.c` (`record_in`, `record_out`, `record_recv`,
//! `record_send`, `record_cmp`, `record_eq`, `record_image_*`, `hash_record`).
//! Field types: for a named composite type `ty.oid` → `ctx.env.composite_fields(ty.oid)` (dropped
//! fields are stored as `Datum::Null` and skipped by I/O); for anonymous `record` (2249) the field types
//! cannot be known from the Datum: `output_record` takes them explicitly (the executor knows them),
//! and `input` of plain `record` → 0A000 `input of anonymous composite types is not implemented` (PG text).
//!
//! Text output: `(1,"a b",)` — fields separated by `,`, NULL = nothing, a field is double-quoted if it is
//! empty or contains `"` `\` `(` `)` `,` or whitespace; inside quotes `"` → `""` and `\` → `\\`.
//! Oracle: `row(1,'a b',null)` → `(1,"a b",)`.
//! Text input: `(f1,f2,...)`; unquoted field text is passed VERBATIM (whitespace included) to the field
//! type's input function (`'( 1 , x y )'` → a = 1 (int4in trims), b = `' x y '`); an empty unquoted
//! field is NULL, `""` is the empty string; inside quotes `""` is a quote and backslash escapes the next
//! char. Errors 22P02 `malformed record literal: "<input>"` with DETAIL `Too few columns.` /
//! `Too many columns.` / `Missing left parenthesis.` / `Junk after right parenthesis.` /
//! `Unexpected end of input.` (all oracle-verified).
//! Binary: `nfields: i32`, per non-dropped field `type_oid: u32, len: i32 (-1 NULL), bytes`.
//!
//! Owned procs (ImplModule::Record): `record_eq record_ne record_lt record_le record_gt record_ge
//! btrecordcmp record_larger record_smaller` (column-wise with the column types' default btree
//! ordering; NULLs sort after non-NULLs, NULL = NULL for ordering purposes; comparing records with
//! different column counts → 42804 `cannot compare record types with different numbers of columns`),
//! `record_image_*`/`btrecordimagecmp` (binary image comparison: compare `tuple::encode_value` bytes),
//! `hash_record(_extended)`. Field types come from `ctx.arg_types` (composite) — for anonymous records
//! the binder must supply a resolvable type; otherwise 42883 `could not identify an equality operator
//! for type unknown` (oracle text for `(1,null)::record = (1,null)::record`).
use crate::catalog_data::BuiltinProc;
use crate::compare;
use crate::context::{FuncCtx, IoCtx, TypeEnv};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::{FuncImpl, ScalarFn};
use crate::io;
use crate::names;
use crate::oids;
use crate::tuple;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;

/// PG `FirstGenbkiObjectId`: type OIDs below this are built-in.
const FIRST_GENBKI_OBJECT_ID: Oid = 10000;

/// `record_out` with explicit field types (works for anonymous records).
// CONTRACT
#[allow(unused_variables)]
pub fn output_record(ctx: &IoCtx, field_types: &[TypeId], values: &[Datum]) -> Result<String> {
    if field_types.len() != values.len() {
        return Err(PgError::internal(format!(
            "record has {} values but {} field types",
            values.len(),
            field_types.len()
        )));
    }
    let mut parts = Vec::with_capacity(values.len());
    for (ty, v) in field_types.iter().zip(values.iter()) {
        parts.push(io::output_opt(ctx, *ty, v)?);
    }
    Ok(format_record(&parts))
}

/// `record_in` for a named composite type.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    if ty.oid == oids::RECORD {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "input of anonymous composite types is not implemented",
        ));
    }
    let fields = ctx
        .env
        .composite_fields(ty.oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {}", ty.oid)))?;
    let b = s.as_bytes();
    // C-string view: past the end (or a NUL byte) reads as 0.
    let at = |i: usize| -> u8 { b.get(i).copied().unwrap_or(0) };
    let mut p = 0usize;
    while at(p) != 0 && is_c_space(at(p)) {
        p += 1;
    }
    if at(p) != b'(' {
        return Err(malformed(s, "Missing left parenthesis."));
    }
    p += 1;
    let mut values = vec![Datum::Null; fields.len()];
    let mut need_comma = false;
    for (i, f) in fields.iter().enumerate() {
        if f.dropped {
            continue;
        }
        if need_comma {
            if at(p) == b',' {
                p += 1;
            } else {
                return Err(malformed(s, "Too few columns."));
            }
        }
        need_comma = true;
        if at(p) == b',' || at(p) == b')' {
            // empty unquoted field: NULL
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut inquote = false;
        while inquote || !(at(p) == b',' || at(p) == b')') {
            let ch = at(p);
            p += 1;
            if ch == 0 {
                return Err(malformed(s, "Unexpected end of input."));
            }
            if ch == b'\\' {
                let next = at(p);
                if next == 0 {
                    return Err(malformed(s, "Unexpected end of input."));
                }
                buf.push(next);
                p += 1;
            } else if ch == b'"' {
                if !inquote {
                    inquote = true;
                } else if at(p) == b'"' {
                    buf.push(b'"');
                    p += 1;
                } else {
                    inquote = false;
                }
            } else {
                buf.push(ch);
            }
        }
        let text = String::from_utf8(buf).map_err(|_| PgError::internal("record field is not valid UTF-8"))?;
        values[i] = io::input(ctx, f.type_id, &text)?;
    }
    if at(p) != b')' {
        return Err(malformed(s, "Too many columns."));
    }
    p += 1;
    while at(p) != 0 && is_c_space(at(p)) {
        p += 1;
    }
    if at(p) != 0 {
        return Err(malformed(s, "Junk after right parenthesis."));
    }
    Ok(Datum::Record(values))
}

/// `record_out` for a named composite type (fields from `ctx.env`).
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    if ty.oid == oids::RECORD {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "output of anonymous composite types requires field types",
        ));
    }
    let stored = record_values(d)?;
    let mut expanded;
    let fields = ctx.env.composite_fields(ty.oid);
    // A metadata-only ADD ATTRIBUTE leaves older stored tuples physically shorter. PostgreSQL
    // heap_getattr reads the missing trailing attributes as NULL; output follows the current type.
    let vals = match fields {
        Some(fields) if stored.len() < fields.iter().filter(|f| !f.dropped).count() => {
            expanded = stored.to_vec();
            expanded.resize(fields.len(), Datum::Null);
            expanded.as_slice()
        }
        _ => stored,
    };
    let mut parts = Vec::with_capacity(vals.len());
    for (idx, tid) in named_fields(ctx.env, ty.oid, vals.len())? {
        parts.push(io::output_opt(ctx, tid, &vals[idx])?);
    }
    Ok(format_record(&parts))
}

/// `record_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    if ty.oid == oids::RECORD {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "input of anonymous composite types is not implemented",
        ));
    }
    let fields = ctx
        .env
        .composite_fields(ty.oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {}", ty.oid)))?;
    let mut r = Reader { buf, pos: 0 };
    let usercols = r.i32()?;
    let validcols = fields.iter().filter(|f| !f.dropped).count();
    if i64::from(usercols) != validcols as i64 {
        return Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("wrong number of columns: {usercols}, expected {validcols}"),
        ));
    }
    let mut values = vec![Datum::Null; fields.len()];
    for (i, f) in fields.iter().enumerate() {
        if f.dropped {
            continue;
        }
        let coltype = r.u32()?;
        if coltype != f.type_id.oid && coltype < FIRST_GENBKI_OBJECT_ID && f.type_id.oid < FIRST_GENBKI_OBJECT_ID {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "binary data has type {coltype} ({}) instead of expected {} ({}) in record column {}",
                    names::display_name(ctx.env, coltype),
                    f.type_id.oid,
                    names::display_name(ctx.env, f.type_id.oid),
                    i + 1
                ),
            ));
        }
        let itemlen = r.i32()?;
        if itemlen < -1 || i64::from(itemlen) > r.remaining() as i64 {
            return Err(PgError::new(SqlState::INVALID_BINARY_REPRESENTATION, "insufficient data left in message"));
        }
        if itemlen == -1 {
            continue;
        }
        let bytes = r.take(itemlen as usize)?;
        values[i] = io::recv(ctx, f.type_id, bytes)?;
    }
    Ok(Datum::Record(values))
}

/// `record_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    if ty.oid == oids::RECORD {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "output of anonymous composite types requires field types",
        ));
    }
    let vals = record_values(d)?;
    let pairs = named_fields(ctx.env, ty.oid, vals.len())?;
    let mut out = Vec::new();
    out.extend_from_slice(&(pairs.len() as i32).to_be_bytes());
    for (idx, tid) in pairs {
        out.extend_from_slice(&tid.oid.to_be_bytes());
        let v = &vals[idx];
        if v.is_null() {
            out.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        }
        let bytes = io::send(ctx, tid, v)?;
        out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "record_eq" => record_eq,
        "record_ne" => record_ne,
        "record_lt" => record_lt,
        "record_le" => record_le,
        "record_gt" => record_gt,
        "record_ge" => record_ge,
        "btrecordcmp" => btrecordcmp,
        "record_larger" => record_larger,
        "record_smaller" => record_smaller,
        "record_image_eq" => record_image_eq,
        "record_image_ne" => record_image_ne,
        "record_image_lt" => record_image_lt,
        "record_image_gt" => record_image_gt,
        "record_image_le" => record_image_le,
        "record_image_ge" => record_image_ge,
        "btrecordimagecmp" => btrecordimagecmp,
        "hash_record" => hash_record,
        "hash_record_extended" => hash_record_extended,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Record-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-40", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Helpers: text format, field typing, errors
// ---------------------------------------------------------------------------------------------

/// C-locale `isspace`.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// 22P02 `malformed record literal: "<input>"` with a DETAIL line.
fn malformed(s: &str, detail: &str) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("malformed record literal: \"{s}\"")).with_detail(detail)
}

/// Assemble `record_out` text from per-field text (`None` = SQL NULL).
fn format_record(parts: &[Option<String>]) -> String {
    let mut out = String::from("(");
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if let Some(v) = part {
            let quote = v.is_empty()
                || v.bytes().any(|c| matches!(c, b'"' | b'\\' | b'(' | b')' | b',') || is_c_space(c));
            if quote {
                out.push('"');
            }
            for ch in v.chars() {
                if ch == '"' || ch == '\\' {
                    out.push(ch);
                }
                out.push(ch);
            }
            if quote {
                out.push('"');
            }
        }
    }
    out.push(')');
    out
}

fn record_values(d: &Datum) -> Result<&[Datum]> {
    match d {
        Datum::Record(v) => Ok(v.as_slice()),
        other => Err(type_mismatch("Record", other)),
    }
}

fn record_pair<'a>(a: &'a Datum, b: &'a Datum) -> Result<(&'a [Datum], &'a [Datum])> {
    Ok((record_values(a)?, record_values(b)?))
}

/// Live (non-dropped) attributes of composite `oid` as `(index into the Datum::Record, type)`.
/// A value vector may carry every attribute (dropped ones as NULL) or only the live ones.
fn named_fields(env: &dyn TypeEnv, oid: Oid, nvals: usize) -> Result<Vec<(usize, TypeId)>> {
    let fields = env
        .composite_fields(oid)
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for composite type {oid}")))?;
    if nvals == fields.len() {
        Ok(fields.iter().enumerate().filter(|(_, f)| !f.dropped).map(|(i, f)| (i, f.type_id)).collect())
    } else {
        let live: Vec<TypeId> = fields.iter().filter(|f| !f.dropped).map(|f| f.type_id).collect();
        if nvals == live.len() {
            Ok(live.into_iter().enumerate().collect())
        } else {
            Err(PgError::internal(format!(
                "record value has {nvals} fields but composite type {oid} has {} columns",
                live.len()
            )))
        }
    }
}

/// Type used for comparisons: the binder's argument type, with `unknown` read as anonymous `record`.
fn rec_type(ctx: &FuncCtx) -> TypeId {
    let t = ctx.arg_type(0);
    if t.oid == oids::UNKNOWN {
        TypeId::of(oids::RECORD)
    } else {
        t
    }
}

fn column_count_error() -> PgError {
    PgError::new(SqlState::DATATYPE_MISMATCH, "cannot compare record types with different numbers of columns")
}

/// Index of the first column where either side is NULL (or the column count when none is).
fn first_null_column(x: &[Datum], y: &[Datum]) -> usize {
    x.iter().zip(y.iter()).position(|(p, q)| p.is_null() || q.is_null()).unwrap_or(x.len())
}

/// Big-endian cursor over a binary record message (PG `pq_getmsg*`: short reads are 08P01).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "insufficient data left in message"));
        }
        let buf: &'a [u8] = self.buf;
        let s = &buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        let arr: [u8; 4] = b
            .try_into()
            .map_err(|_| PgError::internal("record binary length read returned a short slice"))?;
        Ok(u32::from_be_bytes(arr))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
}

/// Two-argument view of a strict proc's arguments.
fn two(args: &[Datum]) -> Result<(&Datum, &Datum)> {
    match args {
        [a, b, ..] => Ok((a, b)),
        _ => Err(PgError::internal("record function called with fewer than two arguments")),
    }
}

// ---------------------------------------------------------------------------------------------
// Comparison (record_eq / record_cmp)
// ---------------------------------------------------------------------------------------------

/// `record_eq` semantics. Named composites use the column equality operators via `compare::sql_eq`.
/// Anonymous records: columns are compared in order up to the first NULL; a NULL column has no
/// known type, so reaching one gives 42883 (PG's lookup of the unknown column's equality operator).
fn record_eq_value(ctx: &FuncCtx, a: &Datum, b: &Datum) -> Result<bool> {
    let ty = rec_type(ctx);
    if ty.oid != oids::RECORD {
        return Ok(compare::sql_eq(ctx.env, ty, a, b)?.unwrap_or(false));
    }
    let (x, y) = record_pair(a, b)?;
    if x.len() != y.len() {
        return Err(column_count_error());
    }
    let k = first_null_column(x, y);
    if k > 0 {
        let eq = compare::sql_eq(
            ctx.env,
            ty,
            &Datum::Record(x[..k].to_vec()),
            &Datum::Record(y[..k].to_vec()),
        )?;
        if eq != Some(true) {
            return Ok(false);
        }
    }
    if k == x.len() {
        Ok(true)
    } else {
        Err(PgError::new(SqlState::UNDEFINED_FUNCTION, "could not identify an equality operator for type unknown"))
    }
}

/// `record_cmp` semantics (NULL sorts after non-NULL; NULL = NULL).
fn record_ord_value(ctx: &FuncCtx, a: &Datum, b: &Datum) -> Result<Ordering> {
    compare::cmp_datums(ctx.env, rec_type(ctx), a, b)
}

fn record_eq(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(record_eq_value(ctx, a, b)?))
}

fn record_ne(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(!record_eq_value(ctx, a, b)?))
}

fn record_lt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(record_ord_value(ctx, a, b)? == Ordering::Less))
}

fn record_le(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(record_ord_value(ctx, a, b)? != Ordering::Greater))
}

fn record_gt(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(record_ord_value(ctx, a, b)? == Ordering::Greater))
}

fn record_ge(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(record_ord_value(ctx, a, b)? != Ordering::Less))
}

fn btrecordcmp(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    let v = match record_ord_value(ctx, a, b)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Datum::Int4(v))
}

fn record_larger(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    if record_ord_value(ctx, a, b)? == Ordering::Greater {
        Ok(a.clone())
    } else {
        Ok(b.clone())
    }
}

fn record_smaller(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    if record_ord_value(ctx, a, b)? == Ordering::Less {
        Ok(a.clone())
    } else {
        Ok(b.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Binary image comparison (record_image_*): column-wise over `tuple::encode_value` bytes
// ---------------------------------------------------------------------------------------------

fn image_bytes(d: &Datum) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    tuple::encode_value(d, &mut v)?;
    Ok(v)
}

fn image_ord_value(a: &Datum, b: &Datum) -> Result<Ordering> {
    let (x, y) = record_pair(a, b)?;
    if x.len() != y.len() {
        return Err(column_count_error());
    }
    for (p, q) in x.iter().zip(y.iter()) {
        match (p.is_null(), q.is_null()) {
            (true, true) => continue,
            (true, false) => return Ok(Ordering::Greater),
            (false, true) => return Ok(Ordering::Less),
            (false, false) => {}
        }
        let o = image_bytes(p)?.cmp(&image_bytes(q)?);
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

fn image_eq_value(a: &Datum, b: &Datum) -> Result<bool> {
    let (x, y) = record_pair(a, b)?;
    if x.len() != y.len() {
        return Err(column_count_error());
    }
    for (p, q) in x.iter().zip(y.iter()) {
        match (p.is_null(), q.is_null()) {
            (true, true) => continue,
            (true, false) | (false, true) => return Ok(false),
            (false, false) => {}
        }
        if image_bytes(p)? != image_bytes(q)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn record_image_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(image_eq_value(a, b)?))
}

fn record_image_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(!image_eq_value(a, b)?))
}

fn record_image_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(image_ord_value(a, b)? == Ordering::Less))
}

fn record_image_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(image_ord_value(a, b)? == Ordering::Greater))
}

fn record_image_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(image_ord_value(a, b)? != Ordering::Greater))
}

fn record_image_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    Ok(Datum::Bool(image_ord_value(a, b)? != Ordering::Less))
}

fn btrecordimagecmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (a, b) = two(args)?;
    let v = match image_ord_value(a, b)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Datum::Int4(v))
}

// ---------------------------------------------------------------------------------------------
// hash_record / hash_record_extended (rowtypes.c: result = (result << 5) - result + element_hash)
// ---------------------------------------------------------------------------------------------

/// Type of an anonymous record's field, read from its value (None = no known type).
fn value_type(d: &Datum) -> Option<TypeId> {
    let oid = match d {
        Datum::Bool(_) => oids::BOOL,
        Datum::Int2(_) => oids::INT2,
        Datum::Int4(_) => oids::INT4,
        Datum::Int8(_) => oids::INT8,
        Datum::Text(_) => oids::TEXT,
        Datum::Bytea(_) => oids::BYTEA,
        Datum::Uuid(_) => oids::UUID,
        Datum::Date(_) => oids::DATE,
        Datum::Time(_) => oids::TIME,
        Datum::Timestamp(_) => oids::TIMESTAMP,
        Datum::TimestampTz(_) => oids::TIMESTAMPTZ,
        Datum::Oid(_) => oids::OID,
        Datum::Char(_) => oids::CHAR,
        Datum::Record(_) => oids::RECORD,
        _ => return None,
    };
    Some(TypeId::of(oid))
}

/// `hash_uint32` (or `hash_uint32_extended` with a seed).
fn hash_u32_s(v: u32, seed: Option<u64>) -> Result<u64> {
    match seed {
        None => Ok(u64::from(compare::pg_hash_uint32(v)?)),
        Some(s) => compare::pg_hash_uint32_extended(v, s),
    }
}

/// `hashint8` / `hashint8extended`.
fn hash_i64_s(v: i64, seed: Option<u64>) -> Result<u64> {
    let hi = (v >> 32) as u32;
    let mut lo = v as u32;
    lo ^= if v >= 0 { hi } else { !hi };
    hash_u32_s(lo, seed)
}

/// `hash_bytes` / `hash_bytes_extended` (`hash_any`).
fn hash_bytes_s(b: &[u8], seed: Option<u64>) -> Result<u64> {
    match seed {
        None => Ok(u64::from(compare::pg_hash_bytes(b)?)),
        Some(s) => compare::pg_hash_bytes_extended(b, s),
    }
}

/// Hash of one non-NULL column value of type `ty` (PG's per-type hash support function).
fn hash_one(ctx: &FuncCtx, ty: TypeId, v: &Datum, seed: Option<u64>) -> Result<u64> {
    if let Datum::Record(vals) = v {
        return hash_record_value(ctx, ty, vals, seed);
    }
    let oid = ty.oid;
    match (oid, v) {
        (oids::BOOL, Datum::Bool(b)) => hash_u32_s(u32::from(*b), seed),
        (oids::CHAR, Datum::Char(c)) => hash_u32_s((*c as i8) as i32 as u32, seed),
        (oids::INT2, Datum::Int2(x)) => hash_u32_s((*x as i32) as u32, seed),
        (oids::INT4, Datum::Int4(x)) | (oids::DATE, Datum::Date(x)) => hash_u32_s(*x as u32, seed),
        (oids::INT8, Datum::Int8(x)) => hash_i64_s(*x, seed),
        (oids::TIME, Datum::Time(x))
        | (oids::TIMESTAMP, Datum::Timestamp(x))
        | (oids::TIMESTAMPTZ, Datum::TimestampTz(x)) => hash_i64_s(*x, seed),
        (_, Datum::Oid(x)) => hash_u32_s(*x, seed),
        (_, Datum::Text(s)) if ctx.env.enum_labels(oid).is_some() => {
            // hashenum hashes the label's pg_enum OID.
            let labels = ctx.env.enum_labels(oid).ok_or_else(|| PgError::internal("enum labels vanished"))?;
            let label = labels
                .iter()
                .find(|l| l.label == *s)
                .ok_or_else(|| PgError::internal(format!("invalid enum label \"{s}\" for type {oid}")))?;
            hash_u32_s(label.oid, seed)
        }
        (oids::TEXT | oids::VARCHAR | oids::NAME, Datum::Text(s)) => hash_bytes_s(s.as_bytes(), seed),
        (oids::BPCHAR, Datum::Text(s)) => hash_bytes_s(s.trim_end_matches(' ').as_bytes(), seed),
        (oids::BYTEA, Datum::Bytea(b)) => hash_bytes_s(b.as_slice(), seed),
        (oids::UUID, Datum::Uuid(u)) => hash_bytes_s(&u[..], seed),
        (oids::JSON, _) => Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not identify a hash function for type {}", names::display_name(ctx.env, oid)),
        )),
        _ => Err(PgError::unimplemented(
            "TYP-40",
            format!("hash of a {} value in a record column of type {}", v.variant_name(), names::display_name(ctx.env, oid)),
        )),
    }
}

/// `hash_record` body. `ty` is the composite type (or anonymous `record`). Without a seed the
/// accumulation is uint32 and the result is returned zero-extended.
fn hash_record_value(ctx: &FuncCtx, ty: TypeId, vals: &[Datum], seed: Option<u64>) -> Result<u64> {
    let anonymous = ty.oid == oids::RECORD;
    let pairs: Vec<(TypeId, &Datum)> = if anonymous {
        let mut out = Vec::with_capacity(vals.len());
        for v in vals {
            match value_type(v) {
                Some(t) => out.push((t, v)),
                None if v.is_null() => {
                    return Err(PgError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        "could not identify a hash function for type unknown",
                    ))
                }
                None => {
                    return Err(PgError::unimplemented(
                        "TYP-40",
                        format!("hash of a {} value in an anonymous record", v.variant_name()),
                    ))
                }
            }
        }
        out
    } else {
        named_fields(ctx.env, ty.oid, vals.len())?.into_iter().map(|(i, t)| (t, &vals[i])).collect()
    };
    let mut r32: u32 = 0;
    let mut r64: u64 = 0;
    for (tid, v) in pairs {
        let eh: u64 = if v.is_null() { 0 } else { hash_one(ctx, tid, v, seed)? };
        match seed {
            None => {
                let e = eh as u32;
                r32 = r32.wrapping_shl(5).wrapping_sub(r32).wrapping_add(e);
            }
            Some(_) => {
                r64 = r64.wrapping_shl(5).wrapping_sub(r64).wrapping_add(eh);
            }
        }
    }
    Ok(match seed {
        None => u64::from(r32),
        Some(_) => r64,
    })
}

fn hash_record(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let a = args.first().ok_or_else(|| PgError::internal("hash_record called without arguments"))?;
    let h = hash_record_value(ctx, rec_type(ctx), record_values(a)?, None)?;
    Ok(Datum::Int4(h as u32 as i32))
}

fn hash_record_extended(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let (a, seed) = two(args)?;
    let seed = seed.as_i64().ok_or_else(|| type_mismatch("Int8", seed))?;
    let h = hash_record_value(ctx, rec_type(ctx), record_values(a)?, Some(seed as u64))?;
    Ok(Datum::Int8(h as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CompositeField, IoConfig, TestSession};
    use crate::registry::{builtin_env, BuiltinTypeEnv, TypeInfo};
    use std::sync::Arc;

    const ROW_OID: Oid = 900_001;

    /// Builtin types plus one anonymous-free composite `row(a int4, b text)` with a dropped column.
    struct RowEnv(&'static BuiltinTypeEnv);

    impl TypeEnv for RowEnv {
        fn type_info(&self, oid: Oid) -> Option<TypeInfo> {
            self.0.type_info(oid)
        }
        fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid> {
            self.0.type_by_name(schema, typname)
        }
        fn composite_fields(&self, type_oid: Oid) -> Option<Arc<[CompositeField]>> {
            if type_oid != ROW_OID {
                return None;
            }
            Some(Arc::from(vec![
                CompositeField { name: "a".to_string(), type_id: TypeId::of(oids::INT4), dropped: false },
                CompositeField { name: "gone".to_string(), type_id: TypeId::of(oids::INT4), dropped: true },
                CompositeField { name: "b".to_string(), type_id: TypeId::of(oids::TEXT), dropped: false },
            ]))
        }
    }

    fn row_type() -> TypeId {
        TypeId::of(ROW_OID)
    }

    #[test]
    fn output_record_matches_pg_quoting() {
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(builtin_env(), &cfg);
        let types = [TypeId::of(oids::INT4), TypeId::of(oids::TEXT), TypeId::of(oids::TEXT)];
        let vals = [Datum::Int4(1), Datum::Text("a b".into()), Datum::Null];
        assert_eq!(output_record(&ctx, &types, &vals).unwrap(), "(1,\"a b\",)");
        let vals = [Datum::Int4(1), Datum::Text("a\"b".into()), Datum::Null];
        assert_eq!(output_record(&ctx, &types, &vals).unwrap(), "(1,\"a\"\"b\",)");
        let vals = [Datum::Int4(1), Datum::Text(String::new()), Datum::Null];
        assert_eq!(output_record(&ctx, &types, &vals).unwrap(), "(1,\"\",)");
    }

    #[test]
    fn input_parses_fields_and_reports_pg_errors() {
        let env = RowEnv(builtin_env());
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(&env, &cfg);
        match input(&ctx, row_type(), "(7,\"x y\")") {
            Ok(Datum::Record(v)) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0], Datum::Int4(7));
                assert_eq!(v[1], Datum::Null);
                assert_eq!(v[2], Datum::Text("x y".into()));
            }
            Ok(other) => panic!("unexpected {other:?}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(input(&ctx, row_type(), "(1)").is_err());
        assert!(input(&ctx, row_type(), "(1,a,b) x").is_err());
        assert!(input(&ctx, TypeId::of(oids::RECORD), "(1,a)").is_err());
    }

    #[test]
    fn output_skips_dropped_columns() {
        let env = RowEnv(builtin_env());
        let cfg = IoConfig::default();
        let ctx = IoCtx::new(&env, &cfg);
        let v = Datum::Record(vec![Datum::Int4(5), Datum::Null, Datum::Text(String::new())]);
        assert_eq!(output(&ctx, row_type(), &v).unwrap(), "(5,\"\")");
    }

    #[test]
    fn anonymous_eq_with_null_is_unknown_type() {
        let sess = TestSession::default();
        let cfg = IoConfig::default();
        let types = [TypeId::of(oids::RECORD)];
        let fctx = FuncCtx {
            env: builtin_env(),
            io: &cfg,
            session: &sess,
            arg_types: &types,
            result_type: TypeId::of(oids::BOOL),
            collation: 0,
        };
        let a = Datum::Record(vec![Datum::Int4(1), Datum::Null]);
        assert!(record_eq_value(&fctx, &a, &a).is_err());
        let b = Datum::Record(vec![Datum::Int4(2), Datum::Null]);
        assert_eq!(record_eq_value(&fctx, &a, &b).unwrap(), false);
    }
}
