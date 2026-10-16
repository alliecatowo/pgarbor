//! Tuple routing and partition constraints (amendment pgarbor-exec-11). PG references:
//! `executor/execPartition.c` (`ExecFindPartition`, `get_partition_for_tuple`,
//! `ExecBuildSlotPartitionKeyDescription`), `partitioning/partbounds.c`
//! (`compute_partition_hash_value`), `executor/execMain.c` (`ExecPartitionCheck`,
//! `ExecPartitionCheckEmitError`).
//!
//! * [`route`]: a row of a partitioned table (in that table's column layout) descends through the
//!   partition tree to a leaf: at each level the key is computed, the partition whose bound accepts
//!   it is chosen (else the DEFAULT partition, else 23514 `no partition of relation "p" found for
//!   row` DETAIL `Partition key of the failing row contains (a) = (1).`), and the row is converted
//!   to the child's layout by column name.
//! * [`check_partition_constraint`]: a row of a partition (in its own layout) satisfies its bound and
//!   every ancestor's bound (DEFAULT: no sibling accepts it). Direct INSERT/UPDATE/COPY into a
//!   partition and ATTACH/default-partition validation use it.
//! Partition descriptions are cached per statement in `StmtState::partitions`.
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_catalog::model::{RelKind, Relation};
use pgarbor_catalog::partition::{self, PartitionBound, ResolvedBound};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::{func, Datum, TypeId};

use crate::context::ExecContext;
use crate::error::{relation_vanished, row_description};
use crate::eval;
use crate::eval_call::with_func_ctx;
use crate::runtime::Executor;
use crate::tabledesc::{bind_stored_text, columns_to_input_refs, table_desc, TableDesc};

/// One partitioned table's routing description.
#[derive(Debug)]
pub struct PartInfo {
    pub oid: Oid,
    pub name: String,
    pub strategy: u8,
    pub key_types: Vec<TypeId>,
    pub key_names: Vec<String>,
    /// Non-default partitions with their bounds, in descriptor order.
    pub children: Vec<(Oid, ResolvedBound)>,
    pub default: Option<Oid>,
    /// Extended hash support procs per key item (hash strategy).
    pub hash_procs: Vec<Option<Oid>>,
}

/// Per-statement caches (`StmtState::partitions`).
#[derive(Debug, Default)]
pub struct PartCache {
    infos: HashMap<Oid, Rc<PartInfo>>,
    /// (partitioned table, row relation) -> key item expressions over the row relation's full row
    /// (`None` for plain column keys, which are mapped by slot).
    keys: HashMap<(Oid, Oid), Rc<Vec<KeyItem>>>,
}

/// How to compute one key item from a row of some relation.
#[derive(Debug, Clone)]
pub enum KeyItem {
    Slot(usize),
    Missing,
    Expr(BoundExpr),
}

fn relation<'s>(cx: &'s ExecContext, oid: Oid) -> Result<&'s Relation> {
    cx.se.env.snapshot().relation(oid).ok_or_else(|| relation_vanished(oid))
}

/// Routing description of partitioned table `oid` (cached).
pub fn part_info(cx: &ExecContext, oid: Oid) -> Result<Rc<PartInfo>> {
    if let Some(hit) = cx.stmt.partitions.borrow().infos.get(&oid) {
        return Ok(Rc::clone(hit));
    }
    let snap = cx.se.env.snapshot().clone();
    let rel = snap.relation(oid).ok_or_else(|| relation_vanished(oid))?;
    let key = rel
        .partition_key
        .as_ref()
        .ok_or_else(|| PgError::internal(format!("relation \"{}\" has no partition key", rel.name)))?;
    let key_types = partition::key_types(rel);
    let key_names = partition::key_names(rel);
    let env = &cx.se.env;
    let mut children = Vec::new();
    let mut default = None;
    let ordered = snap.ordered_children(oid);
    for c in ordered {
        let Some(crel) = snap.relation(c) else { continue };
        let Some((p, text)) = &crel.partition_of else { continue };
        if *p != oid {
            continue;
        }
        let bound = PartitionBound::parse(text)?;
        if bound.is_default() {
            default = Some(c);
            continue;
        }
        children.push((c, bound.resolve(env, &key_types)?));
    }
    let hash_procs = if key.strategy == b'h' {
        key_types.iter().map(|t| partition::hash_extended_proc(env, t.oid)).collect()
    } else {
        Vec::new()
    };
    let info = Rc::new(PartInfo {
        oid,
        name: rel.name.clone(),
        strategy: key.strategy,
        key_types,
        key_names,
        children,
        default,
        hash_procs,
    });
    cx.stmt.partitions.borrow_mut().infos.insert(oid, Rc::clone(&info));
    Ok(info)
}

