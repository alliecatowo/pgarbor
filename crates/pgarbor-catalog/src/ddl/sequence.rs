//! CREATE SEQUENCE / ALTER SEQUENCE (definition only). Lease CAT-16. PG sources:
//! `commands/sequence.c` (`DefineSequence`, `AlterSequence`, `init_params`, `process_owned_by`,
//! `sequence_options`), `catalog/pg_sequence.c`.
//!
//! The catalog stores the DEFINITION ([`Sequence`]); the mutable state lives in space 2 and is
//! managed by `pgarbor_core::sequence::store` (non-transactional `nextval`). So:
//! - CREATE SEQUENCE queues `FollowUp::CreateSequenceState{sequence}` (the executor reads the
//!   definition and calls `store::create` with start/increment/min/max/cache/cycle);
//! - ALTER SEQUENCE RESTART queues `FollowUp::RestartSequence{sequence, value}`; changing
//!   increment/bounds/cycle/cache changes the definition (the core re-reads it per `nextval`
//!   through its params struct; the executor passes new params with the follow-up if needed);
//! - DROP is `drop.rs` (queues `DropSequenceState`).
//! Renaming / SET SCHEMA / OWNER TO are `alter_column::{rename_relation, set_relation_schema,
//! alter_relation_owner}`.
//!
//! ## [`resolve_sequence_options`] (PG `init_params`), pure and unit-testable
//! Inputs: the written options, the existing sequence (ALTER) or `None` (CREATE), and the
//! `forced_type` (identity/serial columns force the column's type). Result: the full definition
//! values. Rules (oracle-verified for PG 18.6):
//! - type: `AS smallint|integer|bigint` (21/23/20) else 22023 `errors::sequence_bad_type`; default
//!   bigint (20) for CREATE, unchanged for ALTER.
//! - `increment`: default 1; 0 -> 22023 `INCREMENT must not be zero`.
//! - ascending (increment > 0) defaults: `min = 1`, `max = type max`, `start = min`.
//!   Descending: `max = -1`, `min = type min`, `start = max`. `NO MINVALUE`/`NO MAXVALUE`
//!   (`SeqBound::Default`) reset to those defaults.
//! - explicit bounds are checked against the type range: 22023 `MAXVALUE (100000) is out of range
//!   for sequence data type smallint` / `MINVALUE (...)`; `START` and `RESTART` likewise 22023
//!   `START value (x) cannot be less than MINVALUE (y)` / `... greater than MAXVALUE`;
//!   `MINVALUE (10) must be less than MAXVALUE (5)` when min >= max.
//! - ALTER changing the TYPE: if the old min/max equalled the old type's defaults they move to the
//!   new type's defaults; otherwise they are kept and range-checked (error above).
//! - `cache` default 1; < 1 -> 22023 `CACHE (0) must be greater than zero`. `cycle` default false.
//! - ALTER without START keeps start; with `restart` the value is `Some(v)` or the (new) start.
//! Return type: `ResolvedSequence`.
//!
//! ## CREATE SEQUENCE checks
//! schema creatable + CREATE privilege; name free (`check_new_relation_name`, 42P07 / NOTICE
//! skipping with `if_not_exists`); `owned_by (table, attnum)`: the table must be a table or foreign
//! table (55000 `referenced relation "x" is not a table or foreign table`), owned by the same owner
//! as the sequence (55000 `errors::sequence_owner_mismatch`), in the same schema (55000
//! `errors::sequence_schema_mismatch`); `OWNED BY NONE` = `None`. Sequence kind `Sequence::identity`
//! = false, `owned_by` set => AUTO dependency on the column (serial semantics).
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

/// Fully resolved sequence parameters.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedSequence {
    pub type_oid: Oid,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    /// ALTER ... RESTART [WITH n]: the value `nextval` must return next.
    pub restart: Option<i64>,
}

/// PG `init_params` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn resolve_sequence_options(
    options: &SequenceOptions,
    existing: Option<&Sequence>,
    forced_type: Option<Oid>,
) -> Result<ResolvedSequence> {
    Err(PgError::unimplemented("CAT-16", "sequence option resolution"))
}

impl<'a> CatalogTxn<'a> {
    /// CREATE SEQUENCE. `Ok(None)` for `IF NOT EXISTS` skip; otherwise the new OID. Queues
    /// `CreateSequenceState`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_sequence(&mut self, spec: &CreateSequenceSpec) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-16", "CREATE SEQUENCE"))
    }

    /// ALTER SEQUENCE options (owner check, `resolve_sequence_options` against the existing
    /// definition). `owned_by`: `None` = unchanged, `Some(None)` = OWNED BY NONE, `Some(Some((t, a)))` =
    /// OWNED BY t.a (same checks as CREATE; on an identity sequence this is refused with 55000
    /// `errors::cannot_alter_identity_sequence("change ownership", seq, table)`). Direct ALTER
    /// SEQUENCE of option values on an identity sequence is allowed. Queues
    /// `FollowUp::RestartSequence{sequence, value}` when `options.restart` is set.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_sequence(&mut self, seq: Oid, options: &SequenceOptions, owned_by: Option<Option<(Oid, i16)>>) -> Result<()> {
        Err(PgError::unimplemented("CAT-16", "ALTER SEQUENCE"))
    }

    /// Used by `table`/`alter_column`: create the implicit sequence of a serial/identity column
    /// (no privilege or IF NOT EXISTS handling; the name is already chosen). Returns the OID and
    /// queues `CreateSequenceState`.
    // CONTRACT
    #[allow(unused_variables)]
    pub(crate) fn create_column_sequence(
        &mut self,
        namespace: Oid,
        name: &str,
        owner: Oid,
        persistence: Persistence,
        type_oid: Oid,
        options: &SequenceOptions,
        owned_by: (Oid, i16),
        identity: bool,
    ) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-16", "implicit column sequence"))
    }
}
