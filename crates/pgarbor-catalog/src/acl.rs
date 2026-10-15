//! Privileges: default ACLs, ACL algebra, role membership and privilege checks. Lease CAT-22
//! (sonnet: the grant-option and cascade rules are subtle). PG sources: `utils/adt/acl.c`
//! (`acldefault`, `aclupdate`, `aclmerge`, `aclmask`, `has_privs_of_role`, `is_member_of_role`,
//! `roles_is_member_of`, `select_best_grantor`, `check_can_set_role`), `catalog/aclchk.c`
//! (`ExecGrant_*`, `restrict_and_check_grant`, `merge_acl_with_grant`), `include/utils/acl.h`.
//!
//! Data model (see `model::common`): [`Acl`] = `Vec<AclItem>`; `None` in an object's `acl` field
//! means "default privileges" ([`acldefault`]). A grant that makes the ACL equal the default
//! stores `None` again (PG canonicalizes via `aclequal` against `acldefault`).
//!
//! ## Defaults (`acldefault(objtype, owner)`; oracle `select acldefault('r', 10)` prints
//! `{postgres=arwdDxtm/postgres}`, 's' sequence `{postgres=rwU/postgres}`, 'd' database
//! `{=Tc/postgres,postgres=CTc/postgres}`, 'f' `{=X/postgres,postgres=X/postgres}`, 'l'
//! `{=U/postgres,postgres=U/postgres}`, 'n' schema `{postgres=UC/postgres}`, 'T' type
//! `{=U/postgres,postgres=U/postgres}`, 'c' column `{}`; note PUBLIC items come FIRST)
//! | kind | owner gets | PUBLIC gets |
//! |---|---|---|
//! | Table/View/Matview | `arwdDxtm` | - |
//! | Sequence | `rwU` | - |
//! | Column | (none: column ACL starts empty) | - |
//! | Database | `CTc` | `Tc` |
//! | Function/Procedure | `X` | `X` |
//! | Language | `U` | `U` (trusted) |
//! | Schema | `UC` | - |
//! | Type/Domain | `U` | `U` |
//! | Tablespace | `C` | - |
//! Owner items have grantor = owner and carry NO grant-option bits (ownership implies the right to
//! grant); follow `acldefault` exactly: items without `*`, PUBLIC items before the owner item.
//!
//! ## Semantics to implement
//! - [`grant_privileges`] / [`revoke_privileges`]: PG `ExecGrant` core for ONE object. Start from
//!   the object's ACL or the default; for each grantee: GRANT merges privilege bits (and grant
//!   options when `grant_option`); REVOKE clears bits (or only the grant-option bits for
//!   `GRANT OPTION FOR`); REVOKE ... CASCADE also revokes privileges those grantees granted onward
//!   (`recursive_revoke`); RESTRICT with dependent grants -> 2BP01 `dependent privileges exist`
//!   HINT `Use CASCADE to revoke them too.`. Grantor selection (`select_best_grantor`): the
//!   requested grantor must be the owner or hold grant options; a non-owner without grant option ->
//!   42501 `permission denied for table t`; if the caller holds only SOME of the requested privileges
//!   PG grants those and emits WARNING 01007 `not all privileges were granted for "t"` (all missing:
//!   WARNING `no privileges were granted for "t"`; REVOKE: `no privileges could be revoked for "t"` /
//!   `not all privileges could be revoked for "t"`).
//! - [`aclnewowner`]: ownership transfer rewrites items whose grantee/grantor was the old owner to the
//!   new owner (PG `aclnewowner`), merging duplicates.
//! - Role membership: [`RoleGraph`] precomputes memberships from the snapshot.
//!   `has_privs_of_role(member, role)`: member == role, or member is superuser, or reachable through
//!   `RoleMembership` edges where `inherit_option` is true on EVERY hop (PG 16+); the
//!   `pg_database_owner` pseudo-role is implicitly held by the owner of the current database.
//!   `is_member_of_role(member, role)`: reachability through any edges regardless of inherit.
//!   `member_can_set_role`: reachability through edges with `set_option` on every hop.
//! - [`privilege_mask`] / [`has_privilege`]: PG `aclmask` - the union of privileges granted to
//!   PUBLIC, to `role`, and to every role `role` inherits from (`has_privs_of_role`), plus
//!   ownership (owner and members-with-privs-of-owner get ALL applicable privileges) and superuser
//!   (all). `has_grant_option` variant for `has_*_privilege(..., 'SELECT WITH GRANT OPTION')`.
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use std::collections::{BTreeSet, HashMap};

