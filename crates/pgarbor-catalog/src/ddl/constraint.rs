//! Table constraints: ADD / DROP / RENAME / VALIDATE / ALTER CONSTRAINT, NOT NULL constraints and
//! referenced-key lookup. Lease CAT-12 (sonnet). PG sources: `commands/tablecmds.c`
//! (`ATExecAddConstraint`, `ATAddCheckNNConstraint`, `ATAddForeignKeyConstraint`,
//! `ATExecDropConstraint`, `ATExecValidateConstraint`, `ATExecAlterConstraint`,
//! `transformIndexConstraint` in parse_utilcmd.c, `ChooseConstraintName` in pg_constraint.c),
//! `catalog/heap.c` (`StoreRelCheck`, `AddRelationNotNullConstraints`), `catalog/index.c`
//! (`index_constraint_create`), `catalog/pg_constraint.c` (`CreateConstraintEntry`).
//!
//! ## What each constraint kind creates
//! | spec kind | objects written |
//! |---|---|
//! | PrimaryKey | NOT NULL constraints for key columns lacking one (`t_id_not_null`), `Index{primary, unique}` named `t_pkey` (or the constraint name), `Constraint{kind PrimaryKey, columns = key attnums, index}` named `t_pkey`; sets `Column::not_null` |
//! | Unique | `Index{unique}` + `Constraint{Unique}` named `t_a_b_key` |
//! | Check | `Constraint{Check, check: StoredExpr, columns = referenced attnums}` named `t_<firstcol>_check` (`t_check` when no columns) |
//! | NotNull | `Constraint{NotNull, columns=[attnum]}` named `t_a_not_null`; sets `Column::not_null` |
//! | ForeignKey | `Constraint{ForeignKey, columns, fk: ForeignKey{ref_table, ref_columns, ref_index, actions, match}}` named `t_a_fkey` |
//! | Exclusion | `Err(PgError::unimplemented("CAT-12", "EXCLUDE constraints"))` |
//! The constraint's `namespace` is the table's. Names: `naming::choose_constraint_name` for
//! automatic ones, checked against ALL constraints of the namespace (PG's pg_constraint unique
//! index is (conrelid, contypid, conname) for table constraints, but ChooseConstraintName avoids
//! clashes namespace-wide). Explicit name clash on the same table -> 42710 `constraint "c" for
//! relation "t" already exists` (`errors::constraint_already_exists`) - EXCEPT inside CREATE TABLE
//! where two CHECKs with one name give `check constraint "c" already exists`. An explicit
//! PK/UNIQUE constraint name that equals an existing RELATION name in the namespace (its index
//! would clash) -> 42P07 `relation "x" already exists`.
//!
//! ## Validation rules (oracle-verified messages)
//! - key columns must exist: 42703 `column "nosuch" named in key does not exist`;
//!   duplicated key column: 42701 `column "a" appears twice in unique constraint` /
//!   `primary key constraint`.
//! - second PRIMARY KEY: 42P16 `multiple primary keys for table "t" are not allowed`.
//! - NOT NULL on a view/index/... is rejected earlier by the table-kind check
//!   (`errors::alter_action_not_supported`).
//! - **Foreign keys** (`ATAddForeignKeyConstraint`):
//!   referencing and referenced column COUNTS must match: 42830 `number of referencing and
//!   referenced columns for foreign key disagree`; > 32 columns: 54011 `cannot have more than 32
//!   keys in a foreign key`; referenced column duplicates: 42701 `foreign key referenced-columns
//!   list must not contain duplicates`; empty `ref_columns` -> the referenced table's PRIMARY KEY
//!   columns, else 42704 `there is no primary key for referenced table "t"`; the referenced
//!   columns must be exactly (as a set, any order) the key of a non-partial, non-expression UNIQUE
//!   index (`find_referenced_index`), else 42830 `there is no unique constraint matching given keys
//!   for referenced table "t"`. Column type compatibility per pair: identical types, or an `=`
//!   operator resolves for (fk type, pk type) (`pgarbor_types::ops`), or an implicit cast exists;
//!   otherwise 42804 `foreign key constraint "f_x_fkey" cannot be implemented` DETAIL `Key columns
//!   "x" of the referencing table and "a" of the referenced table are of incompatible types: text and integer.`
//!   (`errors::fk_cannot_be_implemented`, types via `pgarbor_types::names::format_type`).
//!   MATCH PARTIAL -> 0A000 `MATCH PARTIAL not yet implemented`. `SET NULL/DEFAULT (cols)` columns
//!   must be among the FK columns (42P10 `column "x" referenced in ON DELETE SET action must be part
//!   of foreign key`). Referenced table must be a table (42809 `referenced relation "v" is not a
//!   table`). Privilege: REFERENCES on the referenced table (owner check suffices in v1).
//!   `ForeignKey::pf_eq_ops/pp_eq_ops/ff_eq_ops`: fill with the `=` operator OIDs when found, else leave empty.
//! - CHECK: `columns` from the spec; `enforced = false` (`NOT ENFORCED`) allowed for CHECK and FK.
//! - `not_valid` is only grammatical for CHECK and FOREIGN KEY (the parser rejects it elsewhere);
//!   on any other kind return `PgError::syntax("NOT VALID is only supported for CHECK and FOREIGN KEY constraints", None)`.
//!
//! ## Follow-ups for ALTER TABLE ADD (`creating = false`)
//! A new index -> `FollowUp::BuildIndex`; NOT NULL / PRIMARY KEY columns -> `ValidateNotNull` per
//! newly NOT NULL column; CHECK (validated) -> `ValidateCheck`; FK (validated) ->
//! `ValidateForeignKey`. `NOT VALID` constraints get `validated = false` and no validation
//! follow-up (VALIDATE CONSTRAINT queues it later). For `add_constraints_to_new_table` (empty
//! table) none are queued.
use super::common::SYSTEM_COLUMNS;
use super::spec::*;
use crate::data::CatalogData;
use crate::errors;
use crate::model::*;
use crate::naming::{self, IndexKeySource};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::ops::{self, OperCandidate};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::{names, oids, registry, TypeEnv};

/// PG `INDEX_MAX_KEYS`.
const INDEX_MAX_KEYS: usize = 32;

/// Plural relation-kind word of PG's `alter_table_type_to_string` / `errdetail_relkind_not_supported`.
fn kind_plural(kind: RelKind) -> &'static str {
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

/// 54011 `cannot have more than 32 keys in a foreign key`.
fn too_many_fk_keys() -> PgError {
    PgError::new(SqlState(*b"54011"), "cannot have more than 32 keys in a foreign key")
}

/// 42703 `column "x" referenced in foreign key constraint does not exist`.
fn fk_column_missing(col: &str) -> PgError {
    PgError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{col}\" referenced in foreign key constraint does not exist"),
    )
}

/// Name of a system column by attnum (for error messages).
fn system_column_name(attnum: i16) -> &'static str {
    SYSTEM_COLUMNS.iter().find(|(_, a)| *a == attnum).map(|(n, _)| *n).unwrap_or("?")
}

/// Input type of a builtin operator class (`pg_opclass.opcintype`).
fn opclass_intype(opclass: Oid) -> Option<Oid> {
    let key = opclass.to_string();
    crate::builtin::OPCLASSES
        .rows
        .iter()
        .find(|r| r.first().copied().flatten() == Some(key.as_str()))
        .and_then(|r| r.get(6).copied().flatten())
        .and_then(|s| s.parse::<Oid>().ok())
}

