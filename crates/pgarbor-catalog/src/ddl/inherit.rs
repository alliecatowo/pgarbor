//! Table inheritance and partition-of column merging for CREATE TABLE (PG `tablecmds.c`
//! `MergeAttributes`, `MergeChildAttribute`, `MergeInheritedAttribute`, `MergeCheckConstraint`,
//! `heap.c` `AddRelationNotNullConstraints`). Amendment pgarbor-catalog-09.
//!
//! ## Positions of the binder
//! The binder binds the child's own expressions (defaults, CHECKs) against a column scope of
//! `local columns ++ inherited-only columns` (parents' columns, merged by name in parent order,
//! that the child does not redeclare). `ObjectAddress::column(SELF_REL, pos)` dependencies use
//! those 1-based positions; [`Merged::position_map`] translates them to final attnums.
//! For a partition the scope is the parent's visible columns in order (positions are final).
use super::spec::*;
use crate::errors;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::names::format_type;

/// An inherited CHECK constraint (expression already mapped to the child's attnums, `SELF_REL`).
#[derive(Clone, Debug)]
pub(crate) struct InhCheck {
    pub name: String,
    pub expr: StoredExpr,
    /// Final attnums referenced.
    pub columns: Vec<i16>,
    pub inh_count: i16,
    pub enforced: bool,
}

/// Result of [`CatalogTxn::merge_attributes`].
#[derive(Clone, Debug)]
pub(crate) struct Merged {
    /// Final column list (attnum = index + 1). Inherited not-null is NOT folded into `not_null`
    /// (see `not_nulls`); local `NOT NULL` keeps its spec value.
    pub columns: Vec<ColumnSpec>,
    pub is_local: Vec<bool>,
    pub inh_count: Vec<i16>,
    /// Binder position (index) -> final attnum.
    pub position_map: Vec<i16>,
    pub checks: Vec<InhCheck>,
    /// Inherited not-null constraints: (final attnum, parent constraint name, count of parents).
    pub not_nulls: Vec<(i16, String, i16)>,
    pub parents: Vec<Oid>,
}

/// Rewrite column dependencies of `expr` on relation `from` through `map` and retarget them to `to`.
pub(crate) fn remap_expr(expr: &StoredExpr, from: Oid, to: Oid, map: &dyn Fn(i16) -> Option<i16>) -> StoredExpr {
    let mut e = expr.clone();
    for d in e.depends_on.iter_mut() {
        if d.class == ObjClass::Relation && d.oid == from {
            d.oid = to;
            if d.sub_id > 0 {
                if let Some(n) = map(d.sub_id as i16) {
                    d.sub_id = n as i32;
                }
            }
        }
    }
    e.bound = None;
    e
}

fn type_conflict(env: &dyn pgarbor_types::TypeEnv, msg: String, a: pgarbor_types::TypeId, b: pgarbor_types::TypeId) -> PgError {
    let fa = format_type(env, a.oid, a.typmod).unwrap_or_else(|_| a.oid.to_string());
    let fb = format_type(env, b.oid, b.typmod).unwrap_or_else(|_| b.oid.to_string());
    PgError::new(SqlState::DATATYPE_MISMATCH, msg).with_detail(format!("{fa} versus {fb}"))
}

impl<'a> CatalogTxn<'a> {
    /// The parents of a CREATE TABLE: `INHERITS` list or the `PARTITION OF` parent.
    pub(crate) fn create_parents(spec: &CreateTableSpec) -> Vec<Oid> {
        match &spec.extra.partition_of {
            Some(p) => vec![p.parent],
            None => spec.inherits.clone(),
        }
    }