/// Object kind for ACL defaults and applicable-privilege masks (PG `ObjectType` subset).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AclObject {
    Table,
    Sequence,
    Column,
    Database,
    Function,
    Language,
    Schema,
    Type,
    Tablespace,
}

impl AclObject {
    /// Privileges applicable to the kind (`ACL_ALL_RIGHTS_*`): `acl_mode::ALL_RELATION` for tables,
    /// `ALL_SEQUENCE`, `ALL_COLUMN`, `ALL_DATABASE`, `ALL_FUNCTION`, `ALL_LANGUAGE`, `ALL_SCHEMA`,
    /// `ALL_TYPE`, tablespace `CREATE`.
    pub fn all_privileges(self) -> u32 {
        use acl_mode::*;
        match self {
            AclObject::Table => ALL_RELATION,
            AclObject::Sequence => ALL_SEQUENCE,
            AclObject::Column => ALL_COLUMN,
            AclObject::Database => ALL_DATABASE,
            AclObject::Function => ALL_FUNCTION,
            AclObject::Language => ALL_LANGUAGE,
            AclObject::Schema => ALL_SCHEMA,
            AclObject::Type => ALL_TYPE,
            AclObject::Tablespace => CREATE,
        }
    }
    /// Words for error messages (`permission denied for table t`): `table`, `sequence`, `column`,
    /// `database`, `function`, `language`, `schema`, `type`, `tablespace`.
    pub fn word(self) -> &'static str {
        match self {
            AclObject::Table => "table",
            AclObject::Sequence => "sequence",
            AclObject::Column => "column",
            AclObject::Database => "database",
            AclObject::Function => "function",
            AclObject::Language => "language",
            AclObject::Schema => "schema",
            AclObject::Type => "type",
            AclObject::Tablespace => "tablespace",
        }
    }
}

/// PG `acldefault` (module docs table).
// CONTRACT
pub fn acldefault(kind: AclObject, owner: Oid) -> Result<Acl> {
    use acl_mode::*;
    // (PUBLIC privileges, owner privileges); PUBLIC item first, as PG `acldefault` builds it.
    let (public, owner_privs): (u32, u32) = match kind {
        AclObject::Table => (0, ALL_RELATION),
        AclObject::Sequence => (0, ALL_SEQUENCE),
        AclObject::Column => (0, 0),
        AclObject::Database => (CREATE_TEMP | CONNECT, ALL_DATABASE),
        AclObject::Function => (EXECUTE, ALL_FUNCTION),
        AclObject::Language => (USAGE, ALL_LANGUAGE),
        AclObject::Schema => (0, ALL_SCHEMA),
        AclObject::Type => (USAGE, ALL_TYPE),
        AclObject::Tablespace => (0, CREATE),
    };
    let mut acl: Acl = Vec::with_capacity(2);
    if public != 0 {
        acl.push(AclItem { grantee: 0, grantor: owner, privileges: public, grant_options: 0 });
    }
    if owner_privs != 0 {
        acl.push(AclItem { grantee: owner, grantor: owner, privileges: owner_privs, grant_options: 0 });
    }
    Ok(acl)
}

/// The object's effective ACL: `acl` or its default.
pub fn effective_acl(kind: AclObject, owner: Oid, acl: Option<&Acl>) -> Result<Acl> {
    match acl {
        Some(a) => Ok(a.clone()),
        None => acldefault(kind, owner),
    }
}

/// Outcome of a grant/revoke on one object.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AclChange {
    /// New ACL to store (`None` = equals default).
    pub acl: Option<Acl>,
    /// WARNINGs to emit (`no privileges were granted for "t"`, ...), SQLSTATE 01007.
    pub warnings: Vec<PgError>,
}

