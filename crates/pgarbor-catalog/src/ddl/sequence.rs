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
//! skipping with `if_not_exists`); `owned_by (table, attnum)`: the table must be a table, foreign
//! table, view or partitioned table (42809 `sequence cannot be owned by relation "x"`, see
//! `check_owned_by_target`), owned by the same owner
//! as the sequence (55000 `errors::sequence_owner_mismatch`), in the same schema (55000
//! `errors::sequence_schema_mismatch`); `OWNED BY NONE` = `None`. Sequence kind `Sequence::identity`
//! = false, `owned_by` set => AUTO dependency on the column (serial semantics).
use super::spec::*;
use crate::errors;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};

const INT2: Oid = 21;
const INT4: Oid = 23;
const INT8: Oid = 20;

/// `(min, max)` of an integer sequence type; 22023 for any other type OID.
fn type_range(type_oid: Oid) -> Result<(i64, i64)> {
    match type_oid {
        INT2 => Ok((i16::MIN as i64, i16::MAX as i64)),
        INT4 => Ok((i32::MIN as i64, i32::MAX as i64)),
        INT8 => Ok((i64::MIN, i64::MAX)),
        _ => Err(errors::sequence_bad_type()),
    }
}

/// `format_type` name of a sequence type.
fn type_name(type_oid: Oid) -> &'static str {
    match type_oid {
        INT2 => "smallint",
        INT4 => "integer",
        _ => "bigint",
    }
}

/// Default MAXVALUE (PG `init_params`): ascending (or a reset type-default maximum) uses the type
/// maximum, descending uses -1.
fn default_max(increment: i64, type_max: i64, reset: bool) -> i64 {
    if increment > 0 || reset {
        type_max
    } else {
        -1
    }
}

/// Default MINVALUE: descending (or a reset type-default minimum) uses the type minimum,
/// ascending uses 1.
fn default_min(increment: i64, type_min: i64, reset: bool) -> i64 {
    if increment < 0 || reset {
        type_min
    } else {
        1
    }
}

/// Plural word of PG `errdetail_relkind_not_supported`.
fn relkind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table => "tables",
        RelKind::Index => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::PartitionedTable => "partitioned tables",
        RelKind::PartitionedIndex => "partitioned indexes",
        RelKind::Toast => "TOAST tables",
    }
}

