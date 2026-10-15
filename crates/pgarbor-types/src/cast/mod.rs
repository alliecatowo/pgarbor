//! Casts (TYPE-03): finding a coercion pathway between two types (`lookup.rs`, PG
//! `find_coercion_pathway` + `find_typmod_coercion_function`) and executing it (`apply.rs`).
//!
//! ```ignore
//! let plan = cast::plan_cast(env, TypeId::INT4, TypeId::new(oids::NUMERIC, typmod), CastContext::Assignment)?;
//! let v = cast::apply_cast(&plan, Datum::Int4(7), &fctx)?;
//! ```
pub mod apply;
pub mod lookup;

pub use apply::{apply_cast, cast_datum};
pub use lookup::{find_cast, plan_cast, CastPathway, CastPlan, CastStep};
pub use crate::catalog_data::CastContext;