/// Input to [`grant_privileges`] / [`revoke_privileges`] for one object.
// CONTRACT
#[derive(Clone, Debug)]
pub struct AclRequest<'a> {
    pub snapshot: &'a CatalogSnapshot,
    pub kind: AclObject,
    /// Name used in messages/warnings (`t`, `s.t`).
    pub object_name: &'a str,
    pub owner: Oid,
    /// Current stored ACL (None = default).
    pub current: Option<&'a Acl>,
    /// Role running the statement (permission to grant is checked against it).
    pub caller: Oid,
    /// Requested grantor (`GRANTED BY`), usually = caller.
    pub grantor: Oid,
    /// `acl_mode` bits (already expanded from ALL).
    pub privileges: u32,
    /// Grantees; `0` = PUBLIC.
    pub grantees: &'a [Oid],
    /// GRANT: `WITH GRANT OPTION`; REVOKE: `GRANT OPTION FOR`.
    pub grant_option: bool,
    /// REVOKE ... CASCADE.
    pub cascade: bool,
}

/// OID of the single SQL database (`postgres`) whose owner holds `pg_database_owner`.
const DEFAULT_DATABASE_OID: Oid = 5;
/// OID of the `pg_database_owner` pseudo-role.
const PG_DATABASE_OWNER_OID: Oid = 6171;

/// SQLSTATE 0LP01 `invalid_grant_operation`.
const INVALID_GRANT_OPERATION: SqlState = SqlState(*b"0LP01");
/// SQLSTATE 01007 `privilege_not_granted`.
const WARNING_PRIVILEGE_NOT_GRANTED: SqlState = SqlState(*b"01007");
/// SQLSTATE 01006 `privilege_not_revoked`.
const WARNING_PRIVILEGE_NOT_REVOKED: SqlState = SqlState(*b"01006");

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeChg {
    Add,
    Del,
}

/// PG `aclupdate`: apply `m` (rights = `privileges`, grant options = `grant_options`) to a copy of
/// `acl`. A new item is appended at the end; items left with no rights are removed.
fn aclupdate(graph: &RoleGraph, acl: &Acl, m: &AclItem, chg: ModeChg, owner: Oid, cascade: bool) -> Result<Acl> {
    if chg != ModeChg::Del && m.grant_options != 0 {
        check_circularity(graph, acl, m, owner)?;
    }
    let mut new_acl: Acl = acl.clone();
    let idx = match new_acl.iter().position(|a| a.grantee == m.grantee && a.grantor == m.grantor) {
        Some(i) => i,
        None => {
            new_acl.push(AclItem { grantee: m.grantee, grantor: m.grantor, privileges: 0, grant_options: 0 });
            new_acl.len() - 1
        }
    };
    let old_go = new_acl[idx].grant_options;
    {
        let it = &mut new_acl[idx];
        match chg {
            ModeChg::Add => {
                it.privileges |= m.privileges;
                it.grant_options |= m.grant_options;
            }
            ModeChg::Del => {
                it.privileges &= !m.privileges;
                it.grant_options &= !m.grant_options;
            }
        }
    }
    let new_go = new_acl[idx].grant_options;
    if new_acl[idx].privileges == 0 && new_go == 0 {
        new_acl.remove(idx);
    }
    // Remove abandoned privileges (cascading revoke).
    if old_go & !new_go != 0 {
        return recursive_revoke(graph, new_acl, m.grantee, old_go & !new_go, owner, cascade);
    }
    Ok(new_acl)
}

/// PG `recursive_revoke`: remove what `grantee` granted onward using the lost grant options.
fn recursive_revoke(
    graph: &RoleGraph,
    mut acl: Acl,
    grantee: Oid,
    mut revoke_privs: u32,
    owner: Oid,
    cascade: bool,
) -> Result<Acl> {
    // The owner can never truly lose grant options.
    if grantee == owner {
        return Ok(acl);
    }
    // The grantee might still have some grant options via another grantor.
    let still_has = privilege_mask(graph, grantee, owner, &acl, revoke_privs, true)?;
    revoke_privs &= !still_has;
    if revoke_privs == 0 {
        return Ok(acl);
    }
    loop {
        let found = acl.iter().find(|a| a.grantor == grantee && a.privileges & revoke_privs != 0).copied();
        let Some(item) = found else { return Ok(acl) };
        if !cascade {
            return Err(PgError::new(SqlState::DEPENDENT_OBJECTS_STILL_EXIST, "dependent privileges exist")
                .with_hint("Use CASCADE to revoke them too."));
        }
        let m = AclItem { grantee: item.grantee, grantor: grantee, privileges: revoke_privs, grant_options: revoke_privs };
        acl = aclupdate(graph, &acl, &m, ModeChg::Del, owner, cascade)?;
    }
}

