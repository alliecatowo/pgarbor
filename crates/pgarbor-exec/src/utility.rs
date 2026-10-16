//! Utility statements executed by the executor that need no session state (lease EXE-39): LOCK TABLE,
//! DO, CALL, maintenance commands. (Transaction control, SET/SHOW, PREPARE/EXECUTE, LISTEN/NOTIFY,
//! DISCARD are SESSION statements handled by the `pgarbor` crate and never reach the executor.)
//!
//! * [`exec_lock`] (`BoundLock{tables, mode, nowait}`): `perm::require_relation_privilege` first (see
//!   `perm.rs`), then for each table `ExecTxn::lock_relation(oid,
//!   core LockMode, policy)` with the mode mapped one-to-one from `pgarbor_sql::ir::LockMode`
//!   (`AccessShare`... `AccessExclusive`) and `WaitPolicy::NoWait` if `nowait` else `Block`; not
//!   acquired -> `error::relation_lock_not_available(name)` (55P03 `could not obtain lock on relation
//!   "t"`). Tag `LOCK TABLE`. (The "can only be used in transaction blocks" check is the session's.)
//! * [`exec_do`] / [`exec_call`]: run PL/pgSQL blocks and procedures (`crate::plpgsql`,
//!   `crate::plfunc`); tags `DO` / `CALL` (CALL with OUT / INOUT parameters returns one row).
//! * [`exec_maintenance`] (`MaintenanceKind`, `targets` = table OIDs, empty = all):
//!   - `Reindex`: for each target table (or the table of each target index) delete the entry data of
//!     its btree indexes (`rowstore::delete_relation_data(index)`) and rebuild them
//!     (`index_maint::build_index`); tag `REINDEX`;
//!   - `Cluster`: `[table]` or `[table, index]`; records the chosen index as `indisclustered`
//!     (clearing its siblings) via [`run_ddl`]; the reorder itself is a no-op; tag `CLUSTER`;
//!   - `Vacuum`, `Analyze`, `Checkpoint`: accepted no-ops (no dead tuples exist, no
//!     statistics are kept, durability is per commit); tags `VACUUM`, `ANALYZE`, `CHECKPOINT`.
use pgarbor_catalog::model::{acl_mode, IndexMethod, RelKind};
use pgarbor_catalog::snapshot::{CatalogSnapshot, ClassRef};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::{Datum, TypeId};
use pgarbor_core::{LockMode as CoreLockMode, WaitPolicy};
use pgarbor_sql::ir::{BoundCall, BoundDo, BoundLock, LockMode, MaintenanceKind};

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::error;
use crate::index_maint;
use crate::perm;
use crate::result::ExecResult;
use crate::rowstore;
use crate::runtime::{Executor, Runtime};
use crate::tabledesc;

pub fn exec_lock(cx: &mut ExecContext, l: &BoundLock) -> Result<ExecResult> {
    // lockcmds.c `LockTableCommand`: modes above ROW EXCLUSIVE are writes.
    if lock_level(l.mode) > 3 {
        cx.txn.check_writable("LOCK TABLE")?;
    }
    let core_mode = core_lock_mode(l.mode);
    let mask = lock_acl_mask(l.mode);
    let policy = if l.nowait { WaitPolicy::NoWait } else { WaitPolicy::Block };
    for &oid in &l.tables {
        let name = {
            let rel = cx.se.env.snapshot().relation(oid).ok_or_else(|| error::relation_vanished(oid))?;
            if !lockable_kind(&rel.kind) {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("cannot lock relation \"{}\"", rel.name),
                )
                .with_detail(format!("This operation is not supported for {}.", relkind_word(&rel.kind))));
            }
            rel.name.clone()
        };
        perm::require_relation_privilege(&cx.se, oid, mask, &[])?;
        cx.check_interrupts()?;
        if !cx.txn.lock_relation(oid, core_mode, policy)? {
            return Err(error::relation_lock_not_available(&name));
        }
    }
    Ok(ExecResult::command("LOCK TABLE"))
}

