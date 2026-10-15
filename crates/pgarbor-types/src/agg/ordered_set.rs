//! Ordered-set and hypothetical-set aggregates: percentile_cont, percentile_disc, mode, and
//! hypothetical rank/dense_rank/percent_rank/cume_dist (TYPE-12). Lease TYP-48.
//!
//! PG18 reference: `src/backend/utils/adt/orderedsetaggs.c`.
//! Contract with the executor: rows arrive ALREADY SORTED by the WITHIN GROUP clause (NULL sort keys
//! included per the clause's NULLS ordering); `spec.direct_args` holds the evaluated direct arguments.
//! - `percentile_disc(fraction) WITHIN GROUP (ORDER BY x)`: first value whose cumulative position >=
//!   fraction (NULL sort values ignored); `percentile_cont(fraction)` over float8 or interval: linear
//!   interpolation; array-of-fractions variants return arrays. fraction outside [0,1] → 22003
//!   `percentile value 1.5 is not between 0 and 1`. Oracle (1,2,4): cont(0.5) = 2, disc(0.5) = 2.
//! - `mode()`: most frequent value, ties → the first in sort order.
//! - hypothetical `rank(args) WITHIN GROUP (ORDER BY ...)` etc.: rank the direct args among the rows.
//!
//! Implementation notes:
//! - Percentiles buffer the non-NULL sort values (PG's tuplesort does the same) and index them.
//! - `mode()` is streaming: sorted input means equal values form runs.
//! - Hypothetical-set aggregates keep every row (NULLs included, as PG does). The sort direction and
//!   NULLS placement of each WITHIN GROUP column are not part of `AggSpec`, so they are inferred from
//!   the sorted input (first informative adjacent pair). A comparison that needs a direction the input
//!   cannot reveal is reported as unimplemented rather than guessed.
use super::{AggSpec, AggState};
use crate::array::Array;
use crate::catalog_data::BuiltinAggregate;
use crate::compare;
use crate::context::{FuncCtx, TypeEnv};
use crate::datetime::interval as ivl;
use crate::datum::{type_mismatch, Datum, Interval, TypeId};
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

const LEASE: &str = "TYP-48";

/// Instantiate an ordered-set / hypothetical-set aggregate.
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    let _ = ctx;
    match agg.oid {
        // percentile_disc(float8) / percentile_cont(float8) / percentile_cont(interval), scalar fraction
        // and the float8[] / interval[] array-of-fractions variants.
        3972 | 3974 | 3976 | 3978 | 3980 | 3982 => {
            let (kind, shape) = match agg.oid {
                3972 => (PctKind::Disc, Shape::Scalar),
                3974 => (PctKind::ContFloat, Shape::Scalar),
                3976 => (PctKind::ContInterval, Shape::Scalar),
                3978 => (PctKind::Disc, Shape::Array),
                3980 => (PctKind::ContFloat, Shape::Array),
                _ => (PctKind::ContInterval, Shape::Array),
            };
            let sort_ty = single_sort_type(agg, spec)?;
            if spec.direct_args.len() != 1 {
                return Err(arity_error(agg, "direct-argument count", spec.direct_args.len()));
            }
            Ok(Box::new(PercentileState {
                kind,
                shape,
                sort_ty,
                direct: spec.direct_args[0].clone(),
                values: Vec::new(),
            }))
        }
        3984 => {
            let sort_ty = single_sort_type(agg, spec)?;
            Ok(Box::new(ModeState {
                sort_ty,
                run_val: None,
                run_len: 0,
                run_is_mode: false,
                mode_val: None,
                mode_freq: 0,
            }))
        }
        3986 | 3988 | 3990 | 3992 => {
            let kind = match agg.oid {
                3986 => HypoKind::Rank,
                3988 => HypoKind::PercentRank,
                3990 => HypoKind::CumeDist,
                _ => HypoKind::DenseRank,
            };
            if spec.arg_types.is_empty() {
                return Err(arity_error(agg, "sort-column count", 0));
            }
            if spec.direct_args.len() != spec.arg_types.len() {
                return Err(arity_error(agg, "hypothetical-row width", spec.direct_args.len()));
            }
            Ok(Box::new(HypoState {
                kind,
                types: spec.arg_types.clone(),
                hypo: spec.direct_args.clone(),
                rows: Vec::new(),
            }))
        }
        _ => Err(PgError::unimplemented(LEASE, format!("aggregate {}", agg.name))),
    }
}

