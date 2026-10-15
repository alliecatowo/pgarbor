//! Collecting aggregates: string_agg, array_agg, json(b)_agg, json(b)_object_agg (+ _strict/_unique)
//! (TYPE-08, TYPE-09, TYPE-12). Lease TYP-46.
//!
//! PG18 reference: `utils/adt/varlena.c` (`string_agg_transfn`, `bytea_string_agg_transfn`),
//! `array_userfuncs.c` (`array_agg_transfn`, `array_agg_array_transfn`), `json.c`
//! (`json_agg_transfn`, `json_object_agg_transfn`, `json_object_agg_unique_*`), `jsonb.c`
//! (`jsonb_agg_transfn`, `jsonb_object_agg_transfn`).
//!
//! Semantics (oracle-verified):
//! - `string_agg(text, delim)` / `string_agg(bytea, delim)`: skips NULL values; the delimiter is placed
//!   BEFORE every value except the first (a NULL delimiter counts as empty); no non-NULL value → NULL.
//! - `array_agg(anynonarray)` → 1-D array INCLUDING NULLs (`{NULL}`); `array_agg(anyarray)` → one more
//!   dimension (inputs must have equal dims: 2202E `cannot accumulate arrays of different dimensionality`;
//!   NULL input array → 22004 `cannot accumulate null arrays`; empty → 2202E `cannot accumulate empty arrays`);
//!   zero rows → NULL.
//! - `json_agg` → json text `[1, 2, 3]` (elements via `json::build::datum_to_json_text`, NULL → `null`,
//!   separator `, `); `json_agg_strict` skips NULLs. `jsonb_agg` → jsonb array (`[null]`).
//! - `json_object_agg(k, v)` → json text `{ "a" : 1, "b" : null }` (note the spaces: `{ ` + `"k" : v`
//!   joined by `, ` + ` }`), duplicates KEPT (`{ "a" : 1, "a" : 2 }`); NULL key → 22023
//!   `field name must not be null`; `_strict` skips NULL values; `_unique` errors on duplicate keys with
//!   22030 `duplicate JSON object key value: "a"`. `jsonb_object_agg` → normalized jsonb object (last wins).
//! Zero rows → NULL for all of these.
//!
//! Implementation notes (from the PG 18 C sources):
//! - The `_strict` json/jsonb aggregates create their state on the FIRST row even when that row is
//!   skipped, so `json_agg_strict(NULL)` gives `[]` and `json_object_agg_strict` over NULL values gives
//!   `{  }` (PG's own double space); only zero rows give NULL.
//! - json_object_agg's NULL-key error is `22004 null value not allowed for object key` (json.c), while
//!   jsonb_object_agg's is `22023 field name must not be null` (jsonb.c); both are reproduced as in C.
//! - json_agg puts `\n ` before a non-first array/composite element (json.c), so
//!   `json_agg(ARRAY[1,2]), ...` yields `[[1,2], \n [3,4]]`.
use super::{AggSpec, AggState};
use crate::array::{Array, MAXDIM};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use crate::datum::{Datum, TypeId};
use crate::json::build::{datum_to_json_text, datum_to_jsonb};
use crate::json::Jsonb;
use crate::{oids, registry};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::collections::HashSet;

/// `22030 duplicate_json_object_key_value` (not in the shared SQLSTATE table).
const DUPLICATE_JSON_OBJECT_KEY_VALUE: SqlState = SqlState(*b"22030");

