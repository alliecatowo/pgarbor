//! Type modifiers: typmodin/typmodout semantics and length/precision coercion (TYPE-03). Lease TYP-02.
//!
//! PG18 reference: `utils/adt/varchar.c` (`anychar_typmodin`, `varchar`, `bpchar` length coercion
//! functions), `numeric.c` (`numerictypmodin`, `numeric` coercion), `date.c`/`timestamp.c`
//! (`anytime_typmodin`, `anytimestamp_typmod_check`, `intervaltypmodin`, `time_scale`,
//! `timestamp_scale`, `interval_scale`), `varbit.c` (bit lengths).
//!
//! Encodings (frozen, PG-identical): varchar/bpchar(n) → n + 4 (VARHDRSZ); numeric(p,s) →
//! `((p << 16) | (s & 0x7ff)) + 4` (s may be negative in PG15+: sign-extend 11 bits when decoding);
//! numeric(p) → scale 0; time/timetz/timestamp/timestamptz(p) → p; interval → `(range << 16) | precision`
//! with range bits from `datetime::interval_fields` and precision 0xFFFF = unspecified; bit(n) → n.
//!
//! `typmod_in(oid, mods)` errors/warnings (oracle-verified):
//! - varchar(0) → 22023 `length for type varchar must be at least 1`; > 10485760 → 22023
//!   `length for type varchar cannot exceed 10485760` (bpchar: same with `char`).
//! - numeric: `NUMERIC precision 1001 must be between 1 and 1000` (22023); scale outside -1000..=1000 →
//!   `NUMERIC scale %d must be between -1000 and 1000`; more than 2 mods → `invalid NUMERIC type modifier`.
//!   (numeric(5,6) is VALID in PG18.)
//! - time/timestamp(p): p < 0 → 22023 `TIMESTAMP(-1) precision must not be negative`
//!   (`TIME(-1) WITH TIME ZONE precision must not be negative`); p > 6 → WARNING (returned, not an
//!   error; severity Warning, SQLSTATE 01000) `TIMESTAMP(7) precision reduced to maximum allowed, 6`,
//!   `TIMESTAMP(7) WITH TIME ZONE precision reduced to maximum allowed, 6`, `TIME(7) precision reduced to
//!   maximum allowed, 6`, and typmod 6 (all oracle-verified).
//! - types without typmodin (text, int4, ...) → 42601 `type modifier is not allowed for type "text"`.
//!
//! `typmod_out(oid, typmod)`: the suffix PG prints: `(10)`, `(10,2)`, `(3)`, interval
//! ` year to month`, `(2)`, ` day to second(3)`.
//!
//! `enforce_typmod(ctx, ty, value, explicit)` (PG length-coercion functions):
//! varchar/bpchar → `text::io::apply_char_typmod`; numeric → `numeric::io::apply_typmod`;
//! time/timetz → `datetime::time::adjust_time_precision`; timestamp/tz →
//! `datetime::timestamp::adjust_timestamp_precision`; interval → `datetime::interval::apply_interval_typmod`;
//! other types → unchanged. NULL → NULL.
//! Owned procs (ImplModule::Typmod): `varchar(varchar, int4, bool)` (prosrc `varchar`), `bpchar(bpchar,
//! int4, bool)`, `numeric(numeric, int4)`, `time_scale`, `timetz_scale`, `timestamp_scale`,
//! `timestamptz_scale`, `interval_scale` — thin wrappers over `enforce_typmod`.
use crate::catalog_data::BuiltinProc;
use crate::context::FuncCtx;
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{Oid, PgError, Result};

/// Decode a numeric typmod into (precision, scale); None for typmod < 4 (unconstrained).
pub fn numeric_typmod_parts(typmod: i32) -> Option<(i32, i32)> {
    if typmod < 4 {
        return None;
    }
    let t = typmod - 4;
    let precision = (t >> 16) & 0xffff;
    // scale is an 11-bit two's complement field
    let scale = ((t & 0x7ff) ^ 1024) - 1024;
    Some((precision, scale))
}

/// Encode numeric (precision, scale) as a typmod.
pub fn numeric_typmod(precision: i32, scale: i32) -> i32 {
    ((precision << 16) | (scale & 0x7ff)) + 4
}

/// Character length n for a varchar/bpchar typmod, None when unconstrained.
pub fn char_typmod_length(typmod: i32) -> Option<i32> {
    if typmod >= 4 { Some(typmod - 4) } else { None }
}

/// PG typmodin semantics for `oid` with integer modifiers; returns (typmod, warnings).
// CONTRACT
#[allow(unused_variables)]
pub fn typmod_in(oid: Oid, mods: &[i32]) -> Result<(i32, Vec<PgError>)> {
    Err(PgError::unimplemented("TYP-02", "type modifiers"))
}

/// PG typmodout suffix for `oid` (empty string when typmod < 0).
// CONTRACT
#[allow(unused_variables)]
pub fn typmod_out(oid: Oid, typmod: i32) -> Result<String> {
    Err(PgError::unimplemented("TYP-02", "type modifier output"))
}

/// Coerce `value` (already of type `ty.oid`) to `ty.typmod` (see module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn enforce_typmod(ctx: &FuncCtx, ty: TypeId, value: Datum, explicit: bool) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-02", "length coercion"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Typmod-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-02", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_typmod_roundtrip() {
        assert_eq!(numeric_typmod_parts(numeric_typmod(10, 2)), Some((10, 2)));
        assert_eq!(numeric_typmod_parts(numeric_typmod(5, -3)), Some((5, -3)));
        assert_eq!(numeric_typmod(10, 2), (10 << 16 | 2) + 4);
        assert_eq!(numeric_typmod_parts(-1), None);
        assert_eq!(char_typmod_length(14), Some(10));
    }
}
