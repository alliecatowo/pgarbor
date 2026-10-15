//! Statistical aggregates: stddev/variance families, covariance, correlation, regression (TYPE-12).
//! Lease TYP-47.
//!
//! PG18 reference: `utils/adt/float.c` (`float8_accum` uses the Youngs-Cramer algorithm,
//! `float8_var_samp`, `float8_regr_accum`, `float8_corr`, ...), `numeric.c` (`numeric_accum`,
//! `numeric_var_samp`, `numeric_stddev_*`, `int8_accum`, `numeric_poly_*`).
//! Aggregates (module AggStats): `variance`/`var_samp`, `var_pop`, `stddev`/`stddev_samp`, `stddev_pop`
//! over int2/int4/int8/numeric (→ numeric, computed EXACTLY then rounded per PG's scale rules in
//! `numeric_stddev_internal`: rscale = max(dscale of sumX² ..., ...)) and float4/float8 (→ float8,
//! Youngs-Cramer); `covar_pop covar_samp corr regr_count regr_sxx regr_syy regr_sxy regr_avgx regr_avgy
//! regr_r2 regr_slope regr_intercept` (float8 args, rows with any NULL skipped).
//! Oracle (values 1,2,4): `stddev(x)` = `1.5275252316519467`, `variance(x)` = `2.3333333333333333`,
//! `stddev_pop(x::float8)` = `1.247219128924647`, `corr(x, x*2)` = `1`, `regr_slope(x*2, x)` = `2`.
//! Fewer than 2 rows for samp variants → NULL; zero rows → NULL.
//!
//! Numeric family (`numeric_stddev_internal`): the accumulator keeps exact N, sumX and sumX² (BigDecimal,
//! with PG's dscale: max input dscale for sumX, twice that for sumX²) plus NaN/±Inf counts. The
//! numerator `N*sumX2 - sumX*sumX` and the denominator `N*(N-1)` (samp) or `N*N` (pop) are exact;
//! the quotient is rounded half-up at `select_div_scale(numerator, denominator)` digits, and stddev
//! takes `sqrt_var` of that rounded quotient, rounded half-up at the same scale. Any NaN or infinite
//! input gives NaN (once the count rule for NULL has been applied). The int2/int4 `numeric_poly_*`
//! variants compute the same values (their sums are integers, dscale 0).
//!
//! Float family (`float8_accum`/`float4_accum`): Youngs-Cramer updates in f64, in the same operation
//! order as float.c so results match bit for bit.
//!
//! Regression family (`float8_regr_accum`): Youngs-Cramer on X, Y and the cross term; the first argument
//! is Y and the second is X, as in `regr_slope(y, x)`.
//!
//! `retract` (moving-window inverse) is not provided: the default `Ok(false)` makes the executor
//! recompute frames from scratch, which gives the same results.
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use crate::datum::Datum;
use crate::numeric::{arith, Numeric};
use bigdecimal::BigDecimal;
use num_bigint::{BigInt, Sign};
use pgarbor_common::{PgError, Result, SqlState};

/// Instantiate a statistical aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    match classify(agg)? {
        Family::Numeric { stddev, sample } => Ok(Box::new(NumericStats {
            stddev,
            sample,
            n: 0,
            nan: 0,
            pinf: 0,
            ninf: 0,
            sum_x: BigDecimal::from(0i64),
            sum_x2: BigDecimal::from(0i64),
        })),
        Family::Float { stddev, sample } => Ok(Box::new(FloatStats { stddev, sample, n: 0.0, sx: 0.0, sxx: 0.0 })),
        Family::RegrCount => Ok(Box::new(RegrCount { n: 0 })),
        Family::Regr(f) => Ok(Box::new(RegrStats { f, n: 0.0, sx: 0.0, sxx: 0.0, sy: 0.0, syy: 0.0, sxy: 0.0 })),
    }
}