/// Assemble the stored definition from resolved values.
#[allow(clippy::too_many_arguments)]
fn build_sequence(
    oid: Oid,
    name: &str,
    namespace: Oid,
    owner: Oid,
    persistence: Persistence,
    r: &ResolvedSequence,
    owned_by: Option<(Oid, i16)>,
    identity: bool,
) -> Sequence {
    Sequence {
        oid,
        name: name.to_string(),
        namespace,
        owner,
        persistence,
        type_oid: r.type_oid,
        start: r.start,
        increment: r.increment,
        min: r.min,
        max: r.max,
        cache: r.cache,
        cycle: r.cycle,
        owned_by,
        identity,
        acl: None,
    }
}

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
pub fn resolve_sequence_options(
    options: &SequenceOptions,
    existing: Option<&Sequence>,
    forced_type: Option<Oid>,
) -> Result<ResolvedSequence> {
    let for_identity = forced_type.is_some() || existing.is_some_and(|s| s.identity);

    // AS type. On ALTER ... AS, a min/max that was the default of the OLD type moves to the NEW
    // type's default (PG `reset_max_value` / `reset_min_value`); explicit bounds are kept.
    let mut reset_max = false;
    let mut reset_min = false;
    let type_oid = match options.type_oid {
        Some(t) => {
            if !matches!(t, INT2 | INT4 | INT8) {
                return Err(if for_identity { errors::identity_bad_type() } else { errors::sequence_bad_type() });
            }
            if let Some(old) = existing {
                let (old_min, old_max) = type_range(old.type_oid)?;
                reset_max = old.max == old_max;
                reset_min = old.min == old_min;
            }
            t
        }
        None => match existing {
            Some(old) => old.type_oid,
            None => forced_type.unwrap_or(INT8),
        },
    };
    let (type_min, type_max) = type_range(type_oid)?;

    // INCREMENT BY
    let increment = match options.increment {
        Some(0) => return Err(errors::sequence_increment_zero()),
        Some(i) => i,
        None => existing.map_or(1, |s| s.increment),
    };

    // MAXVALUE (NO MAXVALUE = `SeqBound::Default`)
    let max = match options.maxvalue {
        Some(SeqBound::Value(v)) => v,
        Some(SeqBound::Default) => default_max(increment, type_max, reset_max),
        None => match existing {
            Some(old) if !reset_max => old.max,
            _ => default_max(increment, type_max, reset_max),
        },
    };
    if max < type_min || max > type_max {
        return Err(errors::sequence_bound_out_of_range("MAXVALUE", max, type_name(type_oid)));
    }

    // MINVALUE
    let min = match options.minvalue {
        Some(SeqBound::Value(v)) => v,
        Some(SeqBound::Default) => default_min(increment, type_min, reset_min),
        None => match existing {
            Some(old) if !reset_min => old.min,
            _ => default_min(increment, type_min, reset_min),
        },
    };
    if min < type_min || min > type_max {
        return Err(errors::sequence_bound_out_of_range("MINVALUE", min, type_name(type_oid)));
    }
    if min >= max {
        return Err(errors::sequence_min_not_less_than_max(min, max));
    }

    // START WITH (ALTER without START keeps the start value)
    let start = match options.start {
        Some(v) => v,
        None => match existing {
            Some(old) => old.start,
            None => {
                if increment > 0 {
                    min
                } else {
                    max
                }
            }
        },
    };
    if start < min {
        return Err(errors::sequence_start_below_min(start, min));
    }
    if start > max {
        return Err(errors::sequence_start_above_max(start, max));
    }

    // RESTART [WITH n]: bare RESTART means the (new) start value.
    let restart = match options.restart {
        None => None,
        Some(Some(v)) => Some(v),
        Some(None) => Some(start),
    };
    // Crosscheck of the restart value. PG also checks the CURRENT value against changed bounds on
    // ALTER; that value lives in sequence state, which this pure function does not see (see the
    // executor/core follow-up contract).
    if let Some(v) = restart {
        if v < min {
            return Err(errors::sequence_restart_below_min(v, min));
        }
        if v > max {
            return Err(errors::sequence_restart_above_max(v, max));
        }
    }

    // CACHE
    let cache = match options.cache {
        Some(c) if c <= 0 => return Err(errors::sequence_cache_not_positive(c)),
        Some(c) => c,
        None => existing.map_or(1, |s| s.cache),
    };

    // CYCLE
    let cycle = options.cycle.unwrap_or_else(|| existing.is_some_and(|s| s.cycle));

    Ok(ResolvedSequence { type_oid, start, increment, min, max, cache, cycle, restart })
}

impl<'a> CatalogTxn<'a> {
    /// CREATE SEQUENCE. `Ok(None)` for `IF NOT EXISTS` skip; otherwise the new OID. Queues
    /// `CreateSequenceState`.
    // CONTRACT
    pub fn create_sequence(&mut self, spec: &CreateSequenceSpec) -> Result<Option<Oid>> {
        let resolved = resolve_sequence_options(&spec.options, None, None)?;
        self.check_creatable_namespace(spec.namespace, &spec.name)?;
        self.require_schema_create(spec.namespace)?;
        if !self.check_new_relation_name(spec.namespace, &spec.name, spec.if_not_exists)? {
            return Ok(None);
        }
        if let Some((table, _attnum)) = spec.owned_by {
            self.check_owned_by_target(spec.owner, spec.namespace, table)?;
        }
        let oid = self.new_oid()?;
        let seq = build_sequence(
            oid,
            &spec.name,
            spec.namespace,
            spec.owner,
            spec.persistence,
            &resolved,
            spec.owned_by,
            false,
        );
        self.put(CatalogObject::Sequence(seq))?;
        self.record_created(ObjectAddress::relation(oid));
        self.follow_up(FollowUp::CreateSequenceState { sequence: oid });
        Ok(Some(oid))
    }