    /// PG `MergeAttributes` (module docs).
    pub(crate) fn merge_attributes(&mut self, spec: &CreateTableSpec) -> Result<Merged> {
        let env = self.env()?;
        let is_partition = spec.extra.partition_of.is_some();
        let parents = Self::create_parents(spec);
        let mut seen_parents: Vec<Oid> = Vec::new();
        let mut inh: Vec<ColumnSpec> = Vec::new();
        let mut inh_count: Vec<i16> = Vec::new();
        // Default per inherited column: None, Some(Some(expr)) or Some(None) for "conflicting".
        let mut defaults: Vec<Option<Option<StoredExpr>>> = Vec::new();
        let mut checks: Vec<InhCheck> = Vec::new();
        let mut not_nulls: Vec<(i16, String)> = Vec::new();
        let mut nn_flags: Vec<bool> = Vec::new();
        for parent in &parents {
            let rel = self
                .data()
                .relations
                .get(parent)
                .cloned()
                .ok_or_else(|| errors::cache_lookup_failed("relation", *parent))?;
            if seen_parents.contains(parent) {
                return Err(PgError::new(
                    SqlState::DUPLICATE_TABLE,
                    format!("relation \"{}\" would be inherited from more than once", rel.name),
                ));
            }
            seen_parents.push(*parent);
            if !is_partition {
                if rel.kind == RelKind::PartitionedTable {
                    return Err(PgError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("cannot inherit from partitioned table \"{}\"", rel.name),
                    ));
                }
                if rel.partition_of.is_some() {
                    return Err(PgError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("cannot inherit from partition \"{}\"", rel.name),
                    ));
                }
            }
            if !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable | RelKind::ForeignTable) {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("inherited relation \"{}\" is not a table or foreign table", rel.name),
                ));
            }
            if is_partition && rel.persistence != Persistence::Temp && spec.persistence == Persistence::Temp {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot create a temporary relation as partition of permanent relation \"{}\"", rel.name),
                ));
            }
            if spec.persistence != Persistence::Temp && rel.persistence == Persistence::Temp {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    if is_partition {
                        format!("cannot create a permanent relation as partition of temporary relation \"{}\"", rel.name)
                    } else {
                        format!("cannot inherit from temporary relation \"{}\"", rel.name)
                    },
                ));
            }
            self.require_owner(&ObjectAddress::relation(*parent))?;
            let parent_nn: Vec<(i16, String)> = self
                .data()
                .constraints
                .values()
                .filter(|c| c.table == *parent && c.kind == ConstraintKind::NotNull && !c.no_inherit)
                .filter_map(|c| c.columns.first().map(|a| (*a, c.name.clone())))
                .collect();
            let mut attmap: Vec<i16> = vec![0; rel.columns.len()];
            let mut pending_defaults: Vec<(usize, StoredExpr)> = Vec::new();
            for col in &rel.columns {
                if col.dropped {
                    continue;
                }
                let mut newdef = ColumnSpec::new(col.name.clone(), col.type_id);
                newdef.collation = Some(col.collation);
                newdef.ndims = col.ndims;
                newdef.storage = col.storage;
                newdef.compression = col.compression;
                let idx = match inh.iter().position(|c| c.name == col.name) {
                    Some(i) => {
                        self.notice(PgError::notice(format!(
                            "merging multiple inherited definitions of column \"{}\"",
                            col.name
                        )));
                        let prev = &inh[i];
                        if prev.type_id != col.type_id {
                            return Err(type_conflict(
                                &env,
                                format!("inherited column \"{}\" has a type conflict", col.name),
                                prev.type_id,
                                col.type_id,
                            ));
                        }
                        if prev.collation != Some(col.collation) {
                            return Err(PgError::new(
                                SqlState(*b"42P21"),
                                format!("inherited column \"{}\" has a collation conflict", col.name),
                            ));
                        }
                        let prev_gen = prev.generated.as_ref().map(|g| g.kind);
                        if prev_gen != col.generated {
                            return Err(PgError::new(
                                SqlState::DATATYPE_MISMATCH,
                                format!("inherited column \"{}\" has a generation conflict", col.name),
                            ));
                        }
                        inh_count[i] += 1;
                        i
                    }
                    None => {
                        if let Some(kind) = col.generated {
                            if let Some(d) = &col.default {
                                newdef.generated = Some(GeneratedSpec { kind, expr: d.expr.clone() });
                            }
                        }
                        inh.push(newdef);
                        inh_count.push(1);
                        defaults.push(None);
                        nn_flags.push(false);
                        inh.len() - 1
                    }
                };
                attmap[(col.attnum - 1) as usize] = (idx + 1) as i16;
                if parent_nn.iter().any(|(a, _)| *a == col.attnum) {
                    nn_flags[idx] = true;
                }
                if col.generated.is_none() {
                    if let Some(d) = &col.default {
                        pending_defaults.push((idx, d.expr.clone()));
                    }
                }
            }
            let map = |a: i16| -> Option<i16> {
                attmap.get((a - 1) as usize).copied().filter(|n| *n > 0)
            };
            for (idx, expr) in pending_defaults {
                let expr = remap_expr(&expr, *parent, SELF_REL, &map);
                defaults[idx] = match defaults[idx].take() {
                    None => Some(Some(expr)),
                    Some(Some(prev)) if prev.sql == expr.sql => Some(Some(prev)),
                    Some(_) => Some(None),
                };
            }
            // Generated expressions of new inherited columns need their dependencies mapped too.
            for c in inh.iter_mut() {
                if let Some(g) = c.generated.as_mut() {
                    if g.expr.depends_on.iter().any(|d| d.class == ObjClass::Relation && d.oid == *parent) {
                        g.expr = remap_expr(&g.expr, *parent, SELF_REL, &map);
                    }
                }
            }
            let parent_checks: Vec<Constraint> = {
                let mut v: Vec<Constraint> = self
                    .data()
                    .constraints
                    .values()
                    .filter(|c| c.table == *parent && c.kind == ConstraintKind::Check && !c.no_inherit)
                    .cloned()
                    .collect();
                v.sort_by_key(|c| c.oid);
                v
            };
            for c in parent_checks {
                let Some(expr) = &c.check else { continue };
                let expr = remap_expr(expr, *parent, SELF_REL, &map);
                let columns: Vec<i16> = c.columns.iter().filter_map(|a| map(*a)).collect();
                match checks.iter_mut().find(|k| k.name == c.name) {
                    Some(k) => {
                        if k.expr.sql != expr.sql {
                            return Err(PgError::new(
                                SqlState::DUPLICATE_OBJECT,
                                format!(
                                    "check constraint name \"{}\" appears multiple times but with different expressions",
                                    c.name
                                ),
                            ));
                        }
                        k.inh_count += 1;
                        if c.enforced {
                            k.enforced = true;
                        }
                    }
                    None => checks.push(InhCheck { name: c.name.clone(), expr, columns, inh_count: 1, enforced: c.enforced }),
                }
            }
            for (a, name) in parent_nn {
                if let Some(n) = map(a) {
                    not_nulls.push((n, name));
                }
            }
        }

        // Merge the child's own column definitions.
        let mut columns: Vec<ColumnSpec> = Vec::new();
        let mut is_local: Vec<bool> = Vec::new();
        let mut counts: Vec<i16> = Vec::new();
        let n_inh = inh.len();
        for (i, c) in inh.iter().enumerate() {
            let mut c = c.clone();
            if let Some(Some(d)) = &defaults[i] {
                if c.generated.is_none() {
                    c.default = Some(d.clone());
                }
            }
            columns.push(c);
            is_local.push(false);
            counts.push(inh_count[i]);
        }
        let mut bogus: Vec<bool> = defaults.iter().map(|d| matches!(d, Some(None))).collect();
        // Expressions that come from the child's own definitions (binder positions).
        let mut local_exprs: Vec<bool> = vec![false; columns.len()];
        let mut local_final: Vec<i16> = Vec::with_capacity(spec.columns.len());
        for (pos, local) in spec.columns.iter().enumerate() {
            match columns.iter().position(|c| c.name == local.name) {
                Some(i) if i < n_inh => {
                    if !is_partition {
                        if i == pos {
                            self.notice(PgError::notice(format!(
                                "merging column \"{}\" with inherited definition",
                                local.name
                            )));
                        } else {
                            self.notice(
                                PgError::notice(format!(
                                    "moving and merging column \"{}\" with inherited definition",
                                    local.name
                                ))
                                .with_detail("User-specified column moved to the position of the inherited column."),
                            );
                        }
                    }
                    let inh_ty = columns[i].type_id;
                    if !is_partition && inh_ty != local.type_id {
                        return Err(type_conflict(
                            &env,
                            format!("column \"{}\" has a type conflict", local.name),
                            inh_ty,
                            local.type_id,
                        ));
                    }
                    if let (Some(lc), Some(ic)) = (local.collation, columns[i].collation) {
                        if lc != ic {
                            return Err(PgError::new(
                                SqlState(*b"42P21"),
                                format!("column \"{}\" has a collation conflict", local.name),
                            ));
                        }
                    }
                    let target = &mut columns[i];
                    if target.generated.is_some() {
                        if local.default.is_some() && local.generated.is_none() {
                            return Err(PgError::new(
                                SqlState::INVALID_COLUMN_DEFINITION,
                                format!("column \"{}\" inherits from generated column but specifies default", local.name),
                            ));
                        }
                        if local.identity.is_some() {
                            return Err(PgError::new(
                                SqlState::INVALID_COLUMN_DEFINITION,
                                format!("column \"{}\" inherits from generated column but specifies identity", local.name),
                            ));
                        }
                    } else if local.generated.is_some() {
                        return Err(PgError::new(
                            SqlState::INVALID_COLUMN_DEFINITION,
                            format!("child column \"{}\" specifies generation expression", local.name),
                        )
                        .with_hint("A child table column cannot be generated unless its parent column is."));
                    }
                    if let (Some(ig), Some(lg)) = (&target.generated, &local.generated) {
                        if ig.kind != lg.kind {
                            let word = |k: GeneratedKind| if k == GeneratedKind::Stored { "STORED" } else { "VIRTUAL" };
                            return Err(PgError::new(
                                SqlState::INVALID_COLUMN_DEFINITION,
                                format!("column \"{}\" inherits from generated column of different kind", local.name),
                            )
                            .with_detail(format!(
                                "Parent column is {}, child column is {}.",
                                word(ig.kind),
                                word(lg.kind)
                            )));
                        }
                    }
                    if local.not_null.is_some() {
                        target.not_null = local.not_null.clone();
                    }
                    target.identity = local.identity.clone();
                    target.serial = local.serial;
                    if let Some(g) = &local.generated {
                        target.generated = Some(g.clone());
                        local_exprs[i] = true;
                    }
                    if local.default.is_some() {
                        target.default = local.default.clone();
                        bogus[i] = false;
                        local_exprs[i] = true;
                    }
                    if local.storage.is_some() {
                        target.storage = local.storage;
                    }
                    if local.compression.is_some() {
                        target.compression = local.compression;
                    }
                    if !is_partition {
                        is_local[i] = true;
                    }
                    local_final.push((i + 1) as i16);
                }
                Some(_) => {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column \"{}\" specified more than once", local.name),
                    ))
                }
                None => {
                    if is_partition {
                        return Err(errors::column_does_not_exist(&local.name));
                    }
                    columns.push(local.clone());
                    local_exprs.push(true);
                    is_local.push(true);
                    counts.push(0);
                    local_final.push(columns.len() as i16);
                }
            }
        }
        if columns.len() > 1600 {
            return Err(PgError::new(SqlState(*b"54011"), "tables can have at most 1600 columns"));
        }
        for (i, b) in bogus.iter().enumerate() {
            if *b {
                let c = &columns[i];
                return Err(if c.generated.is_some() {
                    PgError::new(
                        SqlState::INVALID_COLUMN_DEFINITION,
                        format!("column \"{}\" inherits conflicting generation expressions", c.name),
                    )
                    .with_hint("To resolve the conflict, specify a generation expression explicitly.")
                } else {
                    PgError::new(
                        SqlState::INVALID_COLUMN_DEFINITION,
                        format!("column \"{}\" inherits conflicting default values", c.name),
                    )
                    .with_hint("To resolve the conflict, specify a default explicitly.")
                });
            }
        }
        // Binder positions: local columns, then inherited-only columns (partitions: parent order).
        let position_map: Vec<i16> = if is_partition {
            (1..=columns.len() as i16).collect()
        } else {
            let mut m = local_final.clone();
            for (i, c) in inh.iter().enumerate() {
                if !spec.columns.iter().any(|l| l.name == c.name) {
                    m.push((i + 1) as i16);
                }
            }
            m
        };
        let pmap = |p: i16| -> Option<i16> { position_map.get((p - 1) as usize).copied() };
        for (i, c) in columns.iter_mut().enumerate() {
            if !local_exprs[i] {
                continue;
            }
            if let Some(d) = c.default.as_mut() {
                *d = remap_expr(d, SELF_REL, SELF_REL, &pmap);
            }
            if let Some(g) = c.generated.as_mut() {
                g.expr = remap_expr(&g.expr, SELF_REL, SELF_REL, &pmap);
            }
        }
        // Group inherited not-nulls per column, keeping the first parent's constraint name.
        let mut nn: Vec<(i16, String, i16)> = Vec::new();
        for (a, name) in not_nulls {
            match nn.iter_mut().find(|(x, _, _)| *x == a) {
                Some(e) => e.2 += 1,
                None => nn.push((a, name, 1)),
            }
        }
        let _ = nn_flags;
        Ok(Merged { columns, is_local, inh_count: counts, position_map, checks, not_nulls: nn, parents })
    }

    /// After the child relation exists: column inheritance flags, parent links, inherited CHECK and
    /// NOT NULL constraints (module docs).
    pub(crate) fn finish_inheritance(&mut self, table: Oid, merged: &Merged, spec: &CreateTableSpec) -> Result<()> {
        let parents = merged.parents.clone();
        let partition_of = spec.extra.partition_of.clone();
        let key_text = match &partition_of {
            Some(p) => {
                let parent = self
                    .data()
                    .relations
                    .get(&p.parent)
                    .cloned()
                    .ok_or_else(|| errors::cache_lookup_failed("relation", p.parent))?;
                Some((p.parent, p.bound.to_sql(&crate::partition::key_types(&parent))))
            }
            None => None,
        };
        self.update_relation(table, |r| {
            for (i, c) in r.columns.iter_mut().enumerate() {
                if let (Some(l), Some(n)) = (merged.is_local.get(i), merged.inh_count.get(i)) {
                    c.is_local = *l;
                    c.inh_count = *n;
                }
            }
            r.inherits = parents.clone();
            r.partition_of = key_text.clone();
            Ok(())
        })?;
        let rel = self
            .data()
            .relations
            .get(&table)
            .cloned()
            .ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
        // Inherited CHECK constraints (merge with same-named local ones).
        for k in &merged.checks {
            let existing = self
                .data()
                .constraints
                .values()
                .find(|c| c.table == table && c.name == k.name)
                .cloned();
            let expr = Self::bind_self_deps(&k.expr, table);
            match existing {
                Some(c) => {
                    let same = c.kind == ConstraintKind::Check && c.check.as_ref().map(|e| e.sql.as_str()) == Some(expr.sql.as_str());
                    if !same {
                        return Err(errors::constraint_already_exists(&k.name, &rel.name));
                    }
                    if c.no_inherit {
                        return Err(PgError::new(
                            SqlState::INVALID_OBJECT_DEFINITION,
                            format!(
                                "constraint \"{}\" conflicts with inherited constraint on relation \"{}\"",
                                k.name, rel.name
                            ),
                        ));
                    }
                    self.notice(PgError::notice(format!("merging constraint \"{}\" with inherited definition", k.name)));
                    let n = k.inh_count;
                    self.update_constraint(c.oid, |c| {
                        c.inh_count = n;
                        Ok(())
                    })?;
                }
                None => {
                    let oid = self.new_oid()?;
                    let con = Constraint {
                        oid,
                        name: k.name.clone(),
                        namespace: rel.namespace,
                        kind: ConstraintKind::Check,
                        table,
                        domain: 0,
                        columns: k.columns.clone(),
                        index: 0,
                        deferrable: false,
                        initially_deferred: false,
                        validated: k.enforced,
                        enforced: k.enforced,
                        is_local: false,
                        inh_count: k.inh_count,
                        no_inherit: false,
                        period: false,
                        fk: None,
                        check: Some(expr),
                        parent: 0,
                    };
                    self.put(CatalogObject::Constraint(con))?;
                    self.record_created(ObjectAddress::constraint(oid));
                }
            }
        }
        // Inherited NOT NULL constraints.
        for (attnum, name, count) in &merged.not_nulls {
            let existing = self
                .data()
                .constraints
                .values()
                .find(|c| c.table == table && c.kind == ConstraintKind::NotNull && c.columns.first() == Some(attnum))
                .cloned();
            match existing {
                Some(c) => {
                    if c.no_inherit {
                        let col = rel.column(*attnum).map(|c| c.name.clone()).unwrap_or_default();
                        return Err(PgError::new(
                            SqlState::DATATYPE_MISMATCH,
                            format!("cannot define not-null constraint with NO INHERIT on column \"{col}\""),
                        )
                        .with_detail("The column has an inherited not-null constraint."));
                    }
                    let n = *count;
                    self.update_constraint(c.oid, |c| {
                        c.inh_count = n;
                        Ok(())
                    })?;
                }
                None => {
                    let taken = self.data().constraints.values().any(|c| c.table == table && &c.name == name);
                    let cname = if taken {
                        let snap = self.snapshot()?;
                        let col = rel.column(*attnum).map(|c| c.name.clone()).unwrap_or_default();
                        crate::naming::choose_constraint_name(&snap, rel.namespace, &rel.name, Some(&col), "not_null", &[])?
                    } else {
                        name.clone()
                    };
                    let oid = self.new_oid()?;
                    let con = Constraint {
                        oid,
                        name: cname,
                        namespace: rel.namespace,
                        kind: ConstraintKind::NotNull,
                        table,
                        domain: 0,
                        columns: vec![*attnum],
                        index: 0,
                        deferrable: false,
                        initially_deferred: false,
                        validated: true,
                        enforced: true,
                        is_local: false,
                        inh_count: *count,
                        no_inherit: false,
                        period: false,
                        fk: None,
                        check: None,
                        parent: 0,
                    };
                    self.put(CatalogObject::Constraint(con))?;
                    let a = *attnum;
                    self.update_relation(table, |r| {
                        if let Some(c) = r.columns.iter_mut().find(|c| c.attnum == a) {
                            c.not_null = true;
                        }
                        Ok(())
                    })?;
                    self.record_created(ObjectAddress::constraint(oid));
                }
            }
        }
        Ok(())
    }

    /// Direct inheritance children of `parent` (`pg_inherits.inhrelid`), ordered by OID.
    pub fn inheritance_children(&self, parent: Oid) -> Vec<Oid> {
        let mut v: Vec<Oid> =
            self.data().relations.values().filter(|r| r.inherits.contains(&parent)).map(|r| r.oid).collect();
        v.sort_unstable();
        v
    }

    /// Every descendant of `parent` (excluding itself), breadth-first, each once.
    pub fn inheritance_descendants(&self, parent: Oid) -> Vec<Oid> {
        let mut out: Vec<Oid> = Vec::new();
        let mut queue = vec![parent];
        while let Some(p) = queue.pop() {
            for c in self.inheritance_children(p) {
                if c != parent && !out.contains(&c) {
                    out.push(c);
                    queue.push(c);
                }
            }
        }
        out
    }
}
