//! ALTER TABLE column attributes and whole-relation alterations: SET/DROP DEFAULT, SET/DROP NOT
//! NULL, identity and generation, column storage/statistics options, RENAME TO, SET SCHEMA,
//! OWNER TO, SET (reloptions), SET LOGGED/UNLOGGED, REPLICA IDENTITY, ROW LEVEL SECURITY flags.
//! Lease CAT-15. PG sources: `commands/tablecmds.c` (`ATExecColumnDefault`, `ATExecSetNotNull`,
//! `ATExecDropNotNull`, `ATExecAddIdentity`, `ATExecSetIdentity`, `ATExecDropIdentity`,
//! `ATExecSetExpression`, `ATExecDropExpression`, `ATExecSetStatistics`, `ATExecSetOptions`,
//! `ATExecSetStorage`, `ATExecSetRelOptions`, `ATExecReplicaIdentity`, `RenameRelation`,
//! `AlterTableNamespace`, `ATExecChangeOwner`), `commands/sequence.c` (`DefineSequence` for
//! identity), `catalog/pg_constraint.c`.
//!
//! Every function: relation owner required (`require_owner`), relation-kind check
//! (`errors::alter_action_not_supported`), column lookup (`errors::column_of_relation_does_not_exist`,
//! system columns -> `errors::cannot_alter_system_column`).
//!
//! ## Column functions
//! - [`set_column_default`]: `Some(expr)` stores `ColumnDefault{oid (reuse existing attrdef OID if
//!   one exists, else new), expr}` with `depends_on` (self placeholder rewritten); `None` = DROP
//!   DEFAULT (no-op without error when none). Refused on identity columns (42P16 `column "id" of
//!   relation "t" is an identity column` HINT `Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY
//!   instead.` - `errors::column_is_identity`) and generated columns (`errors::column_is_generated`
//!   with HINT `Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.` - oracle). Changing the
//!   default does not touch existing rows (no follow-up) and clears nothing else.
//! - [`set_not_null`] -> `ensure_not_null(.., validate = true)` (creates `t_a_not_null` unless one
//!   exists; queues `ValidateNotNull`). Views etc. are rejected by the kind check.
//! - [`drop_not_null`]: refuses primary-key columns (`errors::column_in_primary_key`, 42P16
//!   `column "id" is in a primary key`), identity columns (`errors::column_is_identity` HINT `Use
//!   ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.`); removes the NOT NULL constraint row
//!   and clears `Column::not_null`; no-op when the column is nullable.
//! - [`add_identity`] (ADD GENERATED ALWAYS|BY DEFAULT AS IDENTITY, oracle-verified order of checks):
//!   column must already be NOT NULL (42P16 `errors::identity_requires_not_null`: `column "a" of
//!   relation "t" must be declared NOT NULL before identity can be added`), must not be identity
//!   already (55000 `errors::column_already_identity`), must have no default
//!   (`errors::column_already_has_default`), must not be generated, and its type must be
//!   smallint/integer/bigint (22023 `errors::identity_bad_type`; domains over them are rejected too).
//!   Creates the identity sequence `t_id_seq` (`identity = true`, owned_by) with
//!   `SequenceOptions` from the spec (type from the column) and sets `Column::identity`. Queues
//!   `CreateSequenceState`. Existing rows are NOT backfilled by PG here (NULLs are impossible
//!   because the column is NOT NULL, so values already exist).
//! - [`set_identity`]: SET GENERATED ALWAYS|BY DEFAULT and/or sequence options (RESTART -> `RestartSequence`
//!   follow-up); not an identity column -> `errors::column_not_identity`.
//! - [`drop_identity`]: removes the identity and its sequence (via `drop_objects`); not identity ->
//!   `errors::column_not_identity`, or with `missing_ok` NOTICE `errors::column_not_identity_skipping`.
//!   NOT NULL stays.
//! - [`set_generation_expression`] (SET EXPRESSION AS (expr), PG 17+) / [`drop_expression`]
//!   (DROP EXPRESSION: generated column becomes plain column keeping stored values; on a VIRTUAL
//!   column rows are rewritten: `RewriteTable`; STORED -> no rewrite). Non-generated column ->
//!   42P16 `errors::column_not_generated` (`column "x" of relation "t" is not a generated column`),
//!   or NOTICE `errors::column_not_generated_skipping` (`..., skipping`) with `missing_ok`.
//! - [`set_column_attribute`]: STATISTICS n (-1 = default; 22023 `statistics target 10001 is too
//!   high` DETAIL? > 10000 clamps with WARNING `lowering statistics target to 10000`), STORAGE
//!   (`plain|external|extended|main`, `Column::storage`; 42804 `column data type integer can only
//!   have storage PLAIN` when invalid for the type), COMPRESSION, per-column options `SET (n_distinct = 5)`
//!   (`Column::options`; only `n_distinct`/`n_distinct_inherited` known: 22023 `unrecognized
//!   parameter "x"`).
//!
//! ## Relation functions
//! - [`rename_relation`]: ALTER TABLE/VIEW/MATERIALIZED VIEW/SEQUENCE/INDEX/TYPE-relation RENAME TO.
//!   Owner; new name free in the namespace (42P07 `relation "x" already exists`; also the rowtype
//!   name must be free: 42710 `type "x" already exists`); builtin relations: 42501 system-catalog
//!   permission; index/sequence owned by constraint/column allowed (index rename also renames its
//!   constraint, see `ddl::index`). The array type name is re-chosen (`_new`, stored in
//!   `array_type_name` only if non-canonical). Stored expressions that mention the relation
//!   (`nextval('old_seq'::regclass)` after renaming a sequence, views selecting from a renamed table)
//!   are rewritten via `rewriter` with `NameChange::Relation`. Constraint and index names do NOT
//!   change (oracle).
//! - [`set_relation_schema`]: ALTER ... SET SCHEMA. Owner; CREATE on the target schema; name free
//!   there (42P07); not into/out of pg_catalog/pg_toast (0A000 `cannot move objects into or out
//!   of temporary schemas`/`system schemas`: use 42501 system-catalog wording); moves the
//!   relation, its indexes, owned sequences (`Sequence::owned_by`), constraints
//!   (`Constraint::namespace`) and rowtype/array type (derived). Identity sequences refuse an
//!   independent move (`errors::cannot_alter_identity_sequence`). Rewrite stored text that prints
//!   the schema qualifier through `rewriter` (`NameChange::RelationSchema`). Same-schema move is a
//!   no-op.
//! - [`alter_relation_owner`]: new owner must exist; caller must be superuser or (owner and member
//!   of the new owner); changes `owner` of the relation, its indexes and its owned/identity sequences,
//!   and rewrites ACL entries (`acl::aclnewowner`, CAT-22) if an explicit ACL exists; relation kinds:
//!   table, view, matview, sequence, composite type; NOT index (`errors::wrong_object_type...`: 42809
//!   `cannot change owner of index "i"` HINT `Change the ownership of the index's table instead.`) and
//!   not identity/owned sequences (`errors::cannot_alter_identity_sequence("change ownership", ..)`).
//! - [`set_relation_options`]: `reloptions` SET/RESET for tables: known `fillfactor` (10..100),
//!   `autovacuum_enabled` and other `autovacuum_*`/`toast.*` (accepted and stored), `parallel_workers`,
//!   `user_catalog_table`; unknown -> 22023 `unrecognized parameter "x"`; views accept
//!   `security_barrier`, `security_invoker`, `check_option`.
//! - [`set_persistence`]: SET LOGGED / UNLOGGED (stores `Persistence`; no follow-up).
//! - [`set_replica_identity`]: DEFAULT/FULL/NOTHING/USING INDEX (index must be unique, non-partial,
//!   all columns NOT NULL, belong to the table: 42809 `"i" is not an index for table "t"`, 55000
//!   `cannot use non-unique index "i" as replica identity` ...).
//! - [`set_row_security`]: ENABLE/DISABLE/FORCE/NO FORCE ROW LEVEL SECURITY flags only (policies are
//!   unsupported: `CREATE POLICY` is the binder's 0A000).
//!
//! ## Deviations from the lease text (verified against the PG 18.6 source and the recorded oracle
//! transcripts in `tests/expected`, which win over the prose above)
//! - `column "x" of relation "t" is an identity column` / `is a generated column` are SQLSTATE 42601
//!   (the HINT is attached only to the SET DEFAULT / DROP DEFAULT forms PG attaches it to; `DROP NOT
//!   NULL` on an identity column has no hint).
//! - `must be declared NOT NULL before identity can be added`, `already has a default value` and
//!   `is not a generated column` are 55000.
//! - `DROP EXPRESSION` on a VIRTUAL generated column is 0A000 `ALTER TABLE / DROP EXPRESSION is not
//!   supported for virtual generated columns` (DETAIL names the column), even with `IF EXISTS`.
//! - `SET STORAGE` with a non-PLAIN mode on a non-toastable type is 0A000.
//! - `ALTER INDEX ... OWNER TO` on a plain index is a no-op with WARNING 42809 `cannot change owner of
//!   index "i"` (HINT `Change the ownership of the index's table instead.`), as PG 18 does.
//! - An owned (serial or identity) sequence refuses `SET SCHEMA` / `OWNER TO` on its own with 0A000
//!   `cannot move an owned sequence into another schema` / `cannot change owner of sequence "s"` and
//!   DETAIL `Sequence "s" is linked to table "t".`
use super::common::is_system_column_name;
use super::spec::*;
use crate::acl;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState, FIRST_NORMAL_OID};
use pgarbor_types::{names, registry};

/// Column attribute altered by [`CatalogTxn::set_column_attribute`].
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnAttribute {
    /// `SET STATISTICS n` (-1 = default).
    Statistics(i32),
    /// `SET STORAGE x` (b'p' plain, b'e' external, b'm' main, b'x' extended).
    Storage(u8),
    /// `SET COMPRESSION x` (b'p', b'l', or 0 = default).
    Compression(u8),
    /// `SET (opt = value, ...)`.
    Options(Vec<String>),
    /// `RESET (opt, ...)`.
    ResetOptions(Vec<String>),
}

