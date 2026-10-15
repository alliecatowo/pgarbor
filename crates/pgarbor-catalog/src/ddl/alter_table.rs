//! ALTER TABLE column structure: ADD COLUMN, DROP COLUMN, RENAME COLUMN, ALTER COLUMN TYPE.
//! Lease CAT-14 (sonnet). PG sources: `commands/tablecmds.c` (`ATExecAddColumn`,
//! `ATExecDropColumn`, `renameatt`, `ATPrepAlterColumnType`/`ATExecAlterColumnType`,
//! `ATRewriteTables` decision in `ATRewriteCatalogs`), `catalog/heap.c` (`AddRelationNewConstraints`,
//! `RemoveAttrDefault`), `catalog/dependency.c` (column drops via `performDeletion`).
//!
//! All four require the relation to be owned by the current user (`require_owner`) and of an
//! acceptable kind; otherwise `errors::alter_action_not_supported("ADD COLUMN", "v", "views")`
//! (42809 `ALTER action ADD COLUMN cannot be performed on relation "v"` DETAIL `This operation is
//! not supported for views.`) - action words `ADD COLUMN`, `DROP COLUMN`, `ALTER COLUMN TYPE`;
//! RENAME COLUMN is allowed on tables, views, matviews and composite types (not indexes/sequences:
//! `"s" is not a table, view, materialized view, composite type, or foreign table`).
//! System catalogs: 42501 `permission denied: "pg_class" is a system catalog`.
//!
//! ## ADD COLUMN ([`CatalogTxn::add_column`])
//! Same column semantics as CREATE TABLE (collation, serial -> sequence + nextval default,
//! identity, generated, NOT NULL constraint `t_a_not_null`) with attnum = `next_attnum()`.
//! - existing name (visible columns) -> 42701 `column "a" of relation "t" already exists`, or NOTICE
//!   `column "a" of relation "t" already exists, skipping` + `Ok(None)` with `if_not_exists`;
//!   system-column names -> 42701 `column name "xmin" conflicts with a system column name`;
//!   > 1600 columns -> 54011 `tables can have at most 1600 columns`.
//! - Data consequences (existing rows):
//!   * constant/non-volatile default (`spec.fast_default = Some(text)`): stored in
//!     `Column::missing_value`; no follow-up (rows read the missing value).
//!   * default present but `default_is_volatile` (serial, `random()`, `nextval()`), identity, or
//!     STORED generated column: `FollowUp::RewriteTable{table}`.
//!   * `generated` VIRTUAL: nothing stored.
//!   * no default: existing rows read NULL; with NOT NULL queue `FollowUp::ValidateNotNull`
//!     (executor raises 23502 `column "a" of relation "t" contains null values` if rows exist).
//! - Records created sequences (`CreateSequenceState`) like CREATE TABLE.
//! - Returns the new attnum.
//!
//! ## DROP COLUMN ([`CatalogTxn::drop_column`])
//! - unknown column -> 42703 `column "x" of relation "t" does not exist` or NOTICE `... does not
//!   exist, skipping` + `Ok(false)` with `missing_ok`; system column -> 0A000 `errors::cannot_drop_system_column`.
//! - Dependency-driven through `drop_objects(&[ObjectAddress::column(table, attnum)], behavior)`:
//!   constraints and indexes involving the column (AUTO) vanish silently - also multi-column
//!   PRIMARY KEY/UNIQUE/CHECK (oracle) -; defaults, serial sequences (AUTO), identity sequences
//!   (INTERNAL) vanish; views, FKs from other tables, generated columns using it (NORMAL) block with
//!   2BP01 `cannot drop column a of table t because other objects depend on it` DETAIL
//!   `view v depends on column a of table t` ... HINT `Use DROP ... CASCADE ...` (RESTRICT) or are
//!   dropped with the CASCADE NOTICE (`drop cascades to view v`). Dropping the column of a rowtype used
//!   as column type elsewhere is the NORMAL dependency of that column on the type.
//! - Effect on the relation (done by `drop.rs` when the column address is executed; this function
//!   only validates and delegates): the `Column` is rewritten as dropped (`Column::dropped_name`,
//!   `type_id` oid 0 typmod -1, not_null false, default None, identity None, generated None,
//!   missing_value None). The slot stays so row tuples keep decoding.
//! - Dropping the last visible column is allowed (zero-column table).
//!
//! Composite types (`RelKind::CompositeType`, reached through ALTER TYPE ... ATTRIBUTE) are accepted by
//! all four functions; for ADD/DROP/ALTER TYPE they queue `FollowUp::RequireEmptyTable` for each table
//! column declared with the composite type (see `typedef.rs`).
//!
//! ## RENAME COLUMN ([`CatalogTxn::rename_column`])
//! - unknown -> 42703 `column "x" does not exist` (note: no `of relation` here, oracle);
//!   system column -> 0A000 `cannot rename system column "ctid"`; new name taken -> 42701
//!   `column "c" of relation "t" already exists`; renaming to the same name is a no-op error-free?
//!   (PG: 42701 as well - `column "a" of relation "t" already exists`).
//! - Stored text that mentions the column must change (CHECK bodies, defaults, generation
//!   expressions, index expressions/predicates, view queries, ...): collect every `StoredExpr`
//!   whose `depends_on` contains `ObjectAddress::column(table, attnum)` and replace it with
//!   `rewriter.rewrite(expr, &NameChange::Column{..}, env)`. Oracle: after `rename b to bb`,
//!   `pg_get_constraintdef` prints `CHECK ((bb <> ''::text))`, `pg_get_indexdef(ib)` prints `(bb)` -
//!   but the INDEX ATTRIBUTE NAME (`IndexKey::name`) stays `b`, as in PG.
//! - Renaming a column of a VIEW changes the view's output name: the view definition text must be
//!   re-deparsed too (` SELECT a AS aa,\n    bb AS b\n   FROM t;`): for views the dependency list of
//!   the VIEW's own query is not enough (the column is an output of the view, not a dependency), so
//!   views ALWAYS get `rewriter.rewrite(view.query, NameChange::Column{table: view, ...})`; views that
//!   SELECT FROM this view are rewritten through their `depends_on` as usual.
//! - Composite type attribute rename (`ALTER TYPE c RENAME ATTRIBUTE a TO b`) uses this function
//!   (`RelKind::CompositeType`); stored data of composite columns is positional so nothing else.
//!
//! ## ALTER COLUMN TYPE ([`CatalogTxn::alter_column_type`])
//! The binder has already decided what PG's `ATPrepAlterColumnType` decides: it resolved the
//! `USING` expression (or the implicit/assignment cast), checked coercibility (42804 `column "bb"
//! cannot be cast automatically to type integer` HINT `You might need to specify "USING bb::integer".`),
//! and whether the stored representation changes (`needs_rewrite`: int4 -> int8 yes; varchar(10) ->
//! varchar(20), varchar -> text, numeric precision widening no). The catalog:
//! - rejects system columns (`errors::cannot_alter_system_column`), dropped/unknown columns (42703);
//! - rejects columns used by views/matviews: 0A000 `errors::cannot_alter_type_used_by_view(view, col)`
//!   (any view/matview whose `depends_on` lists this column), by generated columns of the same table:
//!   0A000 `errors::cannot_alter_type_used_by_generated`, and by partition keys (later);
//! - re-derives index keys that are plain references to the column (`IndexKey::type_id`, `collation`,
//!   `opclass` = default btree opclass of the new type, 42704 `errors::no_default_opclass` if none);
//!   indexes with expression/predicate on the column: left as is (the binder re-validates);
//! - re-validates FKs on both sides (referencing column type changed, or referenced column type
//!   changed): pair compatibility as in `add_constraint`, else 42804 `errors::fk_cannot_be_implemented`;
//!   `ForeignKey::*_eq_ops` recomputed or cleared;
//! - applies `new_default` (the binder re-coerced the default to the new type;
//!   `Some(Some(e))` replaces, `Some(None)` removes, `None` keeps) ;
//! - updates the column: `type_id`, `collation` (explicit or the new type's), `ndims`, clears
//!   `missing_value` when `needs_rewrite`; identity column: the identity sequence's `type_oid` and
//!   default bounds follow the column type (bounds that were the old type's defaults become the
//!   new type's defaults; custom bounds out of range -> 22023 `MAXVALUE (...) is out of range for
//!   sequence data type smallint`);
//! - queues `FollowUp::RewriteTable{table}` when `needs_rewrite` (indexes are rebuilt by the rewrite).
use super::common::{is_system_column_name, validate_column_specs, MAX_COLUMNS};
use super::spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::snapshot::ClassRef;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::context::TypeEnv;
use pgarbor_types::TypeId;

