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
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use pgarbor_common::{PgError, Result};

/// Instantiate a basic aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    Err(PgError::unimplemented("TYP-45", format!("aggregate {}", agg.name)))
}
