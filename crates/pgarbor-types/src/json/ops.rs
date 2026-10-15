//! json/jsonb operators and jsonb ordering (TYPE-08). Lease TYP-34.
//!
//! PG18 reference: `utils/adt/jsonfuncs.c` (`json_object_field`, `jsonb_array_element`,
//! `jsonb_extract_path`, `jsonb_delete*`, `jsonb_concat`), `jsonb_op.c` (`jsonb_exists*`,
//! `jsonb_contains`, `jsonb_eq`, `jsonb_cmp`), `jsonb_util.c` (`compareJsonbContainers`,
//! `JsonbDeepContains`).
//! Owned procs (ImplModule::JsonOps): `json_array_element json_array_element_text json_extract_path
//! json_extract_path_text` (on `json`: walk `parse::parse_json_value`; `->>` output of a string is its
//! unescaped value, of other values their json text), `json_object_field json_object_field_text
//! jsonb_object_field jsonb_object_field_text` (`->`/`->>` with a text key), `jsonb_array_element jsonb_array_element_text
//! jsonb_extract_path jsonb_extract_path_text` (`->`, `->>` with int, `#>`, `#>>`; negative array index
//! counts from the end: `'[1,2,3]'::jsonb -> -1` = `3`; missing → NULL; `->>` of JSON null → SQL NULL),
//! `jsonb_exists jsonb_exists_any jsonb_exists_all` (`?` `?|` `?&`: top-level keys or string array
//! elements), `jsonb_contains jsonb_contained` (`@>` `<@`, PG containment semantics incl. scalar-in-array
//! and array-contains-array regardless of order/duplicates), `jsonb_concat` (`||`: object merge (right
//! wins), array concat, scalars wrapped as 1-element arrays: `'[1]' || '2'` = `[1, 2]`), `jsonb_delete`
//! (`- text`), `jsonb_delete_idx` (`- int`), `jsonb_delete_array` (`- text[]`), `jsonb_delete_path`
//! (`#-`), `jsonb_eq ne lt le gt ge jsonb_cmp jsonb_hash(_extended)`.
//!
//! jsonb ordering (`compare_jsonb`, also used by `compare`/`keycodec`), oracle-verified:
//! Object > Array > Boolean > Number > String > Null; arrays: more elements is greater
//! (`'[1,2]' > '[3]'`), then element-wise; objects: more pairs is greater, then pair-by-pair in storage
//! order comparing KEYS by text collation order (NOT length-first: `'{"b":1}' > '{"aa":1}'`) then values;
//! numbers numerically; strings by codepoint; false < true. Quirk: an EMPTY top-level array sorts below
//! everything, including null (`'[]'::jsonb < 'null'` is true).
//!
//! Implementation notes (worker-level, not contract):
//! - A top-level scalar is a "raw scalar" in PG: it is stored as a one-element array flagged raw. Every
//!   array-like operation here treats a top-level scalar exactly that way (`'3'::jsonb -> 0` = `3`,
//!   `'[1]' || '2'` = `[1, 2]`, `'1' @> '1'` = true). Nested scalars are never raw.
//! - json (text) operators follow `get_worker` in jsonfuncs.c: the document is validated in full first
//!   (so PG's syntax/escape errors surface exactly as in `json_in`), then a byte-span scanner finds the
//!   value. Object field lookup keeps the LAST matching member whose path resolves, and a JSON null found
//!   last overrides an earlier match (as `->>` yields SQL NULL). Negative array subscripts count from the end.
//! - Key comparison in jsonb ordering is bytewise (codepoint order), i.e. the `C` collation the engine uses.
//! - jsonb hashing reproduces PG's rotate-and-xor fold, using the crate's Jenkins hash helpers and
//!   `hash_numeric(+_extended)` through the function registry.
use super::io::format_jsonb;
use super::parse;
use super::{jsonb_key_cmp, Jsonb};
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::compare::{pg_hash_bytes, pg_hash_bytes_extended, pg_hash_uint32_extended};
use crate::context::FuncCtx;
use crate::datum::Datum;
use crate::func::{FuncImpl, ScalarFn};
use crate::numeric::io::format_numeric;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::cmp::Ordering;

/// `JB_FARRAY` / `JB_FOBJECT` container flags (PG `jsonb.h`); XORed into the jsonb hash.
const JB_FARRAY: u32 = 0x4000_0000;
const JB_FOBJECT: u32 = 0x2000_0000;
/// pg_proc OIDs of `hash_numeric` and `hash_numeric_extended`.
const HASH_NUMERIC_OID: Oid = 432;
const HASH_NUMERIC_EXTENDED_OID: Oid = 780;

// ---------------------------------------------------------------------------------------------
// Ordering and containment (public contract)
// ---------------------------------------------------------------------------------------------

/// PG jsonb btree ordering (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn compare_jsonb(a: &Jsonb, b: &Jsonb) -> Result<Ordering> {
    match (a, b) {
        (Jsonb::Object(pa), Jsonb::Object(pb)) => Ok(cmp_object(pa, pb)),
        (Jsonb::Object(_), _) => Ok(Ordering::Greater),
        (_, Jsonb::Object(_)) => Ok(Ordering::Less),
        _ => {
            // Array-like at the top level (arrays and raw scalars). PG compares the raw flag first
            // (a raw scalar sorts below a real array), then the element count (which overrides the
            // raw flag result), then the elements in order.
            let (ra, ea) = array_view(a);
            let (rb, eb) = array_view(b);
            let mut res = Ordering::Equal;
            if ra != rb {
                res = if ra { Ordering::Less } else { Ordering::Greater };
            }
            if ea.len() != eb.len() {
                res = ea.len().cmp(&eb.len());
            }
            if res != Ordering::Equal {
                return Ok(res);
            }
            for (x, y) in ea.iter().zip(eb.iter()) {
                let o = cmp_nested(x, y);
                if o != Ordering::Equal {
                    return Ok(o);
                }
            }
            Ok(Ordering::Equal)
        }
    }
}