    /// PG `process_owned_by` checks for `OWNED BY table.column` (no writes): the table must be a
    /// table, foreign table, view or partitioned table (oracle-verified for PG 18: other kinds ->
    /// 42809 `sequence cannot be owned by relation "x"` + DETAIL `This operation is not supported for
    /// <kinds>.`), then the same owner (55000) and the same schema (55000).
    fn check_owned_by_target(&self, seq_owner: Oid, seq_namespace: Oid, table: Oid) -> Result<()> {
        let data = self.data();
        let (name, kind_plural) = if let Some(rel) = data.relations.get(&table) {
            match rel.kind {
                RelKind::Table | RelKind::ForeignTable | RelKind::View | RelKind::PartitionedTable => {
                    if rel.owner != seq_owner {
                        return Err(errors::sequence_owner_mismatch());
                    }
                    if rel.namespace != seq_namespace {
                        return Err(errors::sequence_schema_mismatch());
                    }
                    return Ok(());
                }
                kind => (rel.name.as_str(), relkind_plural(kind)),
            }
        } else if let Some(seq) = data.sequences.get(&table) {
            (seq.name.as_str(), "sequences")
        } else if let Some(idx) = data.indexes.get(&table) {
            (idx.name.as_str(), "indexes")
        } else {
            return Err(errors::cache_lookup_failed("relation", table));
        };
        Err(errors::operation_not_supported_for(
            &format!("sequence cannot be owned by relation \"{name}\""),
            kind_plural,
        ))
    }

    /// ALTER SEQUENCE options (owner check, `resolve_sequence_options` against the existing
    /// definition). `owned_by`: `None` = unchanged, `Some(None)` = OWNED BY NONE, `Some(Some((t, a)))` =
    /// OWNED BY t.a (same checks as CREATE; on an identity sequence this is refused with 55000
    /// `errors::cannot_alter_identity_sequence("change ownership", seq, table)`). Direct ALTER
    /// SEQUENCE of option values on an identity sequence is allowed. Queues
    /// `FollowUp::RestartSequence{sequence, value}` when `options.restart` is set.
    // CONTRACT
    pub fn alter_sequence(&mut self, seq: Oid, options: &SequenceOptions, owned_by: Option<Option<(Oid, i16)>>) -> Result<()> {
        self.require_owner(&ObjectAddress::relation(seq))?;
        let existing = self
            .data()
            .sequences
            .get(&seq)
            .cloned()
            .ok_or_else(|| errors::cache_lookup_failed("sequence", seq))?;
        let resolved = resolve_sequence_options(options, Some(&existing), None)?;
        if let Some(change) = owned_by {
            if let Some((table, _attnum)) = change {
                self.check_owned_by_target(existing.owner, existing.namespace, table)?;
            }
            if existing.identity {
                return Err(match existing.owned_by {
                    Some((table, _)) => {
                        let table_name = self
                            .data()
                            .relations
                            .get(&table)
                            .map(|r| r.name.clone())
                            .ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
                        errors::cannot_alter_identity_sequence("change ownership", &existing.name, &table_name)
                    }
                    None => PgError::new(
                        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "cannot change ownership of identity sequence",
                    ),
                });
            }
        }
        let new_owned_by = match owned_by {
            None => existing.owned_by,
            Some(v) => v,
        };
        self.update_sequence(seq, move |s| {
            s.type_oid = resolved.type_oid;
            s.start = resolved.start;
            s.increment = resolved.increment;
            s.min = resolved.min;
            s.max = resolved.max;
            s.cache = resolved.cache;
            s.cycle = resolved.cycle;
            s.owned_by = new_owned_by;
            Ok(())
        })?;
        if let Some(value) = resolved.restart {
            self.follow_up(FollowUp::RestartSequence { sequence: seq, value });
        }
        Ok(())
    }

