//! Sequences (`pg_class` relkind 'S' + `pg_sequence`).
//!
//! The catalog holds the DEFINITION (start/increment/bounds/cache/cycle/owner). The mutable STATE
//! (`last_value`, `is_called`, `log_cnt`) lives in sequence space 2 and is owned by
//! pgarbor-core::sequence (non-transactional `nextval`, ARCHITECTURE §7). `ALTER SEQUENCE RESTART`
//! is therefore a catalog write (definition) plus a core state write.
//!
//! Plain data, frozen (v1).
use super::common::Acl;
use super::relation::Persistence;
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};

/// A sequence definition.
///
/// Defaults (PG `init_params`), for `AS bigint` (default type) and increment > 0:
/// `start = min`, `min = 1`, `max = i64::MAX`; for increment < 0: `max = -1`, `min = i64::MIN`,
/// `start = max`. `AS integer`/`smallint` bound the defaults to that type's range.
/// `cache = 1`, `cycle = false`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequence {
    /// pg_class OID (also the `[relid]` prefix nothing is stored under: sequence state is in space 2).
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    #[serde(default)]
    pub persistence: Persistence,
    /// `seqtypid`: int2 (21), int4 (23) or int8 (20).
    pub type_oid: Oid,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    /// OWNED BY table.column (`serial` and identity sequences are owned by their column).
    /// Serial: AUTO dependency on the column; identity: INTERNAL dependency.
    #[serde(default)]
    pub owned_by: Option<(Oid, i16)>,
    /// True for identity sequences (dropped/renamed with the identity column; INTERNAL dep).
    #[serde(default)]
    pub identity: bool,
    #[serde(default)]
    pub acl: Option<Acl>,
}