/// PG `check_circularity`: refuse grant options granted back to the grantor's own source.
fn check_circularity(graph: &RoleGraph, acl: &Acl, m: &AclItem, owner: Oid) -> Result<()> {
    // The owner always has grant options.
    if m.grantor == owner {
        return Ok(());
    }
    let mut work: Acl = acl.clone();
    // Zap all grant options of the target grantee, plus what depends on them.
    loop {
        let found = work.iter().find(|a| a.grantee == m.grantee && a.grant_options != 0).copied();
        let Some(item) = found else { break };
        work = aclupdate(graph, &work, &item, ModeChg::Del, owner, true)?;
    }
    let own = privilege_mask(graph, m.grantor, owner, &work, m.grant_options, true)?;
    if m.grant_options & !own != 0 {
        return Err(PgError::new(
            INVALID_GRANT_OPERATION,
            "grant options cannot be granted back to your own grantor",
        ));
    }
    Ok(())
}

/// Grant options `role` holds directly (no membership), as PG `aclmask_direct` for grant options.
fn goptions_direct(acl: &Acl, role: Oid, owner: Oid, mask: u32) -> u32 {
    if role == owner {
        return mask;
    }
    let mut r = 0u32;
    for it in acl {
        if it.grantee == role || it.grantee == 0 {
            r |= it.grant_options & mask;
        }
    }
    r
}

/// PG `select_best_grantor`: returns (grantor, grant options available for `privileges`).
fn select_best_grantor(graph: &RoleGraph, role: Oid, privileges: u32, acl: &Acl, owner: Oid) -> (Oid, u32) {
    if role == owner || graph.is_superuser(role) {
        return (owner, privileges);
    }
    let mut grantor = role;
    let mut options = 0u32;
    let mut nrights = 0u32;
    for other in graph.closure(role, Recurse::Privs) {
        let p = goptions_direct(acl, other, owner, privileges);
        if p == privileges {
            return (other, p);
        }
        if p != 0 {
            let n = p.count_ones();
            if n > nrights {
                grantor = other;
                options = p;
                nrights = n;
            }
        }
    }
    (grantor, options)
}

/// PG `aclequal`: same items regardless of order.
fn acl_equal(a: &Acl, b: &Acl) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x)) && b.iter().all(|x| a.contains(x))
}

fn warning(state: SqlState, msg: String) -> PgError {
    PgError::new(state, msg).with_severity(Severity::Warning)
}

