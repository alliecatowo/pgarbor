//! DROP, TRUNCATE, DROP OWNED, REASSIGN OWNED, DROP DATABASE (lease EXE-37). Catalog calls:
//! `pgarbor_catalog::ddl::drop_stmt` (`drop_by_names`), `ddl::comment` (`drop_owned`, `reassign_owned`).
//!
//! * [`exec_drop`]: ONE `run_ddl` with `ddl.drop_by_names(kind, &targets, if_exists, behavior)` where
//!   `behavior = DropBehavior::Cascade` if `cascade` else `Restrict`. The outcome's `DropRelationData`
//!   follow-ups remove the heap/index data (`rowstore::delete_relation_data`), `DropSequenceState` the
//!   sequence state. 2BP01 errors (`cannot drop table t because other objects depend on it`) and
//!   `drop cascades to ...` notices come from the catalog. Tag `DROP <KIND>` (`BoundStatement::tag()`).
//! * [`exec_truncate`] (`BoundTruncate{tables, restart_identity, cascade}`):
//!   1. require the `TRUNCATE` privilege on each table (`perm::require_relation_privilege`), lock every table `AccessExclusive`;
//!   2. refuse a table referenced by a foreign key from a table NOT in the list unless `cascade`:
//!      0A000 `cannot truncate a table referenced in a foreign key constraint` DETAIL `Table "c"
//!      references "p".` HINT `Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.`;
//!      with `cascade` also truncate the referencing tables and emit NOTICE `truncate cascades to table
//!      "c"`;
//!   3. for each table delete all heap rows and all index entries (`delete_relation_data` of the table
//!      and of each of its indexes - transactional via the overlay, so ROLLBACK restores them);
//!   4. `restart_identity`: for each identity/serial sequence OWNED by the table's columns restart it at
//!      its start value (`seq_restart`) - via a `run_ddl` + `alter_sequence(RESTART)`, or directly
//!      through `ExecTxn::seq_restart` for owned sequences (ALTER SEQUENCE ... RESTART semantics).
//!   Tag `TRUNCATE TABLE`.
//! * [`exec_drop_owned`] / [`exec_reassign_owned`]: `drop_owned(roles, behavior)`, `reassign_owned(old,
//!   new)` (tags `DROP OWNED`, `REASSIGN OWNED`) followed by follow-ups.
//! * [`exec_drop_database`]: 0A000 `DROP DATABASE` (single database per branch) except for `IF EXISTS`
//!   on a database that does not exist -> NOTICE `database "x" does not exist, skipping`.
use pgarbor_catalog::ddl::drop_stmt::DropKind;
use pgarbor_catalog::model::acl_mode;
use pgarbor_catalog::{CatalogSnapshot, DropBehavior};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_sql::ir::{BoundDrop, BoundTruncate};

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::ddl_followup::run_followups;
use crate::error::{relation_lock_not_available, relation_vanished};
use crate::perm;
use crate::result::ExecResult;
use crate::rowstore::delete_relation_data;
use crate::runtime::{Executor, Runtime};

/// NOTICE 00000 (PG `ereport(NOTICE, ...)`).
fn notice(message: String) -> PgError {
    PgError::new(SqlState::SUCCESSFUL_COMPLETION, message).with_severity(Severity::Notice)
}

/// `DROP <KIND>` command tag (`BoundStatement::tag()` of a `BoundDrop`).
fn drop_tag(kind: DropKind) -> String {
    let word = match kind {
        DropKind::Table => "TABLE",
        DropKind::View => "VIEW",
        DropKind::MaterializedView => "MATERIALIZED VIEW",
        DropKind::Index => "INDEX",
        DropKind::Sequence => "SEQUENCE",
        DropKind::ForeignTable => "FOREIGN TABLE",
        DropKind::Type => "TYPE",
        DropKind::Domain => "DOMAIN",
        DropKind::Schema => "SCHEMA",
        DropKind::Function => "FUNCTION",
        DropKind::Procedure => "PROCEDURE",
        DropKind::Routine => "ROUTINE",
        DropKind::Aggregate => "AGGREGATE",
        DropKind::Role => "ROLE",
    };
    format!("DROP {word}")
}

fn behavior_of(cascade: bool) -> DropBehavior {
    if cascade {
        DropBehavior::Cascade
    } else {
        DropBehavior::Restrict
    }
}