/// `DO` (PG `ExecuteDoStmt`): run the block with the PL/pgSQL interpreter. Transaction control is
/// allowed when the session marked the statement non-atomic (`cx.stmt.nonatomic`).
pub fn exec_do(cx: &mut ExecContext, d: &BoundDo) -> Result<ExecResult> {
    if d.language != "plpgsql" {
        return Err(PgError::unimplemented("EXE-39", format!("DO blocks in language {}", d.language)));
    }
    let nonatomic = std::mem::take(&mut cx.stmt.nonatomic);
    crate::plpgsql::run_inline(cx, &d.code, nonatomic)?;
    Ok(ExecResult::command("DO"))
}

/// `CALL proc(args)` (PG `ExecuteCallStmt`): evaluate the arguments (OUT positions are NULL
/// placeholders), run the procedure, and return one row of its OUT / INOUT parameters when it has
/// any.
pub fn exec_call(cx: &mut ExecContext, c: &BoundCall) -> Result<ExecResult> {
    let nonatomic = std::mem::take(&mut cx.stmt.nonatomic);
    let f = crate::plfunc::lookup(cx, c.procedure)?;
    let mut args = Vec::with_capacity(c.args.len());
    let mut arg_types = Vec::with_capacity(c.args.len());
    for a in &c.args {
        args.push(crate::eval::eval_standalone(cx, a, &[])?);
        arg_types.push(a.ty);
    }
    let _guard = crate::plfunc::DepthGuard::enter()?;
    let outs: Vec<(String, TypeId)> = f
        .args
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a.mode, pgarbor_catalog::model::ArgMode::Out | pgarbor_catalog::model::ArgMode::InOut))
        .map(|(i, a)| {
            let ty = if pgarbor_types::registry::is_polymorphic(a.type_oid) {
                arg_types.get(i).copied().unwrap_or(TypeId::of(a.type_oid))
            } else {
                TypeId::of(a.type_oid)
            };
            (if a.name.is_empty() { format!("column{}", i + 1) } else { a.name.clone() }, ty)
        })
        .collect();
    let values: Vec<Datum> = match f.language {
        crate::plfunc::LANG_PLPGSQL => crate::plpgsql::call_procedure(cx, &f, args, arg_types.clone(), nonatomic)?,
        crate::plfunc::LANG_SQL => {
            let in_args: Vec<Datum> = f
                .args
                .iter()
                .zip(args.iter())
                .filter(|(a, _)| !matches!(a.mode, pgarbor_catalog::model::ArgMode::Out | pgarbor_catalog::model::ArgMode::Table))
                .map(|(_, v)| v.clone())
                .collect();
            let in_types: Vec<TypeId> = f
                .args
                .iter()
                .zip(arg_types.iter())
                .filter(|(a, _)| !matches!(a.mode, pgarbor_catalog::model::ArgMode::Out | pgarbor_catalog::model::ArgMode::Table))
                .map(|(_, t)| *t)
                .collect();
            let rt = if outs.is_empty() { TypeId::of(pgarbor_types::oids::VOID) } else { TypeId::of(pgarbor_types::oids::RECORD) };
            let out = crate::plfunc::call_sql_function_for_call(cx, &f, &in_args, &in_types, rt)?;
            if outs.is_empty() {
                Vec::new()
            } else if outs.len() == 1 {
                match out {
                    Datum::Record(mut v) if v.len() == 1 => vec![v.pop().unwrap_or(Datum::Null)],
                    other => vec![other],
                }
            } else {
                match out {
                    Datum::Record(v) => v,
                    _ => vec![Datum::Null; outs.len()],
                }
            }
        }
        _ => return Err(PgError::unimplemented("EXE-39", "CALL of a procedure in this language")),
    };
    if outs.is_empty() {
        return Ok(ExecResult::command("CALL"));
    }
    let desc: Vec<pgarbor_plan::plan::ColumnDesc> = outs
        .iter()
        .map(|(n, t)| pgarbor_plan::plan::ColumnDesc { name: n.clone(), type_id: *t, table_oid: 0, column_attnum: 0 })
        .collect();
    Ok(ExecResult::Rows { desc, rows: vec![values], tag: "CALL".to_string() })
}