fn arity_error(agg: &BuiltinAggregate, what: &str, got: usize) -> PgError {
    PgError::internal(format!("aggregate {} received an unexpected {what} ({got})", agg.name))
}

fn single_sort_type(agg: &BuiltinAggregate, spec: &AggSpec) -> Result<TypeId> {
    if spec.arg_types.len() != 1 {
        return Err(arity_error(agg, "sort-column count", spec.arg_types.len()));
    }
    Ok(spec.arg_types[0])
}

fn undeterminable_order() -> PgError {
    PgError::unimplemented(
        LEASE,
        "hypothetical-set aggregate over input whose WITHIN GROUP sort direction cannot be determined",
    )
}

// ---------------------------------------------------------------------------------------------
// Percentiles
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PctKind {
    Disc,
    ContFloat,
    ContInterval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Scalar,
    Array,
}

struct PercentileState {
    kind: PctKind,
    shape: Shape,
    sort_ty: TypeId,
    direct: Datum,
    /// Non-NULL sort values in sorted order.
    values: Vec<Datum>,
}

impl PercentileState {
    /// Value for one validated fraction, given at least one stored value.
    fn value_at(&self, p: f64) -> Result<Datum> {
        let n = self.values.len();
        match self.kind {
            PctKind::Disc => Ok(self.values[disc_index(p, n)].clone()),
            PctKind::ContFloat => {
                let (lo, hi, prop) = cont_position(p, n);
                let a = f8(&self.values[lo])?;
                if lo == hi {
                    return Ok(Datum::Float8(a));
                }
                let b = f8(&self.values[hi])?;
                Ok(Datum::Float8(lerp_f8(a, b, prop)))
            }
            PctKind::ContInterval => {
                let (lo, hi, prop) = cont_position(p, n);
                let a = interval_of(&self.values[lo])?;
                if lo == hi {
                    return Ok(Datum::Interval(a));
                }
                let b = interval_of(&self.values[hi])?;
                Ok(Datum::Interval(lerp_interval(&a, &b, prop)?))
            }
        }
    }
}

impl AggState for PercentileState {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let v = args
            .first()
            .ok_or_else(|| PgError::internal("percentile aggregate received no sort value"))?;
        if !v.is_null() {
            self.values.push(v.clone());
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        match self.shape {
            Shape::Scalar => {
                // PG order: NULL fraction → NULL; range check; no rows → NULL.
                let Some(p) = fraction_of(&self.direct)? else {
                    return Ok(Datum::Null);
                };
                check_fraction(p)?;
                if self.values.is_empty() {
                    return Ok(Datum::Null);
                }
                self.value_at(p)
            }
            Shape::Array => {
                let arr = match &self.direct {
                    Datum::Null => return Ok(Datum::Null),
                    Datum::Array(a) => a,
                    other => return Err(type_mismatch("float8[]", other)),
                };
                if self.values.is_empty() {
                    return Ok(Datum::Null);
                }
                if arr.items.is_empty() {
                    return Ok(Datum::array(Array::empty(self.sort_ty.oid)));
                }
                // PG validates every non-NULL fraction (array order) before computing any result.
                let mut fracs: Vec<Option<f64>> = Vec::with_capacity(arr.items.len());
                for item in &arr.items {
                    let f = fraction_of(item)?;
                    if let Some(p) = f {
                        check_fraction(p)?;
                    }
                    fracs.push(f);
                }
                let mut items = Vec::with_capacity(fracs.len());
                for f in fracs {
                    items.push(match f {
                        None => Datum::Null,
                        Some(p) => self.value_at(p)?,
                    });
                }
                Ok(Datum::array(Array { elem: self.sort_ty.oid, dims: arr.dims.clone(), items }))
            }
        }
    }
}