/// Which accumulator and finalizer an aggregate uses, decided from its `pg_aggregate` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// Exact numeric accumulator (`int2/int4/int8/numeric_accum` with `numeric[_poly]_*` finalizers).
    Numeric { stddev: bool, sample: bool },
    /// Youngs-Cramer f64 accumulator (`float4/float8_accum` with `float8_*` finalizers).
    Float { stddev: bool, sample: bool },
    /// `regr_count` (`int8inc_float8_float8`).
    RegrCount,
    /// `float8_regr_accum` with one of the regression/covariance/correlation finalizers.
    Regr(RegrFn),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegrFn {
    Sxx,
    Syy,
    Sxy,
    AvgX,
    AvgY,
    CovarPop,
    CovarSamp,
    Corr,
    R2,
    Slope,
    Intercept,
}

fn classify(agg: &BuiltinAggregate) -> Result<Family> {
    if agg.transfn == "int8inc_float8_float8" {
        return Ok(Family::RegrCount);
    }
    let regr = match agg.finalfn {
        "float8_regr_sxx" => Some(RegrFn::Sxx),
        "float8_regr_syy" => Some(RegrFn::Syy),
        "float8_regr_sxy" => Some(RegrFn::Sxy),
        "float8_regr_avgx" => Some(RegrFn::AvgX),
        "float8_regr_avgy" => Some(RegrFn::AvgY),
        "float8_covar_pop" => Some(RegrFn::CovarPop),
        "float8_covar_samp" => Some(RegrFn::CovarSamp),
        "float8_corr" => Some(RegrFn::Corr),
        "float8_regr_r2" => Some(RegrFn::R2),
        "float8_regr_slope" => Some(RegrFn::Slope),
        "float8_regr_intercept" => Some(RegrFn::Intercept),
        _ => None,
    };
    if let Some(f) = regr {
        return Ok(Family::Regr(f));
    }
    // (stddev, sample)
    let (stddev, sample) = match agg.finalfn {
        "numeric_var_samp" | "numeric_poly_var_samp" | "float8_var_samp" => (false, true),
        "numeric_var_pop" | "numeric_poly_var_pop" | "float8_var_pop" => (false, false),
        "numeric_stddev_samp" | "numeric_poly_stddev_samp" | "float8_stddev_samp" => (true, true),
        "numeric_stddev_pop" | "numeric_poly_stddev_pop" | "float8_stddev_pop" => (true, false),
        other => {
            return Err(PgError::internal(format!(
                "aggregate {} has unexpected final function {}",
                agg.name, other
            )))
        }
    };
    if agg.finalfn.starts_with("float8_") {
        Ok(Family::Float { stddev, sample })
    } else {
        Ok(Family::Numeric { stddev, sample })
    }
}

fn float_overflow() -> PgError {
    PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

fn bad_argument(agg: &str) -> PgError {
    PgError::internal(format!("unexpected argument type for aggregate {}", agg))
}

/// `a + b` with the display scale `max(scale(a), scale(b))`, as PG's `accum_sum_final` keeps it.
fn add_keep_scale(a: &BigDecimal, b: &BigDecimal) -> BigDecimal {
    let s = a.fractional_digit_count().max(b.fractional_digit_count());
    a.with_scale(s) + b.with_scale(s)
}

/// `a - b` with the display scale `max(scale(a), scale(b))`.
fn sub_keep_scale(a: &BigDecimal, b: &BigDecimal) -> BigDecimal {
    let s = a.fractional_digit_count().max(b.fractional_digit_count());
    a.with_scale(s) - b.with_scale(s)
}

fn pow10(e: i64) -> BigInt {
    BigInt::from(10u32).pow(e.max(0) as u32)
}

/// `sqrt_var(arg, rscale)`: the square root of a non-negative finite value, rounded half-up to `rscale`
/// fractional digits with dscale = `rscale`. PG's Newton iteration gives the floor of the root at a
/// precision of at least `rscale + 1` digits, so a half-up decision on the first discarded digit of the
/// floor equals the exact rounding used here.
fn sqrt_numeric(v: &Numeric, rscale: i64) -> Result<Numeric> {
    let d = match v {
        Numeric::Finite(d) => d,
        other => return Ok(other.clone()),
    };
    let (u, s) = d.as_bigint_and_exponent();
    match u.sign() {
        Sign::Minus => Err(PgError::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
            "cannot take square root of a negative number",
        )),
        Sign::NoSign => Ok(Numeric::Finite(BigDecimal::new(BigInt::from(0u32), rscale))),
        Sign::Plus => {
            // Choose h with 2h >= s and h >= rscale + 1, so that sqrt(x) * 10^h = sqrt(u * 10^(2h - s)).
            let h = (rscale + 1).max((s + 1).div_euclid(2));
            let k = 2 * h - s;
            let t = (u * pow10(k)).sqrt();
            let drop = h - rscale;
            let ten = BigInt::from(10u32);
            let r = &t / pow10(drop);
            let first_discarded = (&t / pow10(drop - 1)) % &ten;
            let r = if first_discarded >= BigInt::from(5u32) { r + BigInt::from(1u32) } else { r };
            Ok(Numeric::Finite(BigDecimal::new(r, rscale)))
        }
    }
}