/// COMMIT / ROLLBACK inside a non-atomic procedure or DO block.
pub fn procedure_txn_control(cx: &mut ExecContext, commit: bool, chain: bool) -> Result<()> {
    let _ = (cx, commit, chain);
    Err(PgError::unimplemented("EXE-39", "transaction control inside procedures"))
}

pub fn exec_maintenance(cx: &mut ExecContext, kind: MaintenanceKind, targets: &[Oid]) -> Result<ExecResult> {
    match kind {
        MaintenanceKind::Vacuum => Ok(ExecResult::command("VACUUM")),
        MaintenanceKind::Analyze => analyze(cx, targets),
        MaintenanceKind::Cluster => cluster(cx, targets),
        MaintenanceKind::Checkpoint => {
            require_checkpoint_privilege(cx)?;
            Ok(ExecResult::command("CHECKPOINT"))
        }
        MaintenanceKind::Reindex | MaintenanceKind::ReindexConcurrently => reindex(cx, targets),
    }
}

/// ANALYZE (PG `vacuum_is_permitted_for_relation`): a table the role may not MAINTAIN is skipped with a
/// WARNING (SQLSTATE 01000), not an error. Other errors propagate.
fn analyze(cx: &mut ExecContext, targets: &[Oid]) -> Result<ExecResult> {
    for &oid in targets {
        match perm::require_relation_privilege(&cx.se, oid, acl_mode::MAINTAIN, &[]) {
            Ok(()) => {}
            Err(e) if e.sqlstate == SqlState::INSUFFICIENT_PRIVILEGE => {
                let name = cx.se.env.snapshot().relation(oid).map(|r| r.name.clone()).unwrap_or_default();
                cx.notice(
                    PgError::new(SqlState::WARNING, format!("permission denied to analyze \"{name}\", skipping it"))
                        .with_severity(Severity::Warning),
                );
            }
            Err(e) => return Err(e),
        }
    }
    Ok(ExecResult::command("ANALYZE"))
}