/// `a @> b` (PG `JsonbDeepContains`).
// CONTRACT
#[allow(unused_variables)]
pub fn jsonb_contains(a: &Jsonb, b: &Jsonb) -> Result<bool> {
    if matches!(a, Jsonb::Object(_)) != matches!(b, Jsonb::Object(_)) {
        return Ok(false);
    }
    Ok(deep_contains(a, b))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

fn bad_arg() -> PgError {
    PgError::internal("json operator received an argument of an unexpected type")
}

fn jsonb_arg(d: &Datum) -> Result<&Jsonb> {
    match d {
        Datum::Jsonb(j) => Ok(j),
        _ => Err(bad_arg()),
    }
}

fn json_arg(d: &Datum) -> Result<&str> {
    match d {
        Datum::Json(s) => Ok(s.as_str()),
        _ => Err(bad_arg()),
    }
}

fn text_arg(d: &Datum) -> Result<&str> {
    match d {
        Datum::Text(s) => Ok(s.as_str()),
        _ => Err(bad_arg()),
    }
}

fn int4_arg(d: &Datum) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        _ => Err(bad_arg()),
    }
}

fn array_arg(d: &Datum) -> Result<&Array> {
    match d {
        Datum::Array(a) => Ok(&**a),
        _ => Err(bad_arg()),
    }
}

/// Elements of a text[] argument; `None` marks a NULL element.
fn text_elems(arr: &Array) -> Result<Vec<Option<&str>>> {
    arr.items
        .iter()
        .map(|d| match d {
            Datum::Null => Ok(None),
            Datum::Text(s) => Ok(Some(s.as_str())),
            _ => Err(bad_arg()),
        })
        .collect()
}

/// All path elements when none is NULL (PG returns NULL for any NULL path element).
fn non_null_elems<'a>(path: &[Option<&'a str>]) -> Option<Vec<&'a str>> {
    path.iter().copied().collect()
}

/// PG `strtoint`: optional leading C whitespace, optional sign, decimal digits covering the whole
/// string, and the value must fit in int4. Anything else is `None` (an invalid subscript).
fn strtoint(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        neg = b[i] == b'-';
        i += 1;
    }
    let digits_start = i;
    let mut val: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        val = val.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
        i += 1;
    }
    if i == digits_start || i != b.len() {
        return None;
    }
    i32::try_from(if neg { -val } else { val }).ok()
}

/// Map a PG subscript (negative counts from the end) onto `n` elements. `None` when it cannot match.
fn resolve_index(idx: i32, n: usize) -> Option<usize> {
    if idx >= 0 {
        Some(idx as usize)
    } else {
        let back = idx.unsigned_abs() as usize;
        if back > n { None } else { Some(n - back) }
    }
}

/// Elements of an array-like jsonb value. A top-level scalar is a raw scalar: a one-element array.
/// Returns `(is_raw_scalar, elements)`; objects yield no elements.
fn array_view(j: &Jsonb) -> (bool, &[Jsonb]) {
    match j {
        Jsonb::Array(items) => (false, items.as_slice()),
        Jsonb::Object(_) => (false, Default::default()),
        scalar => (true, std::slice::from_ref(scalar)),
    }
}

/// PG `JB_ROOT_COUNT`: pairs, elements, or 1 for a raw scalar.
fn jb_count(j: &Jsonb) -> usize {
    match j {
        Jsonb::Array(items) => items.len(),
        Jsonb::Object(pairs) => pairs.len(),
        _ => 1,
    }
}

fn find_key<'a>(j: &'a Jsonb, key: &str) -> Option<&'a Jsonb> {
    match j {
        Jsonb::Object(pairs) => pairs.binary_search_by(|(k, _)| jsonb_key_cmp(k, key)).ok().map(|i| &pairs[i].1),
        _ => None,
    }
}

/// `->` on jsonb with an integer subscript (object roots never match).
fn array_elem(j: &Jsonb, idx: i32) -> Option<&Jsonb> {
    if matches!(j, Jsonb::Object(_)) {
        return None;
    }
    let (_, items) = array_view(j);
    items.get(resolve_index(idx, items.len())?)
}

/// PG `JsonbValueAsText`: `None` for JSON null, the raw string, numeric output, `true`/`false`, or the
/// canonical jsonb text of a container.
fn jsonb_as_text(j: &Jsonb) -> Result<Option<String>> {
    Ok(match j {
        Jsonb::Null => None,
        Jsonb::String(s) => Some(s.clone()),
        Jsonb::Number(n) => Some(format_numeric(n)?),
        Jsonb::Bool(b) => Some(if *b { "true".to_string() } else { "false".to_string() }),
        Jsonb::Array(_) | Jsonb::Object(_) => Some(format_jsonb(j)?),
    })
}

fn text_datum_or_null(s: Option<String>) -> Datum {
    match s {
        Some(s) => Datum::Text(s),
        None => Datum::Null,
    }
}

