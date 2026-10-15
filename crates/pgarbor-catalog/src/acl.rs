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
use pgarbor_common::{Oid, PgError, Result};
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
#[allow(unused_variables)]
pub fn acldefault(kind: AclObject, owner: Oid) -> Result<Acl> {
    Err(PgError::unimplemented("CAT-22", "acldefault"))
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

/// GRANT on one object (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn grant_privileges(req: &AclRequest) -> Result<AclChange> {
    Err(PgError::unimplemented("CAT-22", "GRANT privileges"))
}

/// REVOKE on one object (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn revoke_privileges(req: &AclRequest) -> Result<AclChange> {
    Err(PgError::unimplemented("CAT-22", "REVOKE privileges"))
}

/// PG `aclnewowner`: rewrite grantee/grantor references from `old_owner` to `new_owner`, merging
/// items that become identical.
// CONTRACT
#[allow(unused_variables)]
pub fn aclnewowner(acl: &Acl, old_owner: Oid, new_owner: Oid) -> Result<Acl> {
    Err(PgError::unimplemented("CAT-22", "aclnewowner"))
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
    #[allow(unused_variables)]
    pub fn build(snapshot: &CatalogSnapshot, database: Oid) -> Result<RoleGraph> {
        Err(PgError::unimplemented("CAT-22", "role graph"))
    }
    /// `has_privs_of_role` (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn has_privs_of_role(&self, member: Oid, role: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-22", "has_privs_of_role"))
    }
    /// `is_member_of_role` (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn is_member_of_role(&self, member: Oid, role: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-22", "is_member_of_role"))
    }
    /// `member_can_set_role`: reachability through `set_option` edges.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn can_set_role(&self, member: Oid, role: Oid) -> Result<bool> {
        Err(PgError::unimplemented("CAT-22", "member_can_set_role"))
    }
    /// True if `role` is a superuser.
    pub fn is_superuser(&self, role: Oid) -> bool {
        self.superusers.contains(&role)
    }
}

/// PG `aclmask`: privileges (subset of `mask`) `role` holds on an object with the given effective
/// ACL and owner (module docs: PUBLIC + role + inherited roles + ownership + superuser). With
/// `with_grant_option` only bits held WITH GRANT OPTION count (owner/superuser: all).
// CONTRACT
#[allow(unused_variables)]
pub fn privilege_mask(
    graph: &RoleGraph,
    role: Oid,
    owner: Oid,
    acl: &Acl,
    mask: u32,
    with_grant_option: bool,
) -> Result<u32> {
    Err(PgError::unimplemented("CAT-22", "aclmask"))
}

/// Does `role` hold ALL bits of `mode` (ANY semantic is `privilege_mask(..) != 0`)?
pub fn has_privilege(graph: &RoleGraph, role: Oid, owner: Oid, acl: &Acl, mode: u32) -> Result<bool> {
    Ok(privilege_mask(graph, role, owner, acl, mode, false)? == mode)
}