/// CLUSTER: `targets` is `[table]` (plain) or `[table, index]` (`USING index`). The chosen index
/// becomes the table's `indisclustered` index and clears the flag on its siblings (cluster.c
/// `mark_index_clustered`), which is what a later plain `CLUSTER table` looks up. The physical
/// reorder itself stays a no-op.
fn cluster(cx: &mut ExecContext, targets: &[Oid]) -> Result<ExecResult> {
    if let &[table, index] = targets {
        let changes: Vec<(Oid, bool)> = cx
            .se
            .env
            .snapshot()
            .indexes_of(table)
            .into_iter()
            .filter(|i| i.clustered != (i.oid == index))
            .map(|i| (i.oid, i.oid == index))
            .collect();
        if !changes.is_empty() {
            run_ddl(cx, move |d| {
                for (oid, flag) in changes {
                    d.update_index(oid, |i| {
                        i.clustered = flag;
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
        }
    }
    Ok(ExecResult::command("CLUSTER"))
}

/// `pg_checkpoint` predefined role (builtin oid 4544, catalog `builtin::objects`).
const PG_CHECKPOINT_ROLE: Oid = 4544;

/// utility.c `CHECKPOINT`: only superusers and roles with the privileges of `pg_checkpoint` may run it.
fn require_checkpoint_privilege(cx: &ExecContext) -> Result<()> {
    let user = cx.se.env.current_user();
    let graph = pgarbor_catalog::acl::RoleGraph::build(cx.se.env.snapshot(), cx.se.env.database())?;
    if graph.has_privs_of_role(user, PG_CHECKPOINT_ROLE)? {
        return Ok(());
    }
    Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied to execute CHECKPOINT command")
        .with_detail("Only roles with privileges of the \"pg_checkpoint\" role may execute this command."))
}

/// Map the SQL lock mode onto the core lock manager's mode (one-to-one).
fn core_lock_mode(mode: LockMode) -> CoreLockMode {
    match mode {
        LockMode::AccessShare => CoreLockMode::AccessShare,
        LockMode::RowShare => CoreLockMode::RowShare,
        LockMode::RowExclusive => CoreLockMode::RowExclusive,
        LockMode::ShareUpdateExclusive => CoreLockMode::ShareUpdateExclusive,
        LockMode::Share => CoreLockMode::Share,
        LockMode::ShareRowExclusive => CoreLockMode::ShareRowExclusive,
        LockMode::Exclusive => CoreLockMode::Exclusive,
        LockMode::AccessExclusive => CoreLockMode::AccessExclusive,
    }
}

/// PG lock level number (storage/lockdefs.h): AccessShareLock = 1 ... AccessExclusiveLock = 8.
fn lock_level(mode: LockMode) -> u8 {
    match mode {
        LockMode::AccessShare => 1,
        LockMode::RowShare => 2,
        LockMode::RowExclusive => 3,
        LockMode::ShareUpdateExclusive => 4,
        LockMode::Share => 5,
        LockMode::ShareRowExclusive => 6,
        LockMode::Exclusive => 7,
        LockMode::AccessExclusive => 8,
    }
}

/// Privilege bits, ANY of which permits LOCK TABLE in `mode` (commands/lockcmds.c
/// `LockTableAclCheck`): MAINTAIN, UPDATE, DELETE or TRUNCATE for every mode; SELECT additionally
/// for ACCESS SHARE; INSERT additionally up to ROW EXCLUSIVE.
fn lock_acl_mask(mode: LockMode) -> u32 {
    let level = lock_level(mode);
    let mut mask = acl_mode::MAINTAIN | acl_mode::UPDATE | acl_mode::DELETE | acl_mode::TRUNCATE;
    if level <= 1 {
        mask |= acl_mode::SELECT;
    }
    if level <= 3 {
        mask |= acl_mode::INSERT;
    }
    mask
}

/// Relation kinds LOCK TABLE accepts (lockcmds.c `RangeVarCallbackForLockTable`).
fn lockable_kind(kind: &RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::PartitionedTable | RelKind::View)
}

/// Plural noun used in the `This operation is not supported for ...` detail (PG `errdetail_relkind_not_supported`).
fn relkind_word(kind: &RelKind) -> &'static str {
    match kind {
        RelKind::Index => "indexes",
        RelKind::PartitionedIndex => "partitioned indexes",
        RelKind::Sequence => "sequences",
        RelKind::MatView => "materialized views",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::Toast => "TOAST tables",
        _ => "this kind of relation",
    }
}

/// One table to rebuild: its OID, name and the btree index OIDs to rebuild on it.
struct ReindexItem {
    table: Oid,
    name: String,
    indexes: Vec<Oid>,
}

/// Position of `table` in `items`, appending an empty item when it is not there yet.
fn item_position(items: &mut Vec<ReindexItem>, table: Oid, name: &str) -> usize {
    if let Some(pos) = items.iter().position(|i| i.table == table) {
        return pos;
    }
    items.push(ReindexItem { table, name: name.to_string(), indexes: Vec::new() });
    items.len() - 1
}

/// Resolve the REINDEX targets against the catalog snapshot into owned work items (no borrow of the
/// snapshot survives the call). Empty `targets` = every user table, in OID order. Indexes that are
/// not btree are not stored by the executor and are skipped when reached through their table; naming
/// one directly is 0A000.
fn reindex_plan(snap: &CatalogSnapshot, targets: &[Oid]) -> Result<Vec<ReindexItem>> {
    let mut items: Vec<ReindexItem> = Vec::new();
    if targets.is_empty() {
        let mut tables: Vec<(Oid, String)> = snap
            .user_relations()
            .filter(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
            .map(|r| (r.oid, r.name.clone()))
            .collect();
        tables.sort();
        for (oid, name) in tables {
            let pos = item_position(&mut items, oid, &name);
            for idx in snap.indexes_of(oid) {
                if idx.method == IndexMethod::Btree && !items[pos].indexes.contains(&idx.oid) {
                    items[pos].indexes.push(idx.oid);
                }
            }
        }
        return Ok(items);
    }
    for &oid in targets {
        match snap.class(oid) {
            None => return Err(error::relation_vanished(oid)),
            Some(ClassRef::Relation(rel_oid)) => {
                let rel = snap.relation(rel_oid).ok_or_else(|| error::relation_vanished(rel_oid))?;
                if !matches!(rel.kind, RelKind::Table | RelKind::MatView) {
                    return Err(PgError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a table or materialized view", rel.name),
                    ));
                }
                let pos = item_position(&mut items, rel_oid, &rel.name);
                for idx in snap.indexes_of(rel_oid) {
                    if idx.method == IndexMethod::Btree && !items[pos].indexes.contains(&idx.oid) {
                        items[pos].indexes.push(idx.oid);
                    }
                }
            }
            Some(ClassRef::Index(idx_oid)) => {
                let idx = snap.index(idx_oid).ok_or_else(|| error::relation_vanished(idx_oid))?;
                if idx.method != IndexMethod::Btree {
                    return Err(PgError::unimplemented(
                        "EXE-39",
                        format!("REINDEX of non-btree index \"{}\"", idx.name),
                    ));
                }
                let table = snap.relation(idx.table).ok_or_else(|| error::relation_vanished(idx.table))?;
                let pos = item_position(&mut items, table.oid, &table.name);
                if !items[pos].indexes.contains(&idx_oid) {
                    items[pos].indexes.push(idx_oid);
                }
            }
            Some(ClassRef::Sequence(seq_oid)) => {
                let name = snap.class_identity(seq_oid).map(|c| c.1.to_string()).unwrap_or_default();
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is not a table or materialized view"),
                ));
            }
            Some(ClassRef::SystemIndex(sys_oid)) => {
                let name = snap.class_identity(sys_oid).map(|c| c.1.to_string()).unwrap_or_default();
                return Err(PgError::unimplemented(
                    "EXE-39",
                    format!("REINDEX of system catalog index \"{name}\""),
                ));
            }
        }
    }
    Ok(items)
}