/// Operator class input types that stand for a whole family of concrete types.
fn is_pseudo_opcintype(t: Oid) -> bool {
    t == 0
        || matches!(
            t,
            oids::ANYARRAY
                | oids::ANYENUM
                | oids::ANYRANGE
                | oids::ANYMULTIRANGE
                | oids::ANYELEMENT
                | oids::ANYNONARRAY
                | oids::RECORD
        )
}

/// The `=` operator taking exactly (`l`, `r`), if any.
fn eq_operator(env: &dyn TypeEnv, cands: &[OperCandidate], l: Oid, r: Oid) -> Option<Oid> {
    let res = ops::resolve_operator(env, "=", Some(l), r, cands).ok()?;
    if res.left == Some(l) && res.right == r {
        Some(res.oid)
    } else {
        None
    }
}

/// `from` converts to `to` implicitly (identical, binary-coercible or an implicit cast).
fn implicit_ok(env: &dyn TypeEnv, from: Oid, to: Oid) -> bool {
    from == to || matches!(pgarbor_types::cast::find_cast(env, from, to, CastContext::Implicit), Ok(Some(_)))
}

/// PG `transformFkeyCheckAttrs` index search: the UNIQUE/PRIMARY btree index of `ref_table` whose
/// key column set equals `attnums` (non-partial, no expression keys, valid, immediate). Prefers the
/// primary key, then the lowest OID.
fn find_unique_index(data: &CatalogData, ref_table: Oid, attnums: &[i16], require_immediate: bool) -> Option<Oid> {
    let mut want: Vec<i16> = attnums.to_vec();
    want.sort_unstable();
    want.dedup();
    if want.len() != attnums.len() || want.is_empty() {
        return None;
    }
    let mut best: Option<(bool, Oid)> = None;
    for idx in data.indexes.values() {
        if idx.table != ref_table || !idx.unique || !idx.valid || (require_immediate && !idx.immediate) || idx.predicate.is_some() {
            continue;
        }
        let keys = idx.key_columns();
        if keys.len() != want.len() {
            continue;
        }
        let mut cols: Vec<i16> = Vec::with_capacity(keys.len());
        let mut plain = true;
        for k in keys {
            match k.column {
                Some(c) if k.expr.is_none() => cols.push(c),
                _ => {
                    plain = false;
                    break;
                }
            }
        }
        if !plain {
            continue;
        }
        cols.sort_unstable();
        if cols != want {
            continue;
        }
        match best {
            None => best = Some((idx.primary, idx.oid)),
            Some((false, _)) if idx.primary => best = Some((true, idx.oid)),
            Some(_) => {}
        }
    }
    best.map(|(_, o)| o)
}

/// Does `acl` (None = owner-only default) grant `bit` to `user` or PUBLIC directly?
fn acl_grants(acl: &Option<Acl>, user: Oid, bit: u32) -> bool {
    match acl {
        Some(items) => items.iter().any(|i| (i.grantee == user || i.grantee == 0) && i.privileges & bit != 0),
        None => false,
    }
}

/// The parts of a FOREIGN KEY spec.
struct FkParams<'a> {
    columns: &'a [String],
    ref_table: Oid,
    ref_columns: &'a [String],
    on_update: FkAction,
    on_delete: FkAction,
    match_type: FkMatch,
    delete_set_columns: &'a [String],
}

impl<'a> CatalogTxn<'a> {
    // ------------------------------------------------------------------ private helpers

    /// Current relation row (user or builtin) by OID.
    fn con_rel(&mut self, oid: Oid) -> Result<Relation> {
        if let Some(r) = self.data.relations.get(&oid) {
            return Ok(r.clone());
        }
        let snap = self.snapshot()?;
        snap.relation(oid).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", oid))
    }

    /// Owner + table-kind checks shared by every ALTER TABLE constraint action.
    fn con_check_target(&mut self, table: Oid, action: &str) -> Result<Relation> {
        let rel = self.con_rel(table)?;
        self.require_owner(&ObjectAddress::relation(table))?;
        if rel.kind != RelKind::Table {
            return Err(errors::alter_action_not_supported(action, &rel.name, kind_plural(rel.kind)));
        }
        if rel.is_virtual() {
            return Err(errors::system_catalog_permission(&rel.name));
        }
        Ok(rel)
    }

    /// The constraint `name` of `table`.
    fn con_find(&self, table: Oid, name: &str) -> Option<Constraint> {
        self.data.constraints.values().find(|c| c.table == table && c.name == name).cloned()
    }

    /// Record an index as created unless the index builder already did.
    fn con_record_index_created(&mut self, index: Oid) {
        let addr = ObjectAddress::relation(index);
        if !self.outcome.created.contains(&addr) {
            self.record_created(addr);
        }
    }