/// Instantiate a collecting aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    let arg0 = spec.arg_types.first().copied().unwrap_or(TypeId::UNKNOWN);
    let arg1 = spec.arg_types.get(1).copied().unwrap_or(TypeId::UNKNOWN);
    let state: Box<dyn AggState> = match agg.oid {
        // string_agg(text, text) / string_agg(bytea, bytea)
        3538 => Box::new(StringAgg { buf: None }),
        3545 => Box::new(ByteaAgg { buf: None }),
        // array_agg(anynonarray) / array_agg(anyarray)
        2335 => Box::new(ArrayAgg { nested: false, elem: arg0.oid, count: 0, inner_dims: Vec::new(), items: Vec::new() }),
        4053 => {
            let elem = registry::element_type_of(ctx.env, arg0.oid).unwrap_or(arg0.oid);
            Box::new(ArrayAgg { nested: true, elem, count: 0, inner_dims: Vec::new(), items: Vec::new() })
        }
        // json_agg / json_agg_strict
        3175 | 6276 => Box::new(JsonAgg::new(arg0, agg.oid == 6276, ctx)),
        // jsonb_agg / jsonb_agg_strict
        3267 | 6284 => Box::new(JsonbAgg { arg: arg0, absent_on_null: agg.oid == 6284, started: false, items: Vec::new() }),
        // json_object_agg, _strict, _unique, _unique_strict
        3197 | 6280 | 6281 | 6282 => Box::new(JsonObjectAgg {
            key_ty: arg0,
            val_ty: arg1,
            absent_on_null: agg.oid == 6280 || agg.oid == 6282,
            unique: agg.oid == 6281 || agg.oid == 6282,
            buf: None,
            seen: HashSet::new(),
        }),
        // jsonb_object_agg, _strict, _unique, _unique_strict
        3270 | 6288 | 6289 | 6290 => Box::new(JsonbObjectAgg {
            key_ty: arg0,
            val_ty: arg1,
            absent_on_null: agg.oid == 6288 || agg.oid == 6290,
            unique: agg.oid == 6289 || agg.oid == 6290,
            started: false,
            pairs: Vec::new(),
            seen: HashSet::new(),
        }),
        _ => return Err(crate::extra_types::unsupported_aggregate(agg)),
    };
    Ok(state)
}

// ---------------------------------------------------------------------------------------------
// helpers

/// Argument `i` of a transition call, or an internal error when the executor passed too few.
fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("collecting aggregate: missing argument {}", i + 1)))
}

fn unexpected(what: &str, d: &Datum) -> PgError {
    PgError::internal(format!("{what}: unexpected {} value", d.variant_name()))
}

/// How PG's `json_categorize_type` classifies a value for JSON purposes (only the cases that matter here).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Array,
    Composite,
    Json,
    Other,
}

fn category(ctx: &FuncCtx, ty: TypeId) -> Category {
    let base = registry::base_type(ctx.env, ty.oid);
    if base == oids::JSON || base == oids::JSONB {
        return Category::Json;
    }
    if base == oids::RECORD {
        return Category::Composite;
    }
    if registry::is_array_type(ctx.env, base) {
        return Category::Array;
    }
    match registry::type_info(ctx.env, base) {
        Ok(info) if info.typtype == b'c' => Category::Composite,
        _ => Category::Other,
    }
}

/// JSON text of a (non-NULL) object key, quoted as a JSON string (PG `datum_to_json_internal` with
/// `key_scalar = true`). Scalars whose JSON form is already quoted (text, dates, timestamps) keep it.
fn key_json(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    if matches!(category(ctx, ty), Category::Array | Category::Composite | Category::Json) {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json",
        ));
    }
    let text = datum_to_json_text(d, ty, ctx)?;
    if text.starts_with('"') {
        Ok(text)
    } else {
        Ok(format!("\"{text}\""))
    }
}