/// Key items of partitioned table `keyrel` computed over rows of `row_rel` (cached).
fn key_items(cx: &ExecContext, keyrel: Oid, row_rel: Oid) -> Result<Rc<Vec<KeyItem>>> {
    if let Some(hit) = cx.stmt.partitions.borrow().keys.get(&(keyrel, row_rel)) {
        return Ok(Rc::clone(hit));
    }
    let krel = relation(cx, keyrel)?.clone();
    let rrel = relation(cx, row_rel)?.clone();
    let key = krel
        .partition_key
        .as_ref()
        .ok_or_else(|| PgError::internal(format!("relation \"{}\" has no partition key", krel.name)))?;
    let mut exprs = key.exprs.iter();
    let mut items = Vec::with_capacity(key.columns.len());
    for a in &key.columns {
        if *a > 0 {
            let name = krel.column(*a).map(|c| c.name.clone()).unwrap_or_default();
            items.push(match rrel.column_by_name(&name) {
                Some(c) => KeyItem::Slot((c.attnum - 1) as usize),
                None => KeyItem::Missing,
            });
        } else {
            let e = exprs.next().ok_or_else(|| PgError::internal("partition key expression missing"))?;
            let bound = columns_to_input_refs(bind_stored_text(&cx.se, &e.sql, row_rel)?)?;
            items.push(KeyItem::Expr(bound));
        }
    }
    let items = Rc::new(items);
    cx.stmt.partitions.borrow_mut().keys.insert((keyrel, row_rel), Rc::clone(&items));
    Ok(items)
}

/// Key values of partitioned table `keyrel` for `row` (a full row of relation `row_rel`).
pub fn key_values(ex: &mut Executor, keyrel: Oid, row_rel: Oid, row: &[Datum]) -> Result<Vec<Datum>> {
    let items = key_items(&*ex.cx, keyrel, row_rel)?;
    let mut out = Vec::with_capacity(items.len());
    for it in items.iter() {
        out.push(match it {
            KeyItem::Slot(s) => row.get(*s).cloned().unwrap_or(Datum::Null),
            KeyItem::Missing => Datum::Null,
            KeyItem::Expr(e) => eval::eval(ex, e, row)?,
        });
    }
    Ok(out)
}

/// PG `compute_partition_hash_value`.
fn row_hash(ex: &mut Executor, info: &PartInfo, key: &[Datum]) -> Result<u64> {
    let mut h: u64 = 0;
    for (i, v) in key.iter().enumerate() {
        if v.is_null() {
            continue;
        }
        let proc = info
            .hash_procs
            .get(i)
            .copied()
            .flatten()
            .ok_or_else(|| PgError::internal("no extended hash function for partition key"))?;
        let ty = info.key_types.get(i).copied().unwrap_or(TypeId::TEXT);
        let handle = func::implementation(proc)?;
        let args = [v.clone(), Datum::Int8(partition::HASH_PARTITION_SEED as i64)];
        let r = with_func_ctx(&*ex.cx, &[ty, TypeId::INT8], TypeId::INT8, |fcx| handle.call(&args, fcx))?;
        let hv = match r {
            Datum::Int8(x) => x as u64,
            _ => return Err(PgError::internal("extended hash function did not return bigint")),
        };
        h = partition::hash_combine64(h, hv);
    }
    Ok(h)
}

/// The partition of `info` accepting `key`, else the default partition.
pub fn find_partition(ex: &mut Executor, info: &PartInfo, key: &[Datum]) -> Result<Option<Oid>> {
    let mut hash: Option<u64> = None;
    for (oid, bound) in &info.children {
        if let ResolvedBound::Hash { .. } = bound {
            if hash.is_none() {
                hash = Some(row_hash(ex, info, key)?);
            }
        }
        let h = hash.unwrap_or(0);
        let env = ex.cx.se.env.clone();
        if partition::bound_accepts(&env, &info.key_types, bound, key, &mut || Ok(h))? {
            return Ok(Some(*oid));
        }
    }
    Ok(info.default)
}

