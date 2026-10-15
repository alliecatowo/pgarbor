//! count / sum / avg / min / max / bool_and / bool_or / every / bit_and / bit_or / bit_xor / any_value
//! (TYPE-12). Lease TYP-45.
//!
//! PG18 reference: `src/include/catalog/pg_aggregate.dat` (which transition/final functions and
//! transition types each aggregate uses), `utils/adt/numeric.c` (`int8_avg_accum`, `numeric_avg`,
//! `int2_sum`, `int4_sum`, `int8_sum`), `float.c` (`float8_accum`, `float8_avg`), `timestamp.c`
//! (`interval_avg_accum`, `interval_sum`), `int.c` (bit ops), `bool.c` (`booland_statefunc`).
//! Dispatch on `agg.oid` / `agg.name` + `spec.arg_types` (see `catalog_data::AGGREGATES`, module AggBasic).
//!
//! Result types and semantics (oracle-verified, values (1),(2),(4)):
//! - `count(*)` (oid 2803, zero args): counts all rows → int8. `count(x)`: non-NULL rows → int8. Empty
//!   input → 0 (never NULL).
//! - `sum(int2)`, `sum(int4)` → int8 (no overflow possible in practice: accumulate in i128 and error
//!   22003 `bigint out of range` only if the final value exceeds i64; `sum(2147483647)` over 3 rows =
//!   6442450941); `sum(int8)` → numeric (`sum(9223372036854775807::int8)` over 2 rows =
//!   18446744073709551614); `sum(numeric)` → numeric (dscale = max input dscale); `sum(float4)` → float4,
//!   `sum(float8)` → float8 (IEEE accumulation, overflow → 22003 `value out of range: overflow`);
//!   `sum(interval)` → interval; `sum(money)` unsupported.
//! - `avg(int2|int4|int8|numeric)` → numeric = `arith::div` of exact sum by count (`avg(1,2,4)` =
//!   `2.3333333333333333`); `avg(float4|float8)` → float8 (`2.3333333333333335`); `avg(interval)` → interval
//!   (`00:00:02.333333`).
//! - `min`/`max`: exactly the overloads present in `catalog_data::AGGREGATES` (ints, floats, numeric,
//!   text, bpchar, bytea (PG18), date/time types, interval, oid, enum (anyenum), arrays (anyarray),
//!   money/inet/pg_lsn/tid/xid8 (unsupported types → 0A000)), compared with `compare::cmp_datums`.
//!   Result type = input type (bpchar keeps padding).
//! - `bool_and`/`every`, `bool_or`; `bit_and`/`bit_or`/`bit_xor` over int2/int4/int8 (and bit, unsupported).
//! - `any_value(anyelement)`: first non-NULL value.
//! All of them except count(*) ignore NULL inputs; with no non-NULL input the result is NULL (count → 0).
//! `retract` (moving aggregates) should be implemented where PG defines an inverse transition
//! (`aggminvtransfn`): count, sum/avg of int2/int4/int8/numeric/interval, bool_and/bool_or (via counts).
//! Floats (`float8pl`, `float8_accum`), min/max and bit ops have none: return `Ok(false)`.
//!
//! Implementation notes:
//! - Ties in min/max keep PG's transition semantics: `max` keeps the state only when it is strictly
//!   greater (`arg1 > arg2 ? arg1 : arg2`), `min` only when strictly less, so ties take the new value.
//! - Numeric and interval sums track NaN / +Inf / -Inf counts separately (like PG's aggregate state),
//!   so infinities can be added and removed again by `retract`.
//! - Float sums copy the first input unchanged (PG's `float4pl`/`float8pl` start from a NULL state) and
//!   report overflow only when finite inputs produce an infinite result. `avg(float)` follows
//!   `float8_accum` (Youngs-Cramer) including its overflow check, so `avg(1e200)` errors like PG.
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::compare::cmp_datums;
use crate::context::FuncCtx;
use crate::datetime::interval as ival;
use crate::datum::{type_mismatch, Datum, Interval, TypeId};
use crate::names::display_name;
use crate::numeric::arith;
use crate::numeric::Numeric;
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};
use std::cmp::Ordering;