/// New replica identity setting.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaIdentitySetting {
    Default,
    Full,
    Nothing,
    UsingIndex(Oid),
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// Plural used by `ALTER action ... cannot be performed on relation` DETAIL lines.
fn kind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table => "tables",
        RelKind::PartitionedTable => "partitioned tables",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::Sequence => "sequences",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::Toast => "TOAST tables",
    }
}

/// 42601 `column "c" of relation "t" is an identity column` (+ HINT only for DROP DEFAULT).
fn identity_column_error(col: &str, rel: &str, with_hint: bool) -> PgError {
    let e = PgError::new(SqlState::SYNTAX_ERROR, format!("column \"{col}\" of relation \"{rel}\" is an identity column"));
    if with_hint {
        e.with_hint("Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.")
    } else {
        e
    }
}

/// 42601 `column "c" of relation "t" is a generated column` (+ optional HINT).
fn generated_column_error(col: &str, rel: &str, hint: Option<&str>) -> PgError {
    let e = PgError::new(SqlState::SYNTAX_ERROR, format!("column \"{col}\" of relation \"{rel}\" is a generated column"));
    match hint {
        Some(h) => e.with_hint(h),
        None => e,
    }
}

/// 55000 `column "c" of relation "t" is not a generated column`.
fn not_generated_error(col: &str, rel: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("column \"{col}\" of relation \"{rel}\" is not a generated column"),
    )
}

fn warning(state: SqlState, msg: String) -> PgError {
    PgError::new(state, msg).with_severity(Severity::Warning)
}

/// Locate a visible column. System-column names are refused (`cannot alter system column`) on kinds
/// that have system columns; everything else unknown is 42703.
fn find_column(rel: &Relation, column: &str) -> Result<usize> {
    if rel.kind.has_system_columns() && is_system_column_name(column) {
        return Err(errors::cannot_alter_system_column(column));
    }
    rel.columns
        .iter()
        .position(|c| !c.dropped && c.name == column)
        .ok_or_else(|| errors::column_of_relation_does_not_exist(column, &rel.name))
}

/// Temporary schemas (`pg_temp_N`, `pg_toast_temp_N`).
fn is_temp_schema_name(name: &str) -> bool {
    name.starts_with("pg_temp_") || name.starts_with("pg_toast_temp_")
}

/// Split a `name=value` option string; a bare `name` means `name=true` (PG `defGetString`).
fn split_option(s: &str) -> (String, String) {
    match s.split_once('=') {
        Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
        None => (s.trim().to_string(), "true".to_string()),
    }
}

/// PG `parse_bool_with_len`: unique prefixes of true/false/yes/no, on/off, 1/0.
fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    let len = v.len();
    if len == 0 {
        return None;
    }
    let prefix_of = |word: &str| word.starts_with(v.as_str());
    match v.as_bytes()[0] {
        b't' => prefix_of("true").then_some(true),
        b'f' => prefix_of("false").then_some(false),
        b'y' => prefix_of("yes").then_some(true),
        b'n' => prefix_of("no").then_some(false),
        b'o' => {
            if v == "on" {
                Some(true)
            } else if len >= 2 && prefix_of("off") {
                Some(false)
            } else {
                None
            }
        }
        b'1' => (len == 1).then_some(true),
        b'0' => (len == 1).then_some(false),
        _ => None,
    }
}

/// Type of a reloption.
#[derive(Clone, Copy, Debug)]
enum OptTy {
    Int(i64, i64),
    Real(f64, f64),
    Bool,
    Enum(&'static [&'static str]),
}

/// One known reloption; `toast` = also valid as `toast.<name>`.
#[derive(Clone, Copy, Debug)]
struct OptDef {
    name: &'static str,
    ty: OptTy,
    toast: bool,
}

const INT_MAX: i64 = i32::MAX as i64;

const VACUUM_INDEX_CLEANUP_VALUES: &[&str] = &["auto", "on", "off", "true", "false", "yes", "no", "1", "0"];

/// Reloptions of ordinary tables and materialized views (PG `RELOPT_KIND_HEAP` / `TOAST`).
const HEAP_OPTIONS: &[OptDef] = &[
    OptDef { name: "fillfactor", ty: OptTy::Int(10, 100), toast: false },
    OptDef { name: "toast_tuple_target", ty: OptTy::Int(128, 8160), toast: false },
    OptDef { name: "parallel_workers", ty: OptTy::Int(0, 1024), toast: false },
    OptDef { name: "user_catalog_table", ty: OptTy::Bool, toast: false },
    OptDef { name: "autovacuum_enabled", ty: OptTy::Bool, toast: true },
    OptDef { name: "vacuum_truncate", ty: OptTy::Bool, toast: true },
    OptDef { name: "vacuum_index_cleanup", ty: OptTy::Enum(VACUUM_INDEX_CLEANUP_VALUES), toast: true },
    OptDef { name: "autovacuum_vacuum_threshold", ty: OptTy::Int(0, INT_MAX), toast: true },
    OptDef { name: "autovacuum_vacuum_max_threshold", ty: OptTy::Int(-1, INT_MAX), toast: true },
    OptDef { name: "autovacuum_vacuum_insert_threshold", ty: OptTy::Int(-1, INT_MAX), toast: true },
    OptDef { name: "autovacuum_analyze_threshold", ty: OptTy::Int(0, INT_MAX), toast: false },
    OptDef { name: "autovacuum_vacuum_cost_limit", ty: OptTy::Int(1, 10000), toast: true },
    OptDef { name: "autovacuum_freeze_min_age", ty: OptTy::Int(0, 1_000_000_000), toast: true },
    OptDef { name: "autovacuum_multixact_freeze_min_age", ty: OptTy::Int(0, 1_000_000_000), toast: true },
    OptDef { name: "autovacuum_freeze_max_age", ty: OptTy::Int(100_000, 2_000_000_000), toast: true },
    OptDef { name: "autovacuum_multixact_freeze_max_age", ty: OptTy::Int(10_000, 2_000_000_000), toast: true },
    OptDef { name: "autovacuum_freeze_table_age", ty: OptTy::Int(0, 2_000_000_000), toast: true },
    OptDef { name: "autovacuum_multixact_freeze_table_age", ty: OptTy::Int(0, 2_000_000_000), toast: true },
    OptDef { name: "log_autovacuum_min_duration", ty: OptTy::Int(-1, INT_MAX), toast: true },
    OptDef { name: "autovacuum_vacuum_cost_delay", ty: OptTy::Real(0.0, 100.0), toast: true },
    OptDef { name: "autovacuum_vacuum_scale_factor", ty: OptTy::Real(0.0, 100.0), toast: true },
    OptDef { name: "autovacuum_vacuum_insert_scale_factor", ty: OptTy::Real(0.0, 100.0), toast: true },
    OptDef { name: "autovacuum_analyze_scale_factor", ty: OptTy::Real(0.0, 100.0), toast: false },
];

/// Per-attribute options (`attoptions`).
const ATTRIBUTE_OPTIONS: &[OptDef] = &[
    OptDef { name: "n_distinct", ty: OptTy::Real(-1.0, f64::MAX), toast: false },
    OptDef { name: "n_distinct_inherited", ty: OptTy::Real(-1.0, f64::MAX), toast: false },
];

/// Validate `value` against `ty` with PG's reloption wording.
fn check_option_value(name: &str, value: &str, ty: &OptTy) -> Result<()> {
    let invalid = |what: &str| {
        PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid value for {what} option \"{name}\": {value}"))
    };
    match ty {
        OptTy::Bool => {
            if parse_bool(value).is_none() {
                return Err(invalid("boolean"));
            }
        }
        OptTy::Int(min, max) => {
            let n: i64 = value.trim().parse().map_err(|_| invalid("integer"))?;
            if n < i32::MIN as i64 || n > i32::MAX as i64 {
                return Err(invalid("integer"));
            }
            if n < *min || n > *max {
                return Err(errors::option_out_of_bounds(value, name, &min.to_string(), &max.to_string()));
            }
        }
        OptTy::Real(min, max) => {
            let f: f64 = value.trim().parse().map_err(|_| invalid("floating point"))?;
            if f.is_nan() {
                return Err(invalid("floating point"));
            }
            if f < *min || f > *max {
                return Err(errors::option_out_of_bounds(value, name, &format!("{:.6}", *min), &format!("{:.6}", *max)));
            }
        }
        OptTy::Enum(allowed) => {
            let v = value.trim().to_ascii_lowercase();
            if !allowed.iter().any(|a| *a == v) {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for enum option \"{name}\": {value}"),
                )
                .with_detail("Valid values are \"auto\", \"on\", \"off\"."));
            }
        }
    }
    Ok(())
}

/// Look `full_name` (`fillfactor`, `toast.autovacuum_enabled`) up in `defs` and validate `value`.
fn validate_option(defs: &[OptDef], full_name: &str, value: &str) -> Result<()> {
    let (ns, base) = match full_name.split_once('.') {
        Some((n, b)) => (Some(n), b),
        None => (None, full_name),
    };
    if let Some(n) = ns {
        if n != "toast" {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter namespace \"{n}\""),
            ));
        }
    }
    let def = defs.iter().find(|d| d.name == base && (ns.is_none() || d.toast));
    match def {
        Some(d) => check_option_value(full_name, value, &d.ty),
        None => Err(errors::unrecognized_parameter(full_name)),
    }
}