/// Numeric accumulator: PG's `NumericAggState` (N, sumX, sumX², NaN/±Inf counts).
struct NumericStats {
    stddev: bool,
    sample: bool,
    /// Finite inputs (`N`).
    n: i64,
    nan: i64,
    pinf: i64,
    ninf: i64,
    sum_x: BigDecimal,
    sum_x2: BigDecimal,
}

impl NumericStats {
    fn add_finite(&mut self, d: &BigDecimal) {
        self.n += 1;
        self.sum_x = add_keep_scale(&self.sum_x, d);
        let sq = d * d;
        self.sum_x2 = add_keep_scale(&self.sum_x2, &sq);
    }
}

impl AggState for NumericStats {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if args.len() != 1 {
            return Err(PgError::internal("variance-family aggregate expects one argument"));
        }
        match &args[0] {
            Datum::Null => Ok(()),
            Datum::Int2(v) => {
                self.add_finite(&BigDecimal::from(i64::from(*v)));
                Ok(())
            }
            Datum::Int4(v) => {
                self.add_finite(&BigDecimal::from(i64::from(*v)));
                Ok(())
            }
            Datum::Int8(v) => {
                self.add_finite(&BigDecimal::from(*v));
                Ok(())
            }
            Datum::Numeric(Numeric::Finite(d)) => {
                self.add_finite(d);
                Ok(())
            }
            Datum::Numeric(Numeric::NaN) => {
                self.nan += 1;
                Ok(())
            }
            Datum::Numeric(Numeric::PosInf) => {
                self.pinf += 1;
                Ok(())
            }
            Datum::Numeric(Numeric::NegInf) => {
                self.ninf += 1;
                Ok(())
            }
            _ => Err(bad_argument("variance/stddev")),
        }
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        // NULL when N = 0, or N <= 1 for the sample forms (NaN and infinities count as inputs).
        let total = self.n + self.nan + self.pinf + self.ninf;
        if total == 0 || (self.sample && total <= 1) {
            return Ok(Datum::Null);
        }
        if self.nan + self.pinf + self.ninf > 0 {
            return Ok(Datum::Numeric(Numeric::NaN));
        }
        let n = BigDecimal::from(self.n);
        let num = sub_keep_scale(&(&n * &self.sum_x2), &(&self.sum_x * &self.sum_x));
        let zero = BigDecimal::from(0i64);
        if num <= zero {
            // Roundoff guard in PG: a non-positive numerator gives 0 with dscale 0.
            return Ok(Datum::Numeric(Numeric::zero()));
        }
        let den = if self.sample {
            &n * &BigDecimal::from(self.n - 1)
        } else {
            &n * &n
        };
        let num_n = Numeric::Finite(num);
        let den_n = Numeric::from_decimal(den);
        let rscale = arith::select_div_scale(&num_n, &den_n)?;
        let var = arith::div_scale(&num_n, &den_n, rscale)?;
        if !self.stddev {
            return Ok(Datum::Numeric(var));
        }
        Ok(Datum::Numeric(sqrt_numeric(&var, rscale)?))
    }
}