/// Instantiate a basic aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    let name = agg.name;
    match name {
        "count" => Ok(Box::new(Count { star: spec.arg_types.is_empty(), n: 0 })),
        "sum" | "avg" => {
            let t = first_arg_type(name, spec)?;
            let avg = name == "avg";
            match t.oid {
                oids::INT2 | oids::INT4 => {
                    let out = if avg { IntOut::NumericAvg } else { IntOut::Int8 };
                    Ok(Box::new(IntSum { sum: 0, n: 0, out }))
                }
                oids::INT8 => {
                    let out = if avg { IntOut::NumericAvg } else { IntOut::Numeric };
                    Ok(Box::new(IntSum { sum: 0, n: 0, out }))
                }
                oids::NUMERIC => Ok(Box::new(NumSum {
                    finite: Numeric::zero(),
                    n: 0,
                    nan: 0,
                    pinf: 0,
                    ninf: 0,
                    avg,
                })),
                oids::FLOAT4 => {
                    if avg {
                        Ok(Box::new(FloatAvg { n: 0, sx: 0.0, sxx: 0.0 }))
                    } else {
                        Ok(Box::new(Float4Sum { sum: None }))
                    }
                }
                oids::FLOAT8 => {
                    if avg {
                        Ok(Box::new(FloatAvg { n: 0, sx: 0.0, sxx: 0.0 }))
                    } else {
                        Ok(Box::new(Float8Sum { sum: None }))
                    }
                }
                oids::INTERVAL => Ok(Box::new(IntervalSum {
                    sum: Interval::ZERO,
                    n: 0,
                    pinf: 0,
                    ninf: 0,
                    avg,
                })),
                _ => Err(unsupported_aggregate(name, t, ctx)),
            }
        }
        "min" | "max" => {
            let t = first_arg_type(name, spec)?;
            match t.oid {
                // money (cash), inet/cidr, pg_lsn, tid, xid8: no PGArbor value semantics yet.
                790 | 869 | 3220 | 27 | 5069 => Err(unsupported_aggregate(name, t, ctx)),
                _ => Ok(Box::new(MinMax { ty: t, is_max: name == "max", cur: None })),
            }
        }
        "bool_and" | "every" => Ok(Box::new(BoolAgg { is_and: true, n: 0, t: 0, f: 0 })),
        "bool_or" => Ok(Box::new(BoolAgg { is_and: false, n: 0, t: 0, f: 0 })),
        "bit_and" | "bit_or" | "bit_xor" => {
            let t = first_arg_type(name, spec)?;
            let width = match t.oid {
                oids::INT2 => BitWidth::I16,
                oids::INT4 => BitWidth::I32,
                oids::INT8 => BitWidth::I64,
                _ => return Err(unsupported_aggregate(name, t, ctx)),
            };
            let op = match name {
                "bit_and" => BitOp::And,
                "bit_or" => BitOp::Or,
                _ => BitOp::Xor,
            };
            Ok(Box::new(BitAgg { op, width, cur: None }))
        }
        "any_value" => Ok(Box::new(AnyValue { cur: None })),
        _ => Err(PgError::unimplemented("TYP-45", format!("aggregate {}", name))),
    }
}

fn first_arg_type(name: &str, spec: &AggSpec) -> Result<TypeId> {
    spec.arg_types.first().copied().ok_or_else(|| {
        PgError::internal(format!("aggregate {} called without an argument type", name))
    })
}

fn unsupported_aggregate(name: &str, t: TypeId, ctx: &FuncCtx) -> PgError {
    PgError::unimplemented("TYP-45", format!("aggregate {}({})", name, display_name(ctx.env, t.oid)))
}

fn missing_arg(name: &str) -> PgError {
    PgError::internal(format!("aggregate {} received no argument", name))
}