/// PG `jsonb_get_element`: walk `path` through objects (by key) and arrays (by integer subscript).
/// A scalar mid-path yields nothing, and so does a raw scalar root with a non-empty path.
fn jsonb_path<'a>(root: &'a Jsonb, path: &[&str]) -> Option<&'a Jsonb> {
    let mut cur = root;
    for &elem in path {
        cur = match cur {
            Jsonb::Object(_) => find_key(cur, elem)?,
            Jsonb::Array(items) => {
                let idx = strtoint(elem)?;
                items.get(resolve_index(idx, items.len())?)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn cannot_delete_from_scalar() -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot delete from scalar")
}

fn wrong_subscripts() -> PgError {
    PgError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
}

fn ordering_to_i32(o: Ordering) -> i32 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// jsonb ordering and containment internals
// ---------------------------------------------------------------------------------------------

/// PG value-type rank: Null < String < Numeric < Bool < Array < Object (jbvType order).
fn rank(j: &Jsonb) -> u8 {
    match j {
        Jsonb::Null => 0,
        Jsonb::String(_) => 1,
        Jsonb::Number(_) => 2,
        Jsonb::Bool(_) => 3,
        Jsonb::Array(_) => 4,
        Jsonb::Object(_) => 5,
    }
}

/// Ordering of two nested (non-top-level) values, as `compareJsonbContainers` sees them token by token.
fn cmp_nested(a: &Jsonb, b: &Jsonb) -> Ordering {
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Jsonb::Null, Jsonb::Null) => Ordering::Equal,
        (Jsonb::String(x), Jsonb::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Jsonb::Number(x), Jsonb::Number(y)) => x.pg_cmp(y),
        (Jsonb::Bool(x), Jsonb::Bool(y)) => x.cmp(y),
        (Jsonb::Array(x), Jsonb::Array(y)) => cmp_seq(x, y),
        (Jsonb::Object(x), Jsonb::Object(y)) => cmp_object(x, y),
        _ => Ordering::Equal,
    }
}

