//! Schemas (`pg_namespace`), roles (`pg_authid` / `pg_auth_members`) and databases (`pg_database`).
//!
//! Plain data, frozen (v1). Persisted as versioned JSON (see `crate::persist`).
use super::common::Acl;
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};

/// A schema. Builtin `pg_catalog` (11), `pg_toast` (99) and `information_schema` (13293) are
/// compiled in and immutable; `public` (2200, owner `pg_database_owner` 6171, acl
/// `{pg_database_owner=UC/pg_database_owner,=U/pg_database_owner}`) is written at bootstrap and is an
/// ordinary mutable/droppable schema like any user schema.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Namespace {
    pub oid: Oid,
    /// Max 63 bytes (NAMEDATALEN-1); names starting with `pg_` are reserved for user creation
    /// (42939 `unacceptable schema name "pg_x"` + DETAIL `The prefix "pg_" is reserved for system schemas.`).
    pub name: String,
    pub owner: Oid,
    #[serde(default)]
    pub acl: Option<Acl>,
}

/// A role (`pg_authid`). Builtin predefined roles (`pg_monitor`, ...) are compiled in; the bootstrap
/// superuser `postgres` (10) is written at bootstrap so it can be altered (password, attributes).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub oid: Oid,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub can_login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    /// -1 = unlimited.
    pub conn_limit: i32,
    /// SCRAM-SHA-256 verifier (`SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`) or an md5 hash;
    /// never plaintext. `pg_authid.rolpassword`; `pg_roles.rolpassword` always shows `********`.
    #[serde(default)]
    pub password: Option<String>,
    /// `rolvaliduntil` as TimestampTz µs since 2000-01-01, None = forever.
    #[serde(default)]
    pub valid_until: Option<i64>,
    /// Per-role GUC settings (`ALTER ROLE r SET x = y`), as `name=value` strings (pg_db_role_setting).
    #[serde(default)]
    pub config: Vec<String>,
}

/// A role membership (`pg_auth_members`, `GRANT role TO member`).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleMembership {
    pub oid: Oid,
    pub role: Oid,
    pub member: Oid,
    pub grantor: Oid,
    pub admin_option: bool,
    pub inherit_option: bool,
    pub set_option: bool,
}

/// A database (`pg_database`). v1: one SQL database (`postgres`, OID 5) per branch; `template1`
/// (1) and `template0` (4) are compiled in for catalog fidelity. `CREATE DATABASE` is CAT-19 later
/// work (0A000 until then).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Database {
    pub oid: Oid,
    pub name: String,
    pub owner: Oid,
    /// 6 = UTF8.
    pub encoding: i32,
    /// b'b' builtin (pinned oracle: `--locale-provider=builtin --locale=C.UTF-8`).
    pub locale_provider: u8,
    pub is_template: bool,
    pub allow_conn: bool,
    pub conn_limit: i32,
    pub collate: String,
    pub ctype: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub acl: Option<Acl>,
    /// Per-database GUC settings (`ALTER DATABASE d SET x = y`).
    #[serde(default)]
    pub config: Vec<String>,
}
