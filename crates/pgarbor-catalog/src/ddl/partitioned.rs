//! Partitioned tables: partition key validation and storage, partition bound checks
//! (`tablecmds.c` `ComputePartitionAttrs`, `partbounds.c` `check_new_partition_bound`,
//! `check_default_partition_contents`). Amendment pgarbor-catalog-09.
use super::spec::*;
use crate::errors;
use crate::model::*;
use crate::partition::{self, PartitionBound, ResolvedBound};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::names::format_type;
use pgarbor_types::TypeEnv;

/// Default operator class of `ty` for access method `am` (403 btree, 405 hash), following domains
/// and the string / enum / array pseudo families.
pub(crate) fn has_default_opclass(env: &dyn TypeEnv, ty: Oid, am: Oid) -> bool {
    let am_s = am.to_string();
    let lookup = |t: Oid| {
        let key = t.to_string();
        crate::builtin::OPCLASSES.rows.iter().any(|r| {
            r.get(1).copied().flatten() == Some(am_s.as_str())
                && r.get(6).copied().flatten() == Some(key.as_str())
                && r.get(7).copied().flatten() == Some("t")
        })
    };
    if lookup(ty) {
        return true;
    }
    let Some(info) = env.type_info(ty) else { return false };
    if info.basetype != 0 && info.basetype != ty {
        return has_default_opclass(env, info.basetype, am);
    }
    match info.category {
        b'S' => lookup(25),
        b'E' => lookup(3500),
        b'A' => lookup(2277),
        b'R' => lookup(3831),
        b'C' => lookup(2249),
        _ => false,
    }
}

impl<'a> CatalogTxn<'a> {
    /// Temporary relations may be created in the session's temp schema only.
    pub(crate) fn temp_namespace_allowed(&self, ns: Oid) -> bool {
        self.data().namespaces.get(&ns).is_some_and(|n| n.name.starts_with("pg_temp_"))
    }

    /// `PARTITION BY` checks over the final column list (PG `ComputePartitionAttrs`).
    pub(crate) fn check_partition_key_spec(&mut self, spec: &CreateTableSpec) -> Result<()> {
        let Some(key) = &spec.extra.partition_key else { return Ok(()) };
        let env = self.env()?;
        if key.strategy == b'l' && key.items.len() != 1 {
            return Err(PgError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "cannot use \"list\" partition strategy with more than one column",
            ));
        }
        for item in &key.items {
            let ty = match &item.column {
                Some(name) => {
                    if super::common::SYSTEM_COLUMNS.iter().any(|(n, _)| n == name) {
                        return Err(PgError::new(
                            SqlState::INVALID_OBJECT_DEFINITION,
                            format!("cannot use system column \"{name}\" in partition key"),
                        ));
                    }
                    let Some(col) = spec.columns.iter().find(|c| &c.name == name) else {
                        return Err(PgError::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{name}\" named in partition key does not exist"),
                        ));
                    };
                    if col.generated.is_some() {
                        return Err(PgError::new(SqlState::INVALID_OBJECT_DEFINITION, "cannot use generated column in partition key")
                            .with_detail(format!("Column \"{name}\" is a generated column.")));
                    }
                    col.serial.map(|s| pgarbor_types::TypeId::of(s.type_oid())).unwrap_or(col.type_id)
                }
                None => {
                    if let Some(e) = &item.expr {
                        for d in &e.depends_on {
                            if d.class == ObjClass::Relation && d.oid == SELF_REL && d.sub_id > 0 {
                                if let Some(c) = spec.columns.get((d.sub_id - 1) as usize) {
                                    if c.generated.is_some() {
                                        return Err(PgError::new(
                                            SqlState::INVALID_OBJECT_DEFINITION,
                                            "cannot use generated column in partition key",
                                        )
                                        .with_detail(format!("Column \"{}\" is a generated column.", c.name)));
                                    }
                                }
                            }
                        }
                    }
                    item.expr_type.unwrap_or(pgarbor_types::TypeId::TEXT)
                }
            };
            let (am, am_name) = if key.strategy == b'h' { (405, "hash") } else { (403, "btree") };
            if !has_default_opclass(&env, ty.oid, am) {
                let t = format_type(&env, ty.oid, -1)?;
                return Err(PgError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("data type {t} has no default operator class for access method \"{am_name}\""),
                )
                .with_hint(format!(
                    "You must specify a {am_name} operator class or define a default {am_name} operator class for the data type."
                )));
            }
        }
        Ok(())
    }

    /// Store the partition key and validate a new partition's bound against its siblings.
    pub(crate) fn finish_partitioning(&mut self, table: Oid, spec: &CreateTableSpec) -> Result<()> {
        if let Some(key) = &spec.extra.partition_key {
            let rel = self.data().relations.get(&table).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
            let mut columns = Vec::with_capacity(key.items.len());
            let mut exprs = Vec::new();
            let mut expr_types = Vec::new();
            for item in &key.items {
                match (&item.column, &item.expr) {
                    (Some(name), _) => {
                        let col = rel.column_by_name(name).ok_or_else(|| errors::column_does_not_exist(name))?;
                        columns.push(col.attnum);
                    }
                    (None, Some(e)) => {
                        columns.push(0);
                        exprs.push(Self::bind_self_deps(e, table));
                        expr_types.push(item.expr_type.unwrap_or(pgarbor_types::TypeId::TEXT));
                    }
                    (None, None) => return Err(PgError::internal("empty partition key item")),
                }
            }
            let pk = PartitionKey { strategy: key.strategy, columns, exprs, expr_types };
            self.update_relation(table, |r| {
                r.partition_key = Some(pk);
                Ok(())
            })?;
        }
        if let Some(p) = &spec.extra.partition_of {
            self.check_partition_bound(table, &spec.name, p.parent, &p.bound, &p.positions)?;
        }
        Ok(())
    }

    /// Validate `bound` of new partition `name` (OID `table`) under `parent`: the parent must be
    /// partitioned, the bound must not overlap a sibling, and an existing DEFAULT partition is
    /// re-validated (follow-up).
    pub(crate) fn check_partition_bound(
        &mut self,
        table: Oid,
        name: &str,
        parent: Oid,
        bound: &PartitionBound,
        positions: &partition::BoundPositions,
    ) -> Result<()> {
        let env = self.env()?;
        let prel = self.data().relations.get(&parent).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", parent))?;
        if prel.kind != RelKind::PartitionedTable || prel.partition_key.is_none() {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not partitioned", prel.name)));
        }
        let types = partition::key_types(&prel);
        let new_bound = bound.resolve(&env, &types)?;
        let snap = self.snapshot()?;
        let mut existing: Vec<(String, ResolvedBound)> = Vec::new();
        let mut default_part: Option<Oid> = None;
        for (oid, b) in partition::partitions_of(&snap, parent)? {
            if oid == table {
                continue;
            }
            let n = snap.relation(oid).map(|r| r.name.clone()).unwrap_or_default();
            if b.is_default() {
                default_part = Some(oid);
            }
            existing.push((n, b.resolve(&env, &types)?));
        }
        partition::check_new_partition_bound(&env, &types, name, &new_bound, &existing, positions)?;
        if let (Some(def), false) = (default_part, bound.is_default()) {
            self.follow_up(FollowUp::ValidatePartitionRows { table: def, default_update: true });
        }
        Ok(())
    }
}