/// Float accumulator: Youngs-Cramer (N, Sx, Sxx), as `float8_accum` / `float4_accum`.
struct FloatStats {
    stddev: bool,
    sample: bool,
    n: f64,
    sx: f64,
    sxx: f64,
}

impl AggState for FloatStats {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if args.len() != 1 {
            return Err(PgError::internal("variance-family aggregate expects one argument"));
        }
        let newval = match &args[0] {
            Datum::Null => return Ok(()),
            Datum::Float4(v) => f64::from(*v),
            Datum::Float8(v) => *v,
            _ => return Err(bad_argument("variance/stddev")),
        };
        let old_n = self.n;
        let n = old_n + 1.0;
        let sx = self.sx + newval;
        let mut sxx = self.sxx;
        if old_n > 0.0 {
            let tmp = newval * n - sx;
            sxx += tmp * tmp / (n * old_n);
            if sx.is_infinite() || sxx.is_infinite() {
                if !self.sx.is_infinite() && !newval.is_infinite() {
                    return Err(float_overflow());
                }
                sxx = f64::NAN;
            }
        } else if newval.is_nan() || newval.is_infinite() {
            sxx = f64::NAN;
        }
        self.n = n;
        self.sx = sx;
        self.sxx = sxx;
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        let var = if self.sample {
            if self.n <= 1.0 {
                return Ok(Datum::Null);
            }
            self.sxx / (self.n - 1.0)
        } else {
            if self.n == 0.0 {
                return Ok(Datum::Null);
            }
            self.sxx / self.n
        };
        Ok(Datum::Float8(if self.stddev { var.sqrt() } else { var }))
    }
}

fn float_arg(d: &Datum) -> Result<f64> {
    match d {
        Datum::Float8(v) => Ok(*v),
        Datum::Float4(v) => Ok(f64::from(*v)),
        _ => Err(bad_argument("regression")),
    }
}

/// `regr_count`: rows where both arguments are non-NULL.
struct RegrCount {
    n: i64,
}

impl AggState for RegrCount {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if args.len() != 2 {
            return Err(PgError::internal("regr_count expects two arguments"));
        }
        if !args[0].is_null() && !args[1].is_null() {
            self.n += 1;
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        Ok(Datum::Int8(self.n))
    }
}

/// Regression accumulator: Youngs-Cramer on X, Y and the cross term, as `float8_regr_accum`.
struct RegrStats {
    f: RegrFn,
    n: f64,
    sx: f64,
    sxx: f64,
    sy: f64,
    syy: f64,
    sxy: f64,
}

impl AggState for RegrStats {
    fn transition(&mut self, args: &[Datum], _ctx: &FuncCtx) -> Result<()> {
        if args.len() != 2 {
            return Err(PgError::internal("regression aggregate expects two arguments"));
        }
        // Rows with a NULL in either argument are skipped.
        if args[0].is_null() || args[1].is_null() {
            return Ok(());
        }
        let y = float_arg(&args[0])?;
        let x = float_arg(&args[1])?;
        let old_n = self.n;
        let n = old_n + 1.0;
        let sx = self.sx + x;
        let sy = self.sy + y;
        let mut sxx = self.sxx;
        let mut syy = self.syy;
        let mut sxy = self.sxy;
        if old_n > 0.0 {
            let tmp_x = x * n - sx;
            let tmp_y = y * n - sy;
            let scale = 1.0 / (n * old_n);
            sxx += tmp_x * tmp_x * scale;
            syy += tmp_y * tmp_y * scale;
            sxy += tmp_x * tmp_y * scale;
            if sx.is_infinite() || sxx.is_infinite() || sy.is_infinite() || syy.is_infinite() || sxy.is_infinite() {
                let x_overflow = (sx.is_infinite() || sxx.is_infinite()) && !self.sx.is_infinite() && !x.is_infinite();
                let y_overflow = (sy.is_infinite() || syy.is_infinite()) && !self.sy.is_infinite() && !y.is_infinite();
                let xy_overflow = sxy.is_infinite()
                    && !self.sx.is_infinite()
                    && !x.is_infinite()
                    && !self.sy.is_infinite()
                    && !y.is_infinite();
                if x_overflow || y_overflow || xy_overflow {
                    return Err(float_overflow());
                }
                if sxx.is_infinite() {
                    sxx = f64::NAN;
                }
                if syy.is_infinite() {
                    syy = f64::NAN;
                }
                if sxy.is_infinite() {
                    sxy = f64::NAN;
                }
            }
        } else {
            if x.is_nan() || x.is_infinite() {
                sxx = f64::NAN;
                sxy = f64::NAN;
            }
            if y.is_nan() || y.is_infinite() {
                syy = f64::NAN;
                sxy = f64::NAN;
            }
        }
        self.n = n;
        self.sx = sx;
        self.sxx = sxx;
        self.sy = sy;
        self.syy = syy;
        self.sxy = sxy;
        Ok(())
    }

