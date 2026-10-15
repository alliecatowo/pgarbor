//! PostgreSQL `numeric` (TYPE-05): arbitrary precision decimal with NaN and ±Infinity.
//!
//! Representation: `Numeric::Finite(BigDecimal)` where the BigDecimal's *scale* is PostgreSQL's
//! display scale (`dscale`): `1.50` has scale 2 and prints `1.50`; `'1e3'::numeric` prints `1000`
//! (scale 0). INVARIANT (enforced by every constructor and every worker-written operation):
//! `0 <= scale <= NUMERIC_MAX_DSCALE`; a negative bigdecimal scale must be normalized with
//! `with_scale(0)` before being wrapped (use `Numeric::from_decimal`).
//!
//! Semantics (PG18 `utils/adt/numeric.c`):
//! - Ordering: `-Infinity < finite values < +Infinity < NaN`; `NaN = NaN` is TRUE (unlike float IEEE).
//! - Equality is by value: `1.5 = 1.50`. `Hash` (via `compare::hash_datum`) and the memcomparable key
//!   must therefore ignore trailing fractional zeros.
//! - Arithmetic result scales follow PG's rules (add/sub: max(dscale); mul: sum of dscales;
//!   div: `select_div_scale`), rounding is half away from zero (`RoundingMode::HalfUp` in bigdecimal).
//!   Never rely on bigdecimal's default context precision.
//!
//! Files: `io.rs` (text/binary I/O + typmod), `arith.rs` (core arithmetic), `transcendental.rs`
//! (sqrt/exp/ln/log/power), `funcs.rs` (pg_proc implementations + casts).
use bigdecimal::BigDecimal;
use std::cmp::Ordering;

pub mod arith;
pub mod funcs;
pub mod io;
pub mod transcendental;

pub use bigdecimal;

/// Maximum display scale PG can store (`NUMERIC_DSCALE_MAX`).
pub const NUMERIC_MAX_DSCALE: i64 = 0x3FFF;
/// Maximum precision accepted in a `numeric(p,s)` typmod (`NUMERIC_MAX_PRECISION`).
pub const NUMERIC_MAX_PRECISION: i32 = 1000;
/// Minimum scale accepted in a typmod (`NUMERIC_MIN_SCALE`, PG15+ allows negative scale).
pub const NUMERIC_MIN_SCALE: i32 = -1000;
/// Maximum scale accepted in a typmod (`NUMERIC_MAX_SCALE`).
pub const NUMERIC_MAX_SCALE: i32 = 1000;
/// Max digits before the decimal point (`NUMERIC_WEIGHT_MAX` * 4 digits ≈ 131072).
pub const NUMERIC_MAX_INT_DIGITS: i64 = 131072;
/// `NUMERIC_MIN_SIG_DIGITS`: minimum significant digits for division / transcendental results.
pub const NUMERIC_MIN_SIG_DIGITS: i64 = 16;
/// `NUMERIC_MAX_DISPLAY_SCALE` (= NUMERIC_MAX_PRECISION): cap for computed result scales.
pub const NUMERIC_MAX_DISPLAY_SCALE: i64 = 1000;
/// `NUMERIC_MIN_DISPLAY_SCALE`: floor for computed result scales of division / transcendentals.
pub const NUMERIC_MIN_DISPLAY_SCALE: i64 = 0;

/// A PostgreSQL numeric value.
// CONTRACT
#[derive(Clone, Debug)]
pub enum Numeric {
    NaN,
    PosInf,
    NegInf,
    /// Finite value; bigdecimal scale = PG dscale (always >= 0).
    Finite(BigDecimal),
}

impl Numeric {
    /// Wrap a BigDecimal, normalizing a negative scale to 0 (`1E+3` → `1000`).
    pub fn from_decimal(d: BigDecimal) -> Numeric {
        let (_, scale) = d.as_bigint_and_exponent();
        if scale < 0 { Numeric::Finite(d.with_scale(0)) } else { Numeric::Finite(d) }
    }
    pub fn zero() -> Numeric {
        Numeric::Finite(BigDecimal::from(0))
    }
    pub fn from_i64(v: i64) -> Numeric {
        Numeric::Finite(BigDecimal::from(v))
    }
    pub fn from_i128(v: i128) -> Numeric {
        Numeric::Finite(BigDecimal::from(v))
    }
    pub fn is_nan(&self) -> bool {
        matches!(self, Numeric::NaN)
    }
    pub fn is_infinite(&self) -> bool {
        matches!(self, Numeric::PosInf | Numeric::NegInf)
    }
    pub fn is_finite(&self) -> bool {
        matches!(self, Numeric::Finite(_))
    }
    pub fn as_decimal(&self) -> Option<&BigDecimal> {
        match self {
            Numeric::Finite(d) => Some(d),
            _ => None,
        }
    }
    /// Display scale (0 for specials).
    pub fn dscale(&self) -> i64 {
        match self {
            Numeric::Finite(d) => d.as_bigint_and_exponent().1.max(0),
            _ => 0,
        }
    }
    /// PG total ordering: -Inf < finite < +Inf < NaN, NaN == NaN, finite by value.
    pub fn pg_cmp(&self, other: &Numeric) -> Ordering {
        fn rank(n: &Numeric) -> u8 {
            match n {
                Numeric::NegInf => 0,
                Numeric::Finite(_) => 1,
                Numeric::PosInf => 2,
                Numeric::NaN => 3,
            }
        }
        match (self, other) {
            (Numeric::Finite(a), Numeric::Finite(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl PartialEq for Numeric {
    /// PG equality (`NaN = NaN`, `1.5 = 1.50`).
    fn eq(&self, other: &Numeric) -> bool {
        self.pg_cmp(other) == Ordering::Equal
    }
}