/// Unqualified relation name, or XX000 `cache lookup failed for relation N` when the OID is unknown.
fn relation_name(snap: &CatalogSnapshot, oid: Oid) -> Result<String> {
    match snap.relation(oid) {
        Some(rel) => Ok(rel.name.clone()),
        None => Err(relation_vanished(oid)),
    }
}

/// 0A000 raised by PG `heap_truncate_check_FKs` when a referencing table is not truncated too.
fn truncate_fk_error(child: &str, parent: &str) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot truncate a table referenced in a foreign key constraint")
        .with_detail(format!("Table \"{child}\" references \"{parent}\"."))
        .with_hint(format!("Truncate table \"{child}\" at the same time, or use TRUNCATE ... CASCADE."))
}

/// Non-CASCADE check: every table referencing one of `rels` must itself be in `rels`.
fn check_fk_restrict(snap: &CatalogSnapshot, rels: &[Oid]) -> Result<()> {
    for &parent in rels {
        for con in snap.foreign_keys_referencing(parent) {
            if !rels.contains(&con.table) {
                let child = relation_name(snap, con.table)?;
                let parent_name = relation_name(snap, parent)?;
                return Err(truncate_fk_error(&child, &parent_name));
            }
        }
    }
    Ok(())
}

/// Lock one table for TRUNCATE (`AccessExclusive`, waiting for holders like PG's `table_open`).
fn lock_for_truncate(cx: &mut ExecContext, name: &str, oid: Oid) -> Result<()> {
    if !cx.txn.lock_relation(oid, LockMode::AccessExclusive, WaitPolicy::Block)? {
        return Err(relation_lock_not_available(name));
    }
    Ok(())
}

/// `TRUNCATE` privilege on one table for the current role (PG `truncate_check_perms`).
fn truncate_perm(cx: &ExecContext, oid: Oid) -> Result<()> {
    perm::require_relation_privilege(&cx.se, oid, acl_mode::TRUNCATE, &[])
}

pub fn exec_drop(cx: &mut ExecContext, d: &BoundDrop) -> Result<ExecResult> {
    let tag = drop_tag(d.kind);
    cx.txn.check_writable(&tag)?;
    let behavior = behavior_of(d.cascade);
    let (_, outcome) = run_ddl(cx, |ddl| ddl.drop_by_names(d.kind, &d.targets, d.if_exists, behavior))?;
    run_followups(cx, &outcome.follow_ups, None, &[])?;
    Ok(ExecResult::command(tag))
}

pub fn exec_truncate(cx: &mut ExecContext, t: &BoundTruncate) -> Result<ExecResult> {
    const TAG: &str = "TRUNCATE TABLE";
    cx.txn.check_writable(TAG)?;
    if t.tables.is_empty() {
        return Ok(ExecResult::command(TAG));
    }
    let env = cx.se.env.clone();
    let snap = env.snapshot();

    // 1. Explicit tables: resolve, lock AccessExclusive, then check the TRUNCATE privilege.
    let mut rels: Vec<Oid> = Vec::with_capacity(t.tables.len());
    for &oid in &t.tables {
        if rels.contains(&oid) {
            continue;
        }
        let name = relation_name(snap, oid)?;
        lock_for_truncate(cx, &name, oid)?;
        truncate_perm(cx, oid)?;
        rels.push(oid);
    }

    // 2. Foreign keys. CASCADE adds referencing tables round by round (PG `heap_truncate_find_FKs`),
    //    each with a NOTICE, a lock and a privilege check; RESTRICT refuses instead.
    if t.cascade {
        loop {
            let mut found: Vec<Oid> = Vec::new();
            for &parent in &rels {
                for con in snap.foreign_keys_referencing(parent) {
                    if !rels.contains(&con.table) && !found.contains(&con.table) {
                        found.push(con.table);
                    }
                }
            }
            if found.is_empty() {
                break;
            }
            for child in found {
                let name = relation_name(snap, child)?;
                lock_for_truncate(cx, &name, child)?;
                cx.notice(notice(format!("truncate cascades to table \"{name}\"")));
                truncate_perm(cx, child)?;
                rels.push(child);
            }
        }
    } else {
        check_fk_restrict(snap, &rels)?;
    }

    // 3. RESTART IDENTITY: every sequence owned by a column of a truncated table goes back to its
    //    START value (PG `ResetSequence`; `ExecTxn::seq_restart` sets the next nextval result).
    if t.restart_identity {
        for &oid in &rels {
            if let Some(rel) = snap.relation(oid) {
                for col in &rel.columns {
                    for seq in snap.sequences_owned_by(oid, col.attnum) {
                        cx.txn.seq_restart(seq.oid, seq.start)?;
                    }
                }
            }
        }
    }

    // 4. Delete every heap row and every index entry through the transaction view (ROLLBACK restores).
    {
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut *cx);
        for &oid in &rels {
            delete_relation_data(&mut ex, oid)?;
            for idx in snap.indexes_of(oid) {
                delete_relation_data(&mut ex, idx.oid)?;
            }
        }
    }
    Ok(ExecResult::command(TAG))
}