/// `(a, b) = (1, null)` description of a key.
fn key_description(ex: &Executor, info: &PartInfo, key: &[Datum]) -> Result<String> {
    let io = ex.cx.se.io_ctx();
    let mut vals = Vec::with_capacity(key.len());
    for (i, v) in key.iter().enumerate() {
        vals.push(if v.is_null() {
            "null".to_string()
        } else {
            pgarbor_types::io::output(&io, info.key_types.get(i).copied().unwrap_or(TypeId::TEXT), v)?
        });
    }
    Ok(format!("({}) = ({})", info.key_names.join(", "), vals.join(", ")))
}

/// Convert a row of `from` to the layout of `to` (columns matched by name; others NULL).
pub fn convert_row(from: &TableDesc, to: &TableDesc, row: &[Datum]) -> Vec<Datum> {
    if from.oid == to.oid {
        return row.to_vec();
    }
    to.cols
        .iter()
        .map(|c| {
            if c.dropped {
                return Datum::Null;
            }
            match from.col_by_name(&c.name) {
                Some(fc) => row.get((fc.attnum - 1) as usize).cloned().unwrap_or(Datum::Null),
                None => Datum::Null,
            }
        })
        .collect()
}

/// Route a row of partitioned table `td` (in `td`'s layout) to its leaf partition (module docs).
pub fn route(ex: &mut Executor, td: Rc<TableDesc>, row: Vec<Datum>) -> Result<(Rc<TableDesc>, Vec<Datum>)> {
    let mut td = td;
    let mut row = row;
    let mut depth = 0;
    while td.kind == RelKind::PartitionedTable {
        depth += 1;
        if depth > 64 {
            return Err(PgError::internal("partition tree too deep"));
        }
        let info = part_info(&*ex.cx, td.oid)?;
        let key = key_values(ex, td.oid, td.oid, &row)?;
        let child = match find_partition(ex, &info, &key)? {
            Some(c) => c,
            None => {
                let desc = key_description(ex, &info, &key)?;
                return Err(PgError::new(
                    SqlState::CHECK_VIOLATION,
                    format!("no partition of relation \"{}\" found for row", info.name),
                )
                .with_detail(format!("Partition key of the failing row contains {desc}."))
                .with_table(&td.schema, &td.name));
            }
        };
        let ctd = table_desc(&*ex.cx, child)?;
        row = convert_row(&td, &ctd, &row);
        td = ctd;
    }
    Ok((td, row))
}

/// Does `row` (layout of partition `td`) satisfy `td`'s partition constraint, including every
/// ancestor's? Non-partitions always pass.
pub fn check_partition_constraint(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<bool> {
    let mut cur = td.oid;
    let mut depth = 0;
    loop {
        depth += 1;
        if depth > 64 {
            return Err(PgError::internal("partition tree too deep"));
        }
        let parent = match relation(&*ex.cx, cur)?.partition_of.as_ref() {
            Some((p, _)) => *p,
            None => return Ok(true),
        };
        let info = part_info(&*ex.cx, parent)?;
        let key = key_values(ex, parent, td.oid, row)?;
        let accepted = find_partition(ex, &info, &key)?;
        if accepted != Some(cur) {
            return Ok(false);
        }
        cur = parent;
    }
}

/// 23514 `new row for relation "p1" violates partition constraint` + failing row.
pub fn partition_constraint_error(ex: &Executor, td: &TableDesc, row: &[Datum]) -> PgError {
    let desc = row_description(&ex.cx.se.io_ctx(), &td.desc_cols(), row).unwrap_or_default();
    PgError::new(
        SqlState::CHECK_VIOLATION,
        format!("new row for relation \"{}\" violates partition constraint", td.name),
    )
    .with_detail(format!("Failing row contains {desc}."))
    .with_table(&td.schema, &td.name)
}

/// Insert-side handling of a row for table `td` (already assembled in `td`'s layout): route a
/// partitioned table's row to its leaf, or check a directly targeted partition's constraint.
/// Returns the table and row to store.
pub fn prepare_insert(ex: &mut Executor, td: Rc<TableDesc>, row: Vec<Datum>) -> Result<(Rc<TableDesc>, Vec<Datum>)> {
    if td.kind == RelKind::PartitionedTable {
        return route(ex, td, row);
    }
    Ok((td, row))
}

/// After NOT NULL / CHECK passed for a row inserted directly into partition `td`.
pub fn check_direct_insert(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    if is_partition(&*ex.cx, td.oid)? && !check_partition_constraint(ex, td, row)? {
        return Err(partition_constraint_error(ex, td, row));
    }
    Ok(())
}

/// Is `oid` a partition (`relispartition`)?
pub fn is_partition(cx: &ExecContext, oid: Oid) -> Result<bool> {
    Ok(relation(cx, oid)?.partition_of.is_some())
}