/// `percentile` direct argument: `Ok(None)` for SQL NULL.
fn fraction_of(d: &Datum) -> Result<Option<f64>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Float8(v) => Ok(Some(*v)),
        Datum::Float4(v) => Ok(Some(f64::from(*v))),
        other => Err(type_mismatch("float8", other)),
    }
}

fn check_fraction(p: f64) -> Result<()> {
    if p < 0.0 || p > 1.0 || p.is_nan() {
        return Err(PgError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("percentile value {} is not between 0 and 1", fmt_g(p)),
        ));
    }
    Ok(())
}

/// 0-based index of the discrete percentile: smallest K with K/N >= p, K >= 1, then minus one.
fn disc_index(p: f64, n: usize) -> usize {
    let k = (p * (n as f64)).ceil() as usize;
    k.max(1).min(n) - 1
}

/// Continuous percentile position: (floor row, ceil row, proportion), 0-based.
fn cont_position(p: f64, n: usize) -> (usize, usize, f64) {
    let x = p * ((n - 1) as f64);
    let lo = x.floor();
    let hi = x.ceil();
    (lo as usize, hi as usize, x - lo)
}

/// PG `float8_lerp`: `lo + (pct * (hi - lo))`.
fn lerp_f8(lo: f64, hi: f64, pct: f64) -> f64 {
    lo + (pct * (hi - lo))
}

/// PG `percentile_cont` over intervals: `(hi - lo) * pct + lo` with PG interval arithmetic.
fn lerp_interval(lo: &Interval, hi: &Interval, pct: f64) -> Result<Interval> {
    let diff = ivl::add(hi, &interval_neg(lo)?)?;
    let scaled = ivl::mul(&diff, pct)?;
    ivl::add(&scaled, lo)
}

fn interval_neg(a: &Interval) -> Result<Interval> {
    if *a == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    if *a == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    match (a.months.checked_neg(), a.days.checked_neg(), a.micros.checked_neg()) {
        (Some(m), Some(d), Some(u)) => Ok(Interval::new(m, d, u)),
        _ => Err(PgError::new(SqlState::INTERVAL_FIELD_OVERFLOW, "interval out of range")),
    }
}

fn f8(d: &Datum) -> Result<f64> {
    match d {
        Datum::Float8(v) => Ok(*v),
        Datum::Float4(v) => Ok(f64::from(*v)),
        other => Err(type_mismatch("float8", other)),
    }
}

fn interval_of(d: &Datum) -> Result<Interval> {
    d.as_interval().ok_or_else(|| type_mismatch("interval", d))
}

/// C `%g` (precision 6) for the out-of-range percentile message.
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".to_string() } else { "-inf".to_string() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    const P: i32 = 6;
    let sci = format!("{:.*e}", (P - 1) as usize, v);
    let (mant, exp_str) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let x: i32 = exp_str.parse().unwrap_or(0);
    if x < -4 || x >= P {
        let sign = if x < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", strip_zeros(mant), sign, x.unsigned_abs())
    } else {
        let decimals = (P - 1 - x).max(0) as usize;
        strip_zeros(&format!("{:.*}", decimals, v))
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// mode()
// ---------------------------------------------------------------------------------------------

struct ModeState {
    sort_ty: TypeId,
    /// First value of the current run of equal values.
    run_val: Option<Datum>,
    run_len: u64,
    /// True when the current run is the one holding `mode_val`.
    run_is_mode: bool,
    mode_val: Option<Datum>,
    mode_freq: u64,
}

impl AggState for ModeState {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let v = args.first().ok_or_else(|| PgError::internal("mode aggregate received no sort value"))?;
        if v.is_null() {
            return Ok(());
        }
        let same = match &self.run_val {
            Some(prev) => compare::sql_eq(ctx.env, self.sort_ty, v, prev)? == Some(true),
            None => false,
        };
        if same {
            self.run_len += 1;
        } else {
            self.run_val = Some(v.clone());
            self.run_len = 1;
            self.run_is_mode = false;
        }
        // Strictly greater: a tie keeps the earlier run (first in sort order).
        if self.run_len > self.mode_freq {
            self.mode_freq = self.run_len;
            if !self.run_is_mode {
                self.mode_val = self.run_val.clone();
                self.run_is_mode = true;
            }
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(self.mode_val.clone().unwrap_or(Datum::Null))
    }
}

// ---------------------------------------------------------------------------------------------
// Hypothetical-set aggregates
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HypoKind {
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
}

struct HypoState {
    kind: HypoKind,
    /// Sort-column types (one per WITHIN GROUP expression).
    types: Vec<TypeId>,
    /// The hypothetical row (direct arguments).
    hypo: Vec<Datum>,
    /// Every input row, NULLs included, in sorted order.
    rows: Vec<Vec<Datum>>,
}

/// Sort direction and NULLS placement of one column, as far as the sorted input reveals them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ColOrder {
    desc: Option<bool>,
    nulls_first: Option<bool>,
}