/// REINDEX: per item take ShareLock on the table (PG `reindex_relation`), then for each btree index
/// delete its entry data and rebuild it from the heap.
fn reindex(cx: &mut ExecContext, targets: &[Oid]) -> Result<ExecResult> {
    cx.txn.check_writable("REINDEX")?;
    let items = reindex_plan(cx.se.env.snapshot(), targets)?;
    let mut rt = Runtime::default();
    let mut ex = Executor::new(&mut rt, cx);
    for item in &items {
        ex.check_interrupts()?;
        if !ex.txn().lock_relation(item.table, CoreLockMode::Share, WaitPolicy::Block)? {
            return Err(error::relation_lock_not_available(&item.name));
        }
        let td = tabledesc::build_table_desc(ex.se(), item.table)?;
        for &index_oid in &item.indexes {
            let idx = td.index(index_oid).ok_or_else(|| error::relation_vanished(index_oid))?;
            rowstore::delete_relation_data(&mut ex, index_oid)?;
            index_maint::build_index(&mut ex, &td, idx)?;
        }
    }
    Ok(ExecResult::command("REINDEX"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    #[test]
    fn lock_mode_maps_one_to_one() {
        assert_eq!(core_lock_mode(LockMode::AccessShare), CoreLockMode::AccessShare);
        assert_eq!(core_lock_mode(LockMode::ShareRowExclusive), CoreLockMode::ShareRowExclusive);
        assert_eq!(core_lock_mode(LockMode::AccessExclusive), CoreLockMode::AccessExclusive);
    }

    #[test]
    fn lock_write_modes_are_above_row_exclusive() {
        assert!(lock_level(LockMode::RowExclusive) <= 3);
        assert!(lock_level(LockMode::ShareUpdateExclusive) > 3);
        assert!(lock_level(LockMode::AccessExclusive) > 3);
    }

    #[test]
    fn lock_acl_mask_follows_lockcmds() {
        let access_share = lock_acl_mask(LockMode::AccessShare);
        assert!(access_share & acl_mode::SELECT != 0);
        assert!(access_share & acl_mode::INSERT != 0);
        let row_exclusive = lock_acl_mask(LockMode::RowExclusive);
        assert!(row_exclusive & acl_mode::SELECT == 0);
        assert!(row_exclusive & acl_mode::INSERT != 0);
        let share = lock_acl_mask(LockMode::Share);
        assert!(share & (acl_mode::SELECT | acl_mode::INSERT) == 0);
        assert!(share & acl_mode::TRUNCATE != 0);
        assert!(share & acl_mode::MAINTAIN != 0);
    }

    #[test]
    fn maintenance_tags_without_work() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        assert_eq!(exec_maintenance(&mut cx, MaintenanceKind::Vacuum, &[]).unwrap().tag(), "VACUUM");
        assert_eq!(exec_maintenance(&mut cx, MaintenanceKind::Analyze, &[]).unwrap().tag(), "ANALYZE");
        assert_eq!(exec_maintenance(&mut cx, MaintenanceKind::Cluster, &[]).unwrap().tag(), "CLUSTER");
        assert_eq!(exec_maintenance(&mut cx, MaintenanceKind::Checkpoint, &[]).unwrap().tag(), "CHECKPOINT");
    }

    #[test]
    fn reindex_empty_catalog_is_tagged() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        assert_eq!(exec_maintenance(&mut cx, MaintenanceKind::Reindex, &[]).unwrap().tag(), "REINDEX");
    }

    #[test]
    fn reindex_unknown_oid_is_error() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        assert!(exec_maintenance(&mut cx, MaintenanceKind::Reindex, &[999_999]).is_err());
    }

    #[test]
    fn lock_unknown_oid_is_error() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let stmt = BoundLock { tables: vec![999_999], mode: LockMode::AccessShare, nowait: false };
        assert!(exec_lock(&mut cx, &stmt).is_err());
    }

    #[test]
    #[ignore = "needs EXE-42"]
    fn lock_catalog_table_is_tagged() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        // pg_class is OID 1259 and readable by everyone.
        let stmt = BoundLock { tables: vec![1259], mode: LockMode::AccessShare, nowait: true };
        assert_eq!(exec_lock(&mut cx, &stmt).unwrap().tag(), "LOCK TABLE");
    }

    #[test]
    fn do_runs_plpgsql() {
        let mut txn = MemTxn::new();
        let cell = Cell::new(1u64);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let d = BoundDo { language: "plpgsql".to_string(), code: "BEGIN NULL; END".to_string() };
        assert_eq!(exec_do(&mut cx, &d).map(|r| r.tag().to_string()).ok().as_deref(), Some("DO"));
        let bad = BoundDo { language: "plpgsql".to_string(), code: "BEGIN RAISE EXCEPTION 'boom'; END".to_string() };
        assert_eq!(exec_do(&mut cx, &bad).err().map(|e| e.message), Some("boom".to_string()));
    }
}
