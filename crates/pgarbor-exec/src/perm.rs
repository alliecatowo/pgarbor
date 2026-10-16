//! Privilege checks on data access (lease EXE-42). PG: `executor/execMain.c` (`ExecCheckPermissions`,
//! `ExecCheckOneRelPerms`), `utils/adt/acl.c`. Catalog primitives: `pgarbor_catalog::acl`
//! (`RoleGraph`, `effective_acl`, `privilege_mask`, `AclObject`) and `model::acl_mode`.
//!
//! The effective role is `se.env.current_user()`; superusers pass everything
//! (`RoleGraph::is_superuser`). A relation's ACL is `Relation::acl` (None = `acldefault(Table, owner)`),
//! a sequence's `Sequence::acl` (`AclObject::Sequence`), column ACLs `Column::acl`.
//!
//! ## Where it is called (the callers' docs state it too)
//! * `node_scan::build_seq_scan/build_index_scan`: `SELECT` on the table (`columns` = attnums whose
//!   `needed` flag is set; if the table-level privilege is missing PG falls back to requiring `SELECT` on
//!   EVERY referenced column);
//! * `modify::build_modify_table`: `INSERT` (columns = supplied attnums), `UPDATE` (columns = assigned
//!   attnums), `DELETE`; additionally `SELECT` on columns read by WHERE/RETURNING is checked by the SCAN
//!   nodes below it;
//! * `ddl_drop::exec_truncate`: `TRUNCATE`; `utility::exec_lock`: `UPDATE`/`DELETE`/`TRUNCATE` (any for
//!   `ACCESS EXCLUSIVE`) or `SELECT` for `ACCESS SHARE`; `copy::execute_copy`: `SELECT` (TO) / `INSERT` (FROM);
//! * `session_fns`: `nextval` needs `USAGE` or `UPDATE` on the sequence, `currval` `SELECT` or `USAGE`,
//!   `setval` `UPDATE`.
//! Foreign-key checks run with the privileges of the table OWNER (no check); catalog-owned virtual
//! relations (`oid < 16384`) are readable by everyone.
//! Failure: 42501 `permission denied for table t` (word `sequence`/`view` as appropriate: use
//! `pgarbor_catalog::errors::permission_denied_for(word, name)`); views: the view owner's privileges
//! apply to the underlying tables (security-definer semantics of plain views) - v1 checks the view itself
//! only because the binder expands views inline (known gap).
use pgarbor_catalog::acl::{effective_acl, privilege_mask, AclObject, RoleGraph};
use pgarbor_catalog::errors::permission_denied_for;
use pgarbor_catalog::model::acl_mode;
use pgarbor_catalog::model::RelKind;
use pgarbor_common::{Oid, PgError, Result, SqlState, FIRST_NORMAL_OID};

use crate::context::SessionEnv;

/// Object word used in 42501 messages for a relation kind (PG `get_relkind_objtype` + `ObjectTypeString`).
fn relation_word(kind: RelKind) -> &'static str {
    match kind {
        RelKind::View => "view",
        RelKind::MatView => "materialized view",
        RelKind::ForeignTable => "foreign table",
        RelKind::Sequence => "sequence",
        _ => "table",
    }
}

/// 42P01 for an OID that names no relation or sequence (should not happen after binding).
fn missing_relation(oid: Oid) -> PgError {
    PgError::new(SqlState::UNDEFINED_TABLE, format!("relation with OID {oid} does not exist"))
}

/// Require privilege bits `mode` (`acl_mode::*`, ANY of them) on relation `rel` for the current role;
/// `columns` = attnums the statement touches (checked per column when the table-level privilege is
/// missing; empty = whole-table only).
///
/// Order (PG `ExecCheckOneRelPerms`): superusers pass; catalog-owned relations pass for SELECT; the
/// table-level ACL (owner default, PUBLIC, role and inherited roles) is tested for ANY of `mode`; then,
/// if `mode` contains column-level bits (`acl_mode::ALL_COLUMN`) and column attnums were given, every
/// listed column must carry one of those bits in its own ACL. Otherwise the whole-table 42501 error.
pub fn require_relation_privilege(se: &SessionEnv, rel: Oid, mode: u32, columns: &[i16]) -> Result<()> {
    if mode == 0 {
        return Ok(());
    }
    // pg_catalog-style relations are readable by everyone (PG grants PUBLIC SELECT on them).
    if rel < FIRST_NORMAL_OID && mode & !acl_mode::SELECT == 0 {
        return Ok(());
    }
    let snap = se.env.snapshot();
    let user = se.env.current_user();
    let graph = RoleGraph::build(snap, se.env.database())?;
    if graph.is_superuser(user) {
        return Ok(());
    }
    if snap.relation(rel).is_none() && snap.sequence(rel).is_some() {
        // `SELECT * FROM seq`: the sequence's own ACL decides.
        return require_sequence_privilege(se, rel, mode);
    }
    let r = snap.relation(rel).ok_or_else(|| missing_relation(rel))?;
    let owner = r.owner;

    let acl = effective_acl(AclObject::Table, owner, r.acl.as_ref())?;
    if privilege_mask(&graph, user, owner, &acl, mode, false)? != 0 {
        return Ok(());
    }

    let col_mask = mode & acl_mode::ALL_COLUMN;
    if col_mask != 0 && !columns.is_empty() {
        for &attnum in columns {
            let col = r.columns.iter().find(|c| c.attnum == attnum).ok_or_else(|| {
                PgError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column number {attnum} of relation \"{}\" does not exist", r.name),
                )
            })?;
            let col_acl = effective_acl(AclObject::Column, owner, col.acl.as_ref())?;
            if privilege_mask(&graph, user, owner, &col_acl, col_mask, false)? == 0 {
                // PG `ExecCheckOneRelPerms`: a column-level miss is still reported as the table-level 42501.
                return Err(permission_denied_for(relation_word(r.kind), &r.name));
            }
        }
        return Ok(());
    }

    Err(permission_denied_for(relation_word(r.kind), &r.name))
}