    /// Dispatch one constraint spec (`creating` = inside CREATE TABLE of an empty table).
    fn con_add(&mut self, table: Oid, spec: &ConstraintSpec, creating: bool) -> Result<Vec<Oid>> {
        if spec.not_valid && !matches!(spec.kind, ConstraintSpecKind::Check { .. } | ConstraintSpecKind::ForeignKey { .. }) {
            return Err(PgError::syntax("NOT VALID is only supported for CHECK and FOREIGN KEY constraints", None));
        }
        if spec.initially_deferred && !spec.deferrable {
            return Err(PgError::syntax("constraint declared INITIALLY DEFERRED must be DEFERRABLE", None));
        }
        if !spec.enforced {
            let kind_word = match &spec.kind {
                ConstraintSpecKind::PrimaryKey { .. } => Some("PRIMARY KEY"),
                ConstraintSpecKind::Unique { .. } => Some("UNIQUE"),
                ConstraintSpecKind::NotNull { .. } => Some("NOT NULL"),
                _ => None,
            };
            if let Some(w) = kind_word {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("{w} constraints cannot be marked NOT ENFORCED"),
                ));
            }
        }
        match &spec.kind {
            ConstraintSpecKind::PrimaryKey { columns, include } => {
                self.con_add_index_constraint(table, spec, columns, include, true, false, creating)
            }
            ConstraintSpecKind::Unique { columns, include, nulls_not_distinct } => {
                self.con_add_index_constraint(table, spec, columns, include, false, *nulls_not_distinct, creating)
            }
            ConstraintSpecKind::Check { expr, columns, no_inherit } => {
                if spec.deferrable {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "CHECK constraints cannot be marked DEFERRABLE",
                    ));
                }
                self.con_add_check(table, spec, expr, columns, *no_inherit, creating)
            }
            ConstraintSpecKind::NotNull { column } => {
                if spec.deferrable {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "NOT NULL constraints cannot be marked DEFERRABLE",
                    ));
                }
                let rel = self.con_rel(table)?;
                let attnum = rel
                    .column_by_name(column)
                    .map(|c| c.attnum)
                    .ok_or_else(|| errors::column_of_relation_does_not_exist(column, &rel.name))?;
                let (oid, created) = self.ensure_not_null(table, attnum, spec.name.as_deref(), !creating)?;
                Ok(if created { vec![oid] } else { Vec::new() })
            }
            ConstraintSpecKind::ForeignKey {
                columns,
                ref_table,
                ref_columns,
                on_update,
                on_delete,
                match_type,
                delete_set_columns,
            } => {
                let p = FkParams {
                    columns,
                    ref_table: *ref_table,
                    ref_columns,
                    on_update: *on_update,
                    on_delete: *on_delete,
                    match_type: *match_type,
                    delete_set_columns,
                };
                self.con_add_fk(table, spec, &p, creating)
            }
            ConstraintSpecKind::Exclusion => Err(PgError::unimplemented("CAT-12", "EXCLUDE constraints")),
        }
    }

    /// PRIMARY KEY / UNIQUE: NOT NULL constraints (PK), the backing index and the constraint row.
    #[allow(clippy::too_many_arguments)]
    fn con_add_index_constraint(
        &mut self,
        table: Oid,
        spec: &ConstraintSpec,
        columns: &[String],
        include: &[String],
        primary: bool,
        nulls_not_distinct: bool,
        creating: bool,
    ) -> Result<Vec<Oid>> {
        let rel = self.con_rel(table)?;
        let what = if primary { "primary key constraint" } else { "unique constraint" };

        if primary && self.data.constraints.values().any(|c| c.table == table && c.kind == ConstraintKind::PrimaryKey) {
            return Err(errors::multiple_primary_keys(&rel.name));
        }
        if columns.is_empty() {
            return Err(PgError::syntax(format!("{what} requires at least one column"), None));
        }

        // Key columns.
        let mut attnums: Vec<i16> = Vec::with_capacity(columns.len());
        for name in columns {
            let col = rel.column_by_name(name).ok_or_else(|| errors::key_column_does_not_exist(name))?;
            if attnums.contains(&col.attnum) {
                return Err(errors::column_appears_twice(name, what));
            }
            attnums.push(col.attnum);
        }
        for name in include {
            if rel.column_by_name(name).is_none() {
                return Err(errors::key_column_does_not_exist(name));
            }
        }
        if columns.len() + include.len() > INDEX_MAX_KEYS {
            return Err(errors::too_many_index_columns());
        }

        // Name: the constraint and its index share it.
        let snap = self.snapshot()?;
        let name = match &spec.name {
            Some(n) => {
                if snap.class_in_namespace(rel.namespace, n).is_some() {
                    return Err(errors::relation_already_exists(n));
                }
                if self.con_find(table, n).is_some() {
                    return Err(errors::constraint_already_exists(n, &rel.name));
                }
                n.clone()
            }
            None => {
                let sources: Vec<IndexKeySource> =
                    columns.iter().chain(include.iter()).map(|c| IndexKeySource::Column(c.clone())).collect();
                let col_names = naming::choose_index_column_names(&sources)?;
                naming::choose_index_name(&snap, rel.namespace, &rel.name, &col_names, primary, !primary, &[])?
            }
        };

        let mut created: Vec<Oid> = Vec::new();

        // PRIMARY KEY columns become NOT NULL (constraint `t_id_not_null`).
        if primary {
            for a in &attnums {
                let (oid, new) = self.ensure_not_null(table, *a, None, !creating)?;
                if new {
                    created.push(oid);
                }
            }
        }

        // Backing index, then the constraint that owns it.
        let coid = self.new_oid()?;
        let keys: Vec<IndexKeySpec> = columns
            .iter()
            .map(|c| IndexKeySpec {
                column: Some(c.clone()),
                expr: None,
                expr_func_name: None,
                expr_type: None,
                opclass: None,
                collation: None,
                descending: false,
                nulls_first: None,
            })
            .collect();
        let ispec = IndexSpec {
            table,
            name: Some(name.clone()),
            unique: true,
            nulls_not_distinct,
            method: IndexMethod::Btree,
            keys,
            include: include.to_vec(),
            predicate: None,
            options: spec.index_options.clone(),
            if_not_exists: false,
            concurrently: false,
        };
        let index = self.create_index_internal(&ispec, Some(coid), primary, !spec.deferrable)?;

        let con = Constraint {
            oid: coid,
            name,
            namespace: rel.namespace,
            kind: if primary { ConstraintKind::PrimaryKey } else { ConstraintKind::Unique },
            table,
            domain: 0,
            columns: attnums,
            index,
            deferrable: spec.deferrable,
            initially_deferred: spec.initially_deferred,
            validated: true,
            enforced: true,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: None,
            check: None,
            parent: 0,
        };
        self.put(CatalogObject::Constraint(con))?;
        self.con_record_index_created(index);
        self.record_created(ObjectAddress::constraint(coid));
        if !creating {
            self.follow_up(FollowUp::BuildIndex { index, table });
        }
        created.push(coid);
        Ok(created)
    }

    /// CHECK constraint.
    fn con_add_check(
        &mut self,
        table: Oid,
        spec: &ConstraintSpec,
        expr: &StoredExpr,
        columns: &[String],
        no_inherit: bool,
        creating: bool,
    ) -> Result<Vec<Oid>> {
        let rel = self.con_rel(table)?;
        let mut attnums: Vec<i16> = Vec::with_capacity(columns.len());
        for name in columns {
            let col = rel.column_by_name(name).ok_or_else(|| errors::column_does_not_exist(name))?;
            if !attnums.contains(&col.attnum) {
                attnums.push(col.attnum);
            }
        }
        let name = match &spec.name {
            Some(n) => {
                if let Some(existing) = self.con_find(table, n) {
                    return Err(if creating && existing.kind == ConstraintKind::Check {
                        errors::check_constraint_already_exists(n)
                    } else {
                        errors::constraint_already_exists(n, &rel.name)
                    });
                }
                n.clone()
            }
            None => {
                let snap = self.snapshot()?;
                naming::choose_constraint_name(
                    &snap,
                    rel.namespace,
                    &rel.name,
                    columns.first().map(|s| s.as_str()),
                    "check",
                    &[],
                )?
            }
        };
        let validated = spec.enforced && (creating || !spec.not_valid);
        let oid = self.new_oid()?;
        let con = Constraint {
            oid,
            name,
            namespace: rel.namespace,
            kind: ConstraintKind::Check,
            table,
            domain: 0,
            columns: attnums,
            index: 0,
            deferrable: false,
            initially_deferred: false,
            validated,
            enforced: spec.enforced,
            is_local: true,
            inh_count: 0,
            no_inherit,
            period: false,
            fk: None,
            check: Some(Self::bind_self_deps(expr, table)),
            parent: 0,
        };
        self.put(CatalogObject::Constraint(con))?;
        self.record_created(ObjectAddress::constraint(oid));
        if !creating && validated {
            self.follow_up(FollowUp::ValidateCheck { table, constraint: oid });
        }
        Ok(vec![oid])
    }

    /// Resolve FK column names against `rel`.
    fn con_fk_attnums(rel: &Relation, names: &[String]) -> Result<Vec<i16>> {
        let mut out: Vec<i16> = Vec::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            let col = rel.column_by_name(n).ok_or_else(|| fk_column_missing(n))?;
            if i >= INDEX_MAX_KEYS {
                return Err(too_many_fk_keys());
            }
            out.push(col.attnum);
        }
        Ok(out)
    }

    /// REFERENCES privilege (table-level, or on every referenced column) for the current user.
    /// Role membership is not followed (documented v1 simplification).
    fn con_may_reference(&mut self, pkrel: &Relation, pk_attnums: &[i16]) -> Result<bool> {
        let user = self.current_user();
        if pkrel.owner == user || self.is_superuser(user)? {
            return Ok(true);
        }
        if acl_grants(&pkrel.acl, user, acl_mode::REFERENCES) {
            return Ok(true);
        }
        Ok(!pk_attnums.is_empty()
            && pk_attnums.iter().all(|a| pkrel.column(*a).is_some_and(|c| acl_grants(&c.acl, user, acl_mode::REFERENCES))))
    }

    /// FOREIGN KEY constraint (PG `ATAddForeignKeyConstraint`).
    fn con_add_fk(&mut self, table: Oid, spec: &ConstraintSpec, p: &FkParams<'_>, creating: bool) -> Result<Vec<Oid>> {
        if p.match_type == FkMatch::Partial {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "MATCH PARTIAL not yet implemented"));
        }
        let rel = self.con_rel(table)?;
        let ref_oid = if p.ref_table == SELF_REL { table } else { p.ref_table };
        let pkrel = self.con_rel(ref_oid)?;
        if !matches!(pkrel.kind, RelKind::Table | RelKind::PartitionedTable) {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("referenced relation \"{}\" is not a table", pkrel.name),
            ));
        }
        if pkrel.is_virtual() {
            return Err(errors::system_catalog_permission(&pkrel.name));
        }
        match rel.persistence {
            Persistence::Permanent if pkrel.persistence != Persistence::Permanent => {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "constraints on permanent tables may reference only permanent tables",
                ));
            }
            Persistence::Unlogged if pkrel.persistence == Persistence::Temp => {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "constraints on unlogged tables may reference only permanent or unlogged tables",
                ));
            }
            Persistence::Temp if pkrel.persistence != Persistence::Temp => {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "constraints on temporary tables may reference only temporary tables",
                ));
            }
            _ => {}
        }

        // Referencing columns, ON DELETE SET column list.
        let fk_attnums = Self::con_fk_attnums(&rel, p.columns)?;
        let mut set_cols: Vec<i16> = Vec::new();
        if !p.delete_set_columns.is_empty() {
            let raw = Self::con_fk_attnums(&rel, p.delete_set_columns)?;
            for (i, a) in raw.iter().enumerate() {
                if !fk_attnums.contains(a) {
                    return Err(PgError::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!(
                            "column \"{}\" referenced in ON DELETE SET action must be part of foreign key",
                            p.delete_set_columns[i]
                        ),
                    ));
                }
                if !set_cols.contains(a) {
                    set_cols.push(*a);
                }
            }
        }

        // Referenced columns and the unique index that backs them.
        let (pk_attnums, ref_index): (Vec<i16>, Oid) = if p.ref_columns.is_empty() {
            let pk = self
                .data
                .constraints
                .values()
                .find(|c| c.table == ref_oid && c.kind == ConstraintKind::PrimaryKey)
                .ok_or_else(|| errors::no_primary_key_for_referenced_table(&pkrel.name))?;
            let (cols, idx) = (pk.columns.clone(), pk.index);
            if self.data.indexes.get(&idx).is_some_and(|i| !i.immediate) {
                return Err(PgError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot use a deferrable primary key for referenced table \"{}\"", pkrel.name),
                ));
            }
            (cols, idx)
        } else {
            let attnums = Self::con_fk_attnums(&pkrel, p.ref_columns)?;
            for (i, a) in attnums.iter().enumerate() {
                if attnums[..i].contains(a) {
                    return Err(errors::fk_duplicate_referenced_columns());
                }
            }
            let Some(idx) = self.find_referenced_index(ref_oid, &attnums)? else {
                if find_unique_index(&self.data, ref_oid, &attnums, false).is_some() {
                    return Err(PgError::new(
                        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("cannot use a deferrable unique constraint for referenced table \"{}\"", pkrel.name),
                    ));
                }
                return Err(errors::no_unique_constraint_matching(&pkrel.name));
            };
            (attnums, idx)
        };

        if !self.con_may_reference(&pkrel, &pk_attnums)? {
            return Err(errors::permission_denied_for("table", &pkrel.name));
        }

        // Generated columns (PG tablecmds.c checks).
        for a in &fk_attnums {
            let Some(generated) = rel.column(*a).and_then(|c| c.generated) else { continue };
            if matches!(p.on_update, FkAction::SetNull | FkAction::SetDefault | FkAction::Cascade) {
                return Err(PgError::new(
                    SqlState::SYNTAX_ERROR,
                    "invalid ON UPDATE action for foreign key constraint containing generated column",
                ));
            }
            if matches!(p.on_delete, FkAction::SetNull | FkAction::SetDefault) {
                return Err(PgError::new(
                    SqlState::SYNTAX_ERROR,
                    "invalid ON DELETE action for foreign key constraint containing generated column",
                ));
            }
            if generated == GeneratedKind::Virtual {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "foreign key constraints on virtual generated columns are not supported",
                ));
            }
        }

        if fk_attnums.len() != pk_attnums.len() {
            return Err(errors::fk_column_count_mismatch());
        }

        // Constraint name.
        let name = match &spec.name {
            Some(n) => {
                if self.con_find(table, n).is_some() {
                    return Err(errors::constraint_already_exists(n, &rel.name));
                }
                n.clone()
            }
            None => {
                let snap = self.snapshot()?;
                let addition = naming::column_list_addition(p.columns)?;
                naming::choose_constraint_name(&snap, rel.namespace, &rel.name, Some(&addition), "fkey", &[])?
            }
        };

        // Type compatibility and equality operators, pair by pair.
        let env = self.env()?;
        let cands = ops::builtin_operator_candidates("=").unwrap_or_default();
        let ref_idx = self.data.indexes.get(&ref_index).cloned();
        let mut pf_ops: Vec<Oid> = Vec::with_capacity(fk_attnums.len());
        let mut pp_ops: Vec<Oid> = Vec::with_capacity(fk_attnums.len());
        let mut ff_ops: Vec<Oid> = Vec::with_capacity(fk_attnums.len());
        let mut ops_complete = true;
        for (i, (fa, pa)) in fk_attnums.iter().zip(pk_attnums.iter()).enumerate() {
            let (Some(fkc), Some(pkc)) = (rel.column(*fa), pkrel.column(*pa)) else {
                return Err(PgError::internal("foreign key column vanished during constraint creation"));
            };
            let fk_base = registry::base_type(&env, fkc.type_id.oid);
            let pk_base = registry::base_type(&env, pkc.type_id.oid);
            let raw_opc = ref_idx
                .as_ref()
                .and_then(|ix| ix.key_columns().iter().find(|k| k.column == Some(*pa)).and_then(|k| opclass_intype(k.opclass)))
                .unwrap_or(pk_base);
            let opc = if is_pseudo_opcintype(raw_opc) { pk_base } else { raw_opc };
            let pp = eq_operator(&env, &cands, opc, opc);
            let mut pf = eq_operator(&env, &cands, opc, fk_base);
            let mut ff = if pf.is_some() { eq_operator(&env, &cands, fk_base, fk_base) } else { None };
            if pf.is_none() || ff.is_none() {
                if implicit_ok(&env, pkc.type_id.oid, opc) && implicit_ok(&env, fkc.type_id.oid, opc) {
                    pf = pp;
                    ff = pp;
                } else {
                    pf = None;
                    ff = None;
                }
            }
            let (Some(pf), Some(ff)) = (pf, ff) else {
                let fk_type = names::format_type(&env, fkc.type_id.oid, -1)?;
                let pk_type = names::format_type(&env, pkc.type_id.oid, -1)?;
                let fk_name = p.columns.get(i).map(|s| s.as_str()).unwrap_or(fkc.name.as_str());
                let pk_name = if p.ref_columns.is_empty() { pkc.name.as_str() } else { p.ref_columns[i].as_str() };
                return Err(errors::fk_cannot_be_implemented(&name, fk_name, pk_name, &fk_type, &pk_type));
            };
            pf_ops.push(pf);
            ff_ops.push(ff);
            match pp {
                Some(o) => pp_ops.push(o),
                None => ops_complete = false,
            }
        }
        if !ops_complete {
            pf_ops.clear();
            pp_ops.clear();
            ff_ops.clear();
        }

        let validated = spec.enforced && (creating || !spec.not_valid);
        let oid = self.new_oid()?;
        let con = Constraint {
            oid,
            name,
            namespace: rel.namespace,
            kind: ConstraintKind::ForeignKey,
            table,
            domain: 0,
            columns: fk_attnums,
            index: ref_index,
            deferrable: spec.deferrable,
            initially_deferred: spec.initially_deferred,
            validated,
            enforced: spec.enforced,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: Some(ForeignKey {
                ref_table: ref_oid,
                ref_columns: pk_attnums,
                ref_index,
                on_update: p.on_update,
                on_delete: p.on_delete,
                match_type: p.match_type,
                delete_set_columns: set_cols,
                pf_eq_ops: pf_ops,
                pp_eq_ops: pp_ops,
                ff_eq_ops: ff_ops,
            }),
            check: None,
            parent: 0,
        };
        self.put(CatalogObject::Constraint(con))?;
        self.record_created(ObjectAddress::constraint(oid));
        if !creating && validated {
            self.follow_up(FollowUp::ValidateForeignKey { table, constraint: oid });
        }
        Ok(vec![oid])
    }

    // ------------------------------------------------------------------ public operations

    /// Create all constraints of a NEW (empty) table in PG's order: CHECKs, NOT NULLs (explicit,
    /// serial/identity, then PRIMARY KEY columns lacking one), PRIMARY KEY/UNIQUE, FOREIGN KEYs.
    /// `NotNull` specs and `ColumnSpec::not_null` are already folded by `create_table` into `specs`
    /// (it appends a `ConstraintSpecKind::NotNull` per not-null/identity/serial column). Returns
    /// the created constraint OIDs in creation order.
    // CONTRACT
    pub fn add_constraints_to_new_table(&mut self, table: Oid, specs: &[ConstraintSpec]) -> Result<Vec<Oid>> {
        let rel = self.con_rel(table)?;
        let mut checks: Vec<&ConstraintSpec> = Vec::new();
        let mut not_nulls: Vec<(i16, &ConstraintSpec)> = Vec::new();
        let mut primaries: Vec<&ConstraintSpec> = Vec::new();
        let mut uniques: Vec<&ConstraintSpec> = Vec::new();
        let mut fks: Vec<&ConstraintSpec> = Vec::new();
        for s in specs {
            match &s.kind {
                ConstraintSpecKind::Exclusion => {
                    return Err(PgError::unimplemented("CAT-12", "EXCLUDE constraints"));
                }
                ConstraintSpecKind::Check { .. } => checks.push(s),
                ConstraintSpecKind::NotNull { column } => {
                    let attnum = rel
                        .column_by_name(column)
                        .map(|c| c.attnum)
                        .ok_or_else(|| errors::column_of_relation_does_not_exist(column, &rel.name))?;
                    not_nulls.push((attnum, s));
                }
                ConstraintSpecKind::PrimaryKey { .. } => primaries.push(s),
                ConstraintSpecKind::Unique { .. } => uniques.push(s),
                ConstraintSpecKind::ForeignKey { .. } => fks.push(s),
            }
        }
        not_nulls.sort_by_key(|(a, _)| *a);

        let mut out: Vec<Oid> = Vec::new();
        for s in checks {
            out.extend(self.con_add(table, s, true)?);
        }
        for (_, s) in not_nulls {
            out.extend(self.con_add(table, s, true)?);
        }
        for s in primaries.into_iter().chain(uniques) {
            out.extend(self.con_add(table, s, true)?);
        }
        for s in fks {
            out.extend(self.con_add(table, s, true)?);
        }
        Ok(out)
    }

    /// ALTER TABLE `table` ADD CONSTRAINT (module docs). Requires table ownership and a
    /// table-like relation (`Table`); queues follow-ups. Returns the created constraint OIDs
    /// (PK: the NOT NULL constraints it created, then the PK constraint).
    // CONTRACT
    pub fn add_constraint(&mut self, table: Oid, spec: &ConstraintSpec) -> Result<Vec<Oid>> {
        self.con_check_target(table, "ADD CONSTRAINT")?;
        self.con_add(table, spec, false)
    }

    /// Add (or find) the NOT NULL constraint of column `attnum` and set `Column::not_null`.
    /// Returns `(constraint_oid, created)`: when a NOT NULL constraint already exists it is reused
    /// (`created = false`, no follow-up). When created and `validate`, queues
    /// `FollowUp::ValidateNotNull`. `name = None` -> `t_a_not_null`.
    // CONTRACT
    pub fn ensure_not_null(&mut self, table: Oid, attnum: i16, name: Option<&str>, validate: bool) -> Result<(Oid, bool)> {
        let rel = self.con_rel(table)?;
        let col = match rel.column(attnum) {
            Some(c) if !c.dropped => c.clone(),
            _ => {
                if attnum < 1 {
                    return Err(errors::cannot_alter_system_column(system_column_name(attnum)));
                }
                return Err(PgError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column number {attnum} of relation \"{}\" does not exist", rel.name),
                ));
            }
        };

        // Reuse an existing NOT NULL constraint of the column.
        let existing = self
            .data
            .constraints
            .values()
            .find(|c| c.table == table && c.kind == ConstraintKind::NotNull && c.columns.len() == 1 && c.columns[0] == attnum)
            .map(|c| c.oid);
        if let Some(oid) = existing {
            if !col.not_null {
                self.update_relation(table, |r| {
                    if let Some(c) = r.columns.iter_mut().find(|c| c.attnum == attnum) {
                        c.not_null = true;
                    }
                    Ok(())
                })?;
            }
            return Ok((oid, false));
        }

        let cname = match name {
            Some(n) => {
                if self.con_find(table, n).is_some() {
                    return Err(errors::constraint_already_exists(n, &rel.name));
                }
                n.to_string()
            }
            None => {
                let snap = self.snapshot()?;
                naming::choose_constraint_name(&snap, rel.namespace, &rel.name, Some(&col.name), "not_null", &[])?
            }
        };
        let oid = self.new_oid()?;
        let con = Constraint {
            oid,
            name: cname,
            namespace: rel.namespace,
            kind: ConstraintKind::NotNull,
            table,
            domain: 0,
            columns: vec![attnum],
            index: 0,
            deferrable: false,
            initially_deferred: false,
            validated: true,
            enforced: true,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: None,
            check: None,
            parent: 0,
        };
        self.put(CatalogObject::Constraint(con))?;
        self.update_relation(table, |r| {
            if let Some(c) = r.columns.iter_mut().find(|c| c.attnum == attnum) {
                c.not_null = true;
            }
            Ok(())
        })?;
        self.record_created(ObjectAddress::constraint(oid));
        if validate {
            self.follow_up(FollowUp::ValidateNotNull { table, attnum });
        }
        Ok((oid, true))
    }

    /// ALTER TABLE DROP CONSTRAINT `name`: owner check; missing -> 42704 `constraint "c" of
    /// relation "t" does not exist` or (`missing_ok`) NOTICE `constraint "c" of relation "t" does
    /// not exist, skipping` and `Ok(false)`. Drops through `drop_objects([constraint], behavior)`:
    /// the backing index goes with PK/UNIQUE (INTERNAL dep); an FK from another table that depends
    /// on this PK/UNIQUE index blocks under RESTRICT (oracle: 2BP01 `cannot drop constraint t_pkey
    /// on table t because other objects depend on it` DETAIL `constraint t2_x_fkey on table t2
    /// depends on index t_pkey` HINT `Use DROP ... CASCADE to drop the dependent objects too.`).
    /// Dropping a PRIMARY KEY leaves its NOT NULL constraints (oracle: `t_id_not_null` remains and
    /// `attnotnull` stays true). Dropping a NOT NULL constraint clears `Column::not_null` (same as
    /// `DROP NOT NULL`, which refuses identity columns and primary-key columns:
    /// `errors::column_is_identity`, `errors::column_in_primary_key`).
    // CONTRACT
    pub fn drop_constraint(&mut self, table: Oid, name: &str, behavior: DropBehavior, missing_ok: bool) -> Result<bool> {
        let rel = self.con_check_target(table, "DROP CONSTRAINT")?;
        let Some(con) = self.con_find(table, name) else {
            if missing_ok {
                self.notice(errors::constraint_does_not_exist_skipping(name, &rel.name));
                return Ok(false);
            }
            return Err(errors::constraint_does_not_exist(name, &rel.name));
        };

        if con.kind == ConstraintKind::NotNull {
            if let Some(&attnum) = con.columns.first() {
                let col_name = rel.column(attnum).map(|c| c.name.clone()).unwrap_or_default();
                let in_pk = self
                    .data
                    .constraints
                    .values()
                    .any(|c| c.table == table && c.kind == ConstraintKind::PrimaryKey && c.columns.contains(&attnum));
                if in_pk {
                    return Err(errors::column_in_primary_key(&col_name));
                }
                if rel.column(attnum).is_some_and(|c| c.identity.is_some()) {
                    return Err(PgError::new(
                        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("column \"{col_name}\" of relation \"{}\" is an identity column", rel.name),
                    ));
                }
            }
        }

        self.drop_objects(&[ObjectAddress::constraint(con.oid)], behavior)?;
        Ok(true)
    }

    /// ALTER TABLE RENAME CONSTRAINT old TO new: owner; new name free on the table (42710 as
    /// above); for PK/UNIQUE also renames the backing index to the new name (PG
    /// `RenameConstraintById` + `RenameRelationInternal`; index name clash -> 42P07). Missing old
    /// name -> 42704 `constraint "x" for table "t" does not exist` (this PG message differs from
    /// DROP: `constraint \"%s\" for table \"%s\" does not exist`).
    // CONTRACT
    pub fn rename_constraint(&mut self, table: Oid, old_name: &str, new_name: &str) -> Result<()> {
        let rel = self.con_check_target(table, "RENAME CONSTRAINT")?;
        let Some(con) = self.con_find(table, old_name) else {
            return Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{old_name}\" for table \"{}\" does not exist", rel.name),
            ));
        };
        let backed_by_index = matches!(
            con.kind,
            ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::Exclusion
        ) && con.index != 0
            && self.data.indexes.contains_key(&con.index);
        if backed_by_index {
            let snap = self.snapshot()?;
            if snap.class_in_namespace(rel.namespace, new_name).is_some() {
                return Err(errors::relation_already_exists(new_name));
            }
        }
        if self.con_find(table, new_name).is_some() {
            return Err(errors::constraint_already_exists(new_name, &rel.name));
        }
        if backed_by_index {
            self.update_index(con.index, |i| {
                i.name = new_name.to_string();
                Ok(())
            })?;
        }
        self.update_constraint(con.oid, |c| {
            c.name = new_name.to_string();
            Ok(())
        })
    }

    /// ALTER TABLE VALIDATE CONSTRAINT: only CHECK and FOREIGN KEY (`errors::constraint_not_fk_or_check`,
    /// 42809); already valid -> no-op. Sets `validated = true` and queues `ValidateCheck` /
    /// `ValidateForeignKey` (the executor raises 23514/23503 if rows violate; the statement then
    /// rolls back, restoring `validated = false`). Missing -> 42704 `constraint "c" of relation
    /// "t" does not exist`.
    // CONTRACT
    pub fn validate_constraint(&mut self, table: Oid, name: &str) -> Result<()> {
        let rel = self.con_check_target(table, "VALIDATE CONSTRAINT")?;
        let Some(con) = self.con_find(table, name) else {
            return Err(errors::constraint_does_not_exist(name, &rel.name));
        };
        if !matches!(con.kind, ConstraintKind::Check | ConstraintKind::ForeignKey) {
            return Err(errors::constraint_not_fk_or_check(name, &rel.name));
        }
        if !con.enforced {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot validate NOT ENFORCED constraint",
            ));
        }
        if con.validated {
            return Ok(());
        }
        self.update_constraint(con.oid, |c| {
            c.validated = true;
            Ok(())
        })?;
        if con.kind == ConstraintKind::ForeignKey {
            self.follow_up(FollowUp::ValidateForeignKey { table, constraint: con.oid });
        } else {
            self.follow_up(FollowUp::ValidateCheck { table, constraint: con.oid });
        }
        Ok(())
    }

    /// ALTER TABLE ALTER CONSTRAINT c [NOT] DEFERRABLE / INITIALLY ... / [NOT] ENFORCED: FK only for
    /// deferrability (`errors::constraint_not_foreign_key`, 42809); `initially_deferred` without
    /// `deferrable` -> 42601? (binder). Updates the flags.
    // CONTRACT
    pub fn alter_constraint(
        &mut self,
        table: Oid,
        name: &str,
        deferrable: Option<bool>,
        initially_deferred: Option<bool>,
        enforced: Option<bool>,
    ) -> Result<()> {
        let rel = self.con_check_target(table, "ALTER CONSTRAINT")?;
        let Some(con) = self.con_find(table, name) else {
            return Err(errors::constraint_does_not_exist(name, &rel.name));
        };
        let alter_deferrability = deferrable.is_some() || initially_deferred.is_some();
        if alter_deferrability && con.kind != ConstraintKind::ForeignKey {
            return Err(errors::constraint_not_foreign_key(name, &rel.name));
        }
        if enforced.is_some() && con.kind != ConstraintKind::ForeignKey {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot alter enforceability of constraint \"{name}\" of relation \"{}\"", rel.name),
            ));
        }
        if con.parent != 0 {
            return Err(PgError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot alter constraint \"{name}\" on relation \"{}\"", rel.name),
            )
            .with_hint("You may alter the constraint it derives from instead."));
        }

        // New deferrability: INITIALLY DEFERRED implies DEFERRABLE; NOT DEFERRABLE implies IMMEDIATE.
        let mut new_deferrable = con.deferrable;
        let mut new_deferred = con.initially_deferred;
        if alter_deferrability {
            new_deferrable = deferrable.unwrap_or(initially_deferred == Some(true) || con.deferrable);
            new_deferred = initially_deferred.unwrap_or(false);
            if new_deferred && !new_deferrable {
                return Err(PgError::syntax("constraint declared INITIALLY DEFERRED must be DEFERRABLE", None));
            }
        }
        let mut new_enforced = con.enforced;
        let mut new_validated = con.validated;
        let mut queue_validation = false;
        if let Some(e) = enforced {
            if e != con.enforced {
                // PG: NOT ENFORCED constraints are always marked invalid; enabling validates at once.
                new_enforced = e;
                new_validated = e;
                queue_validation = e;
            }
        }

        let changed = new_deferrable != con.deferrable
            || new_deferred != con.initially_deferred
            || new_enforced != con.enforced
            || new_validated != con.validated;
        if !changed {
            return Ok(());
        }
        self.update_constraint(con.oid, |c| {
            c.deferrable = new_deferrable;
            c.initially_deferred = new_deferred;
            c.enforced = new_enforced;
            c.validated = new_validated;
            Ok(())
        })?;
        if queue_validation {
            self.follow_up(FollowUp::ValidateForeignKey { table, constraint: con.oid });
        }
        Ok(())
    }

    /// Find the UNIQUE/PRIMARY index of `ref_table` whose key column set equals `ref_attnums` (any
    /// order; non-partial, no expressions, no INCLUDE columns counted, valid). Prefers the primary
    /// key, then the lowest OID. Returns `(index_oid, ref_attnums reordered?)` - PG keeps the
    /// order given by the FK, so only the index OID is returned. None if no match.
    // CONTRACT
    pub fn find_referenced_index(&mut self, ref_table: Oid, ref_attnums: &[i16]) -> Result<Option<Oid>> {
        Ok(find_unique_index(&self.data, ref_table, ref_attnums, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use pgarbor_types::TypeId;

    const PUBLIC: Oid = crate::builtin::PUBLIC_NAMESPACE;

    fn table_spec(name: &str, cols: &[(&str, TypeId)]) -> CreateTableSpec {
        CreateTableSpec {
            namespace: PUBLIC,
            name: name.to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: cols.iter().map(|(n, t)| ColumnSpec::new(*n, *t)).collect(),
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    fn rel_oid(env: &crate::env::CatalogEnv, name: &str) -> Oid {
        env.snapshot().class_in_namespace(PUBLIC, name).map(|c| c.oid()).unwrap_or(0)
    }

    fn pk(cols: &[&str]) -> ConstraintSpec {
        ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
            columns: cols.iter().map(|s| s.to_string()).collect(),
            include: Vec::new(),
        })
    }

    fn unique(cols: &[&str]) -> ConstraintSpec {
        ConstraintSpec::new(ConstraintSpecKind::Unique {
            columns: cols.iter().map(|s| s.to_string()).collect(),
            include: Vec::new(),
            nulls_not_distinct: false,
        })
    }

    fn fk(cols: &[&str], ref_table: Oid, ref_cols: &[&str]) -> ConstraintSpec {
        ConstraintSpec::new(ConstraintSpecKind::ForeignKey {
            columns: cols.iter().map(|s| s.to_string()).collect(),
            ref_table,
            ref_columns: ref_cols.iter().map(|s| s.to_string()).collect(),
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            match_type: FkMatch::Simple,
            delete_set_columns: Vec::new(),
        })
    }

    /// Bootstrapped catalog with `t(id int, a int, x text)`.
    fn setup() -> Result<(pgarbor_common::MemKv, crate::env::CatalogEnv)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = table_spec("t", &[("id", TypeId::INT4), ("a", TypeId::INT4), ("x", TypeId::TEXT)]);
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        Ok((kv, env))
    }

    #[test]
    fn pk_creates_not_null_index_and_follow_ups() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let mut created = Vec::new();
        let (res, env) = testing::run(&mut kv, &env, |tx| {
            created = tx.add_constraint(t, &pk(&["id"]))?;
            Ok(())
        })?;
        assert_eq!(created.len(), 2);
        let snap = env.snapshot();
        let names: Vec<&str> = snap.constraints_of(t).iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"t_id_not_null"));
        assert!(names.contains(&"t_pkey"));
        assert_eq!(snap.relation(t).and_then(|r| r.column_by_name("id")).map(|c| c.not_null), Some(true));
        let pkc = snap.primary_key(t).expect("pk");
        let idx = snap.index(pkc.index).expect("index");
        assert!(idx.primary && idx.unique);
        assert_eq!(idx.name, "t_pkey");
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::ValidateNotNull { attnum: 1, .. })));
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::BuildIndex { .. })));
        Ok(())
    }

    #[test]
    fn second_primary_key_rejected() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &pk(&["id"])).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &pk(&["a"])).map(|_| ())).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::INVALID_TABLE_DEFINITION);
        assert_eq!(err.message, "multiple primary keys for table \"t\" are not allowed");
        Ok(())
    }

    #[test]
    fn fk_requires_matching_unique_key() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let spec2 = table_spec("t2", &[("x", TypeId::INT4)]);
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.create_table(&spec2).map(|_| ()))?;
        let t2 = rel_oid(&env, "t2");
        let err = testing::run(&mut kv, &env, |tx| tx.add_constraint(t2, &fk(&["x"], t, &["a"])).map(|_| ())).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::INVALID_FOREIGN_KEY);
        assert_eq!(err.message, "there is no unique constraint matching given keys for referenced table \"t\"");
        // Without columns and without a primary key.
        let err = testing::run(&mut kv, &env, |tx| tx.add_constraint(t2, &fk(&["x"], t, &[])).map(|_| ())).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::UNDEFINED_OBJECT);
        assert_eq!(err.message, "there is no primary key for referenced table \"t\"");
        Ok(())
    }

    #[test]
    fn fk_incompatible_types_and_success() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &pk(&["id"])).map(|_| ()))?;
        let spec2 = table_spec("f", &[("x", TypeId::TEXT), ("y", TypeId::INT8)]);
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.create_table(&spec2).map(|_| ()))?;
        let f = rel_oid(&env, "f");
        let err = testing::run(&mut kv, &env, |tx| tx.add_constraint(f, &fk(&["x"], t, &["id"])).map(|_| ())).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::DATATYPE_MISMATCH);
        assert_eq!(err.message, "foreign key constraint \"f_x_fkey\" cannot be implemented");
        assert_eq!(
            err.detail.as_deref(),
            Some("Key columns \"x\" of the referencing table and \"id\" of the referenced table are of incompatible types: text and integer.")
        );
        // bigint -> integer is fine (cross-type operator), referencing the primary key implicitly.
        let mut made = Vec::new();
        let (res, env) = testing::run(&mut kv, &env, |tx| {
            made = tx.add_constraint(f, &fk(&["y"], t, &[]))?;
            Ok(())
        })?;
        assert_eq!(made.len(), 1);
        let c = env.snapshot().constraint(made[0]).expect("fk").clone();
        assert_eq!(c.name, "f_y_fkey");
        assert_eq!(c.fk.as_ref().map(|k| k.ref_columns.clone()), Some(vec![1]));
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::ValidateForeignKey { .. })));
        Ok(())
    }

    #[test]
    fn drop_pk_with_inbound_fk_restricted_and_not_null_stays() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &pk(&["id"])).map(|_| ()))?;
        let spec2 = table_spec("t2", &[("x", TypeId::INT4)]);
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.create_table(&spec2).map(|_| ()))?;
        let t2 = rel_oid(&env, "t2");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t2, &fk(&["x"], t, &["id"])).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |tx| {
            tx.drop_constraint(t, "t_pkey", DropBehavior::Restrict, false).map(|_| ())
        })
        .err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(err.message, "cannot drop constraint t_pkey on table t because other objects depend on it");
        assert_eq!(err.detail.as_deref(), Some("constraint t2_x_fkey on table t2 depends on index t_pkey"));
        // CASCADE drops the FK too; the NOT NULL constraint and attnotnull remain.
        let (_, env) = testing::run(&mut kv, &env, |tx| {
            tx.drop_constraint(t, "t_pkey", DropBehavior::Cascade, false).map(|_| ())
        })?;
        let snap = env.snapshot();
        assert!(snap.constraints_of(t).iter().any(|c| c.name == "t_id_not_null"));
        assert!(snap.primary_key(t).is_none());
        assert_eq!(snap.relation(t).and_then(|r| r.column_by_name("id")).map(|c| c.not_null), Some(true));
        Ok(())
    }

    #[test]
    fn drop_missing_constraint() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let err = testing::run(&mut kv, &env, |tx| tx.drop_constraint(t, "nope", DropBehavior::Restrict, false).map(|_| ()))
            .err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::UNDEFINED_OBJECT);
        assert_eq!(err.message, "constraint \"nope\" of relation \"t\" does not exist");
        let mut dropped = true;
        let (res, _) = testing::run(&mut kv, &env, |tx| {
            dropped = tx.drop_constraint(t, "nope", DropBehavior::Restrict, true)?;
            Ok(())
        })?;
        assert!(!dropped);
        assert_eq!(res.outcome.notices.len(), 1);
        Ok(())
    }

    #[test]
    fn rename_pk_renames_index() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &pk(&["id"])).map(|_| ()))?;
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.rename_constraint(t, "t_pkey", "t_pk"))?;
        let snap = env.snapshot();
        let c = snap.primary_key(t).expect("pk");
        assert_eq!(c.name, "t_pk");
        assert_eq!(snap.index(c.index).map(|i| i.name.as_str()), Some("t_pk"));
        let err = testing::run(&mut kv, &env, |tx| tx.rename_constraint(t, "nope", "z")).err();
        let err = err.expect("must fail");
        assert_eq!(err.message, "constraint \"nope\" for table \"t\" does not exist");
        Ok(())
    }

    #[test]
    fn validate_unique_is_rejected_and_check_validates() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &unique(&["a"])).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |tx| tx.validate_constraint(t, "t_a_key")).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::WRONG_OBJECT_TYPE);
        assert_eq!(err.message, "constraint \"t_a_key\" of relation \"t\" is not a foreign key or check constraint");

        let mut chk = ConstraintSpec::new(ConstraintSpecKind::Check {
            expr: StoredExpr::new("(a > 0)"),
            columns: vec!["a".to_string()],
            no_inherit: false,
        });
        chk.not_valid = true;
        let (res, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &chk).map(|_| ()))?;
        assert!(res.outcome.follow_ups.is_empty());
        let c = env.snapshot().constraints_of(t).into_iter().find(|c| c.name == "t_a_check").cloned().expect("check");
        assert!(!c.validated);
        let (res, env) = testing::run(&mut kv, &env, |tx| tx.validate_constraint(t, "t_a_check"))?;
        assert!(res.outcome.follow_ups.iter().any(|f| matches!(f, FollowUp::ValidateCheck { .. })));
        let c = env.snapshot().constraint(c.oid).cloned().expect("check");
        assert!(c.validated);
        Ok(())
    }

    #[test]
    fn not_null_reuse_and_drop() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let mut first = (0, false);
        let mut second = (0, false);
        let (_, env) = testing::run(&mut kv, &env, |tx| {
            first = tx.ensure_not_null(t, 2, None, true)?;
            second = tx.ensure_not_null(t, 2, None, true)?;
            Ok(())
        })?;
        assert!(first.1 && !second.1);
        assert_eq!(first.0, second.0);
        assert_eq!(env.snapshot().constraint(first.0).map(|c| c.name.as_str()), Some("t_a_not_null"));
        let (_, env) = testing::run(&mut kv, &env, |tx| {
            tx.drop_constraint(t, "t_a_not_null", DropBehavior::Restrict, false).map(|_| ())
        })?;
        assert_eq!(env.snapshot().relation(t).and_then(|r| r.column_by_name("a")).map(|c| c.not_null), Some(false));
        Ok(())
    }

    #[test]
    fn alter_constraint_requires_fk() -> Result<()> {
        let (mut kv, env) = setup()?;
        let t = rel_oid(&env, "t");
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.add_constraint(t, &unique(&["a"])).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |tx| tx.alter_constraint(t, "t_a_key", Some(true), None, None)).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::WRONG_OBJECT_TYPE);
        assert_eq!(err.message, "constraint \"t_a_key\" of relation \"t\" is not a foreign key constraint");
        Ok(())
    }

    #[test]
    fn unique_index_search_is_set_based() {
        let mut data = CatalogData::default();
        let key = |col: i16| IndexKey {
            column: Some(col),
            expr: None,
            name: format!("c{col}"),
            type_id: TypeId::INT4,
            collation: 0,
            opclass: 1978,
            descending: false,
            nulls_first: false,
        };
        let mk = |oid: Oid, cols: &[i16], primary: bool, unique: bool| Index {
            oid,
            name: format!("i{oid}"),
            namespace: PUBLIC,
            table: 20000,
            owner: 10,
            method: IndexMethod::Btree,
            keys: cols.iter().map(|c| key(*c)).collect(),
            n_key_columns: cols.len() as u16,
            unique,
            nulls_not_distinct: false,
            primary,
            exclusion: false,
            immediate: true,
            clustered: false,
            valid: true,
            replident: false,
            predicate: None,
            constraint: None,
            options: Vec::new(),
            persistence: Persistence::Permanent,
        };
        data.indexes.insert(20010, mk(20010, &[1, 2], false, true));
        data.indexes.insert(20011, mk(20011, &[2, 1], true, true));
        data.indexes.insert(20012, mk(20012, &[1], false, false));
        assert_eq!(find_unique_index(&data, 20000, &[2, 1], true), Some(20011));
        assert_eq!(find_unique_index(&data, 20000, &[1], true), None);
        assert_eq!(find_unique_index(&data, 20000, &[1, 1], true), None);
        assert_eq!(find_unique_index(&data, 20001, &[1, 2], true), None);
    }

    #[test]
    fn opclass_input_types() {
        assert_eq!(opclass_intype(1978), Some(23));
        assert_eq!(opclass_intype(3126), Some(25));
        assert_eq!(opclass_intype(1), None);
        assert!(is_pseudo_opcintype(oids::ANYARRAY));
        assert!(!is_pseudo_opcintype(23));
    }

    #[test]
    fn not_valid_ignored_in_create_table_and_not_enforced_rejected_on_unique() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut spec = table_spec("c", &[("a", TypeId::INT4)]);
        let mut chk = ConstraintSpec::new(ConstraintSpecKind::Check {
            expr: StoredExpr::new("(a > 0)"),
            columns: vec!["a".to_string()],
            no_inherit: false,
        });
        chk.not_valid = true;
        spec.constraints.push(chk);
        let (_, env) = testing::run(&mut kv, &env, |tx| tx.create_table(&spec).map(|_| ()))?;
        let c = rel_oid(&env, "c");
        let con = env.snapshot().constraints_of(c).into_iter().find(|k| k.kind == ConstraintKind::Check).cloned();
        assert!(con.is_some_and(|k| k.validated));
        let mut bad = unique(&["a"]);
        bad.enforced = false;
        let err = testing::run(&mut kv, &env, |tx| tx.add_constraint(c, &bad).map(|_| ())).err();
        let err = err.expect("must fail");
        assert_eq!(err.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(err.message, "UNIQUE constraints cannot be marked NOT ENFORCED");
        Ok(())
    }
}