/// Element-count first, then element-wise.
fn cmp_seq(x: &[Jsonb], y: &[Jsonb]) -> Ordering {
    if x.len() != y.len() {
        return x.len().cmp(&y.len());
    }
    for (p, q) in x.iter().zip(y.iter()) {
        let o = cmp_nested(p, q);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Pair-count first, then pair-by-pair in storage order: key (bytewise), then value.
fn cmp_object(x: &[(String, Jsonb)], y: &[(String, Jsonb)]) -> Ordering {
    if x.len() != y.len() {
        return x.len().cmp(&y.len());
    }
    for ((ka, va), (kb, vb)) in x.iter().zip(y.iter()) {
        let o = ka.as_bytes().cmp(kb.as_bytes());
        if o != Ordering::Equal {
            return o;
        }
        let o = cmp_nested(va, vb);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Equality of two scalars (`equalsJsonbScalarValue`): same type and same value.
fn scalar_eq(a: &Jsonb, b: &Jsonb) -> bool {
    match (a, b) {
        (Jsonb::Null, Jsonb::Null) => true,
        (Jsonb::String(x), Jsonb::String(y)) => x == y,
        (Jsonb::Number(x), Jsonb::Number(y)) => x.pg_cmp(y) == Ordering::Equal,
        (Jsonb::Bool(x), Jsonb::Bool(y)) => x == y,
        _ => false,
    }
}

/// One member of a template object: scalars compare by value, containers recurse.
fn contained_value(lhs: &Jsonb, tmpl: &Jsonb) -> bool {
    match (lhs.is_scalar(), tmpl.is_scalar()) {
        (true, true) => scalar_eq(lhs, tmpl),
        (false, false) => deep_contains(lhs, tmpl),
        _ => false,
    }
}

/// PG `JsonbDeepContains` (both sides already known to agree on object-ness at the top level).
fn deep_contains(val: &Jsonb, tmpl: &Jsonb) -> bool {
    match (val, tmpl) {
        (Jsonb::Object(_), Jsonb::Object(tp)) => {
            if jb_count(val) < tp.len() {
                return false;
            }
            tp.iter().all(|(k, tv)| match find_key(val, k) {
                Some(lv) => contained_value(lv, tv),
                None => false,
            })
        }
        (Jsonb::Object(_), _) | (_, Jsonb::Object(_)) => false,
        _ => {
            let (vraw, velems) = array_view(val);
            let (traw, telems) = array_view(tmpl);
            if vraw && !traw {
                return false;
            }
            telems.iter().all(|t| {
                if t.is_scalar() {
                    velems.iter().any(|v| scalar_eq(v, t))
                } else {
                    velems.iter().any(|v| !v.is_scalar() && deep_contains(v, t))
                }
            })
        }
    }
}

/// `jsonb || jsonb` (PG `jsonb_concat` / `IteratorConcat`).
fn concat(a: &Jsonb, b: &Jsonb) -> Jsonb {
    match (a, b) {
        (Jsonb::Object(pa), Jsonb::Object(pb)) => Jsonb::Object(merge_objects(pa, pb)),
        (Jsonb::Object(_), _) => {
            let mut out = vec![a.clone()];
            out.extend(array_view(b).1.iter().cloned());
            Jsonb::Array(out)
        }
        (_, Jsonb::Object(_)) => {
            let mut out: Vec<Jsonb> = array_view(a).1.to_vec();
            out.push(b.clone());
            Jsonb::Array(out)
        }
        _ => {
            let mut out: Vec<Jsonb> = array_view(a).1.to_vec();
            out.extend(array_view(b).1.iter().cloned());
            Jsonb::Array(out)
        }
    }
}

/// Merge two storage-ordered pair lists; on equal keys the right-hand value wins.
fn merge_objects(pa: &[(String, Jsonb)], pb: &[(String, Jsonb)]) -> Vec<(String, Jsonb)> {
    let mut out = Vec::with_capacity(pa.len() + pb.len());
    let (mut i, mut j) = (0, 0);
    while i < pa.len() && j < pb.len() {
        match jsonb_key_cmp(&pa[i].0, &pb[j].0) {
            Ordering::Less => {
                out.push(pa[i].clone());
                i += 1;
            }
            Ordering::Greater => {
                out.push(pb[j].clone());
                j += 1;
            }
            Ordering::Equal => {
                out.push(pb[j].clone());
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&pa[i..]);
    out.extend_from_slice(&pb[j..]);
    out
}

/// Does a top-level key or string element equal `key` (PG `findJsonbValueFromContainer` with
/// JB_FOBJECT | JB_FARRAY)?
fn exists_key(j: &Jsonb, key: &str) -> bool {
    fn is_str_eq(v: &Jsonb, key: &str) -> bool {
        matches!(v, Jsonb::String(s) if s.as_str() == key)
    }
    match j {
        Jsonb::Object(_) => find_key(j, key).is_some(),
        Jsonb::Array(items) => items.iter().any(|v| is_str_eq(v, key)),
        other => is_str_eq(other, key),
    }
}

/// Remove every top-level object pair (by key) or string array element matching `pred`.
/// Scalars are rejected by the callers before this runs.
fn delete_where<F: Fn(&str) -> bool>(j: &Jsonb, pred: F) -> Jsonb {
    match j {
        Jsonb::Object(pairs) => Jsonb::Object(pairs.iter().filter(|(k, _)| !pred(k.as_str())).cloned().collect()),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .iter()
                .filter(|v| !matches!(v, Jsonb::String(s) if pred(s.as_str())))
                .cloned()
                .collect(),
        ),
        other => other.clone(),
    }
}

/// PG `setPath` with `JB_PATH_DELETE`: remove the element addressed by `path` (`#-`).
/// The NULL-element check happens on entry to every level, matching PG's `setPath`.
fn delete_path_rec(node: &Jsonb, path: &[Option<&str>], level: usize) -> Result<Jsonb> {
    let Some(elem) = path[level] else {
        return Err(PgError::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            format!("path element at position {} is null", level + 1),
        ));
    };
    let last = level + 1 == path.len();
    match node {
        Jsonb::Array(items) => {
            let idx = strtoint(elem).ok_or_else(|| {
                PgError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!("path element at position {} is not an integer: \"{}\"", level + 1, elem),
                )
            })?;
            let Some(target) = resolve_index(idx, items.len()).filter(|t| *t < items.len()) else {
                return Ok(node.clone());
            };
            let mut out = Vec::with_capacity(items.len());
            for (i, v) in items.iter().enumerate() {
                if i != target {
                    out.push(v.clone());
                } else if !last {
                    out.push(delete_path_rec(v, path, level + 1)?);
                }
            }
            Ok(Jsonb::Array(out))
        }
        Jsonb::Object(pairs) => {
            let Ok(pos) = pairs.binary_search_by(|(k, _)| jsonb_key_cmp(k, elem)) else {
                return Ok(node.clone());
            };
            let mut out = pairs.clone();
            if last {
                out.remove(pos);
            } else {
                out[pos].1 = delete_path_rec(&pairs[pos].1, path, level + 1)?;
            }
            Ok(Jsonb::Object(out))
        }
        _ => Ok(node.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// Hashing (PG jsonb_hash / jsonb_hash_extended)
// ---------------------------------------------------------------------------------------------

/// PG `ROTATE_HIGH_AND_LOW_32BITS`: rotate each 32-bit half left by one.
fn rotate_high_and_low(v: u64) -> u64 {
    let lo = (v as u32).rotate_left(1) as u64;
    let hi = ((v >> 32) as u32).rotate_left(1) as u64;
    (hi << 32) | lo
}

/// Per-scalar hash contribution (`JsonbHashScalarValue`).
fn hash_scalar32(v: &Jsonb, ctx: &FuncCtx) -> Result<u32> {
    Ok(match v {
        Jsonb::Null => 0x01,
        Jsonb::String(s) => pg_hash_bytes(s.as_bytes())?,
        Jsonb::Number(n) => match crate::func::call(HASH_NUMERIC_OID, &[Datum::Numeric(n.clone())], ctx)? {
            Datum::Int4(h) => h as u32,
            _ => return Err(bad_arg()),
        },
        Jsonb::Bool(b) => {
            if *b {
                0x02
            } else {
                0x04
            }
        }
        Jsonb::Array(_) | Jsonb::Object(_) => return Err(bad_arg()),
    })
}

/// Per-scalar hash contribution (`JsonbHashScalarValueExtended`).
fn hash_scalar64(v: &Jsonb, seed: u64, ctx: &FuncCtx) -> Result<u64> {
    Ok(match v {
        Jsonb::Null => seed.wrapping_add(0x01),
        Jsonb::String(s) => pg_hash_bytes_extended(s.as_bytes(), seed)?,
        Jsonb::Number(n) => match crate::func::call(
            HASH_NUMERIC_EXTENDED_OID,
            &[Datum::Numeric(n.clone()), Datum::Int8(seed as i64)],
            ctx,
        )? {
            Datum::Int8(h) => h as u64,
            _ => return Err(bad_arg()),
        },
        Jsonb::Bool(b) => {
            if seed != 0 {
                pg_hash_uint32_extended(*b as u32, seed)?
            } else if *b {
                0x02
            } else {
                0x04
            }
        }
        Jsonb::Array(_) | Jsonb::Object(_) => return Err(bad_arg()),
    })
}

/// Fold a value into the running hash, visiting it the way PG's iterator does: a container XORs its
/// flag, each object key hashes as a string before its value, and every scalar rotates and XORs.
fn hash_walk32(v: &Jsonb, h: &mut u32, ctx: &FuncCtx) -> Result<()> {
    match v {
        Jsonb::Array(items) => {
            *h ^= JB_FARRAY;
            for e in items {
                hash_walk32(e, h, ctx)?;
            }
        }
        Jsonb::Object(pairs) => {
            *h ^= JB_FOBJECT;
            for (k, val) in pairs {
                let key_hash = pg_hash_bytes(k.as_bytes())?;
                *h = h.rotate_left(1) ^ key_hash;
                hash_walk32(val, h, ctx)?;
            }
        }
        scalar => {
            let tmp = hash_scalar32(scalar, ctx)?;
            *h = h.rotate_left(1) ^ tmp;
        }
    }
    Ok(())
}

fn hash_walk64(v: &Jsonb, seed: u64, h: &mut u64, ctx: &FuncCtx) -> Result<()> {
    match v {
        Jsonb::Array(items) => {
            *h ^= ((JB_FARRAY as u64) << 32) | JB_FARRAY as u64;
            for e in items {
                hash_walk64(e, seed, h, ctx)?;
            }
        }
        Jsonb::Object(pairs) => {
            *h ^= ((JB_FOBJECT as u64) << 32) | JB_FOBJECT as u64;
            for (k, val) in pairs {
                let key_hash = pg_hash_bytes_extended(k.as_bytes(), seed)?;
                *h = rotate_high_and_low(*h) ^ key_hash;
                hash_walk64(val, seed, h, ctx)?;
            }
        }
        scalar => {
            let tmp = hash_scalar64(scalar, seed, ctx)?;
            *h = rotate_high_and_low(*h) ^ tmp;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// json (text) navigation
// ---------------------------------------------------------------------------------------------

/// One step of a json navigation: `name` matches object members, `index` matches array elements.
#[derive(Clone, Copy)]
struct JStep<'a> {
    name: Option<&'a str>,
    index: Option<i32>,
}

fn malformed_json() -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type json")
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `b[i]` is the opening quote; returns the index just past the closing quote.
fn string_end(b: &[u8], i: usize) -> Result<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Ok(j + 1),
            _ => j += 1,
        }
    }
    Err(malformed_json())
}

/// Index just past the value starting at `i` (which must not be whitespace).
fn value_end(b: &[u8], i: usize) -> Result<usize> {
    match b.get(i) {
        None => Err(malformed_json()),
        Some(b'"') => string_end(b, i),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1).ok_or_else(malformed_json)?;
                        if depth == 0 {
                            return Ok(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            Err(malformed_json())
        }
        Some(_) => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b']' | b'}' | b':' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j == i { Err(malformed_json()) } else { Ok(j) }
        }
    }
}

/// Number of elements of the array starting at `start`.
fn count_elements(b: &[u8], start: usize) -> Result<usize> {
    let mut pos = skip_ws(b, start + 1);
    if b.get(pos) == Some(&b']') {
        return Ok(0);
    }
    let mut count = 0usize;
    loop {
        let ve = value_end(b, pos)?;
        count += 1;
        let next = skip_ws(b, ve);
        match b.get(next) {
            Some(b',') => pos = skip_ws(b, next + 1),
            Some(b']') => return Ok(count),
            _ => return Err(malformed_json()),
        }
    }
}

/// Decode a json string literal (quotes included), mirroring PG's unescaping.
fn decode_json_string(raw: &str) -> Result<String> {
    let inner = raw.get(1..raw.len().saturating_sub(1)).ok_or_else(malformed_json)?;
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next().ok_or_else(malformed_json)? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hi = read_hex4(&mut it)?;
                let cp = if (0xD800..0xDC00).contains(&hi) {
                    if it.next() != Some('\\') || it.next() != Some('u') {
                        return Err(malformed_json());
                    }
                    let lo = read_hex4(&mut it)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err(malformed_json());
                    }
                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else if (0xDC00..0xE000).contains(&hi) {
                    return Err(malformed_json());
                } else {
                    hi
                };
                if cp == 0 {
                    return Err(PgError::new(SqlState::UNTRANSLATABLE_CHARACTER, "unsupported Unicode escape sequence")
                        .with_detail("\\u0000 cannot be converted to text."));
                }
                out.push(char::from_u32(cp).ok_or_else(malformed_json)?);
            }
            _ => return Err(malformed_json()),
        }
    }
    Ok(out)
}

fn read_hex4(it: &mut std::str::Chars<'_>) -> Result<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        let d = it.next().and_then(|c| c.to_digit(16)).ok_or_else(malformed_json)?;
        v = v * 16 + d;
    }
    Ok(v)
}