/// Plural kind word used in `This operation is not supported for <plural>.`
fn kind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table => "tables",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::PartitionedTable => "partitioned tables",
        RelKind::Toast => "TOAST tables",
    }
}

/// Kinds on which ADD / DROP / ALTER TYPE of a column are allowed.
fn structure_alterable(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::PartitionedTable | RelKind::CompositeType | RelKind::ForeignTable)
}

/// (min, max) of a sequence data type OID (smallint / integer / bigint).
fn seq_type_bounds(type_oid: Oid) -> Option<(i64, i64)> {
    match type_oid {
        21 => Some((i16::MIN as i64, i16::MAX as i64)),
        23 => Some((i32::MIN as i64, i32::MAX as i64)),
        20 => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

fn seq_type_name(type_oid: Oid) -> &'static str {
    match type_oid {
        21 => "smallint",
        23 => "integer",
        _ => "bigint",
    }
}

/// New `(min, max)` of an identity sequence whose column type changes to `new_type` (PG
/// `init_params` handling of `AS type`: bounds equal to the old type's limits follow the new type,
/// custom bounds must fit it). Also re-checks MINVALUE < MAXVALUE and the START value.
fn retarget_sequence_bounds(seq: &Sequence, new_type: Oid) -> Result<(i64, i64)> {
    let (new_min, new_max) = seq_type_bounds(new_type).ok_or_else(errors::identity_bad_type)?;
    let (old_min, old_max) = seq_type_bounds(seq.type_oid).unwrap_or((i64::MIN, i64::MAX));
    let max = if seq.max == old_max { new_max } else { seq.max };
    let min = if seq.min == old_min { new_min } else { seq.min };
    let tname = seq_type_name(new_type);
    if max < new_min || max > new_max {
        return Err(errors::sequence_bound_out_of_range("MAXVALUE", max, tname));
    }
    if min < new_min || min > new_max {
        return Err(errors::sequence_bound_out_of_range("MINVALUE", min, tname));
    }
    if min >= max {
        return Err(errors::sequence_min_not_less_than_max(min, max));
    }
    if seq.start < min {
        return Err(errors::sequence_start_below_min(seq.start, min));
    }
    if seq.start > max {
        return Err(errors::sequence_start_above_max(seq.start, max));
    }
    Ok((min, max))
}

/// Foreign-key column pair compatibility (same rule family as `add_constraint`: identical types,
/// an `=` operator resolves for the pair, or an implicit cast fk -> pk exists).
fn fk_types_compatible(env: &CatalogEnv, fk: Oid, pk: Oid) -> bool {
    if fk == pk {
        return true;
    }
    if pgarbor_types::ops::resolve_builtin_operator(env, "=", Some(pk), fk).is_ok() {
        return true;
    }
    pgarbor_types::resolve::coerce::can_coerce_type(env, &[fk], &[pk], CastContext::Implicit).unwrap_or(false)
}

/// OID of the `=` operator for `(l, r)`, if one resolves.
fn eq_operator(env: &CatalogEnv, l: Oid, r: Oid) -> Option<Oid> {
    pgarbor_types::ops::resolve_builtin_operator(env, "=", Some(l), r).ok().map(|o| o.oid)
}

/// Re-deparse `expr` if it depends on the renamed column (or `force`).
fn rewrite_if(
    expr: &StoredExpr,
    force: bool,
    addr: &ObjectAddress,
    change: &NameChange,
    rewriter: &dyn ExprRewriter,
    env: &CatalogEnv,
) -> Result<Option<StoredExpr>> {
    if !force && !expr.depends_on.contains(addr) {
        return Ok(None);
    }
    let new = rewriter.rewrite(expr, change, env)?;
    Ok(if new != *expr { Some(new) } else { None })
}

impl<'a> CatalogTxn<'a> {
    /// Ownership, system-catalog and relkind checks shared by ADD / DROP / ALTER TYPE (PG
    /// `RangeVarCallbackForAlterRelation` + `ATSimplePermissions`). Returns a copy of the relation.
    fn alterable_relation(&mut self, oid: Oid, action: &str) -> Result<Relation> {
        let snap = self.snapshot()?;
        match snap.class(oid) {
            None => Err(errors::cache_lookup_failed("relation", oid)),
            Some(ClassRef::Relation(_)) => {
                let rel = snap.relation(oid).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", oid))?;
                self.require_owner(&ObjectAddress::relation(oid))?;
                if !self.data.relations.contains_key(&oid) {
                    return Err(errors::system_catalog_permission(&rel.name));
                }
                if !structure_alterable(rel.kind) {
                    return Err(errors::alter_action_not_supported(action, &rel.name, kind_plural(rel.kind)));
                }
                Ok(rel)
            }
            Some(_) => {
                let (name, kind) = match snap.class_identity(oid) {
                    Some((_, n, k)) => (n.to_string(), k),
                    None => return Err(errors::cache_lookup_failed("relation", oid)),
                };
                self.require_owner(&ObjectAddress::relation(oid))?;
                Err(errors::alter_action_not_supported(action, &name, kind_plural(kind)))
            }
        }
    }

    fn record_sequence_created(&mut self, seq: Oid) {
        let a = ObjectAddress::relation(seq);
        if !self.outcome.created.contains(&a) {
            self.record_created(a);
        }
    }

    /// `nextval('seq'::regclass)` default for a serial column.
    fn nextval_default(&mut self, seq: Oid) -> Result<StoredExpr> {
        let env = self.env()?;
        let qn = env.qualified_class_name(seq)?;
        let sql = format!("nextval('{}'::regclass)", qn.replace('\'', "''"));
        Ok(StoredExpr::new(sql).with_deps(vec![ObjectAddress::relation(seq)]))
    }

    /// Composite type changed: every table column declared with it must be empty (executor check).
    fn queue_composite_followups(&mut self, composite: &Relation) -> Result<()> {
        let snap = self.snapshot()?;
        let mut ups = Vec::new();
        for r in snap.user_relations() {
            if !matches!(r.kind, RelKind::Table | RelKind::PartitionedTable | RelKind::MatView) {
                continue;
            }
            for c in r.visible_columns() {
                let t = c.type_id.oid;
                if t == composite.rowtype || (composite.array_type != 0 && t == composite.array_type) {
                    ups.push(FollowUp::RequireEmptyTable { table: r.oid, attnum: c.attnum, type_oid: composite.rowtype });
                }
            }
        }
        for u in ups {
            self.follow_up(u);
        }
        Ok(())
    }

    /// ALTER TABLE ADD COLUMN (module docs). Returns the new attnum, or `None` when skipped by
    /// `IF NOT EXISTS`.
    // CONTRACT
    pub fn add_column(&mut self, table: Oid, column: &ColumnSpec, if_not_exists: bool) -> Result<Option<i16>> {
        let rel = self.alterable_relation(table, "ADD COLUMN")?;
        let composite = rel.kind == RelKind::CompositeType;

        // Name collisions (PG check_for_column_name_collision).
        if rel.columns.iter().any(|c| c.name == column.name) {
            if if_not_exists {
                self.notice(errors::column_already_exists_skipping(&column.name, &rel.name));
                return Ok(None);
            }
            return Err(errors::column_already_exists(&column.name, &rel.name));
        }
        if rel.kind.has_system_columns() && is_system_column_name(&column.name) {
            return Err(errors::column_conflicts_with_system_column(&column.name));
        }
        if rel.columns.len() + 1 > MAX_COLUMNS {
            return Err(errors::too_many_columns());
        }
        validate_column_specs(&self.env()?, &rel.name, std::slice::from_ref(column))?;

        // Identity columns need an integer type (checked before any write).
        if !composite {
            if let Some(idn) = &column.identity {
                let t = idn.options.type_oid.unwrap_or(column.type_id.oid);
                if seq_type_bounds(column.type_id.oid).is_none() || seq_type_bounds(t).is_none() {
                    return Err(errors::identity_bad_type());
                }
            }
        }

        let env = self.env()?;
        let info = env
            .type_info(column.type_id.oid)
            .ok_or_else(|| errors::cache_lookup_failed("type", column.type_id.oid))?;
        let attnum = rel.next_attnum();
        let collation = column.collation.unwrap_or(info.collation);
        let mut col = Column::new(attnum, column.name.clone(), column.type_id, collation);
        col.ndims = column.ndims;
        col.storage = column.storage;
        col.compression = column.compression;

        let mut rewrite = false;
        let mut want_not_null: Option<Option<String>> = None;
        let mut plain_default = false;
        if !composite {
            want_not_null = column.not_null.clone();
            if let Some(g) = &column.generated {
                let oid = self.new_oid()?;
                col.generated = Some(g.kind);
                col.default = Some(ColumnDefault { oid, expr: Self::bind_self_deps(&g.expr, table) });
                rewrite = g.kind == GeneratedKind::Stored;
            } else if column.serial.is_some() {
                // The nextval default is installed after the sequence exists.
                want_not_null = Some(None);
                rewrite = true;
            } else if column.identity.is_some() {
                want_not_null = Some(None);
                rewrite = true;
            } else if let Some(d) = &column.default {
                let oid = self.new_oid()?;
                col.default = Some(ColumnDefault { oid, expr: Self::bind_self_deps(d, table) });
                if column.default_is_volatile {
                    rewrite = true;
                } else {
                    col.missing_value = column.fast_default.clone();
                }
                plain_default = true;
            }
        }

        self.update_relation(table, move |r| {
            if r.columns.len() as i16 + 1 != attnum {
                return Err(PgError::internal("concurrent column addition"));
            }
            r.columns.push(col);
            Ok(())
        })?;
        let idx = (attnum - 1) as usize;

        if !composite {
            // serial: sequence + nextval default.
            if let Some(kind) = column.serial {
                let snap = self.snapshot()?;
                let seq_name =
                    naming::choose_relation_name(&snap, rel.namespace, &rel.name, Some(&column.name), "seq", &[])?;
                let seq = self.create_column_sequence(
                    rel.namespace,
                    &seq_name,
                    rel.owner,
                    rel.persistence,
                    kind.type_oid(),
                    &SequenceOptions::default(),
                    (table, attnum),
                    false,
                )?;
                self.record_sequence_created(seq);
                let expr = self.nextval_default(seq)?;
                let oid = self.new_oid()?;
                self.update_relation(table, move |r| {
                    let c = r.columns.get_mut(idx).ok_or_else(|| PgError::internal("new column vanished"))?;
                    c.default = Some(ColumnDefault { oid, expr });
                    Ok(())
                })?;
            }
            // identity: sequence + link.
            if let Some(idn) = &column.identity {
                let snap = self.snapshot()?;
                let seq_name = naming::choose_identity_sequence_name(
                    &snap,
                    rel.namespace,
                    &rel.name,
                    Some(&column.name),
                    idn.sequence_name.as_deref(),
                    &[],
                )?;
                let seq_type = idn.options.type_oid.unwrap_or(column.type_id.oid);
                let seq = self.create_column_sequence(
                    rel.namespace,
                    &seq_name,
                    rel.owner,
                    rel.persistence,
                    seq_type,
                    &idn.options,
                    (table, attnum),
                    true,
                )?;
                self.record_sequence_created(seq);
                let kind = idn.kind;
                self.update_relation(table, move |r| {
                    let c = r.columns.get_mut(idx).ok_or_else(|| PgError::internal("new column vanished"))?;
                    c.identity = Some(Identity { kind, sequence: seq });
                    Ok(())
                })?;
            }
            // NOT NULL constraint (t_a_not_null).
            if let Some(name) = &want_not_null {
                self.ensure_not_null(table, attnum, name.as_deref(), false)?;
            }
        }

        // Data consequences for existing rows.
        if rewrite {
            self.follow_up(FollowUp::RewriteTable { table });
        } else if want_not_null.is_some()
            && column.generated.is_none()
            && !column.default_is_volatile
            && (!plain_default || column.fast_default.is_none())
        {
            self.follow_up(FollowUp::ValidateNotNull { table, attnum });
        }
        if composite {
            self.queue_composite_followups(&rel)?;
        }
        Ok(Some(attnum))
    }

    /// ALTER TABLE DROP COLUMN (module docs). `Ok(false)` when skipped by `missing_ok`.
    // CONTRACT
    pub fn drop_column(&mut self, table: Oid, column: &str, behavior: DropBehavior, missing_ok: bool) -> Result<bool> {
        let rel = self.alterable_relation(table, "DROP COLUMN")?;
        let attnum = match rel.column_by_name(column) {
            Some(c) => c.attnum,
            None => {
                if rel.kind.has_system_columns() && is_system_column_name(column) {
                    return Err(errors::cannot_drop_system_column(column));
                }
                if missing_ok {
                    self.notice(errors::column_of_relation_does_not_exist_skipping(column, &rel.name));
                    return Ok(false);
                }
                return Err(errors::column_of_relation_does_not_exist(column, &rel.name));
            }
        };
        self.drop_objects(&[ObjectAddress::column(table, attnum)], behavior)?;
        if rel.kind == RelKind::CompositeType {
            self.queue_composite_followups(&rel)?;
        }
        Ok(true)
    }

    /// ALTER TABLE/VIEW/TYPE RENAME COLUMN (module docs).
    // CONTRACT
    pub fn rename_column(&mut self, rel: Oid, old: &str, new: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        let snap = self.snapshot()?;
        let class = snap.class(rel).ok_or_else(|| errors::cache_lookup_failed("relation", rel))?;
        if matches!(class, ClassRef::Index(_)) {
            return self.rename_index_column(rel, old, new);
        }
        let (relname, kind) = match snap.class_identity(rel) {
            Some((_, n, k)) => (n.to_string(), k),
            None => return Err(errors::cache_lookup_failed("relation", rel)),
        };
        // PG renameatt_check: relkind first, then ownership, then system catalogs.
        let allowed = matches!(
            kind,
            RelKind::Table
                | RelKind::View
                | RelKind::MatView
                | RelKind::CompositeType
                | RelKind::ForeignTable
                | RelKind::PartitionedTable
        );
        if !allowed {
            return Err(errors::operation_not_supported_for(
                &format!("cannot rename columns of relation \"{relname}\""),
                kind_plural(kind),
            ));
        }
        self.require_owner(&ObjectAddress::relation(rel))?;
        if !self.data.relations.contains_key(&rel) {
            return Err(errors::system_catalog_permission(&relname));
        }
        let relation = snap.relation(rel).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", rel))?;

        let attnum = match relation.column_by_name(old) {
            Some(c) => c.attnum,
            None => {
                if relation.kind.has_system_columns() && is_system_column_name(old) {
                    return Err(errors::cannot_rename_system_column(old));
                }
                return Err(errors::column_does_not_exist(old));
            }
        };
        if relation.columns.iter().any(|c| c.name == new) {
            return Err(errors::column_already_exists(new, &relation.name));
        }
        if relation.kind.has_system_columns() && is_system_column_name(new) {
            return Err(errors::column_conflicts_with_system_column(new));
        }

        let idx = (attnum - 1) as usize;
        let new_name = new.to_string();
        self.update_relation(rel, move |r| {
            let c = r.columns.get_mut(idx).ok_or_else(|| PgError::internal("column vanished"))?;
            c.name = new_name;
            Ok(())
        })?;

        // Re-deparse every stored expression that mentions the column.
        let env = self.env()?;
        let addr = ObjectAddress::column(rel, attnum);
        let change = NameChange::Column { table: rel, attnum, old: old.to_string(), new: new.to_string() };
        let force_view = matches!(relation.kind, RelKind::View | RelKind::MatView);
        let mut puts: Vec<CatalogObject> = Vec::new();

        for r in self.data.relations.values() {
            let mut def_edits: Vec<(usize, StoredExpr)> = Vec::new();
            for (i, c) in r.columns.iter().enumerate() {
                if let Some(d) = &c.default {
                    if let Some(n) = rewrite_if(&d.expr, false, &addr, &change, rewriter, &env)? {
                        def_edits.push((i, n));
                    }
                }
            }
            let view_edit = match &r.view {
                Some(v) => rewrite_if(&v.query, force_view && r.oid == rel, &addr, &change, rewriter, &env)?,
                None => None,
            };
            let mut pk_edits: Vec<(usize, StoredExpr)> = Vec::new();
            if let Some(pk) = &r.partition_key {
                for (i, e) in pk.exprs.iter().enumerate() {
                    if let Some(n) = rewrite_if(e, false, &addr, &change, rewriter, &env)? {
                        pk_edits.push((i, n));
                    }
                }
            }
            if def_edits.is_empty() && view_edit.is_none() && pk_edits.is_empty() {
                continue;
            }
            let mut nr = r.clone();
            for (i, n) in def_edits {
                if let Some(d) = nr.columns[i].default.as_mut() {
                    d.expr = n;
                }
            }
            if let (Some(n), Some(v)) = (view_edit, nr.view.as_mut()) {
                v.query = n;
            }
            if let Some(pk) = nr.partition_key.as_mut() {
                for (i, n) in pk_edits {
                    pk.exprs[i] = n;
                }
            }
            puts.push(CatalogObject::Relation(nr));
        }

        for c in self.data.constraints.values() {
            if let Some(chk) = &c.check {
                if let Some(n) = rewrite_if(chk, false, &addr, &change, rewriter, &env)? {
                    let mut nc = c.clone();
                    nc.check = Some(n);
                    puts.push(CatalogObject::Constraint(nc));
                }
            }
        }

        for ix in self.data.indexes.values() {
            let mut key_edits: Vec<(usize, StoredExpr)> = Vec::new();
            for (i, k) in ix.keys.iter().enumerate() {
                if let Some(e) = &k.expr {
                    if let Some(n) = rewrite_if(e, false, &addr, &change, rewriter, &env)? {
                        key_edits.push((i, n));
                    }
                }
            }
            let pred_edit = match &ix.predicate {
                Some(p) => rewrite_if(p, false, &addr, &change, rewriter, &env)?,
                None => None,
            };
            if key_edits.is_empty() && pred_edit.is_none() {
                continue;
            }
            // IndexKey::name is deliberately left alone (PG keeps the index attribute name).
            let mut ni = ix.clone();
            for (i, n) in key_edits {
                ni.keys[i].expr = Some(n);
            }
            if let Some(n) = pred_edit {
                ni.predicate = Some(n);
            }
            puts.push(CatalogObject::Index(ni));
        }

        for f in self.data.functions.values() {
            if let Some(b) = &f.sql_body {
                if let Some(n) = rewrite_if(b, false, &addr, &change, rewriter, &env)? {
                    let mut nf = f.clone();
                    nf.sql_body = Some(n);
                    puts.push(CatalogObject::Function(nf));
                }
            }
        }

        for o in puts {
            self.put(o)?;
        }
        Ok(())
    }

    /// RENAME COLUMN on an index: renames the index attribute.
    fn rename_index_column(&mut self, index: Oid, old: &str, new: &str) -> Result<()> {
        self.require_owner(&ObjectAddress::relation(index))?;
        let ix = self
            .data
            .indexes
            .get(&index)
            .cloned()
            .ok_or_else(|| errors::cache_lookup_failed("index", index))?;
        let pos = match ix.keys.iter().position(|k| k.name == old) {
            Some(p) => p,
            None => return Err(errors::column_does_not_exist(old)),
        };
        if ix.keys.iter().any(|k| k.name == new) {
            return Err(errors::column_already_exists(new, &ix.name));
        }
        let new_name = new.to_string();
        self.update_index(index, move |i| {
            i.keys[pos].name = new_name;
            Ok(())
        })
    }

    /// ALTER TABLE ALTER COLUMN TYPE (module docs).
    // CONTRACT
    #[allow(clippy::too_many_arguments)]
    pub fn alter_column_type(
        &mut self,
        table: Oid,
        column: &str,
        new_type: TypeId,
        new_collation: Option<Oid>,
        new_ndims: i32,
        new_default: Option<Option<StoredExpr>>,
        needs_rewrite: bool,
    ) -> Result<()> {
        let rel = self.alterable_relation(table, "ALTER COLUMN TYPE")?;
        let attnum = match rel.column_by_name(column) {
            Some(c) => c.attnum,
            None => {
                if rel.kind.has_system_columns() && is_system_column_name(column) {
                    return Err(errors::cannot_alter_system_column(column));
                }
                return Err(errors::column_of_relation_does_not_exist(column, &rel.name));
            }
        };
        let old_col = rel.columns[(attnum - 1) as usize].clone();
        let addr = ObjectAddress::column(table, attnum);
        let snap = self.snapshot()?;
        let env = self.env()?;

        // Views / matviews reading the column.
        for r in snap.user_relations() {
            if let Some(v) = &r.view {
                if v.query.depends_on.contains(&addr) {
                    let vname = env.qualified_class_name(r.oid)?;
                    return Err(errors::cannot_alter_type_used_by_view(&vname, column));
                }
            }
        }
        // Generated columns of the same table reading the column.
        for c in rel.columns.iter() {
            if c.dropped || c.attnum == attnum || c.generated.is_none() {
                continue;
            }
            if let Some(d) = &c.default {
                if d.expr.depends_on.contains(&addr) {
                    return Err(errors::cannot_alter_type_used_by_generated(column, &c.name));
                }
            }
        }

        // New type metadata.
        let info = env
            .type_info(new_type.oid)
            .ok_or_else(|| errors::cache_lookup_failed("type", new_type.oid))?;
        if info.typtype == b'p' {
            return Err(errors::column_has_pseudo_type(column, &info.name));
        }
        let collation = new_collation.unwrap_or(info.collation);

        // Identity sequence follows the column type.
        let mut seq_update: Option<(Oid, i64, i64)> = None;
        if let Some(idn) = &old_col.identity {
            if seq_type_bounds(new_type.oid).is_none() {
                return Err(errors::identity_bad_type());
            }
            if let Some(seq) = snap.sequence(idn.sequence) {
                let (min, max) = retarget_sequence_bounds(seq, new_type.oid)?;
                seq_update = Some((seq.oid, min, max));
            }
        }

        // Foreign keys on either side.
        let type_of = |r: Oid, a: i16| -> Option<Oid> {
            if r == table && a == attnum {
                return Some(new_type.oid);
            }
            snap.relation(r).and_then(|x| x.column(a)).map(|c| c.type_id.oid)
        };
        let col_name_of = |r: Oid, a: i16| -> String {
            snap.relation(r).and_then(|x| x.column(a)).map(|c| c.name.clone()).unwrap_or_default()
        };
        let mut fk_updates: Vec<(Oid, Vec<Oid>, Vec<Oid>, Vec<Oid>)> = Vec::new();
        for c in snap.constraints_of(table).into_iter().chain(snap.foreign_keys_referencing(table)) {
            let fk = match (&c.kind, &c.fk) {
                (ConstraintKind::ForeignKey, Some(fk)) => fk,
                _ => continue,
            };
            let touches_fk_side = c.table == table && c.columns.contains(&attnum);
            let touches_pk_side = fk.ref_table == table && fk.ref_columns.contains(&attnum);
            if !touches_fk_side && !touches_pk_side {
                continue;
            }
            if fk_updates.iter().any(|u| u.0 == c.oid) {
                continue;
            }
            let (mut pf, mut pp, mut ff) = (Vec::new(), Vec::new(), Vec::new());
            let mut all_ops = true;
            for (fa, pa) in c.columns.iter().zip(fk.ref_columns.iter()) {
                let ft = type_of(c.table, *fa).ok_or_else(|| errors::cache_lookup_failed("column", c.table))?;
                let pt = type_of(fk.ref_table, *pa).ok_or_else(|| errors::cache_lookup_failed("column", fk.ref_table))?;
                if !fk_types_compatible(&env, ft, pt) {
                    let fts = pgarbor_types::names::format_type(&env, ft, -1)?;
                    let pts = pgarbor_types::names::format_type(&env, pt, -1)?;
                    return Err(errors::fk_cannot_be_implemented(
                        &c.name,
                        &col_name_of(c.table, *fa),
                        &col_name_of(fk.ref_table, *pa),
                        &fts,
                        &pts,
                    ));
                }
                match (eq_operator(&env, pt, ft), eq_operator(&env, pt, pt), eq_operator(&env, ft, ft)) {
                    (Some(a), Some(b), Some(d)) => {
                        pf.push(a);
                        pp.push(b);
                        ff.push(d);
                    }
                    _ => all_ops = false,
                }
            }
            if !all_ops {
                pf.clear();
                pp.clear();
                ff.clear();
            }
            fk_updates.push((c.oid, pf, pp, ff));
        }

        // Plain index keys on the column.
        let mut index_updates: Vec<(Oid, Vec<usize>)> = Vec::new();
        for ix in snap.indexes_of(table) {
            let positions: Vec<usize> =
                ix.keys.iter().enumerate().filter(|(_, k)| k.column == Some(attnum)).map(|(i, _)| i).collect();
            if !positions.is_empty() {
                index_updates.push((ix.oid, positions));
            }
        }
        let mut new_opclass: Option<Oid> = None;
        if !index_updates.is_empty() {
            let any_key = index_updates.iter().any(|(oid, pos)| {
                snap.index(*oid).is_some_and(|ix| pos.iter().any(|p| *p < ix.n_key_columns as usize))
            });
            if any_key {
                match self.default_btree_opclass(new_type.oid)? {
                    Some(o) => new_opclass = Some(o),
                    None => {
                        let tn = pgarbor_types::names::format_type(&env, new_type.oid, -1)?;
                        return Err(errors::no_default_opclass(&tn, "btree"));
                    }
                }
            }
        }

        // New default.
        let default_edit: Option<Option<ColumnDefault>> = match new_default {
            None => None,
            Some(None) => {
                if old_col.generated.is_some() {
                    return Err(PgError::internal("cannot remove the expression of a generated column"));
                }
                Some(None)
            }
            Some(Some(e)) => {
                let oid = match &old_col.default {
                    Some(d) => d.oid,
                    None => self.new_oid()?,
                };
                Some(Some(ColumnDefault { oid, expr: Self::bind_self_deps(&e, table) }))
            }
        };

        // ---- writes ----
        for (ix_oid, positions) in index_updates {
            let n_key = snap.index(ix_oid).map(|i| i.n_key_columns as usize).unwrap_or(0);
            let tid = TypeId::new(new_type.oid, new_type.typmod);
            self.update_index(ix_oid, move |ix| {
                for p in positions {
                    if let Some(k) = ix.keys.get_mut(p) {
                        k.type_id = tid;
                        k.collation = collation;
                        if p < n_key {
                            if let Some(o) = new_opclass {
                                k.opclass = o;
                            }
                        }
                    }
                }
                Ok(())
            })?;
        }
        for (coid, pf, pp, ff) in fk_updates {
            self.update_constraint(coid, move |c| {
                if let Some(fk) = c.fk.as_mut() {
                    fk.pf_eq_ops = pf;
                    fk.pp_eq_ops = pp;
                    fk.ff_eq_ops = ff;
                }
                Ok(())
            })?;
        }
        if let Some((seq, min, max)) = seq_update {
            let t = new_type.oid;
            self.update_sequence(seq, move |s| {
                s.type_oid = t;
                s.min = min;
                s.max = max;
                Ok(())
            })?;
        }
        let idx = (attnum - 1) as usize;
        self.update_relation(table, move |r| {
            let c = r.columns.get_mut(idx).ok_or_else(|| PgError::internal("column vanished"))?;
            c.type_id = new_type;
            c.collation = collation;
            c.ndims = new_ndims;
            if needs_rewrite {
                c.missing_value = None;
            }
            if let Some(d) = default_edit {
                c.default = d;
            }
            Ok(())
        })?;

        if rel.kind == RelKind::CompositeType {
            self.queue_composite_followups(&rel)?;
        } else if needs_rewrite && rel.kind.has_storage() {
            self.follow_up(FollowUp::RewriteTable { table });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use crate::testing;
    use pgarbor_common::MemKv;

    fn create(kv: &mut MemKv, env: &CatalogEnv, name: &str, cols: Vec<ColumnSpec>) -> Result<(Oid, CatalogEnv)> {
        let spec = CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: cols,
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let mut oid = 0;
        let (_, env) = testing::run(kv, env, |t| {
            oid = t.create_table(&spec)?.map(|c| c.table).unwrap_or(0);
            Ok(())
        })?;
        Ok((oid, env))
    }

    #[test]
    fn sequence_bounds_follow_type() {
        let seq = Sequence {
            oid: 1,
            name: "s".into(),
            namespace: PUBLIC_NAMESPACE,
            owner: BOOTSTRAP_SUPERUSER,
            persistence: Persistence::Permanent,
            type_oid: 23,
            start: 1,
            increment: 1,
            min: 1,
            max: i32::MAX as i64,
            cache: 1,
            cycle: false,
            owned_by: None,
            identity: true,
            acl: None,
        };
        assert_eq!(retarget_sequence_bounds(&seq, 20).unwrap(), (1, i64::MAX));
        let mut custom = seq.clone();
        custom.max = 100_000;
        let e = retarget_sequence_bounds(&custom, 21).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "MAXVALUE (100000) is out of range for sequence data type smallint");
        assert!(retarget_sequence_bounds(&seq, 25).is_err());
    }

    #[test]
    fn plural_words() {
        assert_eq!(kind_plural(RelKind::View), "views");
        assert_eq!(kind_plural(RelKind::MatView), "materialized views");
        assert!(!structure_alterable(RelKind::View));
        assert!(structure_alterable(RelKind::CompositeType));
    }

    #[test]
    fn add_column_fast_default_and_volatile() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (t, env) = create(&mut kv, &env, "t", vec![ColumnSpec::new("a", TypeId::INT4)])?;

        let mut c = ColumnSpec::new("b", TypeId::INT4);
        c.default = Some(StoredExpr::new("5"));
        c.fast_default = Some("5".into());
        let (res, env) = testing::run(&mut kv, &env, |x| {
            assert_eq!(x.add_column(t, &c, false)?, Some(2));
            Ok(())
        })?;
        assert!(res.outcome.follow_ups.iter().all(|f| !matches!(f, FollowUp::RewriteTable { .. })));
        let col = res.snapshot.relation(t).unwrap().column(2).unwrap().clone();
        assert_eq!(col.missing_value.as_deref(), Some("5"));

        let mut v = ColumnSpec::new("c", TypeId::FLOAT8);
        v.default = Some(StoredExpr::new("random()"));
        v.default_is_volatile = true;
        let (res, env) = testing::run(&mut kv, &env, |x| x.add_column(t, &v, false).map(|_| ()))?;
        assert!(res.outcome.follow_ups.contains(&FollowUp::RewriteTable { table: t }));

        // Duplicate name.
        let dup = ColumnSpec::new("a", TypeId::INT4);
        let err = testing::run(&mut kv, &env, |x| x.add_column(t, &dup, false).map(|_| ())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42701");
        assert_eq!(err.message, "column \"a\" of relation \"t\" already exists");
        let (res, _) = testing::run(&mut kv, &env, |x| {
            assert_eq!(x.add_column(t, &dup, true)?, None);
            Ok(())
        })?;
        assert_eq!(res.outcome.notices.len(), 1);
        Ok(())
    }

    #[test]
    fn drop_and_rename_column() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (t, env) = create(
            &mut kv,
            &env,
            "t",
            vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::TEXT)],
        )?;
        let (res, env) = testing::run(&mut kv, &env, |x| x.rename_column(t, "b", "bb", &NoRewrite))?;
        assert_eq!(res.snapshot.relation(t).unwrap().column(2).unwrap().name, "bb");

        let err = testing::run(&mut kv, &env, |x| x.rename_column(t, "ctid", "z", &NoRewrite)).unwrap_err();
        assert_eq!(err.message, "cannot rename system column \"ctid\"");
        let err = testing::run(&mut kv, &env, |x| x.rename_column(t, "nope", "z", &NoRewrite)).unwrap_err();
        assert_eq!(err.message, "column \"nope\" does not exist");
        let err = testing::run(&mut kv, &env, |x| x.rename_column(t, "a", "bb", &NoRewrite)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42701");

        let (res, env) = testing::run(&mut kv, &env, |x| {
            assert!(x.drop_column(t, "a", DropBehavior::Restrict, false)?);
            Ok(())
        })?;
        let c = res.snapshot.relation(t).unwrap().column(1).unwrap().clone();
        assert!(c.dropped);
        assert_eq!(c.name, Column::dropped_name(1));
        assert_eq!(c.type_id.oid, 0);
        let (res, _) = testing::run(&mut kv, &env, |x| {
            assert!(!x.drop_column(t, "a", DropBehavior::Restrict, true)?);
            Ok(())
        })?;
        assert_eq!(res.outcome.notices.len(), 1);
        Ok(())
    }

    #[test]
    fn alter_type_int4_to_int8() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (t, env) = create(&mut kv, &env, "t", vec![ColumnSpec::new("a", TypeId::INT4)])?;
        let (res, env) = testing::run(&mut kv, &env, |x| x.alter_column_type(t, "a", TypeId::INT8, None, 0, None, true))?;
        assert_eq!(res.snapshot.relation(t).unwrap().column(1).unwrap().type_id, TypeId::INT8);
        assert!(res.outcome.follow_ups.contains(&FollowUp::RewriteTable { table: t }));
        let err = testing::run(&mut kv, &env, |x| x.alter_column_type(t, "xmin", TypeId::INT8, None, 0, None, true))
            .unwrap_err();
        assert_eq!(err.message, "cannot alter system column \"xmin\"");
        Ok(())
    }
}