/// Shared ExecGrant core (`select_best_grantor` + `restrict_and_check_grant` +
/// `merge_acl_with_grant`) for one object.
fn exec_grant(req: &AclRequest, is_grant: bool) -> Result<AclChange> {
    if req.grantor != req.caller {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "grantor must be current user"));
    }
    let graph = RoleGraph::build(req.snapshot, DEFAULT_DATABASE_OID)?;
    let old_acl = effective_acl(req.kind, req.owner, req.current)?;

    let (grantor, avail_goptions) = select_best_grantor(&graph, req.caller, req.privileges, &old_acl, req.owner);

    // With no grant options, having any privilege at all on the object gets you by.
    if avail_goptions == 0 {
        let whole = req.kind.all_privileges();
        let any = privilege_mask(&graph, grantor, req.owner, &old_acl, whole, false)?
            | privilege_mask(&graph, grantor, req.owner, &old_acl, whole, true)?;
        if any == 0 {
            return Err(crate::errors::permission_denied_for(req.kind.word(), req.object_name));
        }
    }

    let this_privileges = req.privileges & avail_goptions;
    let all_privs = req.privileges == req.kind.all_privileges();
    let mut warnings = Vec::new();
    let name = req.object_name;
    if is_grant {
        if this_privileges == 0 {
            warnings.push(warning(WARNING_PRIVILEGE_NOT_GRANTED, format!("no privileges were granted for \"{name}\"")));
        } else if !all_privs && this_privileges != req.privileges {
            warnings
                .push(warning(WARNING_PRIVILEGE_NOT_GRANTED, format!("not all privileges were granted for \"{name}\"")));
        }
    } else if this_privileges == 0 {
        warnings.push(warning(
            WARNING_PRIVILEGE_NOT_REVOKED,
            format!("no privileges could be revoked for \"{name}\""),
        ));
    } else if !all_privs && this_privileges != req.privileges {
        warnings.push(warning(
            WARNING_PRIVILEGE_NOT_REVOKED,
            format!("not all privileges could be revoked for \"{name}\""),
        ));
    }

    // merge_acl_with_grant
    let chg = if is_grant { ModeChg::Add } else { ModeChg::Del };
    let mut acl = old_acl;
    for &grantee in req.grantees {
        if is_grant && req.grant_option && grantee == 0 {
            return Err(PgError::new(INVALID_GRANT_OPERATION, "grant options can only be granted to roles"));
        }
        // GRANT: privilege (+ option with WITH GRANT OPTION). REVOKE: both, or only the option.
        let m = AclItem {
            grantee,
            grantor,
            privileges: if is_grant || !req.grant_option { this_privileges } else { 0 },
            grant_options: if !is_grant || req.grant_option { this_privileges } else { 0 },
        };
        acl = aclupdate(&graph, &acl, &m, chg, req.owner, req.cascade)?;
    }

    let default = acldefault(req.kind, req.owner)?;
    let stored = if acl_equal(&acl, &default) { None } else { Some(acl) };
    Ok(AclChange { acl: stored, warnings })
}

/// GRANT on one object (module docs).
// CONTRACT
pub fn grant_privileges(req: &AclRequest) -> Result<AclChange> {
    exec_grant(req, true)
}

/// REVOKE on one object (module docs).
// CONTRACT
pub fn revoke_privileges(req: &AclRequest) -> Result<AclChange> {
    exec_grant(req, false)
}

/// PG `aclnewowner`: rewrite grantee/grantor references from `old_owner` to `new_owner`, merging
/// items that become identical.
// CONTRACT
pub fn aclnewowner(acl: &Acl, old_owner: Oid, new_owner: Oid) -> Result<Acl> {
    let mut items: Acl = acl.clone();
    let mut newpresent = false;
    for it in items.iter_mut() {
        if it.grantor == old_owner {
            it.grantor = new_owner;
        } else if it.grantor == new_owner {
            newpresent = true;
        }
        if it.grantee == old_owner {
            it.grantee = new_owner;
        } else if it.grantee == new_owner {
            newpresent = true;
        }
    }
    if !newpresent {
        return Ok(items);
    }
    // Merge duplicates (and drop rights-free items, as PG does).
    let n = items.len();
    let mut out: Acl = Vec::with_capacity(n);
    for targ in 0..n {
        if items[targ].privileges == 0 && items[targ].grant_options == 0 {
            continue;
        }
        for src in (targ + 1)..n {
            if items[src].privileges == 0 && items[src].grant_options == 0 {
                continue;
            }
            if items[targ].grantee == items[src].grantee && items[targ].grantor == items[src].grantor {
                items[targ].privileges |= items[src].privileges;
                items[targ].grant_options |= items[src].grant_options;
                items[src].privileges = 0;
                items[src].grant_options = 0;
            }
        }
        out.push(items[targ]);
    }
    Ok(out)
}

/// Which membership edges a closure walk follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recurse {
    /// `inherit_option` edges (privileges).
    Privs,
    /// Any edge (membership).
    Members,
    /// `set_option` edges (SET ROLE).
    SetRole,
}

/// Role membership closure over one snapshot (PG `roles_is_member_of` with the PG 16+ inherit/set
/// options). Built once per check batch.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct RoleGraph {
    /// member -> [(role, inherit_option, set_option)]
    pub edges: HashMap<Oid, Vec<(Oid, bool, bool)>>,
    pub superusers: BTreeSet<Oid>,
    /// Owner of the current database (holds the `pg_database_owner` pseudo-role, OID 6171).
    pub database_owner: Oid,
}