    fn finalize(&self, _ctx: &FuncCtx) -> Result<Datum> {
        let (n, sx, sxx, sy, syy, sxy) = (self.n, self.sx, self.sxx, self.sy, self.syy, self.sxy);
        let v: Option<f64> = match self.f {
            RegrFn::Sxx => (n >= 1.0).then_some(sxx),
            RegrFn::Syy => (n >= 1.0).then_some(syy),
            RegrFn::Sxy => (n >= 1.0).then_some(sxy),
            RegrFn::AvgX => (n >= 1.0).then(|| sx / n),
            RegrFn::AvgY => (n >= 1.0).then(|| sy / n),
            RegrFn::CovarPop => (n >= 1.0).then(|| sxy / n),
            RegrFn::CovarSamp => (n >= 2.0).then(|| sxy / (n - 1.0)),
            RegrFn::Corr => {
                if n < 1.0 || sxx == 0.0 || syy == 0.0 {
                    None
                } else {
                    Some(sxy / (sxx * syy).sqrt())
                }
            }
            RegrFn::R2 => {
                if n < 1.0 || sxx == 0.0 {
                    None
                } else if syy == 0.0 {
                    Some(1.0)
                } else {
                    Some((sxy * sxy) / (sxx * syy))
                }
            }
            RegrFn::Slope => (n >= 1.0 && sxx != 0.0).then(|| sxy / sxx),
            RegrFn::Intercept => (n >= 1.0 && sxx != 0.0).then(|| (sy - sx * sxy / sxx) / n),
        };
        Ok(v.map_or(Datum::Null, Datum::Float8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_data;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::numeric::io::format_numeric;
    use crate::registry;

    /// Run one aggregate over `rows` (one `Vec<Datum>` per input row) and return the final datum.
    fn run(oid: u32, arg_types: Vec<TypeId>, rows: Vec<Vec<Datum>>) -> Datum {
        let agg = catalog_data::builtin_aggregate(oid).expect("aggregate row");
        let spec = AggSpec { agg_oid: oid, arg_types, result_type: TypeId::NUMERIC, direct_args: Vec::new() };
        let sess = TestSession::default();
        let io = IoConfig::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &io,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::NUMERIC,
            collation: 0,
        };
        let mut st = make(agg, &spec, &ctx).expect("make");
        for r in &rows {
            st.transition(r, &ctx).expect("transition");
        }
        st.finalize(&ctx).expect("finalize")
    }

    fn numeric_text(d: &Datum) -> String {
        match d {
            Datum::Numeric(n) => format_numeric(n).expect("format"),
            other => panic!("expected numeric, got {:?}", other),
        }
    }

    fn int4_rows(vals: &[i32]) -> Vec<Vec<Datum>> {
        vals.iter().map(|v| vec![Datum::Int4(*v)]).collect()
    }

    #[test]
    fn stddev_and_variance_oracle_values() {
        // stddev(int4) = 2155 (numeric_poly_stddev_samp); variance(int4) = 2149.
        let d = run(2155, vec![TypeId::INT4], int4_rows(&[1, 2, 4]));
        assert_eq!(numeric_text(&d), "1.5275252316519467");
        let d = run(2149, vec![TypeId::INT4], int4_rows(&[1, 2, 4]));
        assert_eq!(numeric_text(&d), "2.3333333333333333");
    }

    #[test]
    fn var_pop_of_one_row_is_zero_and_samp_is_null() {
        let d = run(2723, vec![TypeId::NUMERIC], vec![vec![Datum::Numeric(Numeric::from_i64(5))]]);
        assert_eq!(numeric_text(&d), "0");
        let d = run(2153, vec![TypeId::NUMERIC], vec![vec![Datum::Numeric(Numeric::from_i64(5))]]);
        assert_eq!(d, Datum::Null);
    }

    #[test]
    fn zero_rows_is_null() {
        assert_eq!(run(2149, vec![TypeId::INT4], Vec::new()), Datum::Null);
        assert_eq!(run(2723, vec![TypeId::NUMERIC], Vec::new()), Datum::Null);
    }

    #[test]
    fn nulls_are_skipped_and_nan_propagates() {
        let rows = vec![vec![Datum::Int4(1)], vec![Datum::Null], vec![Datum::Int4(2)], vec![Datum::Int4(4)]];
        let d = run(2155, vec![TypeId::INT4], rows);
        assert_eq!(numeric_text(&d), "1.5275252316519467");
        let rows = vec![
            vec![Datum::Numeric(Numeric::from_i64(1))],
            vec![Datum::Numeric(Numeric::NaN)],
        ];
        assert_eq!(run(2153, vec![TypeId::NUMERIC], rows), Datum::Numeric(Numeric::NaN));
    }

    #[test]
    fn stddev_pop_float8() {
        let rows = vec![vec![Datum::Float8(1.0)], vec![Datum::Float8(2.0)], vec![Datum::Float8(4.0)]];
        match run(2728, vec![TypeId::FLOAT8], rows) {
            Datum::Float8(v) => assert!((v - 1.247219128924647).abs() < 1e-12, "got {}", v),
            other => panic!("expected float8, got {:?}", other),
        }
    }

    #[test]
    fn corr_and_regr_slope_of_scaled_values() {
        let rows: Vec<Vec<Datum>> = [1.0f64, 2.0, 4.0]
            .iter()
            .map(|x| vec![Datum::Float8(x * 2.0), Datum::Float8(*x)])
            .collect();
        match run(2829, vec![TypeId::FLOAT8, TypeId::FLOAT8], rows.clone()) {
            Datum::Float8(v) => assert!((v - 1.0).abs() < 1e-12, "corr {}", v),
            other => panic!("expected float8, got {:?}", other),
        }
        assert_eq!(run(2825, vec![TypeId::FLOAT8, TypeId::FLOAT8], rows), Datum::Float8(2.0));
    }

    #[test]
    fn regr_count_skips_pairs_with_null() {
        let rows = vec![
            vec![Datum::Float8(1.0), Datum::Float8(1.0)],
            vec![Datum::Null, Datum::Float8(2.0)],
            vec![Datum::Float8(3.0), Datum::Null],
        ];
        assert_eq!(run(2818, vec![TypeId::FLOAT8, TypeId::FLOAT8], rows), Datum::Int8(1));
        assert_eq!(run(2818, vec![TypeId::FLOAT8, TypeId::FLOAT8], Vec::new()), Datum::Int8(0));
    }

    #[test]
    fn regr_on_empty_input_is_null() {
        assert_eq!(run(2825, vec![TypeId::FLOAT8, TypeId::FLOAT8], Vec::new()), Datum::Null);
        assert_eq!(run(2827, vec![TypeId::FLOAT8, TypeId::FLOAT8], Vec::new()), Datum::Null);
    }

    #[test]
    fn sqrt_numeric_rounds_half_up() {
        // sqrt(2) = 1.41421356237..., to 4 places -> 1.4142; sqrt(0.0625) = 0.25 exactly.
        let two = Numeric::Finite(BigDecimal::from(2i64));
        assert_eq!(format_numeric(&sqrt_numeric(&two, 4).unwrap()).unwrap(), "1.4142");
        let q = Numeric::Finite(BigDecimal::new(BigInt::from(625u32), 4));
        assert_eq!(format_numeric(&sqrt_numeric(&q, 4).unwrap()).unwrap(), "0.2500");
    }
}