fn infer_orders(env: &dyn TypeEnv, types: &[TypeId], rows: &[Vec<Datum>]) -> Result<Vec<ColOrder>> {
    let mut orders = vec![ColOrder::default(); types.len()];
    for pair in rows.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        for k in 0..types.len() {
            let (x, y) = (&a[k], &b[k]);
            if x.is_null() && y.is_null() {
                continue;
            }
            if x.is_null() != y.is_null() {
                // The earlier row holds the NULL, so NULLs sort first in this column.
                if orders[k].nulls_first.is_none() {
                    orders[k].nulls_first = Some(x.is_null());
                }
                break;
            }
            let c = compare::cmp_datums(env, types[k], x, y)?;
            if c == Ordering::Equal {
                continue;
            }
            if orders[k].desc.is_none() {
                orders[k].desc = Some(c == Ordering::Greater);
            }
            break;
        }
    }
    Ok(orders)
}

/// Lexicographic comparison of one stored row against the hypothetical row, using the inferred order.
fn cmp_to_hypo(
    env: &dyn TypeEnv,
    types: &[TypeId],
    orders: &[ColOrder],
    row: &[Datum],
    hypo: &[Datum],
) -> Result<Ordering> {
    for k in 0..types.len() {
        let (x, y) = (&row[k], &hypo[k]);
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) | (false, true) => {
                let nulls_first = orders[k].nulls_first.ok_or_else(undeterminable_order)?;
                if x.is_null() {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                } else if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let c = compare::cmp_datums(env, types[k], x, y)?;
                if c == Ordering::Equal {
                    Ordering::Equal
                } else {
                    let desc = orders[k].desc.ok_or_else(undeterminable_order)?;
                    if desc {
                        c.reverse()
                    } else {
                        c
                    }
                }
            }
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// Peer test (all sort columns `IS NOT DISTINCT FROM`); equality needs no sort direction.
fn peers(env: &dyn TypeEnv, types: &[TypeId], a: &[Datum], b: &[Datum]) -> Result<bool> {
    for k in 0..types.len() {
        if !compare::not_distinct(env, types[k], &a[k], &b[k])? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl AggState for HypoState {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if args.len() != self.types.len() {
            return Err(PgError::internal(format!(
                "hypothetical-set aggregate expected {} sort values, got {}",
                self.types.len(),
                args.len()
            )));
        }
        self.rows.push(args.to_vec());
        Ok(())
    }

    fn finalize(&self, ctx: &FuncCtx) -> Result<Datum> {
        let n = self.rows.len();
        if n == 0 {
            return Ok(match self.kind {
                HypoKind::Rank | HypoKind::DenseRank => Datum::Int8(1),
                HypoKind::PercentRank => Datum::Float8(0.0),
                HypoKind::CumeDist => Datum::Float8(1.0),
            });
        }
        let env = ctx.env;
        let orders = infer_orders(env, &self.types, &self.rows)?;
        // `less`: rows strictly before the hypothetical row; `le`: rows before or peer to it;
        // `distinct`: distinct sort values among the rows strictly before it.
        let mut less: usize = 0;
        let mut le: usize = 0;
        let mut distinct: usize = 0;
        for (i, row) in self.rows.iter().enumerate() {
            let c = cmp_to_hypo(env, &self.types, &orders, row, &self.hypo)?;
            if c != Ordering::Greater {
                le += 1;
            }
            if c == Ordering::Less {
                less += 1;
                if i == 0 || !peers(env, &self.types, &self.rows[i - 1], row)? {
                    distinct += 1;
                }
            }
        }
        Ok(match self.kind {
            HypoKind::Rank => Datum::Int8(less as i64 + 1),
            HypoKind::DenseRank => Datum::Int8(distinct as i64 + 1),
            HypoKind::PercentRank => Datum::Float8(less as f64 / n as f64),
            HypoKind::CumeDist => Datum::Float8((le as f64 + 1.0) / (n as f64 + 1.0)),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::oids;
    use crate::registry::builtin_env;
    use pgarbor_common::Oid;

    fn ctx<'a>(io: &'a IoConfig, s: &'a TestSession, types: &'a [TypeId]) -> FuncCtx<'a> {
        FuncCtx { env: builtin_env(), io, session: s, arg_types: types, result_type: TypeId::UNKNOWN, collation: 0 }
    }

    fn run(oid: Oid, sort: &[TypeId], rt: TypeId, direct: Vec<Datum>, rows: Vec<Vec<Datum>>) -> Result<Datum> {
        let io = IoConfig::default();
        let s = TestSession::default();
        let spec = AggSpec { agg_oid: oid, arg_types: sort.to_vec(), result_type: rt, direct_args: direct };
        let c = ctx(&io, &s, sort);
        let mut st = crate::agg::make_aggregate(&spec, &c)?;
        for r in &rows {
            st.transition(r, &c)?;
        }
        st.finalize(&c)
    }

    fn f8s(vals: &[f64]) -> Vec<Vec<Datum>> {
        vals.iter().map(|v| vec![Datum::Float8(*v)]).collect()
    }

    fn i4s(vals: &[i32]) -> Vec<Vec<Datum>> {
        vals.iter().map(|v| vec![Datum::Int4(*v)]).collect()
    }

    #[test]
    fn cont_median_of_1_2_4_is_2() {
        let r = run(3974, &[TypeId::FLOAT8], TypeId::FLOAT8, vec![Datum::Float8(0.5)], f8s(&[1.0, 2.0, 4.0]));
        assert_eq!(r.ok(), Some(Datum::Float8(2.0)));
    }

    #[test]
    fn disc_median_of_1_2_4_is_2() {
        let r = run(3972, &[TypeId::INT4], TypeId::INT4, vec![Datum::Float8(0.5)], i4s(&[1, 2, 4]));
        assert_eq!(r.ok(), Some(Datum::Int4(2)));
    }

    #[test]
    fn disc_ignores_nulls() {
        let rows = vec![vec![Datum::Null], vec![Datum::Int4(1)], vec![Datum::Int4(2)], vec![Datum::Int4(4)]];
        let r = run(3972, &[TypeId::INT4], TypeId::INT4, vec![Datum::Float8(0.5)], rows);
        assert_eq!(r.ok(), Some(Datum::Int4(2)));
    }

    #[test]
    fn null_fraction_gives_null() {
        let r = run(3972, &[TypeId::INT4], TypeId::INT4, vec![Datum::Null], i4s(&[1, 2]));
        assert_eq!(r.ok(), Some(Datum::Null));
    }

    #[test]
    fn no_rows_gives_null() {
        let r = run(3974, &[TypeId::FLOAT8], TypeId::FLOAT8, vec![Datum::Float8(0.5)], Vec::new());
        assert_eq!(r.ok(), Some(Datum::Null));
    }

    #[test]
    fn percentile_out_of_range_is_22003() {
        let r = run(3974, &[TypeId::FLOAT8], TypeId::FLOAT8, vec![Datum::Float8(1.5)], f8s(&[1.0]));
        match r {
            Err(e) => {
                assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
                assert_eq!(e.message, "percentile value 1.5 is not between 0 and 1");
            }
            Ok(v) => panic!("expected error, got {v:?}"),
        }
    }

    #[test]
    fn disc_array_of_fractions() {
        let param = Datum::array(Array {
            elem: oids::FLOAT8,
            dims: vec![(2, 1)],
            items: vec![Datum::Float8(0.0), Datum::Float8(1.0)],
        });
        let r = run(3978, &[TypeId::FLOAT8], TypeId::of(oids::FLOAT8_ARRAY), vec![param], f8s(&[1.0, 2.0, 4.0]));
        let expected = Datum::array(Array {
            elem: oids::FLOAT8,
            dims: vec![(2, 1)],
            items: vec![Datum::Float8(1.0), Datum::Float8(4.0)],
        });
        assert_eq!(r.ok(), Some(expected));
    }

    #[test]
    fn mode_of_1_2_2_3_is_2() {
        let r = run(3984, &[TypeId::INT4], TypeId::INT4, Vec::new(), i4s(&[1, 2, 2, 3]));
        assert_eq!(r.ok(), Some(Datum::Int4(2)));
    }

    #[test]
    fn mode_tie_takes_first_in_sort_order() {
        let r = run(3984, &[TypeId::INT4], TypeId::INT4, Vec::new(), i4s(&[1, 1, 2, 2]));
        assert_eq!(r.ok(), Some(Datum::Int4(1)));
    }

    #[test]
    fn hypothetical_rank_ascending() {
        let r = run(3986, &[TypeId::INT4], TypeId::INT8, vec![Datum::Int4(2)], i4s(&[1, 2, 2, 4]));
        assert_eq!(r.ok(), Some(Datum::Int8(2)));
    }

    #[test]
    fn hypothetical_dense_rank_ascending() {
        let r = run(3992, &[TypeId::INT4], TypeId::INT8, vec![Datum::Int4(3)], i4s(&[1, 2, 2, 4]));
        assert_eq!(r.ok(), Some(Datum::Int8(3)));
    }

    #[test]
    fn hypothetical_percent_rank_and_cume_dist() {
        let pr = run(3988, &[TypeId::INT4], TypeId::FLOAT8, vec![Datum::Int4(2)], i4s(&[1, 2, 2, 4]));
        assert_eq!(pr.ok(), Some(Datum::Float8(0.25)));
        let cd = run(3990, &[TypeId::INT4], TypeId::FLOAT8, vec![Datum::Int4(2)], i4s(&[1, 2, 2, 4]));
        assert_eq!(cd.ok(), Some(Datum::Float8(0.8)));
    }

    #[test]
    fn hypothetical_rank_descending_input() {
        let r = run(3986, &[TypeId::INT4], TypeId::INT8, vec![Datum::Int4(2)], i4s(&[4, 2, 2, 1]));
        assert_eq!(r.ok(), Some(Datum::Int8(2)));
    }

    #[test]
    fn hypothetical_no_rows() {
        let r = run(3986, &[TypeId::INT4], TypeId::INT8, vec![Datum::Int4(2)], Vec::new());
        assert_eq!(r.ok(), Some(Datum::Int8(1)));
        let c = run(3990, &[TypeId::INT4], TypeId::FLOAT8, vec![Datum::Int4(2)], Vec::new());
        assert_eq!(c.ok(), Some(Datum::Float8(1.0)));
    }

    #[test]
    fn cont_interval_midpoint() {
        let rows = vec![
            vec![Datum::Interval(Interval::new(0, 0, 0))],
            vec![Datum::Interval(Interval::new(0, 0, 10))],
        ];
        let r = run(3976, &[TypeId::INTERVAL], TypeId::INTERVAL, vec![Datum::Float8(0.5)], rows);
        assert_eq!(r.ok(), Some(Datum::Interval(Interval::new(0, 0, 5))));
    }

    #[test]
    fn fmt_g_matches_c_printf() {
        assert_eq!(fmt_g(1.5), "1.5");
        assert_eq!(fmt_g(2.0), "2");
        assert_eq!(fmt_g(1e-5), "1e-05");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(1e20), "1e+20");
        assert_eq!(fmt_g(f64::NAN), "nan");
    }
}
