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
use crate::env::CatalogEnv;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{PgError, Result};

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

/// Compute the deletion set (module docs). RESTRICT violations are returned as `Err`.
// CONTRACT
#[allow(unused_variables)]
pub fn plan_drop(env: &CatalogEnv, targets: &[ObjectAddress], behavior: DropBehavior) -> Result<DropPlan> {
    Err(PgError::unimplemented("CAT-24", "drop planning"))
}

impl<'a> CatalogTxn<'a> {
    /// Plan and apply a drop (module docs). Requires nothing about ownership: callers
    /// (`drop_stmt`, `alter_*`) check permissions first.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_objects(&mut self, targets: &[ObjectAddress], behavior: DropBehavior) -> Result<()> {
        Err(PgError::unimplemented("CAT-24", "drop execution"))
    }
}
