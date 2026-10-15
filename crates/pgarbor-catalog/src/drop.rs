//! DROP planning and execution: RESTRICT/CASCADE dependency closure, PG-exact errors and NOTICEs,
//! and the catalog/data effects of deleting each object kind. Lease CAT-24 (opus-grade subtlety:
//! reporting order and the Auto/Internal/Normal flag algebra; assign sonnet minimum).
//! PG sources: `catalog/dependency.c` (`performDeletion`, `performMultipleDeletions`,
//! `findDependentObjects`, `reportDependentObjects`, `deleteOneObject`, `doDeletion`,
//! `object_address_present_add_flags`, flags `DEPFLAG_*`), `catalog/heap.c` (`heap_drop_with_catalog`,
//! `RemoveAttributeById`), `catalog/index.c` (`index_drop`), `commands/tablecmds.c`
//! (`ATExecDropColumn`), `catalog/objectaddress.c` (`getObjectDescription`).
//!
//! ## [`plan_drop`] (pure)
//! Input: the targets (addresses; a relation target means the whole relation; a column address
//! means DROP COLUMN), the behavior, and an env (snapshot + search path for descriptions).
//! 1. Build `DepGraph` (`depend.rs`). For each target compute the closure exactly like PG's
//!    `findDependentObjects`:
//!    - dependents of the object AND (for relations) of its columns; for `Auto`/`Internal` edges the
//!      dependent joins the deletion set with flag AUTO / INTERNAL, for `Normal` with flag NORMAL;
//!      flags of one object are OR-ed over all edges that reach it.
//!    - transitive: every newly added object's own dependents are added the same way (flags of the
//!      second hop are those of the NEW edge; an object reached through an AUTO/INTERNAL hop only is
//!      "silent").
//!    - **INTERNAL owner rule:** if a target (or an object reached through a NORMAL edge) itself has an
//!      INTERNAL dependency on an object that is NOT in the deletion set, it cannot be dropped alone:
//!      2BP01 `errors::required_by(<describe(target)>, <describe(owner)>)`, e.g. `cannot drop index
//!      t_a_key because constraint t_a_key on table t requires it` HINT `You can drop constraint
//!      t_a_key on table t instead.`; `cannot drop type t because table t requires it`; `cannot drop type
//!      t[] because type t requires it` (array types print as `t[]`); identity sequences:
//!      `cannot drop sequence t_d_seq because default value...`? no: because column d of table t
//!      requires it. Exception: dropping the owner's COLUMN (e.g. DROP COLUMN) or the owner itself
//!      puts the internal dependent in the set.
//!    - pinned objects (`ObjectAddress::is_pinned`) in the set -> 2BP01 `errors::required_by_system`.
//! 2. **RESTRICT:** any object in the set (other than the targets) whose accumulated flags contain no
//!    AUTO/INTERNAL bit -> 2BP01 `errors::dependent_objects_still_exist(desc(target), lines)`; lines =
//!    `<describe(dependent)> depends on <describe(the object it was reached from)>`, in PG's
//!    reporting order (`reportDependentObjects` walks the deletion order BACKWARDS, so objects
//!    closest to the target come first), capped by `errors` at 100 lines. Oracle (PG 18.6):
//!    `drop table t` with FK from t2 and views v -> v2:
//!    `constraint t2_x_fkey on table t2 depends on table t`, `view v depends on table t`,
//!    `view v2 depends on view v`; DROP COLUMN: title `cannot drop column a of table t because
//!    other objects depend on it`, line `view v depends on column a of table t`; DROP SCHEMA:
//!    `table t2 depends on schema zz_cat`; DROP TYPE: `column m of table w depends on type mood`;
//!    DROP SEQUENCE of a serial: `default value for column id of table t depends on sequence
//!    t_id_seq`; DROP CONSTRAINT t_pkey with an FK: title `cannot drop constraint t_pkey on table t
//!    because other objects depend on it`, line `constraint t2_x_fkey on table t2 depends on index
//!    t_pkey`. With several targets the title uses the FIRST target's description.
//! 3. **CASCADE:** the same set; NOTICE via `errors::drop_cascades_notice` listing every object with
//!    no AUTO/INTERNAL flag, excluding the targets, in PG's order (oracle: `drop cascades to
//!    constraint t2_x_fkey on table t2`, `view v`, `view v2`; `drop cascades to column m of table w`
//!    when dropping the type `mood`; schema drop lists `table zz_cat2.t` qualified when the schema is
//!    not on the search path). Descriptions come from `describe::describe` BEFORE anything is
//!    deleted.
//! 4. Deletion order for execution = dependents before referenced (reverse topological; ties by
//!    OID). `DropPlan::items` is that order.
//! Dropping an object listed twice, or reachable from two targets, appears once. Targets that do
//! not exist are the caller's problem (XX000 `cache lookup failed`).
//!
//! ## [`CatalogTxn::drop_objects`] (applies a plan)
//! Calls `plan_drop`, queues the notices, then for each item in order:
//! - Relation (table/view/matview/composite): remove the `Relation` row (its rowtype/array
//!   types vanish with it, they are derived); queue `FollowUp::DropRelationData{relation}` for
//!   `Table`/`MatView`; clear descriptions of the relation (`classoid 1259`, all subids).
//! - Index: remove the row, `DropRelationData{index}`, clear descriptions.
//! - Sequence: remove the row, `FollowUp::DropSequenceState{sequence}`, clear descriptions.
//! - Column address `col(R,n)` (DROP COLUMN, or reached via a dropped type): rewrite the `Column`
//!   as dropped (`Column::dropped_name`, `type_id = TypeId::new(0, -1)`, `not_null = false`,
//!   `default = None`, `identity = None`, `generated = None`, `missing_value = None`, options
//!   cleared), clear its column comment. If the whole relation is also in the set the relation row
//!   simply goes away.
//! - Constraint: remove the row (descriptions too). `NotNull` constraint -> clear
//!   `Column::not_null` of its column unless that column/relation is being dropped. A PK/UNIQUE
//!   constraint's backing index is an INTERNAL dependent so it appears in the set separately.
//! - AttrDefault: set `Column::default = None` (and `generated = None`? generation expression
//!   removal is DROP EXPRESSION in `alter_column`, not a drop item) unless the column is going away.
//! - TypeObj (enum/domain): remove the row; its constraints (domain) are separate items.
//! - Function: remove the row. Namespace: remove the row (its contents are separate items).
//! - Role/Database: not droppable through this path (`role.rs`, `schema.rs`).
//! Every removed object is appended with `record_dropped` and, for relations/indexes/sequences,
//! `follow_up`s are queued in deletion order. A relation that is the `ref_table` of FK constraints
//! is only reachable through Normal edges, so by construction no dangling FK remains.
//! After applying, `refresh()` must succeed (`CatalogSnapshot::build` validates references).
//!
//! ## Implementation notes
//! The closure is a port of PG's recursion: objects are appended to the target list in post-order
//! (dependents before the object), the flags of an object are those of the edge that first reached it
//! OR-ed with later edges, the "dependee" printed in RESTRICT lines is the object we recursed from, and
//! dependents are visited in PG's comparator order (OID descending, then catalog, then sub-id).
//! Divergence from the prose above, taken from `performMultipleDeletions` (`origObject` is NULL unless
//! there is exactly one target): with SEVERAL targets the RESTRICT title is `cannot drop desired
//! object(s) because other objects depend on them`.
use crate::depend::{DepEntry, DepGraph};
use crate::describe::{describe, describe_existing};
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use crate::txn::{CatalogTxn, FollowUp};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::TypeId;
use std::collections::{HashMap, HashSet};