    /// Used by `table`/`alter_column`: create the implicit sequence of a serial/identity column
    /// (no privilege or IF NOT EXISTS handling; the name is already chosen). Returns the OID and
    /// queues `CreateSequenceState`.
    // CONTRACT
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
        let resolved = resolve_sequence_options(options, None, Some(type_oid))?;
        let oid = self.new_oid()?;
        let seq = build_sequence(oid, name, namespace, owner, persistence, &resolved, Some(owned_by), identity);
        self.put(CatalogObject::Sequence(seq))?;
        self.record_created(ObjectAddress::relation(oid));
        self.follow_up(FollowUp::CreateSequenceState { sequence: oid });
        Ok(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::testing;

    fn opts() -> SequenceOptions {
        SequenceOptions::default()
    }

    fn existing_seq(type_oid: Oid, increment: i64, min: i64, max: i64, start: i64, cache: i64, cycle: bool) -> Sequence {
        Sequence {
            oid: 16384,
            name: "s".to_string(),
            namespace: 2200,
            owner: 10,
            persistence: Persistence::Permanent,
            type_oid,
            start,
            increment,
            min,
            max,
            cache,
            cycle,
            owned_by: None,
            identity: false,
            acl: None,
        }
    }

    /// The error of a resolution that must fail.
    fn expect_err(r: Result<ResolvedSequence>) -> Result<PgError> {
        match r {
            Ok(_) => Err(PgError::internal("resolution unexpectedly succeeded")),
            Err(e) => Ok(e),
        }
    }

    fn assert_rejected(r: Result<ResolvedSequence>, state: SqlState, msg: &str) -> Result<()> {
        let e = expect_err(r)?;
        assert_eq!(e.sqlstate.code(), state.code());
        assert_eq!(e.message, msg);
        Ok(())
    }

    fn public_ns(env: &CatalogEnv) -> Result<Oid> {
        env.snapshot()
            .namespace_by_name("public")
            .map(|n| n.oid)
            .ok_or_else(|| PgError::internal("public schema missing"))
    }

    fn create_spec(ns: Oid, owner: Oid, name: &str, if_not_exists: bool) -> CreateSequenceSpec {
        CreateSequenceSpec {
            namespace: ns,
            name: name.to_string(),
            owner,
            persistence: Persistence::Permanent,
            options: SequenceOptions::default(),
            owned_by: None,
            if_not_exists,
        }
    }

    #[test]
    fn defaults_bigint_ascending() -> Result<()> {
        let r = resolve_sequence_options(&opts(), None, None)?;
        assert_eq!(
            r,
            ResolvedSequence {
                type_oid: INT8,
                start: 1,
                increment: 1,
                min: 1,
                max: i64::MAX,
                cache: 1,
                cycle: false,
                restart: None,
            }
        );
        Ok(())
    }

    #[test]
    fn defaults_descending() -> Result<()> {
        let o = SequenceOptions { increment: Some(-1), ..opts() };
        let r = resolve_sequence_options(&o, None, None)?;
        assert_eq!((r.increment, r.max, r.min, r.start), (-1, -1, i64::MIN, -1));
        Ok(())
    }

    #[test]
    fn as_smallint_defaults_to_type_bounds() -> Result<()> {
        let o = SequenceOptions { type_oid: Some(INT2), ..opts() };
        let r = resolve_sequence_options(&o, None, None)?;
        assert_eq!((r.type_oid, r.min, r.max, r.start), (INT2, 1, 32767, 1));
        Ok(())
    }

    #[test]
    fn as_integer_descending_defaults() -> Result<()> {
        let o = SequenceOptions { type_oid: Some(INT4), increment: Some(-2), ..opts() };
        let r = resolve_sequence_options(&o, None, None)?;
        assert_eq!((r.min, r.max, r.start), (i32::MIN as i64, -1, -1));
        Ok(())
    }

    #[test]
    fn maxvalue_out_of_range_for_smallint() -> Result<()> {
        let o = SequenceOptions {
            type_oid: Some(INT2),
            maxvalue: Some(SeqBound::Value(100000)),
            ..opts()
        };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        )
    }