pub fn exec_drop_owned(cx: &mut ExecContext, roles: &[Oid], behavior: DropBehavior) -> Result<ExecResult> {
    cx.txn.check_writable("DROP OWNED")?;
    let (_, outcome) = run_ddl(cx, |ddl| ddl.drop_owned(roles, behavior))?;
    run_followups(cx, &outcome.follow_ups, None, &[])?;
    Ok(ExecResult::command("DROP OWNED"))
}

pub fn exec_reassign_owned(cx: &mut ExecContext, old_roles: &[Oid], new_owner: Oid) -> Result<ExecResult> {
    cx.txn.check_writable("REASSIGN OWNED")?;
    let (_, outcome) = run_ddl(cx, |ddl| ddl.reassign_owned(old_roles, new_owner))?;
    run_followups(cx, &outcome.follow_ups, None, &[])?;
    Ok(ExecResult::command("REASSIGN OWNED"))
}

pub fn exec_drop_database(cx: &mut ExecContext, name: &str, if_exists: bool, force: bool) -> Result<ExecResult> {
    // FORCE only matters for terminating other sessions of a database that can be dropped; a branch
    // hosts one database, which is either the current one (refused below) or does not exist.
    let _ = force;
    cx.txn.check_writable("DROP DATABASE")?;
    let env = cx.se.env.clone();
    match env.snapshot().database_by_name(name) {
        None => {
            if if_exists {
                cx.notice(notice(format!("database \"{name}\" does not exist, skipping")));
                Ok(ExecResult::command("DROP DATABASE"))
            } else {
                Err(PgError::new(SqlState::INVALID_CATALOG_NAME, format!("database \"{name}\" does not exist")))
            }
        }
        Some(db) => {
            if db.oid == env.database() {
                Err(PgError::new(SqlState::OBJECT_IN_USE, "cannot drop the currently open database"))
            } else {
                Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "DROP DATABASE is not supported: a PGArbor branch hosts a single database",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use pgarbor_types::context::IoConfig;
    use std::cell::Cell;

    #[test]
    fn drop_tags_and_behavior() {
        assert_eq!(drop_tag(DropKind::Table), "DROP TABLE");
        assert_eq!(drop_tag(DropKind::MaterializedView), "DROP MATERIALIZED VIEW");
        assert_eq!(drop_tag(DropKind::Role), "DROP ROLE");
        assert_eq!(behavior_of(true), DropBehavior::Cascade);
        assert_eq!(behavior_of(false), DropBehavior::Restrict);
    }

    #[test]
    fn truncate_fk_error_matches_pg_wording() {
        let e = truncate_fk_error("c", "p");
        assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "cannot truncate a table referenced in a foreign key constraint");
        assert_eq!(e.detail.as_deref(), Some("Table \"c\" references \"p\"."));
        assert_eq!(e.hint.as_deref(), Some("Truncate table \"c\" at the same time, or use TRUNCATE ... CASCADE."));
    }

    #[test]
    fn drop_database_if_exists_on_missing_is_notice() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrapped catalog");
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let r = exec_drop_database(&mut cx, "no_such_db", true, false).expect("IF EXISTS skip");
        assert_eq!(r.tag(), "DROP DATABASE");
        let notes = cx.se.take_notices();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].message, "database \"no_such_db\" does not exist, skipping");
        assert!(exec_drop_database(&mut cx, "no_such_db", false, false).is_err());
    }

    #[test]
    fn truncate_unknown_relation_is_internal_error() {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrapped catalog");
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let t = BoundTruncate { tables: vec![999_999_999], restart_identity: false, cascade: false };
        assert!(exec_truncate(&mut cx, &t).is_err());
    }
}