/// Apply `set` (validated, deduplicated) and `reset` to a stored `name=value` list, PG
/// `transformRelOptions`: replaced options move to the end, reset names disappear.
fn merge_options(defs: &[OptDef], existing: &[String], set: &[String], reset: &[String]) -> Result<Vec<String>> {
    let mut parsed: Vec<(String, String)> = Vec::with_capacity(set.len());
    for s in set {
        let (name, value) = split_option(s);
        validate_option(defs, &name, &value)?;
        if parsed.iter().any(|(n, _)| *n == name) {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" specified more than once"),
            ));
        }
        parsed.push((name, value));
    }
    let mut out: Vec<String> = Vec::with_capacity(existing.len() + parsed.len());
    for e in existing {
        let (n, _) = split_option(e);
        if parsed.iter().any(|(pn, _)| *pn == n) || reset.iter().any(|r| r.trim() == n) {
            continue;
        }
        out.push(e.clone());
    }
    for (n, v) in parsed {
        out.push(format!("{n}={v}"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// CatalogTxn operations
// ---------------------------------------------------------------------------------------------

impl<'a> CatalogTxn<'a> {
    /// Common front matter of every ALTER TABLE sub-command (PG `RangeVarCallbackForAlterRelation` +
    /// `ATSimplePermissions`): owner, system-catalog protection, relation kind. Returns a copy of the
    /// relation.
    fn alter_target(&mut self, oid: Oid, allowed: &[RelKind], action: &str) -> Result<Relation> {
        let snap = self.snapshot()?;
        let (name, kind) = match snap.class_identity(oid) {
            Some((_, n, k)) => (n.to_string(), k),
            None => return Err(errors::cache_lookup_failed("relation", oid)),
        };
        self.require_owner(&ObjectAddress::relation(oid))?;
        if oid < FIRST_NORMAL_OID {
            return Err(errors::system_catalog_permission(&name));
        }
        if !allowed.contains(&kind) {
            return Err(errors::alter_action_not_supported(action, &name, kind_plural(kind)));
        }
        match snap.relation(oid) {
            Some(r) => Ok(r.clone()),
            None => Err(errors::alter_action_not_supported(action, &name, kind_plural(kind))),
        }
    }

    /// Re-deparse every stored expression that depends on one of `targets` (any sub-id) through
    /// `rewriter`, against the catalog AFTER the change was applied.
    fn rewrite_dependents(
        &mut self,
        targets: &[ObjectAddress],
        change: &NameChange,
        rewriter: &dyn ExprRewriter,
    ) -> Result<()> {
        let env = self.env()?;
        let hits = |e: &StoredExpr| {
            e.depends_on.iter().any(|d| targets.iter().any(|t| d.class == t.class && d.oid == t.oid))
        };
        let rw = |e: &StoredExpr| -> Result<Option<StoredExpr>> {
            if hits(e) {
                rewriter.rewrite(e, change, &env).map(Some)
            } else {
                Ok(None)
            }
        };
        let mut updated: Vec<CatalogObject> = Vec::new();

        for r in self.data.relations.values() {
            let mut nr: Option<Relation> = None;
            for (i, c) in r.columns.iter().enumerate() {
                if let Some(d) = &c.default {
                    if let Some(ne) = rw(&d.expr)? {
                        let target = nr.get_or_insert_with(|| r.clone());
                        if let Some(cd) = target.columns[i].default.as_mut() {
                            cd.expr = ne;
                        }
                    }
                }
            }
            if let Some(v) = &r.view {
                if let Some(ne) = rw(&v.query)? {
                    let target = nr.get_or_insert_with(|| r.clone());
                    if let Some(vv) = target.view.as_mut() {
                        vv.query = ne;
                    }
                }
            }
            if let Some(pk) = &r.partition_key {
                for (i, e) in pk.exprs.iter().enumerate() {
                    if let Some(ne) = rw(e)? {
                        let target = nr.get_or_insert_with(|| r.clone());
                        if let Some(p) = target.partition_key.as_mut() {
                            p.exprs[i] = ne;
                        }
                    }
                }
            }
            if let Some(n) = nr {
                updated.push(CatalogObject::Relation(n));
            }
        }

        for c in self.data.constraints.values() {
            if let Some(ch) = &c.check {
                if let Some(ne) = rw(ch)? {
                    let mut n = c.clone();
                    n.check = Some(ne);
                    updated.push(CatalogObject::Constraint(n));
                }
            }
        }

        for ix in self.data.indexes.values() {
            let mut ni: Option<Index> = None;
            for (i, k) in ix.keys.iter().enumerate() {
                if let Some(e) = &k.expr {
                    if let Some(ne) = rw(e)? {
                        let target = ni.get_or_insert_with(|| ix.clone());
                        target.keys[i].expr = Some(ne);
                    }
                }
            }
            if let Some(p) = &ix.predicate {
                if let Some(ne) = rw(p)? {
                    let target = ni.get_or_insert_with(|| ix.clone());
                    target.predicate = Some(ne);
                }
            }
            if let Some(n) = ni {
                updated.push(CatalogObject::Index(n));
            }
        }

        for t in self.data.types.values() {
            if let TypeDef::Domain { default: Some(d), .. } = &t.def {
                if let Some(ne) = rw(d)? {
                    let mut n = t.clone();
                    if let TypeDef::Domain { default, .. } = &mut n.def {
                        *default = Some(ne);
                    }
                    updated.push(CatalogObject::Type(n));
                }
            }
        }

        for f in self.data.functions.values() {
            let mut nf: Option<Function> = None;
            for (i, a) in f.args.iter().enumerate() {
                if let Some(d) = &a.default {
                    if let Some(ne) = rw(d)? {
                        let target = nf.get_or_insert_with(|| f.clone());
                        target.args[i].default = Some(ne);
                    }
                }
            }
            if let Some(b) = &f.sql_body {
                if let Some(ne) = rw(b)? {
                    let target = nf.get_or_insert_with(|| f.clone());
                    target.sql_body = Some(ne);
                }
            }
            if let Some(n) = nf {
                updated.push(CatalogObject::Function(n));
            }
        }

        for o in updated {
            self.put(o)?;
        }
        Ok(())
    }

    /// SET DEFAULT `expr` / DROP DEFAULT (`None`). See module docs.
    // CONTRACT
    pub fn set_column_default(&mut self, table: Oid, column: &str, expr: Option<StoredExpr>) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::View, RelKind::ForeignTable],
            "ALTER COLUMN ... SET DEFAULT",
        )?;
        let idx = find_column(&rel, column)?;
        let col = &rel.columns[idx];
        if col.identity.is_some() {
            return Err(identity_column_error(column, &rel.name, expr.is_none()));
        }
        if let Some(gk) = col.generated {
            let hint = if expr.is_some() {
                Some("Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.")
            } else if gk == GeneratedKind::Stored {
                Some("Use ALTER TABLE ... ALTER COLUMN ... DROP EXPRESSION instead.")
            } else {
                None
            };
            return Err(generated_column_error(column, &rel.name, hint));
        }
        match expr {
            None => {
                if col.default.is_none() {
                    return Ok(());
                }
                self.update_relation(table, |r| {
                    r.columns[idx].default = None;
                    Ok(())
                })
            }
            Some(e) => {
                let oid = match &col.default {
                    Some(d) => d.oid,
                    None => self.new_oid()?,
                };
                let bound = Self::bind_self_deps(&e, table);
                self.update_relation(table, |r| {
                    r.columns[idx].default = Some(ColumnDefault { oid, expr: bound });
                    Ok(())
                })
            }
        }
    }

    /// SET NOT NULL.
    // CONTRACT
    pub fn set_not_null(&mut self, table: Oid, column: &str) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::ForeignTable],
            "ALTER COLUMN ... SET NOT NULL",
        )?;
        let idx = find_column(&rel, column)?;
        self.ensure_not_null(table, (idx + 1) as i16, None, true)?;
        Ok(())
    }

    /// DROP NOT NULL.
    // CONTRACT
    pub fn drop_not_null(&mut self, table: Oid, column: &str) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::ForeignTable],
            "ALTER COLUMN ... DROP NOT NULL",
        )?;
        let idx = find_column(&rel, column)?;
        let col = &rel.columns[idx];
        if !col.not_null {
            return Ok(());
        }
        if col.identity.is_some() {
            return Err(identity_column_error(column, &rel.name, false));
        }
        let attnum = (idx + 1) as i16;
        let snap = self.snapshot()?;
        let cons = snap.constraints_of(table);
        if cons.iter().any(|c| c.kind == ConstraintKind::PrimaryKey && c.columns.contains(&attnum)) {
            return Err(errors::column_in_primary_key(column));
        }
        let nn_name: Option<String> = cons
            .iter()
            .find(|c| c.kind == ConstraintKind::NotNull && c.columns.contains(&attnum))
            .map(|c| c.name.clone());
        if let Some(name) = nn_name {
            self.drop_constraint(table, &name, DropBehavior::Restrict, false)?;
        }
        // The NOT NULL constraint row (if any) is gone; make sure the mirror flag is cleared.
        let still = self.snapshot()?.relation(table).and_then(|r| r.column(attnum)).is_some_and(|c| c.not_null);
        if still {
            self.update_relation(table, |r| {
                r.columns[idx].not_null = false;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// ADD GENERATED ... AS IDENTITY.
    // CONTRACT
    pub fn add_identity(&mut self, table: Oid, column: &str, identity: &IdentitySpec) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::View, RelKind::ForeignTable],
            "ALTER COLUMN ... ADD IDENTITY",
        )?;
        // PG resolves the column and checks its type at parse-analysis time, before the
        // execution-time checks. System columns have types (tid, xid, ...) that are never integers.
        if rel.kind.has_system_columns() && is_system_column_name(column) {
            return Err(errors::identity_bad_type());
        }
        let idx = rel
            .columns
            .iter()
            .position(|c| !c.dropped && c.name == column)
            .ok_or_else(|| errors::column_of_relation_does_not_exist(column, &rel.name))?;
        let col = &rel.columns[idx];
        if !matches!(col.type_id.oid, 21 | 23 | 20) {
            return Err(errors::identity_bad_type());
        }
        let attnum = (idx + 1) as i16;
        if !col.not_null {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "column \"{}\" of relation \"{}\" must be declared NOT NULL before identity can be added",
                    column, rel.name
                ),
            ));
        }
        let snap = self.snapshot()?;
        if let Some(c) = snap
            .constraints_of(table)
            .iter()
            .find(|c| c.kind == ConstraintKind::NotNull && c.columns.contains(&attnum) && !c.validated)
        {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("incompatible NOT VALID constraint \"{}\" on relation \"{}\"", c.name, rel.name),
            )
            .with_hint("You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT."));
        }
        if col.identity.is_some() {
            return Err(errors::column_already_identity(column, &rel.name));
        }
        if col.default.is_some() || col.generated.is_some() {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("column \"{}\" of relation \"{}\" already has a default value", column, rel.name),
            ));
        }

        let seq_name = naming::choose_identity_sequence_name(
            &snap,
            rel.namespace,
            &rel.name,
            Some(column),
            identity.sequence_name.as_deref(),
            &[],
        )?;
        let mut options = identity.options.clone();
        options.type_oid = None;
        let seq = self.create_column_sequence(
            rel.namespace,
            &seq_name,
            rel.owner,
            rel.persistence,
            col.type_id.oid,
            &options,
            (table, attnum),
            true,
        )?;
        let kind = identity.kind;
        self.update_relation(table, |r| {
            r.columns[idx].identity = Some(Identity { kind, sequence: seq });
            Ok(())
        })
    }

    /// SET GENERATED ALWAYS|BY DEFAULT and/or SET sequence option / RESTART.
    // CONTRACT
    pub fn set_identity(
        &mut self,
        table: Oid,
        column: &str,
        kind: Option<IdentityKind>,
        options: &SequenceOptions,
    ) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::View, RelKind::ForeignTable],
            "ALTER COLUMN ... SET",
        )?;
        let idx = find_column(&rel, column)?;
        let ident = match rel.columns[idx].identity {
            Some(i) => i,
            None => return Err(errors::column_not_identity(column, &rel.name)),
        };
        if *options != SequenceOptions::default() {
            self.alter_sequence(ident.sequence, options, None)?;
        }
        if let Some(k) = kind {
            if k != ident.kind {
                self.update_relation(table, |r| {
                    if let Some(i) = r.columns[idx].identity.as_mut() {
                        i.kind = k;
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    /// DROP IDENTITY [IF EXISTS].
    // CONTRACT
    pub fn drop_identity(&mut self, table: Oid, column: &str, missing_ok: bool) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::View, RelKind::ForeignTable],
            "ALTER COLUMN ... DROP IDENTITY",
        )?;
        let idx = find_column(&rel, column)?;
        let ident = match rel.columns[idx].identity {
            Some(i) => i,
            None => {
                if missing_ok {
                    self.notice(errors::column_not_identity_skipping(column, &rel.name));
                    return Ok(());
                }
                return Err(errors::column_not_identity(column, &rel.name));
            }
        };
        self.update_relation(table, |r| {
            r.columns[idx].identity = None;
            Ok(())
        })?;
        // Detach the sequence from the column first (PG removes the INTERNAL dependency), then drop
        // it with RESTRICT: anything else that still uses it blocks the drop.
        let seq = ident.sequence;
        if self.data.sequences.contains_key(&seq) {
            self.update_sequence(seq, |s| {
                s.identity = false;
                s.owned_by = None;
                Ok(())
            })?;
            self.drop_objects(&[ObjectAddress::relation(seq)], DropBehavior::Restrict)?;
        }
        Ok(())
    }

    /// SET EXPRESSION AS (expr) on a generated column.
    // CONTRACT
    pub fn set_generation_expression(&mut self, table: Oid, column: &str, expr: StoredExpr) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::ForeignTable],
            "ALTER COLUMN ... SET EXPRESSION",
        )?;
        let idx = find_column(&rel, column)?;
        let col = &rel.columns[idx];
        let gk = match col.generated {
            Some(g) => g,
            None => return Err(not_generated_error(column, &rel.name)),
        };
        let attnum = (idx + 1) as i16;
        if gk == GeneratedKind::Virtual {
            let snap = self.snapshot()?;
            if snap.constraints_of(table).iter().any(|c| c.kind == ConstraintKind::Check) {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in tables with check constraints",
                )
                .with_detail(format!(
                    "Column \"{}\" of relation \"{}\" is a virtual generated column.",
                    column, rel.name
                )));
            }
        }
        let oid = match &col.default {
            Some(d) => d.oid,
            None => self.new_oid()?,
        };
        let bound = Self::bind_self_deps(&expr, table);
        let stored = gk == GeneratedKind::Stored;
        self.update_relation(table, |r| {
            r.columns[idx].default = Some(ColumnDefault { oid, expr: bound });
            if stored {
                // The rewrite recomputes every row, so fast-default values are pointless.
                for c in r.columns.iter_mut() {
                    c.missing_value = None;
                }
            }
            Ok(())
        })?;
        if stored {
            self.follow_up(FollowUp::RewriteTable { table });
        } else if col.not_null {
            self.follow_up(FollowUp::ValidateNotNull { table, attnum });
        }
        Ok(())
    }

    /// DROP EXPRESSION [IF EXISTS].
    // CONTRACT
    pub fn drop_expression(&mut self, table: Oid, column: &str, missing_ok: bool) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::ForeignTable],
            "ALTER COLUMN ... DROP EXPRESSION",
        )?;
        let idx = find_column(&rel, column)?;
        let col = &rel.columns[idx];
        match col.generated {
            Some(GeneratedKind::Virtual) => Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns",
            )
            .with_detail(format!(
                "Column \"{}\" of relation \"{}\" is a virtual generated column.",
                column, rel.name
            ))),
            None => {
                if missing_ok {
                    self.notice(errors::column_not_generated_skipping(column, &rel.name));
                    Ok(())
                } else {
                    Err(not_generated_error(column, &rel.name))
                }
            }
            Some(GeneratedKind::Stored) => self.update_relation(table, |r| {
                r.columns[idx].generated = None;
                r.columns[idx].default = None;
                Ok(())
            }),
        }
    }

    /// SET STATISTICS / STORAGE / COMPRESSION / (options).
    // CONTRACT
    pub fn set_column_attribute(&mut self, table: Oid, column: &str, attr: &ColumnAttribute) -> Result<()> {
        let (allowed, action): (&[RelKind], &str) = match attr {
            ColumnAttribute::Statistics(_) => (
                &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView, RelKind::ForeignTable],
                "ALTER COLUMN ... SET STATISTICS",
            ),
            ColumnAttribute::Storage(_) => (
                &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView, RelKind::ForeignTable],
                "ALTER COLUMN ... SET STORAGE",
            ),
            ColumnAttribute::Compression(_) => (
                &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView],
                "ALTER COLUMN ... SET COMPRESSION",
            ),
            ColumnAttribute::Options(_) => (
                &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView, RelKind::ForeignTable],
                "ALTER COLUMN ... SET",
            ),
            ColumnAttribute::ResetOptions(_) => (
                &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView, RelKind::ForeignTable],
                "ALTER COLUMN ... RESET",
            ),
        };
        let rel = self.alter_target(table, allowed, action)?;

        match attr {
            ColumnAttribute::Statistics(n) => {
                let mut target = *n;
                if target != -1 {
                    if target < 0 {
                        return Err(PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("statistics target {target} is too low"),
                        ));
                    }
                    if target > 10000 {
                        target = 10000;
                        self.notice(warning(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("lowering statistics target to {target}"),
                        ));
                    }
                }
                let idx = find_column(&rel, column)?;
                if rel.columns[idx].generated == Some(GeneratedKind::Virtual) {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot alter statistics on virtual generated column \"{column}\""),
                    ));
                }
                let stored = if *n == -1 { None } else { Some(target as i16) };
                self.update_relation(table, |r| {
                    r.columns[idx].stat_target = stored;
                    Ok(())
                })
            }
            ColumnAttribute::Storage(b) => {
                if !matches!(*b, b'p' | b'e' | b'm' | b'x') {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("invalid storage type \"{}\"", *b as char),
                    ));
                }
                let idx = find_column(&rel, column)?;
                let env = self.env()?;
                let type_oid = rel.columns[idx].type_id.oid;
                let info = registry::type_info(&env, type_oid)?;
                if *b != b'p' && info.storage == b'p' {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("column data type {} can only have storage PLAIN", names::display_name(&env, type_oid)),
                    ));
                }
                let new_storage = if *b == info.storage { None } else { Some(*b) };
                self.update_relation(table, |r| {
                    r.columns[idx].storage = new_storage;
                    Ok(())
                })
            }
            ColumnAttribute::Compression(c) => {
                let idx = find_column(&rel, column)?;
                let new_comp = if *c == 0 {
                    None
                } else {
                    let env = self.env()?;
                    let type_oid = rel.columns[idx].type_id.oid;
                    let info = registry::type_info(&env, type_oid)?;
                    if info.storage == b'p' {
                        return Err(PgError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            format!(
                                "column data type {} does not support compression",
                                names::display_name(&env, type_oid)
                            ),
                        ));
                    }
                    if !matches!(*c, b'p' | b'l') {
                        return Err(PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("invalid compression method \"{}\"", *c as char),
                        ));
                    }
                    Some(*c)
                };
                self.update_relation(table, |r| {
                    r.columns[idx].compression = new_comp;
                    Ok(())
                })
            }
            ColumnAttribute::Options(set) => {
                let idx = find_column(&rel, column)?;
                let merged = merge_options(ATTRIBUTE_OPTIONS, &rel.columns[idx].options, set, &[])?;
                self.update_relation(table, |r| {
                    r.columns[idx].options = merged;
                    Ok(())
                })
            }
            ColumnAttribute::ResetOptions(reset) => {
                let idx = find_column(&rel, column)?;
                let merged = merge_options(ATTRIBUTE_OPTIONS, &rel.columns[idx].options, &[], reset)?;
                self.update_relation(table, |r| {
                    r.columns[idx].options = merged;
                    Ok(())
                })
            }
        }
    }

    /// RENAME TO for table/view/matview/sequence/index/composite type relations.
    // CONTRACT
    pub fn rename_relation(&mut self, rel: Oid, new_name: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        let snap = self.snapshot()?;
        let (ns, old_name, kind) = match snap.class_identity(rel) {
            Some((n, s, k)) => (n, s.to_string(), k),
            None => return Err(errors::cache_lookup_failed("relation", rel)),
        };
        if matches!(kind, RelKind::Index | RelKind::PartitionedIndex) && rel >= FIRST_NORMAL_OID {
            // Index rename also renames the owning constraint (`ddl::index`).
            return self.rename_index(rel, new_name);
        }
        self.require_owner(&ObjectAddress::relation(rel))?;
        if rel < FIRST_NORMAL_OID {
            return Err(errors::system_catalog_permission(&old_name));
        }
        if kind == RelKind::Toast {
            return Err(errors::alter_action_not_supported("RENAME", &old_name, kind_plural(kind)));
        }
        self.require_schema_create(ns)?;

        if kind.has_rowtype() {
            // pg_class clash (42P07) and the rowtype name in pg_type (an auto array type is moved aside).
            self.check_new_relation_name(ns, new_name, false)?;
        } else if snap.class_in_namespace(ns, new_name).is_some() {
            return Err(errors::relation_already_exists(new_name));
        }

        if kind == RelKind::Sequence {
            let n = new_name.to_string();
            self.update_sequence(rel, |s| {
                s.name = n;
                Ok(())
            })?;
        } else {
            // Array type name: PG renames the array type to makeArrayTypeName(new). If the clash
            // handling above already moved this relation's own array out of the way (renaming `foo`
            // to `_foo`), keep that name.
            let before = snap.relation(rel).and_then(|r| r.array_type_name.clone());
            let snap2 = self.snapshot()?;
            let after = snap2.relation(rel).and_then(|r| r.array_type_name.clone());
            let array_name = if after != before {
                after
            } else if kind.has_rowtype() {
                let chosen = naming::choose_array_type_name(&snap2, ns, new_name, &[])?;
                if chosen == naming::array_type_name(new_name) {
                    None
                } else {
                    Some(chosen)
                }
            } else {
                None
            };
            let n = new_name.to_string();
            self.update_relation(rel, |r| {
                r.name = n;
                r.array_type_name = array_name;
                Ok(())
            })?;
        }

        let change = NameChange::Relation { oid: rel, old: old_name.clone(), new: new_name.to_string() };
        self.rewrite_dependents(&[ObjectAddress::relation(rel)], &change, rewriter)?;
        if kind.has_rowtype() {
            let rowtype = self.snapshot()?.relation(rel).map(|r| r.rowtype).unwrap_or(0);
            if rowtype != 0 {
                let tchange = NameChange::Type { oid: rowtype, old: old_name, new: new_name.to_string() };
                self.rewrite_dependents(&[ObjectAddress::type_(rowtype)], &tchange, rewriter)?;
            }
        }
        Ok(())
    }

    /// SET SCHEMA.
    // CONTRACT
    pub fn set_relation_schema(&mut self, rel: Oid, new_schema: Oid, rewriter: &dyn ExprRewriter) -> Result<()> {
        let snap = self.snapshot()?;
        let (old_ns, name, kind) = match snap.class_identity(rel) {
            Some((n, s, k)) => (n, s.to_string(), k),
            None => return Err(errors::cache_lookup_failed("relation", rel)),
        };
        self.require_owner(&ObjectAddress::relation(rel))?;
        if rel < FIRST_NORMAL_OID {
            return Err(errors::system_catalog_permission(&name));
        }
        match kind {
            RelKind::Toast => {
                return Err(errors::alter_action_not_supported("SET SCHEMA", &name, kind_plural(kind)));
            }
            RelKind::Index | RelKind::PartitionedIndex => {
                return Err(PgError::unimplemented(
                    "CAT-15",
                    "ALTER INDEX SET SCHEMA (indexes live in their table's schema)",
                ));
            }
            RelKind::Sequence => {
                if let Some(seq) = snap.sequence(rel) {
                    if seq.owned_by.is_some() || seq.identity {
                        let table_name = seq
                            .owned_by
                            .and_then(|(t, _)| snap.class_identity(t).map(|(_, n, _)| n.to_string()))
                            .unwrap_or_default();
                        return Err(PgError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "cannot move an owned sequence into another schema",
                        )
                        .with_detail(format!("Sequence \"{name}\" is linked to table \"{table_name}\".")));
                    }
                }
            }
            _ => {}
        }

        let new_ns_name = match snap.namespace(new_schema) {
            Some(n) => n.name.clone(),
            None => return Err(errors::cache_lookup_failed("schema", new_schema)),
        };
        self.require_schema_create(new_schema)?;
        let old_ns_name = snap.namespace(old_ns).map(|n| n.name.clone()).unwrap_or_default();
        if is_temp_schema_name(&new_ns_name) || is_temp_schema_name(&old_ns_name) {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if new_schema == crate::builtin::PG_TOAST_NAMESPACE || old_ns == crate::builtin::PG_TOAST_NAMESPACE {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        if old_ns == new_schema {
            return Ok(());
        }
        self.check_creatable_namespace(new_schema, &name)?;

        // Everything that moves with the relation.
        let idx_moves: Vec<(Oid, String)> = snap.indexes_of(rel).iter().map(|i| (i.oid, i.name.clone())).collect();
        let seq_moves: Vec<(Oid, String)> = snap
            .data()
            .sequences
            .values()
            .filter(|s| s.owned_by.map(|(t, _)| t) == Some(rel))
            .map(|s| (s.oid, s.name.clone()))
            .collect();
        let con_moves: Vec<Oid> = snap.constraints_of(rel).iter().map(|c| c.oid).collect();

        let clash = |n: &str| -> Option<PgError> {
            if snap.class_in_namespace(new_schema, n).is_some() {
                Some(PgError::new(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{n}\" already exists in schema \"{new_ns_name}\""),
                ))
            } else {
                None
            }
        };
        if let Some(e) = clash(&name) {
            return Err(e);
        }
        for (_, n) in idx_moves.iter().chain(seq_moves.iter()) {
            if let Some(e) = clash(n) {
                return Err(e);
            }
        }
        if kind.has_rowtype() {
            self.resolve_type_name_clash(new_schema, &name)?;
        }

        // Apply.
        if kind == RelKind::Sequence {
            self.update_sequence(rel, |s| {
                s.namespace = new_schema;
                Ok(())
            })?;
        } else {
            let array_name = if kind.has_rowtype() {
                let snap2 = self.snapshot()?;
                let chosen = naming::choose_array_type_name(&snap2, new_schema, &name, &[])?;
                if chosen == naming::array_type_name(&name) {
                    None
                } else {
                    Some(chosen)
                }
            } else {
                None
            };
            self.update_relation(rel, |r| {
                r.namespace = new_schema;
                if r.kind.has_rowtype() {
                    r.array_type_name = array_name;
                }
                Ok(())
            })?;
        }
        for (oid, _) in &idx_moves {
            self.update_index(*oid, |i| {
                i.namespace = new_schema;
                Ok(())
            })?;
        }
        for (oid, _) in &seq_moves {
            self.update_sequence(*oid, |s| {
                s.namespace = new_schema;
                Ok(())
            })?;
        }
        for oid in &con_moves {
            self.update_constraint(*oid, |c| {
                c.namespace = new_schema;
                Ok(())
            })?;
        }

        // Stored text that prints the schema qualifier (regclass literals, view queries).
        let mut moved: Vec<Oid> = vec![rel];
        moved.extend(seq_moves.iter().map(|(o, _)| *o));
        for oid in moved {
            let change = NameChange::RelationSchema { oid, old_schema: old_ns, new_schema };
            self.rewrite_dependents(&[ObjectAddress::relation(oid)], &change, rewriter)?;
        }
        Ok(())
    }

    /// OWNER TO.
    // CONTRACT
    pub fn alter_relation_owner(&mut self, rel: Oid, new_owner: Oid) -> Result<()> {
        let snap = self.snapshot()?;
        let (ns, name, kind) = match snap.class_identity(rel) {
            Some((n, s, k)) => (n, s.to_string(), k),
            None => return Err(errors::cache_lookup_failed("relation", rel)),
        };
        if snap.role(new_owner).is_none() {
            return Err(errors::cache_lookup_failed("role", new_owner));
        }
        self.require_owner(&ObjectAddress::relation(rel))?;
        if rel < FIRST_NORMAL_OID {
            return Err(errors::system_catalog_permission(&name));
        }
        let old_owner = match kind {
            RelKind::Index | RelKind::PartitionedIndex => snap.index(rel).map(|i| i.owner),
            RelKind::Sequence => snap.sequence(rel).map(|s| s.owner),
            _ => snap.relation(rel).map(|r| r.owner),
        }
        .ok_or_else(|| errors::cache_lookup_failed("relation", rel))?;

        match kind {
            RelKind::Table
            | RelKind::PartitionedTable
            | RelKind::View
            | RelKind::MatView
            | RelKind::ForeignTable
            | RelKind::CompositeType => {}
            RelKind::Index => {
                // PG keeps accepting ALTER INDEX OWNER TO (old pg_dump output) as a warned no-op.
                if old_owner != new_owner {
                    self.notice(
                        warning(SqlState::WRONG_OBJECT_TYPE, format!("cannot change owner of index \"{name}\""))
                            .with_hint("Change the ownership of the index's table instead."),
                    );
                }
                return Ok(());
            }
            RelKind::PartitionedIndex => {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change owner of index \"{name}\""),
                )
                .with_hint("Change the ownership of the index's table instead."));
            }
            RelKind::Sequence => {
                if old_owner != new_owner {
                    if let Some(seq) = snap.sequence(rel) {
                        if seq.owned_by.is_some() || seq.identity {
                            let table_name = seq
                                .owned_by
                                .and_then(|(t, _)| snap.class_identity(t).map(|(_, n, _)| n.to_string()))
                                .unwrap_or_default();
                            return Err(PgError::new(
                                SqlState::FEATURE_NOT_SUPPORTED,
                                format!("cannot change owner of sequence \"{name}\""),
                            )
                            .with_detail(format!("Sequence \"{name}\" is linked to table \"{table_name}\".")));
                        }
                    }
                }
            }
            RelKind::Toast => {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot change owner of relation \"{name}\""),
                ));
            }
        }

        if old_owner == new_owner {
            return Ok(());
        }

        let cur = self.current_user();
        if !self.is_superuser(cur)? {
            let env = self.env()?;
            let graph = acl::RoleGraph::build(env.snapshot(), env.database())?;
            if cur != new_owner && !graph.can_set_role(cur, new_owner)? {
                let rname = snap.role_name(new_owner).unwrap_or("").to_string();
                return Err(PgError::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("must be able to SET ROLE \"{rname}\""),
                ));
            }
            let nsobj = snap.namespace(ns).ok_or_else(|| errors::cache_lookup_failed("schema", ns))?;
            let eff = acl::effective_acl(acl::AclObject::Schema, nsobj.owner, nsobj.acl.as_ref())?;
            if !acl::has_privilege(&graph, new_owner, nsobj.owner, &eff, acl_mode::CREATE)? {
                return Err(errors::permission_denied_for("schema", &nsobj.name));
            }
        }

        if kind == RelKind::Sequence {
            let seq_acl = match snap.sequence(rel).and_then(|s| s.acl.clone()) {
                Some(a) => Some(acl::aclnewowner(&a, old_owner, new_owner)?),
                None => None,
            };
            return self.update_sequence(rel, |s| {
                s.owner = new_owner;
                s.acl = seq_acl;
                Ok(())
            });
        }

        // Objects that follow the relation: its indexes and its owned (serial / identity) sequences.
        let idx_oids: Vec<Oid> = snap.indexes_of(rel).iter().map(|i| i.oid).collect();
        let seq_oids: Vec<Oid> = snap
            .data()
            .sequences
            .values()
            .filter(|s| s.owned_by.map(|(t, _)| t) == Some(rel))
            .map(|s| s.oid)
            .collect();

        // ACL rewrites (PG `aclnewowner`) are computed before any write so a failure leaves nothing half done.
        let rel_acl = match snap.relation(rel).and_then(|r| r.acl.clone()) {
            Some(a) => Some(acl::aclnewowner(&a, old_owner, new_owner)?),
            None => None,
        };
        let mut col_acls: Vec<Option<Acl>> = Vec::new();
        if let Some(r) = snap.relation(rel) {
            for c in &r.columns {
                col_acls.push(match &c.acl {
                    Some(a) => Some(acl::aclnewowner(a, old_owner, new_owner)?),
                    None => None,
                });
            }
        }
        let mut seq_acls: Vec<(Oid, Option<Acl>)> = Vec::with_capacity(seq_oids.len());
        for oid in seq_oids {
            let a = match snap.sequence(oid).and_then(|s| s.acl.clone()) {
                Some(a) => Some(acl::aclnewowner(&a, old_owner, new_owner)?),
                None => None,
            };
            seq_acls.push((oid, a));
        }

        self.update_relation(rel, |r| {
            r.owner = new_owner;
            r.acl = rel_acl;
            for (c, a) in r.columns.iter_mut().zip(col_acls) {
                c.acl = a;
            }
            Ok(())
        })?;
        for oid in idx_oids {
            self.update_index(oid, |i| {
                i.owner = new_owner;
                Ok(())
            })?;
        }
        for (oid, a) in seq_acls {
            self.update_sequence(oid, |s| {
                s.owner = new_owner;
                s.acl = a;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// SET (opts) / RESET (names) on a relation.
    // CONTRACT
    pub fn set_relation_options(&mut self, rel: Oid, set: &[String], reset: &[String]) -> Result<()> {
        let snap = self.snapshot()?;
        let kind = match snap.class_identity(rel) {
            Some((_, _, k)) => k,
            None => return Err(errors::cache_lookup_failed("relation", rel)),
        };
        match kind {
            RelKind::View => return self.set_view_options(rel, set, reset),
            RelKind::Index | RelKind::PartitionedIndex => return self.alter_index_options(rel, set, reset),
            _ => {}
        }
        let cur = self.alter_target(rel, &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView], "SET")?;
        let merged = merge_options(HEAP_OPTIONS, &cur.options, set, reset)?;
        if merged == cur.options {
            return Ok(());
        }
        self.update_relation(rel, |r| {
            r.options = merged;
            Ok(())
        })
    }

    /// SET LOGGED / UNLOGGED.
    // CONTRACT
    pub fn set_persistence(&mut self, table: Oid, persistence: Persistence) -> Result<()> {
        let action = if persistence == Persistence::Unlogged { "SET UNLOGGED" } else { "SET LOGGED" };
        let rel = self.alter_target(table, &[RelKind::Table, RelKind::PartitionedTable], action)?;
        if rel.persistence == Persistence::Temp {
            return Err(PgError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot change logged status of table \"{}\" because it is temporary", rel.name),
            ));
        }
        if persistence == Persistence::Temp {
            return Err(PgError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot change table \"{}\" to temporary", rel.name),
            ));
        }
        if rel.persistence == persistence {
            return Ok(());
        }
        let snap = self.snapshot()?;
        if persistence == Persistence::Permanent {
            // A permanent table must not reference an unlogged one.
            for c in snap.constraints_of(table) {
                if let (ConstraintKind::ForeignKey, Some(fk)) = (c.kind, c.fk.as_ref()) {
                    if fk.ref_table == table {
                        continue;
                    }
                    if let Some(other) = snap.relation(fk.ref_table) {
                        if other.persistence != Persistence::Permanent {
                            return Err(PgError::new(
                                SqlState::INVALID_TABLE_DEFINITION,
                                format!(
                                    "could not change table \"{}\" to logged because it references unlogged table \"{}\"",
                                    rel.name, other.name
                                ),
                            )
                            .with_constraint(c.name.clone()));
                        }
                    }
                }
            }
        } else {
            // ... and an unlogged table must not be referenced by a permanent one.
            for c in snap.foreign_keys_referencing(table) {
                if c.table == table {
                    continue;
                }
                if let Some(other) = snap.relation(c.table) {
                    if other.persistence == Persistence::Permanent {
                        return Err(PgError::new(
                            SqlState::INVALID_TABLE_DEFINITION,
                            format!(
                                "could not change table \"{}\" to unlogged because it references logged table \"{}\"",
                                rel.name, other.name
                            ),
                        )
                        .with_constraint(c.name.clone()));
                    }
                }
            }
        }
        let idx_oids: Vec<Oid> = snap.indexes_of(table).iter().map(|i| i.oid).collect();
        self.update_relation(table, |r| {
            r.persistence = persistence;
            Ok(())
        })?;
        for oid in idx_oids {
            self.update_index(oid, |i| {
                i.persistence = persistence;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// REPLICA IDENTITY.
    // CONTRACT
    pub fn set_replica_identity(&mut self, table: Oid, setting: &ReplicaIdentitySetting) -> Result<()> {
        let rel = self.alter_target(
            table,
            &[RelKind::Table, RelKind::PartitionedTable, RelKind::MatView],
            "REPLICA IDENTITY",
        )?;
        let snap = self.snapshot()?;
        let (ident, index_oid): (ReplicaIdentity, Option<Oid>) = match setting {
            ReplicaIdentitySetting::Default => (ReplicaIdentity::Default, None),
            ReplicaIdentitySetting::Full => (ReplicaIdentity::Full, None),
            ReplicaIdentitySetting::Nothing => (ReplicaIdentity::Nothing, None),
            ReplicaIdentitySetting::UsingIndex(ioid) => {
                let not_index_for = |n: &str| {
                    PgError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not an index for table \"{}\"", n, rel.name),
                    )
                };
                let index = match snap.index(*ioid) {
                    Some(i) => i,
                    None => {
                        return Err(match snap.class_identity(*ioid) {
                            Some((_, n, _)) => not_index_for(n),
                            None => errors::cache_lookup_failed("index", *ioid),
                        });
                    }
                };
                if index.table != table {
                    return Err(not_index_for(&index.name));
                }
                if !index.unique || index.method != IndexMethod::Btree {
                    return Err(PgError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("cannot use non-unique index \"{}\" as replica identity", index.name),
                    ));
                }
                if !index.immediate {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot use non-immediate index \"{}\" as replica identity", index.name),
                    ));
                }
                if index.keys.iter().any(|k| k.expr.is_some()) {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot use expression index \"{}\" as replica identity", index.name),
                    ));
                }
                if index.predicate.is_some() {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot use partial index \"{}\" as replica identity", index.name),
                    ));
                }
                for key in index.key_columns() {
                    let attnum = key.column.unwrap_or(0);
                    if attnum <= 0 {
                        return Err(PgError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            format!(
                                "index \"{}\" cannot be used as replica identity because column {} is a system column",
                                index.name, attnum
                            ),
                        ));
                    }
                    let col = rel.column(attnum).ok_or_else(|| errors::cache_lookup_failed("attribute", table))?;
                    if !col.not_null {
                        return Err(PgError::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!(
                                "index \"{}\" cannot be used as replica identity because column \"{}\" is nullable",
                                index.name, col.name
                            ),
                        ));
                    }
                }
                (ReplicaIdentity::Index, Some(*ioid))
            }
        };
        if rel.replica_identity != ident {
            self.update_relation(table, |r| {
                r.replica_identity = ident;
                Ok(())
            })?;
        }
        // Exactly the chosen index (if any) carries the `indisreplident` flag.
        let flags: Vec<(Oid, bool)> = snap.indexes_of(table).iter().map(|i| (i.oid, i.replident)).collect();
        for (oid, cur) in flags {
            let want = Some(oid) == index_oid;
            if cur != want {
                self.update_index(oid, |i| {
                    i.replident = want;
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    /// ENABLE/DISABLE/FORCE/NO FORCE ROW LEVEL SECURITY (`None` = unchanged).
    // CONTRACT
    pub fn set_row_security(&mut self, table: Oid, enable: Option<bool>, force: Option<bool>) -> Result<()> {
        let action = match (enable, force) {
            (Some(true), _) => "ENABLE ROW SECURITY",
            (Some(false), _) => "DISABLE ROW SECURITY",
            (None, Some(true)) => "FORCE ROW SECURITY",
            (None, Some(false)) => "NO FORCE ROW SECURITY",
            (None, None) => "ENABLE ROW SECURITY",
        };
        let rel = self.alter_target(table, &[RelKind::Table, RelKind::PartitionedTable], action)?;
        let new_enable = enable.unwrap_or(rel.row_security);
        let new_force = force.unwrap_or(rel.force_row_security);
        if new_enable == rel.row_security && new_force == rel.force_row_security {
            return Ok(());
        }
        self.update_relation(table, |r| {
            r.row_security = new_enable;
            r.force_row_security = new_force;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::snapshot::ClassRef;
    use crate::testing;
    use pgarbor_common::MemKv;
    use pgarbor_types::TypeId;

    const PUBLIC: Oid = crate::builtin::PUBLIC_NAMESPACE;
    const OWNER: Oid = crate::builtin::BOOTSTRAP_SUPERUSER;

    fn table_spec(name: &str, columns: Vec<ColumnSpec>) -> CreateTableSpec {
        CreateTableSpec {
            namespace: PUBLIC,
            name: name.to_string(),
            owner: OWNER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns,
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    fn table_oid(env: &CatalogEnv, name: &str) -> Oid {
        match env.snapshot().class_in_namespace(PUBLIC, name) {
            Some(crate::snapshot::ClassRef::Relation(o)) => o,
            other => panic!("no table {name}: {other:?}"),
        }
    }

    /// Bootstrapped catalog with table `t(columns)`.
    fn setup(columns: Vec<ColumnSpec>) -> Result<(MemKv, CatalogEnv, Oid)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = table_spec("t", columns);
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        let oid = table_oid(&env, "t");
        Ok((kv, env, oid))
    }

    fn int_col(name: &str) -> ColumnSpec {
        ColumnSpec::new(name, TypeId::INT4)
    }

    fn rel_of(env: &CatalogEnv, oid: Oid) -> Relation {
        env.snapshot().relation(oid).expect("relation").clone()
    }

    // ------------------------------------------------------------------ pure helpers

    #[test]
    fn parse_bool_prefixes() {
        for t in ["true", "TRUE", "t", "tr", "yes", "y", "on", "1"] {
            assert_eq!(parse_bool(t), Some(true), "{t}");
        }
        for f in ["false", "f", "fa", "no", "n", "off", "of", "0"] {
            assert_eq!(parse_bool(f), Some(false), "{f}");
        }
        for bad in ["", "o", "maybe", "11", "tru e", "2"] {
            assert_eq!(parse_bool(bad), None, "{bad}");
        }
    }

    #[test]
    fn fillfactor_bounds() {
        let e = validate_option(HEAP_OPTIONS, "fillfactor", "5").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "value 5 out of bounds for option \"fillfactor\"");
        assert_eq!(e.detail.as_deref(), Some("Valid values are between \"10\" and \"100\"."));
        assert!(validate_option(HEAP_OPTIONS, "fillfactor", "70").is_ok());
        let e = validate_option(HEAP_OPTIONS, "fillfactor", "abc").unwrap_err();
        assert_eq!(e.message, "invalid value for integer option \"fillfactor\": abc");
    }

    #[test]
    fn unknown_and_namespaced_options() {
        let e = validate_option(HEAP_OPTIONS, "nosuch", "1").unwrap_err();
        assert_eq!(e.message, "unrecognized parameter \"nosuch\"");
        assert!(validate_option(HEAP_OPTIONS, "toast.autovacuum_enabled", "false").is_ok());
        let e = validate_option(HEAP_OPTIONS, "toast.fillfactor", "50").unwrap_err();
        assert_eq!(e.message, "unrecognized parameter \"toast.fillfactor\"");
        let e = validate_option(HEAP_OPTIONS, "foo.fillfactor", "50").unwrap_err();
        assert_eq!(e.message, "unrecognized parameter namespace \"foo\"");
    }

    #[test]
    fn real_and_bool_options() {
        assert!(validate_option(HEAP_OPTIONS, "autovacuum_vacuum_scale_factor", "0.2").is_ok());
        let e = validate_option(HEAP_OPTIONS, "autovacuum_vacuum_scale_factor", "101").unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("Valid values are between \"0.000000\" and \"100.000000\"."));
        let e = validate_option(HEAP_OPTIONS, "autovacuum_enabled", "perhaps").unwrap_err();
        assert_eq!(e.message, "invalid value for boolean option \"autovacuum_enabled\": perhaps");
    }

    #[test]
    fn merge_replaces_and_resets() {
        let existing = vec!["fillfactor=70".to_string(), "autovacuum_enabled=off".to_string()];
        let out = merge_options(HEAP_OPTIONS, &existing, &["fillfactor=80".to_string()], &[]).unwrap();
        assert_eq!(out, vec!["autovacuum_enabled=off".to_string(), "fillfactor=80".to_string()]);
        let out = merge_options(HEAP_OPTIONS, &existing, &[], &["fillfactor".to_string()]).unwrap();
        assert_eq!(out, vec!["autovacuum_enabled=off".to_string()]);
        let dup = merge_options(HEAP_OPTIONS, &[], &["fillfactor=20".to_string(), "fillfactor=30".to_string()], &[]);
        assert_eq!(dup.unwrap_err().message, "parameter \"fillfactor\" specified more than once");
    }

    #[test]
    fn attribute_options() {
        assert!(validate_option(ATTRIBUTE_OPTIONS, "n_distinct", "-0.5").is_ok());
        assert!(validate_option(ATTRIBUTE_OPTIONS, "n_distinct", "5").is_ok());
        let e = validate_option(ATTRIBUTE_OPTIONS, "n_distinct", "-2").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(validate_option(ATTRIBUTE_OPTIONS, "fillfactor", "5").unwrap_err().message, "unrecognized parameter \"fillfactor\"");
    }

    #[test]
    fn split_option_forms() {
        assert_eq!(split_option("a = 5"), ("a".to_string(), "5".to_string()));
        assert_eq!(split_option("flag"), ("flag".to_string(), "true".to_string()));
    }

    // ------------------------------------------------------------------ catalog-backed

    #[test]
    fn add_identity_requires_not_null() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a"), ColumnSpec::new("b", TypeId::TEXT)])?;
        let spec = IdentitySpec { kind: IdentityKind::Always, options: SequenceOptions::default(), sequence_name: None };
        let err = testing::run(&mut kv, &env, |c| c.add_identity(t, "a", &spec)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        assert_eq!(err.message, "column \"a\" of relation \"t\" must be declared NOT NULL before identity can be added");
        // The type check precedes the NOT NULL check.
        let err = testing::run(&mut kv, &env, |c| c.add_identity(t, "b", &spec)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "22023");
        let err = testing::run(&mut kv, &env, |c| c.add_identity(t, "zz", &spec)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42703");
        Ok(())
    }

    #[test]
    fn identity_lifecycle() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a"), int_col("b")])?;
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_not_null(t, "a"))?;
        let spec = IdentitySpec { kind: IdentityKind::ByDefault, options: SequenceOptions::default(), sequence_name: None };
        let (res, env) = testing::run(&mut kv, &env, |c| c.add_identity(t, "a", &spec))?;
        let ident = rel_of(&env, t).columns[0].identity.expect("identity set");
        assert_eq!(ident.kind, IdentityKind::ByDefault);
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::CreateSequenceState { sequence } if *sequence == ident.sequence)));
        assert_eq!(env.snapshot().sequence(ident.sequence).expect("sequence").name, "t_a_seq");

        // second ADD IDENTITY, SET DEFAULT and DROP NOT NULL are refused
        let err = testing::run(&mut kv, &env, |c| c.add_identity(t, "a", &spec)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        let err = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", Some(StoredExpr::new("1")))).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42601");
        assert_eq!(err.message, "column \"a\" of relation \"t\" is an identity column");
        assert!(err.hint.is_none());
        let err = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", None)).unwrap_err();
        assert_eq!(err.hint.as_deref(), Some("Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead."));
        let err = testing::run(&mut kv, &env, |c| c.drop_not_null(t, "a")).unwrap_err();
        assert_eq!(err.message, "column \"a\" of relation \"t\" is an identity column");

        // SET GENERATED ALWAYS
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_identity(t, "a", Some(IdentityKind::Always), &SequenceOptions::default()))?;
        assert_eq!(rel_of(&env, t).columns[0].identity.map(|i| i.kind), Some(IdentityKind::Always));
        // not an identity column
        let err = testing::run(&mut kv, &env, |c| c.set_identity(t, "b", Some(IdentityKind::Always), &SequenceOptions::default())).unwrap_err();
        assert_eq!(err.message, "column \"b\" of relation \"t\" is not an identity column");

        // DROP IDENTITY IF EXISTS on a plain column is a notice
        let (res, env) = testing::run(&mut kv, &env, |c| c.drop_identity(t, "b", true))?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "column \"b\" of relation \"t\" is not an identity column, skipping");

        // DROP IDENTITY removes the sequence and keeps NOT NULL
        let (res, env) = testing::run(&mut kv, &env, |c| c.drop_identity(t, "a", false))?;
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::DropSequenceState { sequence } if *sequence == ident.sequence)));
        assert!(env.snapshot().sequence(ident.sequence).is_none());
        let r = rel_of(&env, t);
        assert!(r.columns[0].identity.is_none());
        assert!(r.columns[0].not_null);
        Ok(())
    }

    #[test]
    fn default_set_and_drop() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a")])?;
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", Some(StoredExpr::new("42"))))?;
        let d = rel_of(&env, t).columns[0].default.clone().expect("default");
        assert_eq!(d.expr.sql, "42");
        // replacing the default keeps the attrdef OID
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", Some(StoredExpr::new("43"))))?;
        let d2 = rel_of(&env, t).columns[0].default.clone().expect("default");
        assert_eq!((d2.oid, d2.expr.sql.as_str()), (d.oid, "43"));
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", None))?;
        assert!(rel_of(&env, t).columns[0].default.is_none());
        // DROP DEFAULT when none is a no-op
        let (res, _) = testing::run(&mut kv, &env, |c| c.set_column_default(t, "a", None))?;
        assert!(!res.changed);
        let err = testing::run(&mut kv, &env, |c| c.set_column_default(t, "nosuch", None)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42703");
        let err = testing::run(&mut kv, &env, |c| c.set_column_default(t, "ctid", None)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "0A000");
        Ok(())
    }

    #[test]
    fn generated_column_rules() -> Result<()> {
        let mut g = int_col("b");
        g.generated = Some(GeneratedSpec {
            kind: GeneratedKind::Virtual,
            expr: StoredExpr::new("(a * 2)").with_deps(vec![ObjectAddress::column(SELF_REL, 1)]),
        });
        let (mut kv, env, t) = setup(vec![int_col("a"), g])?;
        let err = testing::run(&mut kv, &env, |c| c.set_column_default(t, "b", Some(StoredExpr::new("1")))).unwrap_err();
        assert_eq!(err.message, "column \"b\" of relation \"t\" is a generated column");
        assert_eq!(err.hint.as_deref(), Some("Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead."));
        // not generated
        let err = testing::run(&mut kv, &env, |c| c.drop_expression(t, "a", false)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        let (res, env) = testing::run(&mut kv, &env, |c| c.drop_expression(t, "a", true))?;
        assert_eq!(res.outcome.notices[0].message, "column \"a\" of relation \"t\" is not a generated column, skipping");
        // virtual DROP EXPRESSION is not supported
        let err = testing::run(&mut kv, &env, |c| c.drop_expression(t, "b", false)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "0A000");
        assert_eq!(err.detail.as_deref(), Some("Column \"b\" of relation \"t\" is a virtual generated column."));
        // SET EXPRESSION replaces the expression, no rewrite for virtual
        let new_expr = StoredExpr::new("(a * 3)").with_deps(vec![ObjectAddress::column(t, 1)]);
        let (res, env) = testing::run(&mut kv, &env, |c| c.set_generation_expression(t, "b", new_expr.clone()))?;
        assert_eq!(rel_of(&env, t).columns[1].default.as_ref().map(|d| d.expr.sql.as_str()), Some("(a * 3)"));
        assert!(res.outcome.follow_ups.is_empty());
        let err = testing::run(&mut kv, &env, |c| c.set_generation_expression(t, "a", new_expr.clone())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "55000");
        Ok(())
    }

    #[test]
    fn stored_generated_drop_expression_keeps_values() -> Result<()> {
        let mut g = int_col("b");
        g.generated = Some(GeneratedSpec {
            kind: GeneratedKind::Stored,
            expr: StoredExpr::new("(a * 2)").with_deps(vec![ObjectAddress::column(SELF_REL, 1)]),
        });
        let (mut kv, env, t) = setup(vec![int_col("a"), g])?;
        let new_expr = StoredExpr::new("(a * 3)").with_deps(vec![ObjectAddress::column(t, 1)]);
        let (res, env) = testing::run(&mut kv, &env, |c| c.set_generation_expression(t, "b", new_expr.clone()))?;
        assert!(res.outcome.follow_ups.contains(&FollowUp::RewriteTable { table: t }));
        let (res, env) = testing::run(&mut kv, &env, |c| c.drop_expression(t, "b", false))?;
        assert!(res.outcome.follow_ups.is_empty());
        let r = rel_of(&env, t);
        assert!(r.columns[1].generated.is_none() && r.columns[1].default.is_none());
        Ok(())
    }

    #[test]
    fn column_attributes() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a"), ColumnSpec::new("s", TypeId::TEXT)])?;
        let err = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "a", &ColumnAttribute::Storage(b'e'))).unwrap_err();
        assert_eq!(err.message, "column data type integer can only have storage PLAIN");
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "s", &ColumnAttribute::Storage(b'e')))?;
        assert_eq!(rel_of(&env, t).columns[1].storage, Some(b'e'));
        // back to the type default stores nothing
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "s", &ColumnAttribute::Storage(b'x')))?;
        assert_eq!(rel_of(&env, t).columns[1].storage, None);

        let (res, env) = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "a", &ColumnAttribute::Statistics(20000)))?;
        assert_eq!(res.outcome.notices[0].message, "lowering statistics target to 10000");
        assert_eq!(rel_of(&env, t).columns[0].stat_target, Some(10000));
        let err = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "a", &ColumnAttribute::Statistics(-2))).unwrap_err();
        assert_eq!(err.message, "statistics target -2 is too low");
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "a", &ColumnAttribute::Statistics(-1)))?;
        assert_eq!(rel_of(&env, t).columns[0].stat_target, None);

        let (_, env) = testing::run(&mut kv, &env, |c| {
            c.set_column_attribute(t, "a", &ColumnAttribute::Options(vec!["n_distinct=5".to_string()]))
        })?;
        assert_eq!(rel_of(&env, t).columns[0].options, vec!["n_distinct=5".to_string()]);
        let err = testing::run(&mut kv, &env, |c| {
            c.set_column_attribute(t, "a", &ColumnAttribute::Options(vec!["bogus=1".to_string()]))
        })
        .unwrap_err();
        assert_eq!(err.message, "unrecognized parameter \"bogus\"");
        let (_, env) = testing::run(&mut kv, &env, |c| {
            c.set_column_attribute(t, "a", &ColumnAttribute::ResetOptions(vec!["n_distinct".to_string()]))
        })?;
        assert!(rel_of(&env, t).columns[0].options.is_empty());

        let err = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "a", &ColumnAttribute::Compression(b'l'))).unwrap_err();
        assert_eq!(err.message, "column data type integer does not support compression");
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_column_attribute(t, "s", &ColumnAttribute::Compression(b'l')))?;
        assert_eq!(rel_of(&env, t).columns[1].compression, Some(b'l'));
        Ok(())
    }

    #[test]
    fn relation_options_flow() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a")])?;
        let err = testing::run(&mut kv, &env, |c| c.set_relation_options(t, &["fillfactor=5".to_string()], &[])).unwrap_err();
        assert_eq!(err.message, "value 5 out of bounds for option \"fillfactor\"");
        let (_, env) = testing::run(&mut kv, &env, |c| {
            c.set_relation_options(t, &["fillfactor=70".to_string(), "autovacuum_enabled=false".to_string()], &[])
        })?;
        assert_eq!(rel_of(&env, t).options, vec!["fillfactor=70".to_string(), "autovacuum_enabled=false".to_string()]);
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_relation_options(t, &[], &["fillfactor".to_string()]))?;
        assert_eq!(rel_of(&env, t).options, vec!["autovacuum_enabled=false".to_string()]);
        Ok(())
    }

    #[test]
    fn rename_and_clash() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a")])?;
        let other = table_spec("u", vec![int_col("x")]);
        let (_, env) = testing::run(&mut kv, &env, |c| c.create_table(&other).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |c| c.rename_relation(t, "u", &NoRewrite)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42P07");
        assert_eq!(err.message, "relation \"u\" already exists");
        let (_, env) = testing::run(&mut kv, &env, |c| c.rename_relation(t, "t2", &NoRewrite))?;
        assert_eq!(rel_of(&env, t).name, "t2");
        assert!(env.snapshot().class_in_namespace(PUBLIC, "t").is_none());
        assert!(matches!(env.snapshot().class_in_namespace(PUBLIC, "t2"), Some(ClassRef::Relation(o)) if o == t));
        Ok(())
    }

    #[test]
    fn row_security_persistence_and_replica_identity() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a")])?;
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_row_security(t, Some(true), None))?;
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_row_security(t, None, Some(true)))?;
        let r = rel_of(&env, t);
        assert!(r.row_security && r.force_row_security);
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_row_security(t, Some(false), Some(false)))?;
        let r = rel_of(&env, t);
        assert!(!r.row_security && !r.force_row_security);

        let (_, env) = testing::run(&mut kv, &env, |c| c.set_persistence(t, Persistence::Unlogged))?;
        assert_eq!(rel_of(&env, t).persistence, Persistence::Unlogged);
        let (_, env) = testing::run(&mut kv, &env, |c| c.set_persistence(t, Persistence::Permanent))?;
        assert_eq!(rel_of(&env, t).persistence, Persistence::Permanent);

        let (_, env) = testing::run(&mut kv, &env, |c| c.set_replica_identity(t, &ReplicaIdentitySetting::Full))?;
        assert_eq!(rel_of(&env, t).replica_identity, ReplicaIdentity::Full);
        let err = testing::run(&mut kv, &env, |c| c.set_replica_identity(t, &ReplicaIdentitySetting::UsingIndex(1)))
            .unwrap_err();
        assert!(err.sqlstate.code() == "XX000" || err.sqlstate.code() == "42809");
        Ok(())
    }

    #[test]
    fn views_reject_table_only_actions() -> Result<()> {
        let (mut kv, env, t) = setup(vec![int_col("a")])?;
        let q = StoredExpr::new(" SELECT a\n   FROM t;").with_deps(vec![ObjectAddress::relation(t), ObjectAddress::column(t, 1)]);
        let view = ViewSpec {
            namespace: PUBLIC,
            name: "v".to_string(),
            owner: OWNER,
            columns: vec![int_col("a")],
            query: q,
            or_replace: false,
            materialized: false,
            populated: true,
            options: Vec::new(),
            check_option: None,
            if_not_exists: false,
        };
        let (_, env) = testing::run(&mut kv, &env, |c| c.create_view(&view).map(|_| ()))?;
        let v = match env.snapshot().class_in_namespace(PUBLIC, "v") {
            Some(ClassRef::Relation(o)) => o,
            other => panic!("no view: {other:?}"),
        };
        let err = testing::run(&mut kv, &env, |c| c.set_not_null(v, "a")).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42809");
        assert_eq!(err.message, "ALTER action ALTER COLUMN ... SET NOT NULL cannot be performed on relation \"v\"");
        assert_eq!(err.detail.as_deref(), Some("This operation is not supported for views."));
        Ok(())
    }
}