    #[test]
    fn minvalue_out_of_range_for_integer() -> Result<()> {
        let o = SequenceOptions {
            type_oid: Some(INT4),
            minvalue: Some(SeqBound::Value(i32::MIN as i64 - 1)),
            ..opts()
        };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "MINVALUE (-2147483649) is out of range for sequence data type integer",
        )
    }

    #[test]
    fn increment_zero_rejected() -> Result<()> {
        let o = SequenceOptions { increment: Some(0), ..opts() };
        assert_rejected(resolve_sequence_options(&o, None, None), SqlState::INVALID_PARAMETER_VALUE, "INCREMENT must not be zero")
    }

    #[test]
    fn min_not_less_than_max_rejected() -> Result<()> {
        let o = SequenceOptions {
            minvalue: Some(SeqBound::Value(10)),
            maxvalue: Some(SeqBound::Value(5)),
            ..opts()
        };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "MINVALUE (10) must be less than MAXVALUE (5)",
        )
    }

    #[test]
    fn start_below_default_min_rejected() -> Result<()> {
        let o = SequenceOptions { start: Some(0), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "START value (0) cannot be less than MINVALUE (1)",
        )
    }

    #[test]
    fn start_above_max_rejected() -> Result<()> {
        let o = SequenceOptions { maxvalue: Some(SeqBound::Value(10)), start: Some(100), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "START value (100) cannot be greater than MAXVALUE (10)",
        )
    }

    #[test]
    fn no_maxvalue_resets_to_default() -> Result<()> {
        let o = SequenceOptions { maxvalue: Some(SeqBound::Default), ..opts() };
        let r = resolve_sequence_options(&o, None, None)?;
        assert_eq!(r.max, i64::MAX);
        let old = existing_seq(INT8, 1, 1, 500, 1, 1, false);
        let r2 = resolve_sequence_options(&o, Some(&old), None)?;
        assert_eq!(r2.max, i64::MAX);
        Ok(())
    }

    #[test]
    fn bad_type_messages() -> Result<()> {
        let o = SequenceOptions { type_oid: Some(25), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "sequence type must be smallint, integer, or bigint",
        )?;
        assert_rejected(
            resolve_sequence_options(&o, None, Some(INT4)),
            SqlState::INVALID_PARAMETER_VALUE,
            "identity column type must be smallint, integer, or bigint",
        )
    }

    #[test]
    fn forced_type_used_for_serial_and_identity() -> Result<()> {
        let r = resolve_sequence_options(&opts(), None, Some(INT2))?;
        assert_eq!((r.type_oid, r.max), (INT2, 32767));
        Ok(())
    }

    #[test]
    fn cache_must_be_positive() -> Result<()> {
        let o = SequenceOptions { cache: Some(0), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "CACHE (0) must be greater than zero",
        )
    }

    #[test]
    fn restart_forms_and_range() -> Result<()> {
        let bare = SequenceOptions { start: Some(5), restart: Some(None), ..opts() };
        let old = existing_seq(INT8, 1, 1, i64::MAX, 1, 1, false);
        assert_eq!(resolve_sequence_options(&bare, Some(&old), None)?.restart, Some(5));
        let with = SequenceOptions { restart: Some(Some(7)), ..opts() };
        assert_eq!(resolve_sequence_options(&with, Some(&old), None)?.restart, Some(7));
        let none = opts();
        assert_eq!(resolve_sequence_options(&none, Some(&old), None)?.restart, None);
        let bad = SequenceOptions { maxvalue: Some(SeqBound::Value(10)), restart: Some(Some(100)), ..opts() };
        assert_rejected(
            resolve_sequence_options(&bad, None, None),
            SqlState::INVALID_PARAMETER_VALUE,
            "RESTART value (100) cannot be greater than MAXVALUE (10)",
        )
    }

    #[test]
    fn alter_without_options_keeps_definition() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, i64::MAX, 5, 3, true);
        let r = resolve_sequence_options(&opts(), Some(&old), None)?;
        assert_eq!(
            r,
            ResolvedSequence {
                type_oid: INT8,
                start: 5,
                increment: 1,
                min: 1,
                max: i64::MAX,
                cache: 3,
                cycle: true,
                restart: None,
            }
        );
        Ok(())
    }

    #[test]
    fn alter_as_integer_moves_default_bounds() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, i64::MAX, 1, 1, false);
        let o = SequenceOptions { type_oid: Some(INT4), ..opts() };
        let r = resolve_sequence_options(&o, Some(&old), None)?;
        assert_eq!((r.type_oid, r.min, r.max, r.start), (INT4, 1, i32::MAX as i64, 1));
        Ok(())
    }

    #[test]
    fn alter_as_smallint_keeps_explicit_bounds_when_in_range() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, 1000, 1, 1, false);
        let o = SequenceOptions { type_oid: Some(INT2), ..opts() };
        let r = resolve_sequence_options(&o, Some(&old), None)?;
        assert_eq!((r.type_oid, r.min, r.max), (INT2, 1, 1000));
        Ok(())
    }

    #[test]
    fn alter_as_smallint_rejects_explicit_bound_out_of_range() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, 100000, 1, 1, false);
        let o = SequenceOptions { type_oid: Some(INT2), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, Some(&old), None),
            SqlState::INVALID_PARAMETER_VALUE,
            "MAXVALUE (100000) is out of range for sequence data type smallint",
        )
    }

    #[test]
    fn alter_without_start_keeps_start_and_checks_it() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, i64::MAX, 50, 1, false);
        let o = SequenceOptions { maxvalue: Some(SeqBound::Value(10)), ..opts() };
        assert_rejected(
            resolve_sequence_options(&o, Some(&old), None),
            SqlState::INVALID_PARAMETER_VALUE,
            "START value (50) cannot be greater than MAXVALUE (10)",
        )
    }

    #[test]
    fn alter_bare_restart_uses_start() -> Result<()> {
        let old = existing_seq(INT8, 1, 1, i64::MAX, 5, 1, false);
        let o = SequenceOptions { restart: Some(None), ..opts() };
        assert_eq!(resolve_sequence_options(&o, Some(&old), None)?.restart, Some(5));
        Ok(())
    }

    #[test]
    fn create_sequence_queues_state_and_stores_definition() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ns = public_ns(&env)?;
        let spec = create_spec(ns, env.current_user(), "s1", false);
        let mut created: Option<Oid> = None;
        let (res, env2) = testing::run(&mut kv, &env, |t| {
            created = t.create_sequence(&spec)?;
            Ok(())
        })?;
        let oid = created.ok_or_else(|| PgError::internal("no sequence oid returned"))?;
        assert!(res.outcome.follow_ups.contains(&FollowUp::CreateSequenceState { sequence: oid }));
        let s = env2.snapshot().sequence(oid).ok_or_else(|| PgError::internal("sequence not stored"))?;
        assert_eq!((s.name.as_str(), s.start, s.min, s.max, s.cache, s.cycle), ("s1", 1, 1, i64::MAX, 1, false));
        Ok(())
    }

    #[test]
    fn create_sequence_duplicate_name_is_42p07() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ns = public_ns(&env)?;
        let spec = create_spec(ns, env.current_user(), "s1", false);
        let (_, env2) = testing::run(&mut kv, &env, |t| t.create_sequence(&spec).map(|_| ()))?;
        let err = match testing::run(&mut kv, &env2, |t| t.create_sequence(&spec).map(|_| ())) {
            Ok(_) => return Err(PgError::internal("duplicate sequence accepted")),
            Err(e) => e,
        };
        assert_eq!(err.sqlstate.code(), SqlState::DUPLICATE_TABLE.code());
        Ok(())
    }

    #[test]
    fn create_sequence_if_not_exists_skips_with_notice() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ns = public_ns(&env)?;
        let owner = env.current_user();
        let (_, env2) = testing::run(&mut kv, &env, |t| t.create_sequence(&create_spec(ns, owner, "s1", false)).map(|_| ()))?;
        let mut skipped: Option<Oid> = Some(0);
        let (res, _) = testing::run(&mut kv, &env2, |t| {
            skipped = t.create_sequence(&create_spec(ns, owner, "s1", true))?;
            Ok(())
        })?;
        assert_eq!(skipped, None);
        assert!(!res.outcome.notices.is_empty());
        Ok(())
    }

    #[test]
    fn alter_restart_queues_restart_follow_up() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ns = public_ns(&env)?;
        let mut created: Option<Oid> = None;
        let spec = create_spec(ns, env.current_user(), "s1", false);
        let (_, env2) = testing::run(&mut kv, &env, |t| {
            created = t.create_sequence(&spec)?;
            Ok(())
        })?;
        let oid = created.ok_or_else(|| PgError::internal("no sequence oid returned"))?;
        let o = SequenceOptions { restart: Some(None), ..opts() };
        let (res, _) = testing::run(&mut kv, &env2, |t| t.alter_sequence(oid, &o, None))?;
        assert!(res.outcome.follow_ups.contains(&FollowUp::RestartSequence { sequence: oid, value: 1 }));
        Ok(())
    }
}