/// Decode a JSON string literal produced by our own JSON text builders (`escape_json`) back into the
/// raw string. Only the escapes that `escape_json` emits (plus `\/` and `\uXXXX`) need handling.
fn decode_json_string(s: &str) -> String {
    let inner = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{08}'),
            Some('f') => out.push('\u{0C}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let mut code = 0u32;
                let mut ok = true;
                for _ in 0..4 {
                    match chars.next().and_then(|h| h.to_digit(16)) {
                        Some(v) => code = code * 16 + v,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                out.push(if ok { char::from_u32(code).unwrap_or('\u{FFFD}') } else { '\u{FFFD}' });
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Raw text of a jsonb object key (the decoded form of its JSON key text).
fn key_text(d: &Datum, ty: TypeId, ctx: &FuncCtx) -> Result<String> {
    Ok(decode_json_string(&key_json(d, ty, ctx)?))
}

// ---------------------------------------------------------------------------------------------
// string_agg

struct StringAgg {
    buf: Option<String>,
}

impl AggState for StringAgg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let value = match arg(args, 0)? {
            Datum::Null => return Ok(()),
            Datum::Text(s) => s,
            other => return Err(unexpected("string_agg", other)),
        };
        let delim: &str = match arg(args, 1)? {
            Datum::Null => "",
            Datum::Text(d) => d.as_str(),
            other => return Err(unexpected("string_agg", other)),
        };
        let next = match self.buf.take() {
            Some(mut buf) => {
                buf.push_str(delim);
                buf.push_str(value);
                buf
            }
            None => value.clone(),
        };
        self.buf = Some(next);
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match &self.buf {
            Some(b) => Datum::Text(b.clone()),
            None => Datum::Null,
        })
    }
}

struct ByteaAgg {
    buf: Option<Vec<u8>>,
}

impl AggState for ByteaAgg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let value = match arg(args, 0)? {
            Datum::Null => return Ok(()),
            Datum::Bytea(b) => b,
            other => return Err(unexpected("string_agg", other)),
        };
        let empty: &[u8] = &[];
        let delim: &[u8] = match arg(args, 1)? {
            Datum::Null => empty,
            Datum::Bytea(d) => d.as_slice(),
            other => return Err(unexpected("string_agg", other)),
        };
        let next = match self.buf.take() {
            Some(mut buf) => {
                buf.extend_from_slice(delim);
                buf.extend_from_slice(value);
                buf
            }
            None => value.clone(),
        };
        self.buf = Some(next);
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match &self.buf {
            Some(b) => Datum::Bytea(b.clone()),
            None => Datum::Null,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// array_agg

struct ArrayAgg {
    /// `false`: `array_agg(anynonarray)`; `true`: `array_agg(anyarray)` (adds a leading dimension).
    nested: bool,
    /// Element type of the result array.
    elem: Oid,
    /// Rows accumulated so far (zero → NULL result).
    count: usize,
    /// Dimensions of each accumulated input array (empty for `anynonarray`).
    inner_dims: Vec<(i32, i32)>,
    items: Vec<Datum>,
}

impl AggState for ArrayAgg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let v = arg(args, 0)?;
        if !self.nested {
            self.items.push(v.clone());
            self.count += 1;
            return Ok(());
        }
        let a: &Array = match v {
            Datum::Null => {
                return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "cannot accumulate null arrays"));
            }
            Datum::Array(a) => &**a,
            other => return Err(unexpected("array_agg", other)),
        };
        if a.dims.is_empty() {
            return Err(PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "cannot accumulate empty arrays"));
        }
        if self.count == 0 {
            if a.dims.len() + 1 > MAXDIM {
                return Err(PgError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                        a.dims.len() + 1
                    ),
                ));
            }
            self.inner_dims = a.dims.clone();
        } else if a.dims != self.inner_dims {
            return Err(PgError::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                "cannot accumulate arrays of different dimensionality",
            ));
        }
        self.items.extend(a.items.iter().cloned());
        self.count += 1;
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.count == 0 {
            return Ok(Datum::Null);
        }
        let mut dims = Vec::with_capacity(self.inner_dims.len() + 1);
        dims.push((self.count as i32, 1));
        dims.extend_from_slice(&self.inner_dims);
        Ok(Datum::array(Array { elem: self.elem, dims, items: self.items.clone() }))
    }
}

// ---------------------------------------------------------------------------------------------
// json_agg / jsonb_agg

struct JsonAgg {
    arg: TypeId,
    absent_on_null: bool,
    /// Element arrays and composites get a `\n ` before them (PG `json_agg_transfn`).
    structured: bool,
    /// `[` plus the elements so far; `None` until the first row (PG creates the state on first call).
    buf: Option<String>,
}

impl JsonAgg {
    fn new(arg: TypeId, absent_on_null: bool, ctx: &FuncCtx) -> JsonAgg {
        let structured = matches!(category(ctx, arg), Category::Array | Category::Composite);
        JsonAgg { arg, absent_on_null, structured, buf: None }
    }
}