/// Does the quoted key literal `raw` name `name` after unescaping?
fn key_matches(raw: &str, name: &str) -> Result<bool> {
    let inner = raw.get(1..raw.len().saturating_sub(1)).ok_or_else(malformed_json)?;
    if inner.contains('\\') {
        Ok(decode_json_string(raw)? == name)
    } else {
        Ok(inner == name)
    }
}

/// PG `get_worker` navigation. Returns the byte span of the selected value. For a path with several
/// steps, the last member/element that resolves wins, as in PG's field callbacks.
fn json_lookup(s: &str, start: usize, steps: &[JStep]) -> Result<Option<(usize, usize)>> {
    let b = s.as_bytes();
    if steps.is_empty() {
        return Ok(Some((start, value_end(b, start)?)));
    }
    let step = steps[0];
    match b.get(start) {
        Some(b'{') => {
            let Some(name) = step.name else {
                return Ok(None);
            };
            let mut best = None;
            let mut pos = skip_ws(b, start + 1);
            if b.get(pos) == Some(&b'}') {
                return Ok(None);
            }
            loop {
                if b.get(pos) != Some(&b'"') {
                    return Err(malformed_json());
                }
                let key_end = string_end(b, pos)?;
                let colon = skip_ws(b, key_end);
                if b.get(colon) != Some(&b':') {
                    return Err(malformed_json());
                }
                let vs = skip_ws(b, colon + 1);
                let ve = value_end(b, vs)?;
                let raw_key = s.get(pos..key_end).ok_or_else(malformed_json)?;
                if key_matches(raw_key, name)? {
                    if steps.len() == 1 {
                        best = Some((vs, ve));
                    } else if let Some(found) = json_lookup(s, vs, &steps[1..])? {
                        best = Some(found);
                    }
                }
                let next = skip_ws(b, ve);
                match b.get(next) {
                    Some(b',') => pos = skip_ws(b, next + 1),
                    Some(b'}') => return Ok(best),
                    _ => return Err(malformed_json()),
                }
            }
        }
        Some(b'[') => {
            let Some(idx) = step.index else {
                return Ok(None);
            };
            if idx == i32::MIN {
                return Ok(None);
            }
            let target = if idx < 0 {
                let n = count_elements(b, start)?;
                match resolve_index(idx, n) {
                    Some(t) => t,
                    None => return Ok(None),
                }
            } else {
                idx as usize
            };
            let mut pos = skip_ws(b, start + 1);
            if b.get(pos) == Some(&b']') {
                return Ok(None);
            }
            let mut k = 0usize;
            loop {
                let ve = value_end(b, pos)?;
                if k == target {
                    return if steps.len() == 1 {
                        Ok(Some((pos, ve)))
                    } else {
                        json_lookup(s, pos, &steps[1..])
                    };
                }
                k += 1;
                let next = skip_ws(b, ve);
                match b.get(next) {
                    Some(b',') => pos = skip_ws(b, next + 1),
                    Some(b']') => return Ok(None),
                    _ => return Err(malformed_json()),
                }
            }
        }
        _ => Ok(None),
    }
}