impl RoleGraph {
    /// Build from the snapshot's roles and memberships (`current_database` owner via
    /// `snapshot.database(db)`).
    // CONTRACT
    pub fn build(snapshot: &CatalogSnapshot, database: Oid) -> Result<RoleGraph> {
        let mut edges: HashMap<Oid, Vec<(Oid, bool, bool)>> = HashMap::new();
        for m in snapshot.data().memberships.values() {
            edges.entry(m.member).or_default().push((m.role, m.inherit_option, m.set_option));
        }
        let superusers: BTreeSet<Oid> =
            snapshot.indexes().roles.values().filter(|r| r.superuser).map(|r| r.oid).collect();
        let database_owner = snapshot.database(database).map(|d| d.owner).unwrap_or(0);
        Ok(RoleGraph { edges, superusers, database_owner })
    }

    /// Roles reachable from `start` (including `start` first) in discovery order.
    fn closure(&self, start: Oid, how: Recurse) -> Vec<Oid> {
        let mut list: Vec<Oid> = vec![start];
        let mut i = 0;
        while i < list.len() {
            let member = list[i];
            i += 1;
            if let Some(es) = self.edges.get(&member) {
                for &(role, inherit, set) in es {
                    let follow = match how {
                        Recurse::Privs => inherit,
                        Recurse::Members => true,
                        Recurse::SetRole => set,
                    };
                    if follow && !list.contains(&role) {
                        list.push(role);
                    }
                }
            }
            if self.database_owner != 0 && member == self.database_owner && !list.contains(&PG_DATABASE_OWNER_OID) {
                list.push(PG_DATABASE_OWNER_OID);
            }
        }
        list
    }

    /// `has_privs_of_role` (module docs).
    // CONTRACT
    pub fn has_privs_of_role(&self, member: Oid, role: Oid) -> Result<bool> {
        if member == role || self.is_superuser(member) {
            return Ok(true);
        }
        Ok(self.closure(member, Recurse::Privs).contains(&role))
    }
    /// `is_member_of_role` (module docs).
    // CONTRACT
    pub fn is_member_of_role(&self, member: Oid, role: Oid) -> Result<bool> {
        if member == role || self.is_superuser(member) {
            return Ok(true);
        }
        Ok(self.closure(member, Recurse::Members).contains(&role))
    }
    /// `member_can_set_role`: reachability through `set_option` edges.
    // CONTRACT
    pub fn can_set_role(&self, member: Oid, role: Oid) -> Result<bool> {
        if member == role || self.is_superuser(member) {
            return Ok(true);
        }
        Ok(self.closure(member, Recurse::SetRole).contains(&role))
    }
    /// True if `role` is a superuser.
    pub fn is_superuser(&self, role: Oid) -> bool {
        self.superusers.contains(&role)
    }
}

/// PG `aclmask`: privileges (subset of `mask`) `role` holds on an object with the given effective
/// ACL and owner (module docs: PUBLIC + role + inherited roles + ownership + superuser). With
/// `with_grant_option` only bits held WITH GRANT OPTION count (owner/superuser: all).
///
/// Like PG, ownership implies only the grant options; ordinary privileges of the owner come from
/// the ACL itself (the effective ACL contains the owner's default items), so an owner who revoked
/// their own privileges really lost them. Superusers hold everything.
// CONTRACT
pub fn privilege_mask(
    graph: &RoleGraph,
    role: Oid,
    owner: Oid,
    acl: &Acl,
    mask: u32,
    with_grant_option: bool,
) -> Result<u32> {
    if mask == 0 {
        return Ok(0);
    }
    if graph.is_superuser(role) {
        return Ok(mask);
    }
    let roles = graph.closure(role, Recurse::Privs);
    if with_grant_option && roles.contains(&owner) {
        return Ok(mask);
    }
    let bits = |it: &AclItem| if with_grant_option { it.grant_options } else { it.privileges };
    let mut result = 0u32;
    for it in acl {
        if it.grantee == 0 || it.grantee == role {
            result |= bits(it) & mask;
        }
    }
    if result == mask {
        return Ok(result);
    }
    for it in acl {
        if it.grantee == 0 || it.grantee == role {
            continue;
        }
        if bits(it) & mask & !result != 0 && roles.contains(&it.grantee) {
            result |= bits(it) & mask;
            if result == mask {
                break;
            }
        }
    }
    Ok(result)
}