/// Require ANY of the privilege bits `mode` on sequence `seq` (`USAGE | UPDATE` for nextval ...).
///
/// Superusers pass; otherwise the sequence's ACL (`acldefault(Sequence)` when `Sequence::acl` is None)
/// must grant one of `mode` to the role, directly, via PUBLIC or through inherited memberships.
pub fn require_sequence_privilege(se: &SessionEnv, seq: Oid, mode: u32) -> Result<()> {
    if mode == 0 {
        return Ok(());
    }
    let snap = se.env.snapshot();
    let user = se.env.current_user();
    let graph = RoleGraph::build(snap, se.env.database())?;
    if graph.is_superuser(user) {
        return Ok(());
    }
    let s = snap.sequence(seq).ok_or_else(|| missing_relation(seq))?;
    let acl = effective_acl(AclObject::Sequence, s.owner, s.acl.as_ref())?;
    if privilege_mask(&graph, user, s.owner, &acl, mode, false)? != 0 {
        return Ok(());
    }
    Err(permission_denied_for("sequence", &s.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices};
    use pgarbor_catalog::builtin::PUBLIC_NAMESPACE;
    use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateRoleSpec, CreateTableSpec, GrantSpec, GrantTarget, PrivilegeSet};
    use pgarbor_catalog::model::{ObjClass, ObjectAddress, Persistence};
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_common::MemKv;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use std::cell::Cell;

    const POSTGRES: Oid = 10;
    /// pg_class: a catalog-owned relation (below FIRST_NORMAL_OID).
    const PG_CLASS: Oid = 1259;

    /// Env whose current user is `user` (same snapshot and search path).
    fn as_user(env: &CatalogEnv, user: Oid) -> CatalogEnv {
        CatalogEnv::with_resolved(env.snapshot().clone(), env.path().clone(), user)
    }

    /// Two roles `alice` and `bob` plus table `t(a int, b int)` owned by postgres.
    fn setup() -> Result<(MemKv, CatalogEnv, Oid, Oid, Oid)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut alice: Oid = 0;
        let mut bob: Oid = 0;
        let (_, env) = testing::run(&mut kv, &env, |t| {
            alice = t.create_role(&CreateRoleSpec::new("alice"))?;
            bob = t.create_role(&CreateRoleSpec::new("bob"))?;
            Ok(())
        })?;
        let spec = CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: "t".to_string(),
            owner: POSTGRES,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::INT4)],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        };
        let mut table: Oid = 0;
        let (_, env) = testing::run(&mut kv, &env, |t| {
            let created = t
                .create_table(&spec)?
                .ok_or_else(|| PgError::internal("create_table returned no table"))?;
            table = created.table;
            Ok(())
        })?;
        Ok((kv, env, alice, bob, table))
    }

    /// GRANT (or REVOKE) `privs` on `table` to `grantees`, optionally on `columns` only.
    fn grant(
        kv: &mut MemKv,
        env: &CatalogEnv,
        table: Oid,
        privs: u32,
        grantees: Vec<Oid>,
        columns: Vec<i16>,
        revoke: bool,
    ) -> Result<CatalogEnv> {
        let spec = GrantSpec {
            target: GrantTarget::Object(ObjectAddress::new(ObjClass::Relation, table)),
            privileges: PrivilegeSet::Bits(privs),
            grantees,
            grant_option: false,
            grantor: POSTGRES,
            columns,
            cascade: false,
        };
        let (_, next) = testing::run(kv, env, |t| t.grant_object_privileges(&spec, revoke))?;
        Ok(next)
    }

    fn check_rel(env: &CatalogEnv, rel: Oid, mode: u32, columns: &[i16]) -> Result<()> {
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env.clone(), IoConfig::default(), &guc, &services, &rng, 0, 0);
        require_relation_privilege(&se, rel, mode, columns)
    }

    fn check_seq(env: &CatalogEnv, seq: Oid, mode: u32) -> Result<()> {
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env.clone(), IoConfig::default(), &guc, &services, &rng, 0, 0);
        require_sequence_privilege(&se, seq, mode)
    }

    #[test]
    fn role_without_privileges_is_denied() -> Result<()> {
        let (_kv, env, alice, _bob, t) = setup()?;
        let alice_env = as_user(&env, alice);
        let e = check_rel(&alice_env, t, acl_mode::SELECT, &[]).expect_err("select must be denied");
        assert_eq!(e.sqlstate, SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(e.message, "permission denied for table t");
        let e = check_rel(&alice_env, t, acl_mode::INSERT, &[]).expect_err("insert must be denied");
        assert_eq!(e.sqlstate, SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(e.message, "permission denied for table t");
        Ok(())
    }

    #[test]
    fn grant_select_allows_select_but_not_insert() -> Result<()> {
        let (mut kv, env, alice, _bob, t) = setup()?;
        let env = grant(&mut kv, &env, t, acl_mode::SELECT, vec![alice], Vec::new(), false)?;
        let alice_env = as_user(&env, alice);
        check_rel(&alice_env, t, acl_mode::SELECT, &[])?;
        let e = check_rel(&alice_env, t, acl_mode::INSERT, &[]).expect_err("insert must still be denied");
        assert_eq!(e.message, "permission denied for table t");
        Ok(())
    }

    #[test]
    fn revoke_removes_granted_select() -> Result<()> {
        let (mut kv, env, alice, _bob, t) = setup()?;
        let env = grant(&mut kv, &env, t, acl_mode::SELECT, vec![alice], Vec::new(), false)?;
        let env = grant(&mut kv, &env, t, acl_mode::SELECT, vec![alice], Vec::new(), true)?;
        let alice_env = as_user(&env, alice);
        assert!(check_rel(&alice_env, t, acl_mode::SELECT, &[]).is_err());
        Ok(())
    }

    #[test]
    fn column_grant_allows_only_granted_column() -> Result<()> {
        let (mut kv, env, alice, _bob, t) = setup()?;
        // attnum 1 = a, attnum 2 = b
        let env = grant(&mut kv, &env, t, acl_mode::SELECT, vec![alice], vec![1], false)?;
        let alice_env = as_user(&env, alice);
        check_rel(&alice_env, t, acl_mode::SELECT, &[1])?;
        let e = check_rel(&alice_env, t, acl_mode::SELECT, &[2]).expect_err("column b must be denied");
        assert_eq!(e.sqlstate, SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(e.message, "permission denied for table t");
        // Whole-table access needs the table-level privilege.
        assert!(check_rel(&alice_env, t, acl_mode::SELECT, &[]).is_err());
        Ok(())
    }

    #[test]
    fn owner_and_superuser_pass() -> Result<()> {
        let (_kv, env, alice, _bob, t) = setup()?;
        // The owner (postgres, the bootstrap superuser) passes every table-level check.
        check_rel(&env, t, acl_mode::INSERT | acl_mode::SELECT | acl_mode::DELETE, &[])?;
        // A non-superuser who is not the owner does not.
        assert!(check_rel(&as_user(&env, alice), t, acl_mode::DELETE, &[]).is_err());
        Ok(())
    }

    #[test]
    fn catalog_relations_are_readable_by_everyone() -> Result<()> {
        let (_kv, env, alice, _bob, _t) = setup()?;
        check_rel(&as_user(&env, alice), PG_CLASS, acl_mode::SELECT, &[])?;
        Ok(())
    }

    #[test]
    fn public_grant_applies_to_every_role() -> Result<()> {
        let (mut kv, env, alice, bob, t) = setup()?;
        // grantee 0 = PUBLIC
        let env = grant(&mut kv, &env, t, acl_mode::SELECT, vec![0], Vec::new(), false)?;
        check_rel(&as_user(&env, alice), t, acl_mode::SELECT, &[])?;
        check_rel(&as_user(&env, bob), t, acl_mode::SELECT, &[])?;
        Ok(())
    }

    #[test]
    fn missing_sequence_is_an_error_not_a_pass() -> Result<()> {
        // The fixture creates no sequence (CREATE SEQUENCE is outside this test); OID 1 must be an error.
        let (_kv, env, alice, _bob, _t) = setup()?;
        assert!(check_seq(&as_user(&env, alice), 1, acl_mode::USAGE).is_err());
        Ok(())
    }
}