/// One object to delete, in execution order.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropItem {
    pub address: ObjectAddress,
    /// True for the statement's explicit targets.
    pub explicit: bool,
}

/// The ordered deletion set plus the NOTICEs CASCADE produces.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct DropPlan {
    /// Dependents first.
    pub items: Vec<DropItem>,
    /// At most one `drop cascades to ...` NOTICE (CASCADE with non-silent dependents).
    pub notices: Vec<PgError>,
}

// PG `DEPFLAG_*`.
const F_ORIGINAL: u32 = 0x0001;
const F_NORMAL: u32 = 0x0002;
const F_AUTO: u32 = 0x0004;
const F_INTERNAL: u32 = 0x0008;
const F_PARTITION: u32 = 0x0010;
const F_EXTENSION: u32 = 0x0020;
const F_REVERSE: u32 = 0x0040;
const F_IS_PART: u32 = 0x0080;
const F_SUBOBJECT: u32 = 0x0100;

/// Recursion limit (PG: `check_stack_depth`).
const MAX_DEPTH: usize = 2000;

/// Per-object bookkeeping of the target list (PG `ObjectAddressExtra`).
#[derive(Clone, Copy, Debug)]
struct Extra {
    flags: u32,
    /// The object we recursed from when the object was first reached (printed in RESTRICT lines).
    dependee: Option<ObjectAddress>,
}

/// One level of the visiting stack (PG `ObjectAddressStack`).
#[derive(Clone, Copy, Debug)]
struct Frame {
    addr: ObjectAddress,
    flags: u32,
}

/// PG `object_address_present`: a whole-object entry covers its sub-objects.
fn address_present(object: &ObjectAddress, addrs: &[ObjectAddress]) -> bool {
    addrs.iter().any(|t| object.class == t.class && object.oid == t.oid && (object.sub_id == t.sub_id || t.sub_id == 0))
}

/// PG `object_address_present_add_flags` over the target list.
fn present_add_flags(object: &ObjectAddress, flags: u32, list: &mut [(ObjectAddress, Extra)]) -> bool {
    let mut result = false;
    for (t, extra) in list.iter_mut().rev() {
        if object.class == t.class && object.oid == t.oid {
            if object.sub_id == t.sub_id {
                extra.flags |= flags;
                result = true;
            } else if t.sub_id == 0 {
                // A column of an already scheduled whole object: nothing more to do.
                result = true;
            } else if object.sub_id == 0 && flags != 0 {
                // Whole object after one of its columns: mark the column entry as a sub-object.
                extra.flags |= flags | F_SUBOBJECT;
            }
        }
    }
    result
}

