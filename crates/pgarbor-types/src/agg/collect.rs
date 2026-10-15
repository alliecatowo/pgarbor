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
use super::{AggSpec, AggState};
use crate::catalog_data::BuiltinAggregate;
use crate::context::FuncCtx;
use pgarbor_common::{PgError, Result};

/// Instantiate a collecting aggregate.
#[allow(unused_variables)]
pub(crate) fn make(agg: &BuiltinAggregate, spec: &AggSpec, ctx: &FuncCtx) -> Result<Box<dyn AggState>> {
    Err(PgError::unimplemented("TYP-46", format!("aggregate {}", agg.name)))
}