/// Run a json navigation over a whole document. `as_text` selects the `->>` / `#>>` output rules: a
/// JSON string is unescaped and a JSON null yields `None` (SQL NULL); anything else is its raw text.
fn json_get(s: &str, steps: &[JStep], as_text: bool) -> Result<Option<String>> {
    // PG parses the whole document in get_worker (with escape checking), so every syntax error
    // surfaces here regardless of which part the path selects.
    parse::parse_json_value(s)?;
    let b = s.as_bytes();
    let start = skip_ws(b, 0);
    let (vs, ve) = if steps.is_empty() {
        let end = value_end(b, start)?;
        match b.get(start) {
            Some(b'{') | Some(b'[') => (start, end),
            _ => (0, end),
        }
    } else {
        match json_lookup(s, start, steps)? {
            Some(span) => span,
            None => return Ok(None),
        }
    };
    let raw = s.get(vs..ve).ok_or_else(malformed_json)?;
    if !as_text {
        return Ok(Some(raw.to_string()));
    }
    let trimmed = raw.trim_start_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'));
    match trimmed.as_bytes().first() {
        Some(b'"') => Ok(Some(decode_json_string(trimmed)?)),
        Some(b'n') => Ok(None),
        _ => Ok(Some(raw.to_string())),
    }
}

// ---------------------------------------------------------------------------------------------
// Scalar function implementations
// ---------------------------------------------------------------------------------------------

fn f_json_object_field(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let steps = [JStep { name: Some(text_arg(&args[1])?), index: None }];
    Ok(match json_get(s, &steps, false)? {
        Some(t) => Datum::Json(t),
        None => Datum::Null,
    })
}

fn f_json_object_field_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let steps = [JStep { name: Some(text_arg(&args[1])?), index: None }];
    Ok(text_datum_or_null(json_get(s, &steps, true)?))
}

fn f_json_array_element(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let steps = [JStep { name: None, index: Some(int4_arg(&args[1])?) }];
    Ok(match json_get(s, &steps, false)? {
        Some(t) => Datum::Json(t),
        None => Datum::Null,
    })
}

fn f_json_array_element_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let steps = [JStep { name: None, index: Some(int4_arg(&args[1])?) }];
    Ok(text_datum_or_null(json_get(s, &steps, true)?))
}

fn json_path_steps<'a>(names: &[&'a str]) -> Vec<JStep<'a>> {
    names.iter().map(|&n| JStep { name: Some(n), index: strtoint(n) }).collect()
}

fn f_json_extract_path(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let path = text_elems(array_arg(&args[1])?)?;
    let Some(names) = non_null_elems(&path) else {
        return Ok(Datum::Null);
    };
    let steps = json_path_steps(&names);
    Ok(match json_get(s, &steps, false)? {
        Some(t) => Datum::Json(t),
        None => Datum::Null,
    })
}

fn f_json_extract_path_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = json_arg(&args[0])?;
    let path = text_elems(array_arg(&args[1])?)?;
    let Some(names) = non_null_elems(&path) else {
        return Ok(Datum::Null);
    };
    let steps = json_path_steps(&names);
    Ok(text_datum_or_null(json_get(s, &steps, true)?))
}

fn f_jsonb_object_field(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    Ok(match find_key(j, text_arg(&args[1])?) {
        Some(v) => Datum::Jsonb(v.clone()),
        None => Datum::Null,
    })
}

fn f_jsonb_object_field_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    match find_key(j, text_arg(&args[1])?) {
        Some(v) => Ok(text_datum_or_null(jsonb_as_text(v)?)),
        None => Ok(Datum::Null),
    }
}

fn f_jsonb_array_element(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    Ok(match array_elem(j, int4_arg(&args[1])?) {
        Some(v) => Datum::Jsonb(v.clone()),
        None => Datum::Null,
    })
}

fn f_jsonb_array_element_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    match array_elem(j, int4_arg(&args[1])?) {
        Some(v) => Ok(text_datum_or_null(jsonb_as_text(v)?)),
        None => Ok(Datum::Null),
    }
}

fn f_jsonb_extract_path(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let path = text_elems(array_arg(&args[1])?)?;
    let Some(names) = non_null_elems(&path) else {
        return Ok(Datum::Null);
    };
    Ok(match jsonb_path(j, &names) {
        Some(v) => Datum::Jsonb(v.clone()),
        None => Datum::Null,
    })
}

fn f_jsonb_extract_path_text(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let path = text_elems(array_arg(&args[1])?)?;
    let Some(names) = non_null_elems(&path) else {
        return Ok(Datum::Null);
    };
    match jsonb_path(j, &names) {
        Some(v) => Ok(text_datum_or_null(jsonb_as_text(v)?)),
        None => Ok(Datum::Null),
    }
}

fn f_jsonb_exists(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    Ok(Datum::Bool(exists_key(j, text_arg(&args[1])?)))
}

fn f_jsonb_exists_any(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let keys = text_elems(array_arg(&args[1])?)?;
    Ok(Datum::Bool(keys.into_iter().flatten().any(|k| exists_key(j, k))))
}

fn f_jsonb_exists_all(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let keys = text_elems(array_arg(&args[1])?)?;
    Ok(Datum::Bool(keys.into_iter().flatten().all(|k| exists_key(j, k))))
}