/// PG `stack_address_present_add_flags`.
fn stack_present_add_flags(object: &ObjectAddress, flags: u32, stack: &mut [Frame]) -> bool {
    let mut result = false;
    for f in stack.iter_mut().rev() {
        if object.class == f.addr.class && object.oid == f.addr.oid {
            if object.sub_id == f.addr.sub_id {
                f.flags |= flags;
                result = true;
            } else if f.addr.sub_id == 0 {
                result = true;
            } else if object.sub_id == 0 && flags != 0 {
                f.flags |= flags | F_SUBOBJECT;
            }
        }
    }
    result
}

struct Planner<'a> {
    env: &'a CatalogEnv,
    snap: &'a CatalogSnapshot,
    graph: &'a DepGraph,
    pending: &'a [ObjectAddress],
    /// PG `targetObjects`: deletion order (dependents first).
    list: Vec<(ObjectAddress, Extra)>,
}

impl<'a> Planner<'a> {
    /// PG `findDependentObjects`.
    fn find(&mut self, object: ObjectAddress, objflags: u32, stack: &mut Vec<Frame>) -> Result<()> {
        let mut objflags = objflags;
        if stack_present_add_flags(&object, objflags, stack) {
            return Ok(());
        }
        if stack.len() > MAX_DEPTH {
            return Err(PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
                "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
            ));
        }
        if present_add_flags(&object, objflags, &mut self.list) {
            return Ok(());
        }
        if object.is_pinned() {
            return Err(errors::required_by_system(&describe_existing(self.env, &object)?));
        }

        let graph = self.graph;
        let snap = self.snap;

        // What this object depends on (its "owners" matter).
        let mut refs: Vec<(ObjectAddress, DepType)> =
            graph.references_of(&object).iter().map(|e| (e.referenced, e.deptype)).collect();
        if object.sub_id == 0 && object.class == ObjClass::Relation {
            if let Some(r) = snap.relation(object.oid) {
                for c in &r.columns {
                    for e in graph.references_of(&ObjectAddress::column(object.oid, c.attnum)) {
                        refs.push((e.referenced, e.deptype));
                    }
                }
            }
        }

        let mut owning: Option<ObjectAddress> = None;
        let mut partition_object: Option<ObjectAddress> = None;
        for (other, deptype) in refs {
            if other.class == object.class && other.oid == object.oid && object.sub_id == 0 {
                continue;
            }
            match deptype {
                DepType::Normal | DepType::Auto | DepType::AutoExtension => {}
                DepType::Extension | DepType::Internal => {
                    if stack.is_empty() {
                        if address_present(&other, self.pending) {
                            // The owner is also a target: it will drag this object along.
                            return Ok(());
                        }
                        if owning.is_none() || deptype == DepType::Extension {
                            owning = Some(other);
                        }
                        continue;
                    }
                    if stack_present_add_flags(&other, 0, stack) {
                        continue;
                    }
                    // Not recursing from the owner: delete the owner instead.
                    self.find(other, F_REVERSE, stack)?;
                    if !present_add_flags(&object, objflags, &mut self.list) {
                        return Err(PgError::internal(format!(
                            "deletion of owning object {} failed to delete {}",
                            describe_existing(self.env, &other)?,
                            describe_existing(self.env, &object)?
                        )));
                    }
                    return Ok(());
                }
                DepType::PartitionPri => {
                    objflags |= F_IS_PART;
                    partition_object = Some(other);
                }
                DepType::PartitionSec => {
                    if objflags & F_IS_PART == 0 {
                        partition_object = Some(other);
                    }
                    objflags |= F_IS_PART;
                }
            }
        }

        if let Some(owner) = owning {
            let shown = partition_object.unwrap_or(owner);
            return Err(errors::required_by(
                &describe_existing(self.env, &object)?,
                &describe_existing(self.env, &shown)?,
            ));
        }

        // Direct dependents (of the object and, for a whole relation, of its columns).
        let edges: Vec<&DepEntry> = if object.sub_id != 0 || object.class != ObjClass::Relation {
            graph.dependents_of(&object)
        } else {
            graph.dependents_of_object_and_columns(snap, &object)
        };
        let mut deps: Vec<(ObjectAddress, u32)> = Vec::with_capacity(edges.len());
        for e in edges {
            let d = e.dependent;
            if d.class == object.class && d.oid == object.oid && object.sub_id == 0 {
                continue;
            }
            let sub = match e.deptype {
                DepType::Normal => F_NORMAL,
                DepType::Auto | DepType::AutoExtension => F_AUTO,
                DepType::Internal => F_INTERNAL,
                DepType::PartitionPri | DepType::PartitionSec => F_PARTITION,
                DepType::Extension => F_EXTENSION,
            };
            deps.push((d, sub));
        }
        // PG `object_address_comparator`: OID descending, then catalog, then sub-id (unsigned).
        deps.sort_by(|a, b| {
            b.0.oid
                .cmp(&a.0.oid)
                .then(a.0.class.classid().cmp(&b.0.class.classid()))
                .then((a.0.sub_id as u32).cmp(&(b.0.sub_id as u32)))
        });