impl AggState for JsonAgg {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let v = arg(args, 0)?;
        let buf = self.buf.get_or_insert_with(|| String::from("["));
        if self.absent_on_null && v.is_null() {
            return Ok(());
        }
        let not_first = buf.len() > 1;
        if not_first {
            buf.push_str(", ");
        }
        if v.is_null() {
            buf.push_str("null");
            return Ok(());
        }
        if not_first && self.structured {
            buf.push_str("\n ");
        }
        let text = datum_to_json_text(v, self.arg, ctx)?;
        buf.push_str(&text);
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match &self.buf {
            Some(b) => {
                let mut s = String::with_capacity(b.len() + 1);
                s.push_str(b);
                s.push(']');
                Datum::Json(s)
            }
            None => Datum::Null,
        })
    }
}

struct JsonbAgg {
    arg: TypeId,
    absent_on_null: bool,
    started: bool,
    items: Vec<Jsonb>,
}

impl AggState for JsonbAgg {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let v = arg(args, 0)?;
        self.started = true;
        if self.absent_on_null && v.is_null() {
            return Ok(());
        }
        let item = if v.is_null() { Jsonb::Null } else { datum_to_jsonb(v, self.arg, ctx)? };
        self.items.push(item);
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if !self.started {
            return Ok(Datum::Null);
        }
        Ok(Datum::Jsonb(Jsonb::Array(self.items.clone())))
    }
}

// ---------------------------------------------------------------------------------------------
// json_object_agg / jsonb_object_agg

struct JsonObjectAgg {
    key_ty: TypeId,
    val_ty: TypeId,
    absent_on_null: bool,
    unique: bool,
    /// `{ ` plus the pairs so far; `None` until the first row.
    buf: Option<String>,
    /// Key texts (JSON-quoted) seen so far, for the `_unique` variants.
    seen: HashSet<String>,
}

impl AggState for JsonObjectAgg {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let key = arg(args, 0)?;
        let val = arg(args, 1)?;
        let buf = self.buf.get_or_insert_with(|| String::from("{ "));
        if key.is_null() {
            return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key"));
        }
        let skip = self.absent_on_null && val.is_null();
        if skip && !self.unique {
            return Ok(());
        }
        let kj = key_json(key, self.key_ty, ctx)?;
        if self.unique {
            if !self.seen.insert(kj.clone()) {
                return Err(PgError::new(
                    DUPLICATE_JSON_OBJECT_KEY_VALUE,
                    format!("duplicate JSON object key value: {kj}"),
                ));
            }
            if skip {
                return Ok(());
            }
        }
        if buf.len() > 2 {
            buf.push_str(", ");
        }
        buf.push_str(&kj);
        buf.push_str(" : ");
        if val.is_null() {
            buf.push_str("null");
        } else {
            let vt = datum_to_json_text(val, self.val_ty, ctx)?;
            buf.push_str(&vt);
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match &self.buf {
            Some(b) => {
                let mut s = String::with_capacity(b.len() + 2);
                s.push_str(b);
                s.push_str(" }");
                Datum::Json(s)
            }
            None => Datum::Null,
        })
    }
}

struct JsonbObjectAgg {
    key_ty: TypeId,
    val_ty: TypeId,
    absent_on_null: bool,
    unique: bool,
    started: bool,
    pairs: Vec<(String, Jsonb)>,
    /// Raw key texts seen so far, for the `_unique` variants.
    seen: HashSet<String>,
}