fn float_overflow() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

fn interval_out_of_range() -> PgError {
    PgError::new(SqlState::DATETIME_VALUE_OUT_OF_RANGE, "interval out of range")
}

/// The argument of a single-argument transition, or an internal error if none was passed.
fn one_arg<'a>(name: &str, args: &'a [Datum]) -> Result<&'a Datum> {
    args.first().ok_or_else(|| missing_arg(name))
}

fn int_arg(d: &Datum) -> Result<i128> {
    match d {
        Datum::Int2(v) => Ok(*v as i128),
        Datum::Int4(v) => Ok(*v as i128),
        Datum::Int8(v) => Ok(*v as i128),
        other => Err(type_mismatch("Int2|Int4|Int8", other)),
    }
}

fn numeric_arg(d: &Datum) -> Result<&Numeric> {
    match d {
        Datum::Numeric(n) => Ok(n),
        other => Err(type_mismatch("Numeric", other)),
    }
}

fn interval_arg(d: &Datum) -> Result<Interval> {
    match d {
        Datum::Interval(v) => Ok(*v),
        other => Err(type_mismatch("Interval", other)),
    }
}

/// `a - b` for finite intervals with PG's per-field overflow checks (`finite_interval_mi`).
fn interval_sub(a: &Interval, b: &Interval) -> Result<Interval> {
    match (a.months.checked_sub(b.months), a.days.checked_sub(b.days), a.micros.checked_sub(b.micros)) {
        (Some(months), Some(days), Some(micros)) => {
            let r = Interval::new(months, days, micros);
            if r.is_infinite() {
                Err(interval_out_of_range())
            } else {
                Ok(r)
            }
        }
        _ => Err(interval_out_of_range()),
    }
}

// ---------------------------------------------------------------------------------------------
// count
// ---------------------------------------------------------------------------------------------

struct Count {
    /// `count(*)`: zero arguments, every row counts.
    star: bool,
    n: i64,
}

impl Count {
    fn counts(&self, args: &[Datum]) -> bool {
        self.star || args.first().is_some_and(|a| !a.is_null())
    }
}

impl AggState for Count {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if self.counts(args) {
            self.n += 1;
        }
        Ok(())
    }
    fn retract(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        if self.counts(args) {
            self.n -= 1;
        }
        Ok(true)
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(Datum::Int8(self.n))
    }
}

// ---------------------------------------------------------------------------------------------
// sum / avg over int2, int4, int8
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum IntOut {
    /// `sum(int2|int4)` → int8.
    Int8,
    /// `sum(int8)` → numeric.
    Numeric,
    /// `avg(int2|int4|int8)` → numeric.
    NumericAvg,
}

struct IntSum {
    sum: i128,
    n: i64,
    out: IntOut,
}

impl IntSum {
    fn apply(&mut self, args: &[Datum], sign: i128) -> Result<()> {
        let a = one_arg("sum", args)?;
        if a.is_null() {
            return Ok(());
        }
        self.sum += sign * int_arg(a)?;
        self.n += sign as i64;
        Ok(())
    }
}