fn f_jsonb_contains(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(jsonb_contains(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)?))
}

fn f_jsonb_contained(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(jsonb_contains(jsonb_arg(&args[1])?, jsonb_arg(&args[0])?)?))
}

fn f_jsonb_concat(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Jsonb(concat(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)))
}

/// `jsonb - text`.
fn f_jsonb_delete_key(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let key = text_arg(&args[1])?;
    if j.is_scalar() {
        return Err(cannot_delete_from_scalar());
    }
    if jb_count(j) == 0 {
        return Ok(Datum::Jsonb(j.clone()));
    }
    Ok(Datum::Jsonb(delete_where(j, |k| k == key)))
}

/// `jsonb - int`.
fn f_jsonb_delete_idx(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let idx = int4_arg(&args[1])?;
    if j.is_scalar() {
        return Err(cannot_delete_from_scalar());
    }
    let Jsonb::Array(items) = j else {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot delete from object using integer index",
        ));
    };
    let n = items.len();
    let Some(target) = resolve_index(idx, n).filter(|t| *t < n) else {
        return Ok(Datum::Jsonb(j.clone()));
    };
    let mut out = items.clone();
    out.remove(target);
    Ok(Datum::Jsonb(Jsonb::Array(out)))
}

/// `jsonb - text[]`.
fn f_jsonb_delete_array(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let arr = array_arg(&args[1])?;
    if arr.ndims() > 1 {
        return Err(wrong_subscripts());
    }
    if j.is_scalar() {
        return Err(cannot_delete_from_scalar());
    }
    if jb_count(j) == 0 {
        return Ok(Datum::Jsonb(j.clone()));
    }
    let keys: Vec<&str> = text_elems(arr)?.into_iter().flatten().collect();
    if keys.is_empty() {
        return Ok(Datum::Jsonb(j.clone()));
    }
    Ok(Datum::Jsonb(delete_where(j, |k| keys.iter().any(|key| *key == k))))
}

/// `jsonb #- text[]`.
fn f_jsonb_delete_path(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let arr = array_arg(&args[1])?;
    if arr.ndims() > 1 {
        return Err(wrong_subscripts());
    }
    if j.is_scalar() {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "cannot delete path in scalar"));
    }
    if jb_count(j) == 0 {
        return Ok(Datum::Jsonb(j.clone()));
    }
    let path = text_elems(arr)?;
    if path.is_empty() {
        return Ok(Datum::Jsonb(j.clone()));
    }
    Ok(Datum::Jsonb(delete_path_rec(j, &path, 0)?))
}

fn f_jsonb_eq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? == Ordering::Equal))
}

fn f_jsonb_ne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? != Ordering::Equal))
}

fn f_jsonb_lt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? == Ordering::Less))
}

fn f_jsonb_le(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? != Ordering::Greater))
}

fn f_jsonb_gt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? == Ordering::Greater))
}

fn f_jsonb_ge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)? != Ordering::Less))
}

fn f_jsonb_cmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let o = compare_jsonb(jsonb_arg(&args[0])?, jsonb_arg(&args[1])?)?;
    Ok(Datum::Int4(ordering_to_i32(o)))
}

fn f_jsonb_hash(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    if jb_count(j) == 0 {
        return Ok(Datum::Int4(0));
    }
    let mut h: u32 = 0;
    if j.is_scalar() {
        // A raw scalar iterates as a one-element array.
        h ^= JB_FARRAY;
    }
    hash_walk32(j, &mut h, ctx)?;
    Ok(Datum::Int4(h as i32))
}

fn f_jsonb_hash_extended(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let j = jsonb_arg(&args[0])?;
    let seed = match &args[1] {
        Datum::Int8(v) => *v as u64,
        _ => return Err(bad_arg()),
    };
    if jb_count(j) == 0 {
        return Ok(Datum::Int8(seed as i64));
    }
    let mut h: u64 = 0;
    if j.is_scalar() {
        h ^= ((JB_FARRAY as u64) << 32) | JB_FARRAY as u64;
    }
    hash_walk64(j, seed, &mut h, ctx)?;
    Ok(Datum::Int8(h as i64))
}

