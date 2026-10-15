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
use pgarbor_common::{Oid, PgError, Result};

use crate::context::SessionEnv;

/// Require privilege bits `mode` (`acl_mode::*`, ANY of them) on relation `rel` for the current role;
/// `columns` = attnums the statement touches (checked per column when the table-level privilege is
/// missing; empty = whole-table only).
#[allow(unused_variables)]
pub fn require_relation_privilege(se: &SessionEnv, rel: Oid, mode: u32, columns: &[i16]) -> Result<()> {
    Err(PgError::unimplemented("EXE-42", "relation privilege check (perm::require_relation_privilege)"))
}

/// Require ANY of the privilege bits `mode` on sequence `seq` (`USAGE | UPDATE` for nextval ...).
#[allow(unused_variables)]
pub fn require_sequence_privilege(se: &SessionEnv, seq: Oid, mode: u32) -> Result<()> {
    Err(PgError::unimplemented("EXE-42", "sequence privilege check (perm::require_sequence_privilege)"))
}