        stack.push(Frame { addr: object, flags: objflags });
        for (d, sub) in deps {
            self.find(d, sub, stack)?;
        }
        let flags = stack.pop().map(|f| f.flags).unwrap_or(objflags);
        let dependee = if flags & F_IS_PART != 0 { partition_object } else { stack.last().map(|f| f.addr) };
        self.list.push((object, Extra { flags, dependee }));
        Ok(())
    }
}

/// Compute the deletion set (module docs). RESTRICT violations are returned as `Err`.
// CONTRACT
pub fn plan_drop(env: &CatalogEnv, targets: &[ObjectAddress], behavior: DropBehavior) -> Result<DropPlan> {
    if targets.is_empty() {
        return Ok(DropPlan::default());
    }
    let graph = DepGraph::build(env.snapshot())?;
    plan_with_graph(env, &graph, targets, behavior)
}

/// `plan_drop` over an explicit graph (unit tests inject hand-built edges).
fn plan_with_graph(
    env: &CatalogEnv,
    graph: &DepGraph,
    targets: &[ObjectAddress],
    behavior: DropBehavior,
) -> Result<DropPlan> {
    // A target that does not exist is XX000 `cache lookup failed`.
    for t in targets {
        describe_existing(env, t)?;
    }
    let mut p = Planner { env, snap: env.snapshot(), graph, pending: targets, list: Vec::new() };
    let mut stack: Vec<Frame> = Vec::new();
    for t in targets {
        p.find(*t, F_ORIGINAL, &mut stack)?;
        stack.clear();
    }

    // PG `reportDependentObjects`.
    for (obj, ex) in &p.list {
        if ex.flags & F_IS_PART != 0 && ex.flags & F_PARTITION == 0 {
            let other = match ex.dependee {
                Some(d) => describe_existing(env, &d)?,
                None => String::new(),
            };
            return Err(errors::required_by(&describe_existing(env, obj)?, &other));
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut descs: Vec<String> = Vec::new();
    let mut ok = true;
    for (obj, ex) in p.list.iter().rev() {
        if ex.flags & (F_ORIGINAL | F_SUBOBJECT) != 0 {
            continue;
        }
        let Some(desc) = describe(env, obj)? else { continue };
        if ex.flags & (F_AUTO | F_INTERNAL | F_PARTITION | F_EXTENSION) != 0 {
            continue;
        }
        match behavior {
            DropBehavior::Restrict => {
                ok = false;
                if let Some(dep) = ex.dependee {
                    if let Some(other) = describe(env, &dep)? {
                        lines.push(format!("{desc} depends on {other}"));
                    }
                }
            }
            DropBehavior::Cascade => descs.push(desc),
        }
    }
    if !ok {
        let mut e = errors::dependent_objects_still_exist(&describe_existing(env, &targets[0])?, &lines);
        if targets.len() != 1 {
            e.message = "cannot drop desired object(s) because other objects depend on them".to_string();
        }
        return Err(e);
    }
    let mut plan = DropPlan::default();
    if let Some(n) = errors::drop_cascades_notice(&descs) {
        plan.notices.push(n);
    }
    plan.items = p.list.iter().map(|(a, _)| DropItem { address: *a, explicit: targets.contains(a) }).collect();
    Ok(plan)
}

fn col_mut(r: &mut Relation, attnum: i16) -> Option<&mut Column> {
    if attnum < 1 {
        return None;
    }
    r.columns.get_mut(attnum as usize - 1)
}

impl<'a> CatalogTxn<'a> {
    /// Plan and apply a drop (module docs). Requires nothing about ownership: callers
    /// (`drop_stmt`, `alter_*`) check permissions first.
    // CONTRACT
    pub fn drop_objects(&mut self, targets: &[ObjectAddress], behavior: DropBehavior) -> Result<()> {
        let env = self.env()?;
        let plan = plan_drop(&env, targets, behavior)?;
        self.apply_plan(&plan)
    }

    /// Queue the plan's notices and delete its items in order.
    fn apply_plan(&mut self, plan: &DropPlan) -> Result<()> {
        for n in &plan.notices {
            self.notice(n.clone());
        }
        // Facts about the pre-drop catalog needed while items disappear one by one.
        let mut rel_set: HashSet<Oid> = HashSet::new();
        let mut type_set: HashSet<Oid> = HashSet::new();
        let mut dropped_cols: HashSet<(Oid, i16)> = HashSet::new();
        for it in &plan.items {
            let a = it.address;
            match (a.class, a.sub_id) {
                (ObjClass::Relation, 0) => {
                    rel_set.insert(a.oid);
                }
                (ObjClass::Relation, n) => {
                    if let Ok(n) = i16::try_from(n) {
                        dropped_cols.insert((a.oid, n));
                    }
                }
                (ObjClass::Type, _) => {
                    type_set.insert(a.oid);
                }
                _ => {}
            }
        }
        let mut derived_types: HashSet<Oid> = HashSet::new();
        let mut attrdef_owner: HashMap<Oid, (Oid, i16)> = HashMap::new();
        for r in self.data.relations.values() {
            if r.rowtype != 0 {
                derived_types.insert(r.rowtype);
            }
            if r.array_type != 0 {
                derived_types.insert(r.array_type);
            }
            for c in &r.columns {
                if let Some(d) = &c.default {
                    attrdef_owner.insert(d.oid, (r.oid, c.attnum));
                }
            }
        }
        for t in self.data.types.values() {
            if t.array_type != 0 {
                derived_types.insert(t.array_type);
            }
        }

        for it in &plan.items {
            let a = it.address;
            let recorded = match a.class {
                ObjClass::Relation if a.sub_id == 0 => {
                    self.drop_pg_class(a.oid)?;
                    true
                }
                ObjClass::Relation => {
                    let attnum = i16::try_from(a.sub_id)
                        .map_err(|_| PgError::internal(format!("invalid column number {}", a.sub_id)))?;
                    self.remove_dropped_column(a.oid, attnum, &rel_set)?
                }
                ObjClass::Type => {
                    if derived_types.contains(&a.oid) {
                        // Rowtype / array type of a relation or user type: derived, goes with its owner.
                        self.clear_descriptions(class_oid::PG_TYPE, a.oid)?;
                    } else {
                        let array = self.data.types.get(&a.oid).map(|t| t.array_type);
                        self.remove(ObjKind::Type, a.oid)?;
                        self.clear_descriptions(class_oid::PG_TYPE, a.oid)?;
                        if let Some(arr) = array {
                            if arr != 0 {
                                self.clear_descriptions(class_oid::PG_TYPE, arr)?;
                            }
                        }
                    }
                    true
                }
                ObjClass::Constraint => {
                    self.remove_dropped_constraint(a.oid, &rel_set, &type_set, &dropped_cols)?;
                    true
                }
                ObjClass::AttrDefault => {
                    let (rel, att) = attrdef_owner
                        .get(&a.oid)
                        .copied()
                        .ok_or_else(|| errors::cache_lookup_failed("attrdef", a.oid))?;
                    if !rel_set.contains(&rel)
                        && !dropped_cols.contains(&(rel, att))
                        && self.data.relations.contains_key(&rel)
                    {
                        let ad = a.oid;
                        self.update_relation(rel, |r| {
                            if let Some(c) = col_mut(r, att) {
                                if c.default.as_ref().map(|d| d.oid) == Some(ad) {
                                    c.default = None;
                                }
                            }
                            Ok(())
                        })?;
                    }
                    true
                }
                ObjClass::Proc => {
                    self.remove(ObjKind::Function, a.oid)?;
                    self.clear_descriptions(class_oid::PG_PROC, a.oid)?;
                    true
                }
                ObjClass::Namespace => {
                    self.remove(ObjKind::Namespace, a.oid)?;
                    self.clear_descriptions(class_oid::PG_NAMESPACE, a.oid)?;
                    true
                }
                // The `_RETURN` rule is part of the view row.
                ObjClass::Rewrite => true,
                ObjClass::Role | ObjClass::Database => {
                    return Err(PgError::internal(format!(
                        "{:?} {} cannot be dropped through drop_objects",
                        a.class, a.oid
                    )));
                }
                ObjClass::Collation
                | ObjClass::Operator
                | ObjClass::Cast
                | ObjClass::Language
                | ObjClass::AccessMethod
                | ObjClass::Extension
                | ObjClass::Trigger
                | ObjClass::Tablespace => {
                    return Err(PgError::unimplemented("CAT-24", format!("DROP of {:?} objects", a.class)));
                }
            };
            if recorded {
                self.record_dropped(a);
            }
        }
        self.refresh()
    }

    /// Delete a pg_class entry: relation, index or sequence.
    fn drop_pg_class(&mut self, oid: Oid) -> Result<()> {
        if let Some((kind, rowtype, array)) = self.data.relations.get(&oid).map(|r| (r.kind, r.rowtype, r.array_type)) {
            self.remove(ObjKind::Relation, oid)?;
            if matches!(kind, RelKind::Table | RelKind::MatView) {
                self.follow_up(FollowUp::DropRelationData { relation: oid });
            }
            for ty in [rowtype, array] {
                if ty != 0 {
                    self.clear_descriptions(class_oid::PG_TYPE, ty)?;
                }
            }
        } else if self.data.indexes.contains_key(&oid) {
            self.remove(ObjKind::Index, oid)?;
            self.follow_up(FollowUp::DropRelationData { relation: oid });
        } else if self.data.sequences.contains_key(&oid) {
            self.remove(ObjKind::Sequence, oid)?;
            self.follow_up(FollowUp::DropSequenceState { sequence: oid });
        } else {
            return Err(errors::cache_lookup_failed("relation", oid));
        }
        self.clear_descriptions(class_oid::PG_CLASS, oid)
    }

    /// Turn a column into a dropped slot (PG `RemoveAttributeById`). Returns false when the whole
    /// relation is dropped too (nothing to do).
    fn remove_dropped_column(&mut self, rel: Oid, attnum: i16, rel_set: &HashSet<Oid>) -> Result<bool> {
        if rel_set.contains(&rel) {
            return Ok(false);
        }
        self.update_relation(rel, |r| {
            let relid = r.oid;
            let Some(c) = col_mut(r, attnum) else {
                return Err(PgError::internal(format!("column {attnum} of relation {relid} does not exist")));
            };
            if c.dropped {
                return Ok(());
            }
            c.name = Column::dropped_name(attnum);
            c.type_id = TypeId::new(0, -1);
            c.not_null = false;
            c.default = None;
            c.identity = None;
            c.generated = None;
            c.missing_value = None;
            c.options.clear();
            c.acl = None;
            c.stat_target = None;
            c.collation = 0;
            c.dropped = true;
            Ok(())
        })?;
        let key = DescriptionKey { objoid: rel, classoid: class_oid::PG_CLASS, objsubid: attnum as i32 };
        if self.data.descriptions.contains_key(&key) {
            self.set_description(key, None)?;
        }
        Ok(true)
    }

    /// Delete a constraint row and undo its NOT NULL effect on the owning column/domain.
    fn remove_dropped_constraint(
        &mut self,
        oid: Oid,
        rel_set: &HashSet<Oid>,
        type_set: &HashSet<Oid>,
        dropped_cols: &HashSet<(Oid, i16)>,
    ) -> Result<()> {
        let c = self
            .data
            .constraints
            .get(&oid)
            .cloned()
            .ok_or_else(|| errors::cache_lookup_failed("constraint", oid))?;
        self.remove(ObjKind::Constraint, oid)?;
        self.clear_descriptions(class_oid::PG_CONSTRAINT, oid)?;
        if c.kind == ConstraintKind::NotNull {
            if c.domain != 0 {
                if !type_set.contains(&c.domain) && self.data.types.contains_key(&c.domain) {
                    self.update_type(c.domain, |t| {
                        if let TypeDef::Domain { not_null, .. } = &mut t.def {
                            *not_null = false;
                        }
                        Ok(())
                    })?;
                }
            } else if c.table != 0 && !rel_set.contains(&c.table) && self.data.relations.contains_key(&c.table) {
                let cols: Vec<i16> =
                    c.columns.iter().copied().filter(|a| !dropped_cols.contains(&(c.table, *a))).collect();
                if !cols.is_empty() {
                    self.update_relation(c.table, |r| {
                        for a in &cols {
                            if let Some(col) = col_mut(r, *a) {
                                col.not_null = false;
                            }
                        }
                        Ok(())
                    })?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use pgarbor_common::MemKv;

    const NS: Oid = 2200;
    const T: Oid = 20010;
    const T2: Oid = 20020;
    const FK: Oid = 20030;
    const V: Oid = 20040;
    const V2: Oid = 20050;
    const IDX: Oid = 20060;
    const UQ: Oid = 20070;

    fn rel(oid: Oid, name: &str, kind: RelKind, ncols: i16) -> Relation {
        let columns = (1..=ncols)
            .map(|i| Column::new(i, if i == 1 { "a" } else { "b" }, TypeId::INT4, 0))
            .collect::<Vec<_>>();
        Relation {
            oid,
            name: name.to_string(),
            namespace: NS,
            kind,
            owner: 10,
            persistence: Persistence::Permanent,
            columns,
            rowtype: oid + 1,
            array_type: oid + 2,
            array_type_name: None,
            options: Vec::new(),
            acl: None,
            replica_identity: ReplicaIdentity::Default,
            row_security: false,
            force_row_security: false,
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        }
    }

    fn view(oid: Oid, name: &str) -> Relation {
        let mut r = rel(oid, name, RelKind::View, 1);
        r.view = Some(ViewDef { query: StoredExpr::new("SELECT a FROM t"), check_option: None, populated: true });
        r
    }

    fn con(oid: Oid, name: &str, kind: ConstraintKind, table: Oid, columns: Vec<i16>, index: Oid) -> Constraint {
        Constraint {
            oid,
            name: name.to_string(),
            namespace: NS,
            kind,
            table,
            domain: 0,
            columns,
            index,
            deferrable: false,
            initially_deferred: false,
            validated: true,
            enforced: true,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: None,
            check: if kind == ConstraintKind::Check { Some(StoredExpr::new("true")) } else { None },
            parent: 0,
        }
    }

    fn index() -> Index {
        Index {
            oid: IDX,
            name: "t_a_key".to_string(),
            namespace: NS,
            table: T,
            owner: 10,
            method: IndexMethod::Btree,
            keys: vec![IndexKey {
                column: Some(1),
                expr: None,
                name: "a".to_string(),
                type_id: TypeId::INT4,
                collation: 0,
                opclass: 1978,
                descending: false,
                nulls_first: false,
            }],
            n_key_columns: 1,
            unique: true,
            nulls_not_distinct: false,
            primary: false,
            exclusion: false,
            immediate: true,
            clustered: false,
            valid: true,
            replident: false,
            predicate: None,
            constraint: Some(UQ),
            options: Vec::new(),
            persistence: Persistence::Permanent,
        }
    }

    /// t(a,b) with unique t_a_key (index IDX / constraint UQ), t2(a) with `t2_x_fkey` -> t.b,
    /// view v over t.a and view v2 over v.
    fn world() -> Result<(MemKv, CatalogEnv)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env) = testing::run(&mut kv, &env, |t| {
            t.put(CatalogObject::Relation(rel(T, "t", RelKind::Table, 2)))?;
            t.put(CatalogObject::Relation(rel(T2, "t2", RelKind::Table, 1)))?;
            t.put(CatalogObject::Relation(view(V, "v")))?;
            t.put(CatalogObject::Relation(view(V2, "v2")))?;
            t.put(CatalogObject::Constraint(con(FK, "t2_x_fkey", ConstraintKind::Check, T2, vec![1], 0)))?;
            t.put(CatalogObject::Constraint(con(UQ, "t_a_key", ConstraintKind::Unique, T, vec![1], IDX)))?;
            t.put(CatalogObject::Index(index()))?;
            Ok(())
        })?;
        Ok((kv, env))
    }

    fn graph() -> DepGraph {
        let c1 = ObjectAddress::column(T, 1);
        let c2 = ObjectAddress::column(T, 2);
        let e = |dependent: ObjectAddress, referenced: ObjectAddress, deptype: DepType| DepEntry {
            dependent,
            referenced,
            deptype,
        };
        DepGraph::from_entries(vec![
            e(ObjectAddress::constraint(UQ), c1, DepType::Auto),
            e(ObjectAddress::relation(IDX), ObjectAddress::constraint(UQ), DepType::Internal),
            e(ObjectAddress::relation(V), c1, DepType::Normal),
            e(ObjectAddress::relation(V2), ObjectAddress::relation(V), DepType::Normal),
            e(ObjectAddress::constraint(FK), c2, DepType::Normal),
            e(ObjectAddress::constraint(FK), ObjectAddress::column(T2, 1), DepType::Auto),
        ])
    }

    fn addrs(plan: &DropPlan) -> Vec<ObjectAddress> {
        plan.items.iter().map(|i| i.address).collect()
    }

    #[test]
    fn restrict_table_reports_dependents_in_pg_order() -> Result<()> {
        let (_kv, env) = world()?;
        let err = plan_with_graph(&env, &graph(), &[ObjectAddress::relation(T)], DropBehavior::Restrict).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(err.message, "cannot drop table t because other objects depend on it");
        assert_eq!(
            err.detail.as_deref(),
            Some(
                "constraint t2_x_fkey on table t2 depends on table t\nview v depends on table t\nview v2 depends on view v"
            )
        );
        assert_eq!(err.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
        Ok(())
    }

    #[test]
    fn cascade_table_notice_and_order() -> Result<()> {
        let (_kv, env) = world()?;
        let plan = plan_with_graph(&env, &graph(), &[ObjectAddress::relation(T)], DropBehavior::Cascade)?;
        assert_eq!(
            addrs(&plan),
            vec![
                ObjectAddress::relation(IDX),
                ObjectAddress::constraint(UQ),
                ObjectAddress::relation(V2),
                ObjectAddress::relation(V),
                ObjectAddress::constraint(FK),
                ObjectAddress::relation(T),
            ]
        );
        assert!(plan.items[5].explicit && !plan.items[0].explicit);
        assert_eq!(plan.notices.len(), 1);
        assert_eq!(plan.notices[0].message, "drop cascades to 3 other objects");
        assert_eq!(
            plan.notices[0].detail.as_deref(),
            Some("drop cascades to constraint t2_x_fkey on table t2\ndrop cascades to view v\ndrop cascades to view v2")
        );
        Ok(())
    }

    #[test]
    fn restrict_column_reports_view_chain() -> Result<()> {
        let (_kv, env) = world()?;
        let err =
            plan_with_graph(&env, &graph(), &[ObjectAddress::column(T, 1)], DropBehavior::Restrict).unwrap_err();
        assert_eq!(err.message, "cannot drop column a of table t because other objects depend on it");
        assert_eq!(err.detail.as_deref(), Some("view v depends on column a of table t\nview v2 depends on view v"));
        Ok(())
    }

    #[test]
    fn internal_owner_refuses_direct_index_drop() -> Result<()> {
        let (_kv, env) = world()?;
        let err = plan_with_graph(&env, &graph(), &[ObjectAddress::relation(IDX)], DropBehavior::Cascade).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(err.message, "cannot drop index t_a_key because constraint t_a_key on table t requires it");
        assert_eq!(err.hint.as_deref(), Some("You can drop constraint t_a_key on table t instead."));
        Ok(())
    }

    #[test]
    fn dropping_owner_takes_internal_dependent_silently() -> Result<()> {
        let (_kv, env) = world()?;
        let plan = plan_with_graph(&env, &graph(), &[ObjectAddress::constraint(UQ)], DropBehavior::Restrict)?;
        assert_eq!(addrs(&plan), vec![ObjectAddress::relation(IDX), ObjectAddress::constraint(UQ)]);
        assert!(plan.notices.is_empty());
        // Both listed: the index waits for its owner and appears once.
        let both = [ObjectAddress::relation(IDX), ObjectAddress::constraint(UQ)];
        let plan = plan_with_graph(&env, &graph(), &both, DropBehavior::Restrict)?;
        assert_eq!(addrs(&plan), vec![ObjectAddress::relation(IDX), ObjectAddress::constraint(UQ)]);
        assert!(plan.items.iter().all(|i| i.explicit));
        Ok(())
    }

    #[test]
    fn pinned_schema_is_required_by_system() -> Result<()> {
        let (_kv, env) = world()?;
        let err =
            plan_with_graph(&env, &graph(), &[ObjectAddress::namespace(11)], DropBehavior::Cascade).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(err.message, "cannot drop schema pg_catalog because it is required by the database system");
        Ok(())
    }

    #[test]
    fn missing_target_fails() -> Result<()> {
        let (_kv, env) = world()?;
        assert!(plan_with_graph(&env, &graph(), &[ObjectAddress::relation(99999)], DropBehavior::Cascade).is_err());
        Ok(())
    }

    #[test]
    fn duplicate_and_overlapping_targets_appear_once() -> Result<()> {
        let (_kv, env) = world()?;
        let t = ObjectAddress::relation(T);
        let plan = plan_with_graph(&env, &graph(), &[t, t], DropBehavior::Cascade)?;
        assert_eq!(plan.items.iter().filter(|i| i.address == t).count(), 1);
        // v and v2 both targets under RESTRICT: v2 is explicit, so nothing is reported.
        let both = [ObjectAddress::relation(V), ObjectAddress::relation(V2)];
        let plan = plan_with_graph(&env, &graph(), &both, DropBehavior::Restrict)?;
        assert_eq!(addrs(&plan), vec![ObjectAddress::relation(V2), ObjectAddress::relation(V)]);
        Ok(())
    }

    #[test]
    fn several_targets_use_generic_title() -> Result<()> {
        let (_kv, env) = world()?;
        let targets = [ObjectAddress::relation(V), ObjectAddress::relation(T2)];
        let err = plan_with_graph(&env, &graph(), &targets, DropBehavior::Restrict).unwrap_err();
        assert_eq!(err.message, "cannot drop desired object(s) because other objects depend on them");
        assert_eq!(err.detail.as_deref(), Some("view v2 depends on view v"));
        Ok(())
    }

    #[test]
    fn apply_cascade_drop_table_removes_dependents_and_queues_data_removal() -> Result<()> {
        let (mut kv, env) = world()?;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            let env = t.env()?;
            let plan = plan_with_graph(&env, &graph(), &[ObjectAddress::relation(T)], DropBehavior::Cascade)?;
            t.apply_plan(&plan)
        })?;
        let s = &res.snapshot;
        assert!(s.relation(T).is_none() && s.relation(V).is_none() && s.relation(V2).is_none());
        assert!(s.index(IDX).is_none() && s.constraint(UQ).is_none() && s.constraint(FK).is_none());
        assert!(s.relation(T2).is_some());
        assert!(res.outcome.follow_ups.contains(&FollowUp::DropRelationData { relation: T }));
        assert!(res.outcome.follow_ups.contains(&FollowUp::DropRelationData { relation: IDX }));
        assert!(!res.outcome.follow_ups.contains(&FollowUp::DropRelationData { relation: V }));
        assert_eq!(res.outcome.dropped.len(), 6);
        assert_eq!(res.outcome.notices.len(), 1);
        Ok(())
    }

    #[test]
    fn apply_drop_column_rewrites_slot() -> Result<()> {
        let (mut kv, env) = world()?;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            let env = t.env()?;
            let plan = plan_with_graph(&env, &graph(), &[ObjectAddress::column(T, 2)], DropBehavior::Cascade)?;
            t.apply_plan(&plan)
        })?;
        let r = res.snapshot.relation(T).expect("t survives");
        let c = &r.columns[1];
        assert!(c.dropped);
        assert_eq!(c.name, Column::dropped_name(2));
        assert_eq!(c.type_id.oid, 0);
        assert!(!r.columns[0].dropped);
        assert!(res.snapshot.constraint(FK).is_none());
        Ok(())
    }

    #[test]
    fn apply_index_and_constraint() -> Result<()> {
        let (mut kv, env) = world()?;
        let plan = DropPlan {
            items: vec![
                DropItem { address: ObjectAddress::relation(IDX), explicit: false },
                DropItem { address: ObjectAddress::constraint(UQ), explicit: true },
            ],
            notices: Vec::new(),
        };
        let (res, _env) = testing::run(&mut kv, &env, |t| t.apply_plan(&plan))?;
        assert!(res.snapshot.index(IDX).is_none() && res.snapshot.constraint(UQ).is_none());
        assert_eq!(res.outcome.follow_ups, vec![FollowUp::DropRelationData { relation: IDX }]);
        assert_eq!(res.outcome.dropped, vec![ObjectAddress::relation(IDX), ObjectAddress::constraint(UQ)]);
        Ok(())
    }
}