impl AggState for IntSum {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        self.apply(args, 1)
    }
    fn retract(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        self.apply(args, -1)?;
        Ok(true)
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.n == 0 {
            return Ok(Datum::Null);
        }
        match self.out {
            IntOut::Int8 => {
                let v = i64::try_from(self.sum)
                    .map_err(|_| PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?;
                Ok(Datum::Int8(v))
            }
            IntOut::Numeric => Ok(Datum::Numeric(Numeric::from_i128(self.sum))),
            IntOut::NumericAvg => {
                let q = arith::div(&Numeric::from_i128(self.sum), &Numeric::from_i64(self.n))?;
                Ok(Datum::Numeric(q))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sum / avg over numeric (NaN and infinities counted separately, as PG's aggregate state does)
// ---------------------------------------------------------------------------------------------

struct NumSum {
    /// Sum of the finite inputs.
    finite: Numeric,
    /// All non-NULL inputs (finite and special).
    n: i64,
    nan: i64,
    pinf: i64,
    ninf: i64,
    avg: bool,
}

impl NumSum {
    fn apply(&mut self, x: &Numeric, sign: i64) -> Result<()> {
        match x {
            Numeric::NaN => self.nan += sign,
            Numeric::PosInf => self.pinf += sign,
            Numeric::NegInf => self.ninf += sign,
            Numeric::Finite(_) => {
                self.finite = if sign > 0 {
                    arith::add(&self.finite, x)?
                } else {
                    arith::sub(&self.finite, x)?
                };
            }
        }
        self.n += sign;
        Ok(())
    }
}

impl AggState for NumSum {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("sum", args)?;
        if a.is_null() {
            return Ok(());
        }
        let x = numeric_arg(a)?;
        self.apply(x, 1)
    }
    fn retract(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        let a = one_arg("sum", args)?;
        if a.is_null() {
            return Ok(true);
        }
        let x = numeric_arg(a)?;
        self.apply(x, -1)?;
        Ok(true)
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.n == 0 {
            return Ok(Datum::Null);
        }
        let special = if self.nan > 0 || (self.pinf > 0 && self.ninf > 0) {
            Some(Numeric::NaN)
        } else if self.pinf > 0 {
            Some(Numeric::PosInf)
        } else if self.ninf > 0 {
            Some(Numeric::NegInf)
        } else {
            None
        };
        if let Some(s) = special {
            return Ok(Datum::Numeric(s));
        }
        if self.avg {
            // With no special values, `n` is exactly the finite count.
            let q = arith::div(&self.finite, &Numeric::from_i64(self.n))?;
            Ok(Datum::Numeric(q))
        } else {
            Ok(Datum::Numeric(self.finite.clone()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sum / avg over interval (infinities counted separately, as PG's IntervalAggState does)
// ---------------------------------------------------------------------------------------------

struct IntervalSum {
    /// Sum of the finite inputs.
    sum: Interval,
    /// Count of finite inputs (PG's `N`).
    n: i64,
    pinf: i64,
    ninf: i64,
    avg: bool,
}

impl IntervalSum {
    fn apply(&mut self, x: Interval, sign: i64) -> Result<()> {
        if x == Interval::INFINITY {
            self.pinf += sign;
        } else if x == Interval::NEG_INFINITY {
            self.ninf += sign;
        } else {
            self.sum = if sign > 0 {
                ival::add(&self.sum, &x)?
            } else {
                interval_sub(&self.sum, &x)?
            };
            self.n += sign;
        }
        Ok(())
    }
}

impl AggState for IntervalSum {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("sum", args)?;
        if a.is_null() {
            return Ok(());
        }
        let x = interval_arg(a)?;
        self.apply(x, 1)
    }
    fn retract(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        let a = one_arg("sum", args)?;
        if a.is_null() {
            return Ok(true);
        }
        let x = interval_arg(a)?;
        self.apply(x, -1)?;
        Ok(true)
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.n == 0 && self.pinf == 0 && self.ninf == 0 {
            return Ok(Datum::Null);
        }
        if self.pinf > 0 && self.ninf > 0 {
            return Err(interval_out_of_range());
        }
        if self.pinf > 0 {
            return Ok(Datum::Interval(Interval::INFINITY));
        }
        if self.ninf > 0 {
            return Ok(Datum::Interval(Interval::NEG_INFINITY));
        }
        if self.avg {
            // `n > 0` here: with no infinities, the total count is the finite count.
            let q = ival::div(&self.sum, self.n as f64)?;
            Ok(Datum::Interval(q))
        } else {
            Ok(Datum::Interval(self.sum))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sum over float4 / float8 (IEEE accumulation; first input copied as PG's NULL start state)
// ---------------------------------------------------------------------------------------------

struct Float4Sum {
    sum: Option<f32>,
}

impl AggState for Float4Sum {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("sum", args)?;
        let v = match a {
            Datum::Null => return Ok(()),
            Datum::Float4(v) => *v,
            other => return Err(type_mismatch("Float4", other)),
        };
        self.sum = Some(match self.sum {
            None => v,
            Some(s) => {
                let r = s + v;
                if r.is_infinite() && !s.is_infinite() && !v.is_infinite() {
                    return Err(float_overflow());
                }
                r
            }
        });
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match self.sum {
            None => Datum::Null,
            Some(s) => Datum::Float4(s),
        })
    }
}

struct Float8Sum {
    sum: Option<f64>,
}

impl AggState for Float8Sum {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("sum", args)?;
        let v = match a {
            Datum::Null => return Ok(()),
            Datum::Float8(v) => *v,
            other => return Err(type_mismatch("Float8", other)),
        };
        self.sum = Some(match self.sum {
            None => v,
            Some(s) => {
                let r = s + v;
                if r.is_infinite() && !s.is_infinite() && !v.is_infinite() {
                    return Err(float_overflow());
                }
                r
            }
        });
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match self.sum {
            None => Datum::Null,
            Some(s) => Datum::Float8(s),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// avg over float4 / float8 (float8_accum, Youngs-Cramer; result float8)
// ---------------------------------------------------------------------------------------------

struct FloatAvg {
    /// PG's `N` (count of non-NULL inputs).
    n: i64,
    /// PG's `Sx`.
    sx: f64,
    /// PG's `Sxx`, kept only for its overflow check.
    sxx: f64,
}

impl AggState for FloatAvg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("avg", args)?;
        let v = match a {
            Datum::Null => return Ok(()),
            Datum::Float4(x) => *x as f64,
            Datum::Float8(x) => *x,
            other => return Err(type_mismatch("Float4|Float8", other)),
        };
        let prev_n = self.n as f64;
        let n = prev_n + 1.0;
        let sx = self.sx + v;
        let mut sxx = self.sxx;
        if self.n > 0 {
            let tmp = v * n - sx;
            sxx += tmp * tmp / (n * prev_n);
            if sx.is_infinite() || sxx.is_infinite() {
                if !self.sx.is_infinite() && !v.is_infinite() {
                    return Err(float_overflow());
                }
                sxx = f64::NAN;
            }
        } else if v.is_nan() || v.is_infinite() {
            sxx = f64::NAN;
        }
        self.n += 1;
        self.sx = sx;
        self.sxx = sxx;
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.n == 0 {
            return Ok(Datum::Null);
        }
        Ok(Datum::Float8(self.sx / self.n as f64))
    }
}

// ---------------------------------------------------------------------------------------------
// min / max (any type with cmp_datums ordering; ties take the newer value like PG's larger/smaller)
// ---------------------------------------------------------------------------------------------

struct MinMax {
    ty: TypeId,
    is_max: bool,
    cur: Option<Datum>,
}

impl AggState for MinMax {
    fn transition(&mut self, args: &[Datum], ctx: &FuncCtx) -> Result<()> {
        let a = one_arg(if self.is_max { "max" } else { "min" }, args)?;
        if a.is_null() {
            return Ok(());
        }
        let replace = match self.cur.as_ref() {
            None => true,
            Some(c) => {
                let ord = cmp_datums(ctx.env, self.ty, c, a)?;
                // PG: max keeps the state only when state > new; min only when state < new.
                if self.is_max {
                    ord != Ordering::Greater
                } else {
                    ord != Ordering::Less
                }
            }
        };
        if replace {
            self.cur = Some(a.clone());
        }
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(self.cur.clone().unwrap_or(Datum::Null))
    }
}

// ---------------------------------------------------------------------------------------------
// bool_and / bool_or (counts, so retract is exact)
// ---------------------------------------------------------------------------------------------

struct BoolAgg {
    is_and: bool,
    n: i64,
    t: i64,
    f: i64,
}

impl BoolAgg {
    fn apply(&mut self, args: &[Datum], sign: i64) -> Result<()> {
        let a = one_arg(if self.is_and { "bool_and" } else { "bool_or" }, args)?;
        match a {
            Datum::Null => {}
            Datum::Bool(b) => {
                self.n += sign;
                if *b {
                    self.t += sign;
                } else {
                    self.f += sign;
                }
            }
            other => return Err(type_mismatch("Bool", other)),
        }
        Ok(())
    }
}

impl AggState for BoolAgg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        self.apply(args, 1)
    }
    fn retract(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<bool> {
        self.apply(args, -1)?;
        Ok(true)
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        if self.n == 0 {
            return Ok(Datum::Null);
        }
        Ok(Datum::Bool(if self.is_and { self.f == 0 } else { self.t > 0 }))
    }
}

// ---------------------------------------------------------------------------------------------
// bit_and / bit_or / bit_xor over int2 / int4 / int8
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum BitOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy)]
enum BitWidth {
    I16,
    I32,
    I64,
}

struct BitAgg {
    op: BitOp,
    width: BitWidth,
    /// Running value, sign-extended. Bitwise ops commute with sign extension, so narrowing at the end is exact.
    cur: Option<i64>,
}

impl AggState for BitAgg {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("bit_and", args)?;
        let v: i64 = match a {
            Datum::Null => return Ok(()),
            Datum::Int2(x) => *x as i64,
            Datum::Int4(x) => *x as i64,
            Datum::Int8(x) => *x,
            other => return Err(type_mismatch("Int2|Int4|Int8", other)),
        };
        self.cur = Some(match self.cur {
            None => v,
            Some(c) => match self.op {
                BitOp::And => c & v,
                BitOp::Or => c | v,
                BitOp::Xor => c ^ v,
            },
        });
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(match self.cur {
            None => Datum::Null,
            Some(c) => match self.width {
                BitWidth::I16 => Datum::Int2(c as i16),
                BitWidth::I32 => Datum::Int4(c as i32),
                BitWidth::I64 => Datum::Int8(c),
            },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// any_value
// ---------------------------------------------------------------------------------------------

struct AnyValue {
    cur: Option<Datum>,
}

impl AggState for AnyValue {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        let a = one_arg("any_value", args)?;
        if self.cur.is_none() && !a.is_null() {
            self.cur = Some(a.clone());
        }
        Ok(())
    }
    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(self.cur.clone().unwrap_or(Datum::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::make_aggregate;
    use crate::context::{IoConfig, TestSession};
    use crate::numeric::Numeric as Num;
    use crate::registry::builtin_env;

    fn run(agg_oid: u32, arg_types: &[TypeId], result: TypeId, rows: &[Vec<Datum>]) -> Result<Datum> {
        let env = builtin_env();
        let cfg = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env,
            io: &cfg,
            session: &sess,
            arg_types,
            result_type: result,
            collation: 0,
        };
        let spec = AggSpec {
            agg_oid,
            arg_types: arg_types.to_vec(),
            result_type: result,
            direct_args: Vec::new(),
        };
        let mut st = make_aggregate(&spec, &ctx)?;
        for r in rows {
            st.transition(r, &ctx)?;
        }
        st.finalize(&ctx)
    }

    fn numstr(d: &Datum) -> String {
        match d {
            Datum::Numeric(Num::Finite(b)) => b.to_string(),
            other => format!("{:?}", other),
        }
    }

    fn i4(v: i32) -> Vec<Datum> {
        vec![Datum::Int4(v)]
    }

    #[test]
    fn sum_int4_is_int8() {
        let rows = [i4(1), i4(2), i4(4)];
        assert_eq!(run(2108, &[TypeId::INT4], TypeId::INT8, &rows).unwrap(), Datum::Int8(7));
    }

    #[test]
    fn sum_int4_large_is_int8() {
        let rows = [i4(i32::MAX), i4(i32::MAX), i4(i32::MAX)];
        assert_eq!(
            run(2108, &[TypeId::INT4], TypeId::INT8, &rows).unwrap(),
            Datum::Int8(6442450941)
        );
    }

    #[test]
    fn sum_int8_is_numeric() {
        let rows = [vec![Datum::Int8(i64::MAX)], vec![Datum::Int8(i64::MAX)]];
        let d = run(2107, &[TypeId::INT8], TypeId::NUMERIC, &rows).unwrap();
        assert_eq!(numstr(&d), "18446744073709551614");
    }

    #[test]
    fn avg_int4_is_numeric() {
        let rows = [i4(1), i4(2), i4(4)];
        let d = run(2101, &[TypeId::INT4], TypeId::NUMERIC, &rows).unwrap();
        assert_eq!(numstr(&d), "2.3333333333333333");
    }

    #[test]
    fn avg_float4_is_float8() {
        let rows = [vec![Datum::Float4(1.0)], vec![Datum::Float4(2.0)], vec![Datum::Float4(4.0)]];
        let d = run(2104, &[TypeId::FLOAT4], TypeId::FLOAT8, &rows).unwrap();
        assert_eq!(d, Datum::Float8(7.0 / 3.0));
    }

    #[test]
    fn empty_input_count_zero_sum_null() {
        let rows: [Vec<Datum>; 0] = [];
        assert_eq!(run(2803, &[], TypeId::INT8, &rows).unwrap(), Datum::Int8(0));
        assert_eq!(run(2108, &[TypeId::INT4], TypeId::INT8, &rows).unwrap(), Datum::Null);
    }

    #[test]
    fn count_column_skips_nulls() {
        let rows = [vec![Datum::Null], i4(1)];
        assert_eq!(run(2147, &[TypeId::INT4], TypeId::INT8, &rows).unwrap(), Datum::Int8(1));
    }

    #[test]
    fn max_text_codepoint_order() {
        let rows = [
            vec![Datum::Text("b".into())],
            vec![Datum::Text("B".into())],
            vec![Datum::Text("a".into())],
        ];
        assert_eq!(run(2129, &[TypeId::TEXT], TypeId::TEXT, &rows).unwrap(), Datum::Text("b".into()));
    }

    #[test]
    fn avg_interval_seconds() {
        let rows = [
            vec![Datum::Interval(Interval::new(0, 0, 1_000_000))],
            vec![Datum::Interval(Interval::new(0, 0, 2_000_000))],
            vec![Datum::Interval(Interval::new(0, 0, 4_000_000))],
        ];
        let d = run(2106, &[TypeId::INTERVAL], TypeId::INTERVAL, &rows).unwrap();
        assert_eq!(d, Datum::Interval(Interval::new(0, 0, 2_333_333)));
    }

    #[test]
    fn bit_and_or_int4() {
        let rows = [i4(1), i4(2), i4(4)];
        assert_eq!(run(2238, &[TypeId::INT4], TypeId::INT4, &rows).unwrap(), Datum::Int4(0));
        assert_eq!(run(2239, &[TypeId::INT4], TypeId::INT4, &rows).unwrap(), Datum::Int4(7));
    }

    #[test]
    fn bool_and_or() {
        let rows = [vec![Datum::Bool(true)], vec![Datum::Bool(false)]];
        assert_eq!(run(2517, &[TypeId::BOOL], TypeId::BOOL, &rows).unwrap(), Datum::Bool(false));
        assert_eq!(run(2518, &[TypeId::BOOL], TypeId::BOOL, &rows).unwrap(), Datum::Bool(true));
    }

    #[test]
    fn float_sum_overflow_errors() {
        let rows = [vec![Datum::Float8(f64::MAX)], vec![Datum::Float8(f64::MAX)]];
        let err = run(2111, &[TypeId::FLOAT8], TypeId::FLOAT8, &rows).unwrap_err();
        assert_eq!(err.sqlstate.code(), "22003");
    }
}
