//! PostgreSQL 18 type system for PGArbor (ARCHITECTURE §8, seed tasks TYPE-01..TYPE-15).
//!
//! Map of the crate (see contracts/crates/pgarbor-types.md for the full contract):
//! - values: [`datum::Datum`], [`datum::TypeId`], [`numeric::Numeric`], [`json::Jsonb`], [`array::Array`]
//! - builtin catalog rows (generated): [`catalog_data`], [`oids`]
//! - type metadata: [`registry`] (`lookup_type`, `type_by_name`, `builtin_env`), SQL names: [`names`],
//!   typmods: [`typmod`]
//! - contexts: [`context`] (`TypeEnv`, `SessionContext`, `IoConfig`, `IoCtx`, `FuncCtx`)
//! - text/binary I/O: [`io`] (`input`, `output`, `send`, `recv`)
//! - casts: [`cast`] (`find_cast`, `plan_cast`, `apply_cast`); resolution: [`resolve`]
//!   (`resolve_function`, `resolve_operator`, `select_common_type`, polymorphism); [`ops`] re-exports
//!   the operator API
//! - functions: [`func`] (`implementation`, `FuncHandle::call`), aggregates: [`agg`]
//! - semantics: [`logic`] (three-valued logic), [`compare`] (ordering/equality/hash)
//! - storage encodings: [`keycodec`] (memcomparable index keys), [`tuple`] (row encoding)
//!
//! The engine is synchronous; nothing here allocates threads or does I/O.
#![allow(clippy::too_many_arguments)]

pub mod agg;
pub mod array;
pub mod bytea;
pub mod cast;
pub mod catalog_data;
pub mod compare;
pub mod context;
pub mod crypto;
pub mod datetime;
pub mod datum;
pub mod enum_type;
pub mod extra_types;
pub mod formatting;
pub mod func;
pub mod io;
pub mod json;
pub mod keycodec;
pub mod logic;
pub mod names;
pub mod numeric;
pub mod oids;
pub mod record;
pub mod registry;
pub mod resolve;
pub mod scalar;
pub mod system_funcs;
pub mod text;
pub mod tuple;
pub mod typmod;
pub mod uuid;

/// Operator resolution API (alias of [`resolve::oper`]).
pub mod ops {
    pub use crate::resolve::oper::*;
}

pub use context::{FuncCtx, IoConfig, IoCtx, SessionContext, TypeEnv};
pub use datum::{Datum, Interval, TypeId};
pub use json::Jsonb;
pub use numeric::Numeric;
pub use array::Array;