impl AggState for JsonbObjectAgg {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let key = arg(args, 0)?;
        let val = arg(args, 1)?;
        self.started = true;
        if key.is_null() {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "field name must not be null"));
        }
        let skip = self.absent_on_null && val.is_null();
        if skip && !self.unique {
            return Ok(());
        }
        let k = key_text(key, self.key_ty, ctx)?;
        if self.unique {
            // PG reports this without the key text (jsonb_util.c `uniqueifyJsonbObject`).
            if !self.seen.insert(k.clone()) {
                return Err(PgError::new(DUPLICATE_JSON_OBJECT_KEY_VALUE, "duplicate JSON object key value"));
            }
            if skip {
                return Ok(());
            }
        }
        let v = if val.is_null() { Jsonb::Null } else { datum_to_jsonb(val, self.val_ty, ctx)? };
        self.pairs.push((k, v));
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if !self.started {
            return Ok(Datum::Null);
        }
        Ok(Datum::Jsonb(Jsonb::object_from_pairs(self.pairs.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};

    /// Run one aggregate over `rows` (each row = the aggregated argument values) and finalize it.
    fn run(agg_oid: Oid, arg_types: Vec<TypeId>, rows: Vec<Vec<Datum>>) -> Result<Datum> {
        let env = registry::builtin_env();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &io,
            session: &session,
            arg_types: &arg_types,
            result_type: TypeId::UNKNOWN,
            collation: 0,
        };
        let spec = AggSpec { agg_oid, arg_types: arg_types.clone(), result_type: TypeId::UNKNOWN, direct_args: vec![] };
        let mut st = crate::agg::make_aggregate(&spec, &ctx)?;
        for row in &rows {
            st.transition(row, &ctx)?;
        }
        st.finalize(&ctx)
    }

    fn int_arr(elem: Oid, dims: Vec<(i32, i32)>, items: Vec<Datum>) -> Datum {
        Datum::array(Array { elem, dims, items })
    }

    #[test]
    fn array_agg_keeps_nulls() {
        let got = run(2335, vec![TypeId::INT4], vec![vec![Datum::Null]]).unwrap();
        assert_eq!(got, int_arr(oids::INT4, vec![(1, 1)], vec![Datum::Null]));
    }

    #[test]
    fn array_agg_zero_rows_is_null() {
        assert_eq!(run(2335, vec![TypeId::INT4], vec![]).unwrap(), Datum::Null);
    }

    #[test]
    fn array_agg_of_arrays_builds_two_dimensions() {
        let a = Datum::array(Array { elem: oids::INT4, dims: vec![(2, 1)], items: vec![Datum::Int4(1), Datum::Int4(2)] });
        let b = Datum::array(Array { elem: oids::INT4, dims: vec![(2, 1)], items: vec![Datum::Int4(3), Datum::Int4(4)] });
        let got = run(4053, vec![TypeId::INT4_ARRAY], vec![vec![a], vec![b]]).unwrap();
        let want = int_arr(
            oids::INT4,
            vec![(2, 1), (2, 1)],
            vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3), Datum::Int4(4)],
        );
        assert_eq!(got, want);
    }

    #[test]
    fn array_agg_of_arrays_rejects_mismatched_dims() {
        let a = Datum::array(Array { elem: oids::INT4, dims: vec![(1, 1)], items: vec![Datum::Int4(1)] });
        let b = Datum::array(Array { elem: oids::INT4, dims: vec![(2, 1)], items: vec![Datum::Int4(2), Datum::Int4(3)] });
        let err = run(4053, vec![TypeId::INT4_ARRAY], vec![vec![a], vec![b]]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::ARRAY_SUBSCRIPT_ERROR);
        assert_eq!(err.message, "cannot accumulate arrays of different dimensionality");
    }

    #[test]
    fn array_agg_of_null_array_is_22004() {
        let err = run(4053, vec![TypeId::INT4_ARRAY], vec![vec![Datum::Null]]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::NULL_VALUE_NOT_ALLOWED);
        assert_eq!(err.message, "cannot accumulate null arrays");
    }

    #[test]
    fn string_agg_skips_nulls_and_places_delimiter_before_values() {
        let t = |s: &str| Datum::Text(s.to_string());
        let rows = vec![
            vec![t("a"), t(",")],
            vec![Datum::Null, t(",")],
            vec![t("b"), t(",")],
            vec![t("c"), Datum::Null],
        ];
        assert_eq!(run(3538, vec![TypeId::TEXT, TypeId::TEXT], rows).unwrap(), Datum::Text("a,bc".into()));
    }

    #[test]
    fn string_agg_all_nulls_is_null() {
        let rows = vec![vec![Datum::Null, Datum::Text(",".into())]];
        assert_eq!(run(3538, vec![TypeId::TEXT, TypeId::TEXT], rows).unwrap(), Datum::Null);
    }

    #[test]
    fn bytea_string_agg_joins_with_delimiter() {
        let rows = vec![
            vec![Datum::Bytea(vec![1]), Datum::Bytea(vec![0])],
            vec![Datum::Bytea(vec![2]), Datum::Bytea(vec![0])],
        ];
        let got = run(3545, vec![TypeId::BYTEA, TypeId::BYTEA], rows).unwrap();
        assert_eq!(got, Datum::Bytea(vec![1, 0, 2]));
    }

    #[test]
    fn json_agg_null_is_array_with_null() {
        let got = run(3175, vec![TypeId::INT4], vec![vec![Datum::Null]]).unwrap();
        assert_eq!(got, Datum::Json("[null]".into()));
    }

    #[test]
    fn json_agg_strict_all_nulls_is_empty_array() {
        let got = run(6276, vec![TypeId::INT4], vec![vec![Datum::Null]]).unwrap();
        assert_eq!(got, Datum::Json("[]".into()));
    }

    #[test]
    fn json_agg_zero_rows_is_null() {
        assert_eq!(run(3175, vec![TypeId::INT4], vec![]).unwrap(), Datum::Null);
    }

    #[test]
    fn jsonb_agg_strict_all_nulls_is_empty_array() {
        let got = run(6284, vec![TypeId::INT4], vec![vec![Datum::Null]]).unwrap();
        assert_eq!(got, Datum::Jsonb(Jsonb::Array(vec![])));
    }

    #[test]
    fn json_object_agg_keeps_duplicate_keys() {
        let rows = vec![
            vec![Datum::Text("a".into()), Datum::Int4(1)],
            vec![Datum::Text("a".into()), Datum::Int4(2)],
        ];
        let got = run(3197, vec![TypeId::TEXT, TypeId::INT4], rows).unwrap();
        assert_eq!(got, Datum::Json("{ \"a\" : 1, \"a\" : 2 }".into()));
    }

    #[test]
    fn json_object_agg_null_key_is_22004() {
        let err = run(3197, vec![TypeId::TEXT, TypeId::INT4], vec![vec![Datum::Null, Datum::Int4(1)]]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::NULL_VALUE_NOT_ALLOWED);
        assert_eq!(err.message, "null value not allowed for object key");
    }

    #[test]
    fn json_object_agg_strict_all_null_values_is_double_space_object() {
        let rows = vec![vec![Datum::Text("a".into()), Datum::Null]];
        let got = run(6280, vec![TypeId::TEXT, TypeId::INT4], rows).unwrap();
        assert_eq!(got, Datum::Json("{  }".into()));
    }

    #[test]
    fn json_object_agg_unique_rejects_duplicate_key() {
        let rows = vec![
            vec![Datum::Text("a".into()), Datum::Int4(1)],
            vec![Datum::Text("a".into()), Datum::Int4(2)],
        ];
        let err = run(6281, vec![TypeId::TEXT, TypeId::INT4], rows).unwrap_err();
        assert_eq!(err.sqlstate, DUPLICATE_JSON_OBJECT_KEY_VALUE);
        assert_eq!(err.message, "duplicate JSON object key value: \"a\"");
    }

    #[test]
    fn jsonb_object_agg_null_key_is_22023() {
        let err = run(3270, vec![TypeId::TEXT, TypeId::INT4], vec![vec![Datum::Null, Datum::Int4(1)]]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(err.message, "field name must not be null");
    }

    #[test]
    fn jsonb_object_agg_last_duplicate_wins() {
        let rows = vec![
            vec![Datum::Text("a".into()), Datum::Text("x".into())],
            vec![Datum::Text("a".into()), Datum::Text("y".into())],
        ];
        let got = run(3270, vec![TypeId::TEXT, TypeId::TEXT], rows).unwrap();
        assert_eq!(got, Datum::Jsonb(Jsonb::Object(vec![("a".into(), Jsonb::String("y".into()))])));
    }

    #[test]
    fn jsonb_object_agg_zero_rows_is_null() {
        assert_eq!(run(3270, vec![TypeId::TEXT, TypeId::TEXT], vec![]).unwrap(), Datum::Null);
    }

    #[test]
    fn decode_json_string_inverts_escape_json() {
        let original = "a\"b\\c\nd\u{01}";
        let quoted = crate::json::parse::escape_json(original);
        assert_eq!(decode_json_string(&quoted), original);
    }
}