// ---------------------------------------------------------------------------------------------
// Registry hook
// ---------------------------------------------------------------------------------------------

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "json_object_field" => f_json_object_field,
        "json_object_field_text" => f_json_object_field_text,
        "json_array_element" => f_json_array_element,
        "json_array_element_text" => f_json_array_element_text,
        "json_extract_path" => f_json_extract_path,
        "json_extract_path_text" => f_json_extract_path_text,
        "jsonb_object_field" => f_jsonb_object_field,
        "jsonb_object_field_text" => f_jsonb_object_field_text,
        "jsonb_array_element" => f_jsonb_array_element,
        "jsonb_array_element_text" => f_jsonb_array_element_text,
        "jsonb_extract_path" => f_jsonb_extract_path,
        "jsonb_extract_path_text" => f_jsonb_extract_path_text,
        "jsonb_exists" => f_jsonb_exists,
        "jsonb_exists_any" => f_jsonb_exists_any,
        "jsonb_exists_all" => f_jsonb_exists_all,
        "jsonb_contains" => f_jsonb_contains,
        "jsonb_contained" => f_jsonb_contained,
        "jsonb_concat" => f_jsonb_concat,
        "jsonb_delete" => f_jsonb_delete_key,
        "jsonb_delete_idx" => f_jsonb_delete_idx,
        "jsonb_delete_array" => f_jsonb_delete_array,
        "jsonb_delete_path" => f_jsonb_delete_path,
        "jsonb_eq" => f_jsonb_eq,
        "jsonb_ne" => f_jsonb_ne,
        "jsonb_lt" => f_jsonb_lt,
        "jsonb_le" => f_jsonb_le,
        "jsonb_gt" => f_jsonb_gt,
        "jsonb_ge" => f_jsonb_ge,
        "jsonb_cmp" => f_jsonb_cmp,
        "jsonb_hash" => f_jsonb_hash,
        "jsonb_hash_extended" => f_jsonb_hash_extended,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a JsonOps-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-34", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession, TypeEnv};
    use crate::datum::TypeId;
    use crate::json::io::format_jsonb;
    use crate::json::parse::parse_jsonb;
    use crate::registry;

    fn jb(s: &str) -> Datum {
        Datum::Jsonb(parse_jsonb(s).unwrap())
    }

    fn txt(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    fn tarr(items: &[&str]) -> Datum {
        let elems = items.iter().map(|s| Datum::Text(s.to_string())).collect();
        Datum::Array(Box::new(Array::from_vec(crate::oids::TEXT, elems)))
    }

    fn call(oid: Oid, args: &[Datum]) -> Result<Datum> {
        let env: &dyn TypeEnv = registry::builtin_env();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &io,
            session: &session,
            arg_types: &[],
            result_type: TypeId::UNKNOWN,
            collation: 0,
        };
        crate::func::call(oid, args, &ctx)
    }

    /// Render a result datum as text for assertions.
    fn out(d: Datum) -> String {
        match d {
            Datum::Null => "NULL".to_string(),
            Datum::Jsonb(j) => format_jsonb(&j).unwrap(),
            Datum::Json(s) | Datum::Text(s) => s,
            Datum::Bool(b) => b.to_string(),
            Datum::Int4(v) => v.to_string(),
            Datum::Int8(v) => v.to_string(),
            other => format!("{other:?}"),
        }
    }

    fn cmp_str(a: &str, b: &str) -> Ordering {
        compare_jsonb(&parse_jsonb(a).unwrap(), &parse_jsonb(b).unwrap()).unwrap()
    }

    #[test]
    fn subscripts_and_paths() {
        assert_eq!(out(call(3215, &[jb("[1,2,3]"), Datum::Int4(-1)]).unwrap()), "3");
        assert_eq!(out(call(3216, &[jb("[1,2,3]"), Datum::Int4(-4)]).unwrap()), "NULL");
        assert_eq!(out(call(3215, &[jb("3"), Datum::Int4(0)]).unwrap()), "3");
        assert_eq!(out(call(3217, &[jb("{\"a\":[1,2]}"), tarr(&["a", "0"])]).unwrap()), "1");
        assert_eq!(out(call(3940, &[jb("{\"a\":[1,2]}"), tarr(&["a", "0"])]).unwrap()), "1");
        assert_eq!(out(call(3214, &[jb("{\"a\":null}"), txt("a")]).unwrap()), "NULL");
        assert_eq!(out(call(3478, &[jb("{\"a\":null}"), txt("a")]).unwrap()), "null");
    }

    #[test]
    fn json_text_navigation() {
        let doc = Datum::Json("{\"a\": [1, 2]}".to_string());
        assert_eq!(out(call(3947, &[doc.clone(), txt("a")]).unwrap()), "[1, 2]");
        assert_eq!(out(call(3950, &[Datum::Json("[1, \"x\\n\"]".to_string()), Datum::Int4(1)]).unwrap()), "x\n");
        assert_eq!(out(call(3953, &[doc, tarr(&["a", "1"])]).unwrap()), "2");
    }

    #[test]
    fn concat_and_delete() {
        assert_eq!(out(call(3301, &[jb("[1]"), jb("2")]).unwrap()), "[1, 2]");
        assert_eq!(out(call(3301, &[jb("{\"a\":1}"), jb("{\"a\":2,\"b\":3}")]).unwrap()), "{\"a\": 2, \"b\": 3}");
        assert_eq!(out(call(3302, &[jb("{\"a\":1,\"b\":2}"), txt("a")]).unwrap()), "{\"b\": 2}");
        assert_eq!(out(call(3303, &[jb("[1,2,3]"), Datum::Int4(-1)]).unwrap()), "[1, 2]");
        assert_eq!(out(call(3304, &[jb("{\"a\":{\"b\":1}}"), tarr(&["a", "b"])]).unwrap()), "{\"a\": {}}");
        assert!(call(3303, &[jb("{\"a\":1}"), Datum::Int4(0)]).is_err());
    }

    #[test]
    fn ordering_quirks() {
        assert_eq!(cmp_str("[]", "null"), Ordering::Less);
        assert_eq!(cmp_str("[1,2]", "[3]"), Ordering::Greater);
        assert_eq!(cmp_str("{\"b\":1}", "{\"aa\":1}"), Ordering::Greater);
        assert_eq!(cmp_str("false", "true"), Ordering::Less);
    }

    #[test]
    fn containment_and_exists() {
        assert!(jsonb_contains(&parse_jsonb("[1,2]").unwrap(), &parse_jsonb("1").unwrap()).unwrap());
        assert!(!jsonb_contains(&parse_jsonb("1").unwrap(), &parse_jsonb("[1]").unwrap()).unwrap());
        assert!(jsonb_contains(
            &parse_jsonb("{\"a\":{\"b\":1,\"c\":2}}").unwrap(),
            &parse_jsonb("{\"a\":{\"b\":1}}").unwrap()
        )
        .unwrap());
        assert_eq!(out(call(4047, &[jb("\"foo\""), txt("foo")]).unwrap()), "true");
        assert_eq!(out(call(4048, &[jb("{\"a\":1}"), tarr(&["x", "a"])]).unwrap()), "true");
        assert_eq!(out(call(4049, &[jb("{\"a\":1}"), tarr(&["a", "x"])]).unwrap()), "false");
    }

    #[test]
    fn hash_of_empty_container_is_zero() {
        assert_eq!(out(call(4045, &[jb("{}")]).unwrap()), "0");
        assert_eq!(out(call(3416, &[jb("[]"), Datum::Int8(7)]).unwrap()), "7");
    }
}