/// Does `role` hold ALL bits of `mode` (ANY semantic is `privilege_mask(..) != 0`)?
pub fn has_privilege(graph: &RoleGraph, role: Oid, owner: Oid, acl: &Acl, mode: u32) -> Result<bool> {
    Ok(privilege_mask(graph, role, owner, acl, mode, false)? == mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acl_mode::*;

    const OWNER: Oid = 10;
    const ALICE: Oid = 16384;
    const BOB: Oid = 16385;
    const CAROL: Oid = 16386;

    fn graph() -> RoleGraph {
        let mut g = RoleGraph::default();
        g.superusers.insert(OWNER);
        g.database_owner = OWNER;
        g
    }

    fn item(grantee: Oid, grantor: Oid, p: u32, go: u32) -> AclItem {
        AclItem { grantee, grantor, privileges: p, grant_options: go }
    }

    #[test]
    fn defaults() {
        let name = |o: Oid| if o == 10 { "postgres".to_string() } else { format!("r{o}") };
        let txt = |a: &Acl| format!("{{{}}}", a.iter().map(|i| i.to_text(&name)).collect::<Vec<_>>().join(","));
        assert_eq!(txt(&acldefault(AclObject::Table, 10).unwrap()), "{postgres=arwdDxtm/postgres}");
        assert_eq!(txt(&acldefault(AclObject::Sequence, 10).unwrap()), "{postgres=rwU/postgres}");
        assert_eq!(txt(&acldefault(AclObject::Database, 10).unwrap()), "{=Tc/postgres,postgres=CTc/postgres}");
        assert_eq!(txt(&acldefault(AclObject::Function, 10).unwrap()), "{=X/postgres,postgres=X/postgres}");
        assert_eq!(txt(&acldefault(AclObject::Schema, 10).unwrap()), "{postgres=UC/postgres}");
        assert_eq!(txt(&acldefault(AclObject::Type, 10).unwrap()), "{=U/postgres,postgres=U/postgres}");
        assert!(acldefault(AclObject::Column, 10).unwrap().is_empty());
    }

    #[test]
    fn aclupdate_grant_and_revoke_roundtrip() {
        let g = graph();
        let def = acldefault(AclObject::Table, OWNER).unwrap();
        let add = item(ALICE, OWNER, SELECT, 0);
        let a = aclupdate(&g, &def, &add, ModeChg::Add, OWNER, false).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[1], add);
        let b = aclupdate(&g, &a, &item(ALICE, OWNER, SELECT, SELECT), ModeChg::Del, OWNER, false).unwrap();
        assert!(acl_equal(&b, &def));
    }

    #[test]
    fn restrict_with_dependent_grants_fails_and_cascade_works() {
        let g = graph();
        let mut acl = acldefault(AclObject::Table, OWNER).unwrap();
        acl = aclupdate(&g, &acl, &item(ALICE, OWNER, SELECT, SELECT), ModeChg::Add, OWNER, false).unwrap();
        acl = aclupdate(&g, &acl, &item(BOB, ALICE, SELECT, 0), ModeChg::Add, OWNER, false).unwrap();
        let m = item(ALICE, OWNER, SELECT, SELECT);
        let e = aclupdate(&g, &acl, &m, ModeChg::Del, OWNER, false).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(e.message, "dependent privileges exist");
        assert_eq!(e.hint.as_deref(), Some("Use CASCADE to revoke them too."));
        let ok = aclupdate(&g, &acl, &m, ModeChg::Del, OWNER, true).unwrap();
        assert_eq!(ok, acldefault(AclObject::Table, OWNER).unwrap());
    }

    #[test]
    fn grant_option_back_to_grantor_rejected() {
        let g = graph();
        let mut acl = acldefault(AclObject::Table, OWNER).unwrap();
        acl = aclupdate(&g, &acl, &item(ALICE, OWNER, SELECT, SELECT), ModeChg::Add, OWNER, false).unwrap();
        // alice grants SELECT WITH GRANT OPTION to bob, bob tries to grant it back to alice.
        acl = aclupdate(&g, &acl, &item(BOB, ALICE, SELECT, SELECT), ModeChg::Add, OWNER, false).unwrap();
        let e = aclupdate(&g, &acl, &item(ALICE, BOB, SELECT, SELECT), ModeChg::Add, OWNER, false).unwrap_err();
        assert_eq!(e.sqlstate, INVALID_GRANT_OPERATION);
    }

    #[test]
    fn newowner_rewrites_and_merges() {
        let acl = vec![
            item(0, 10, CONNECT, 0),
            item(10, 10, ALL_DATABASE, 0),
            item(ALICE, 10, CREATE, 0),
            item(ALICE, ALICE, CONNECT, 0),
        ];
        let out = aclnewowner(&acl, 10, ALICE).unwrap();
        assert_eq!(out[0], item(0, ALICE, CONNECT, 0));
        assert_eq!(out[1], item(ALICE, ALICE, ALL_DATABASE | CONNECT, 0));
        // the explicit alice/10 item became alice/alice and merged.
        assert_eq!(out.len(), 2);
        let none = aclnewowner(&acl[..2].to_vec(), 10, BOB).unwrap();
        assert_eq!(none[1], item(BOB, BOB, ALL_DATABASE, 0));
    }

    #[test]
    fn role_graph_edges() {
        let mut g = RoleGraph::default();
        // alice -> bob (inherit, set), bob -> carol (no inherit, set)
        g.edges.insert(ALICE, vec![(BOB, true, true)]);
        g.edges.insert(BOB, vec![(CAROL, false, true)]);
        assert!(g.has_privs_of_role(ALICE, BOB).unwrap());
        assert!(!g.has_privs_of_role(ALICE, CAROL).unwrap());
        assert!(g.is_member_of_role(ALICE, CAROL).unwrap());
        assert!(g.can_set_role(ALICE, CAROL).unwrap());
        assert!(!g.is_member_of_role(CAROL, ALICE).unwrap());
        g.superusers.insert(CAROL);
        assert!(g.has_privs_of_role(CAROL, ALICE).unwrap());
    }

    #[test]
    fn mask_public_and_inherited() {
        let mut g = RoleGraph::default();
        g.edges.insert(ALICE, vec![(BOB, true, true)]);
        let acl = vec![item(OWNER, OWNER, ALL_RELATION, 0), item(0, OWNER, SELECT, 0), item(BOB, OWNER, INSERT, 0)];
        assert_eq!(privilege_mask(&g, CAROL, OWNER, &acl, SELECT | INSERT, false).unwrap(), SELECT);
        assert_eq!(privilege_mask(&g, ALICE, OWNER, &acl, SELECT | INSERT, false).unwrap(), SELECT | INSERT);
        assert!(has_privilege(&g, ALICE, OWNER, &acl, SELECT | INSERT).unwrap());
        assert!(!has_privilege(&g, CAROL, OWNER, &acl, SELECT | INSERT).unwrap());
        assert_eq!(privilege_mask(&g, OWNER, OWNER, &acl, SELECT, true).unwrap(), SELECT);
        assert_eq!(privilege_mask(&g, CAROL, OWNER, &acl, SELECT, true).unwrap(), 0);
        g.superusers.insert(CAROL);
        assert_eq!(privilege_mask(&g, CAROL, OWNER, &acl, DELETE, false).unwrap(), DELETE);
    }

    #[test]
    fn best_grantor_prefers_full_options() {
        let mut g = RoleGraph::default();
        g.edges.insert(BOB, vec![(ALICE, true, true)]);
        let acl = vec![item(OWNER, OWNER, ALL_RELATION, 0), item(ALICE, OWNER, SELECT | INSERT, SELECT)];
        let (gr, opts) = select_best_grantor(&g, BOB, SELECT, &acl, OWNER);
        assert_eq!((gr, opts), (ALICE, SELECT));
        let (gr, opts) = select_best_grantor(&g, BOB, SELECT | INSERT, &acl, OWNER);
        assert_eq!((gr, opts), (ALICE, SELECT));
        let (gr, opts) = select_best_grantor(&g, CAROL, SELECT, &acl, OWNER);
        assert_eq!((gr, opts), (CAROL, 0));
    }
}
