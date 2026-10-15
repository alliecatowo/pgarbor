//! CREATE SCHEMA / SEQUENCE / TYPE / DOMAIN / ROLE / DATABASE (SQL-35). PG references:
//! `schemacmds.c` (`CreateSchemaCommand`), `sequence.c` (`DefineSequence`, `init_params`),
//! `typecmds.c` (`DefineEnum`, `DefineCompositeType`, `DefineDomain`), `user.c` (`CreateRole`),
//! `dbcommands.c` (`createdb`), `gram.y`.
//!
//! * [`bind_create_schema`] -> `BoundStatement::CreateSchema { name, owner, if_not_exists }`; `CREATE
//!   SCHEMA s AUTHORIZATION r` (owner resolved by `env.role_oid`, default current user; name defaults to the
//!   role name when only AUTHORIZATION given); schema elements in the same statement (`CREATE SCHEMA s
//!   CREATE TABLE ...`): 0A000. `IF NOT EXISTS` + elements: 42601. Names starting with `pg_`: 42939
//!   `unacceptable schema name "pg_x"` DETAIL `The prefix "pg_" is reserved for system schemas.`
//! * [`bind_create_sequence`] -> `CreateSequence(Box<CreateSequenceSpec>)`: options `AS type`
//!   (smallint/integer/bigint else 22023? `sequence type must be smallint, integer, or bigint`), `INCREMENT [BY] n`
//!   (!= 0), `MINVALUE n|NO MINVALUE`, `MAXVALUE n|NO MAXVALUE`, `START [WITH] n`, `CACHE n` (>= 1),
//!   `[NO] CYCLE`, `OWNED BY t.c|NONE`; defaults by sign of increment and type; all range validations with
//!   the PG messages (22023 `MINVALUE (x) must be less than MAXVALUE (y)`, `START value (x) cannot be
//!   less than MINVALUE (y)`, `START value (x) cannot be greater than MAXVALUE (y)`, `CACHE (x) must be
//!   greater than zero`, `INCREMENT must not be zero`, `MAXVALUE (x) is out of range for sequence data
//!   type integer`). Numbers parsed as i128 text then range-checked (values beyond i64: 22003 `value "99999999999999999999" is out of
//!   range for type bigint`, oracle). `TEMP`: 0A000. `SequenceOptions`/`SeqBound` structures
//!   are in `ddl::spec`. Shared with identity columns via [`bind_sequence_options`].
//! * [`bind_create_type`] (`Statement::CreateType { name, representation }`): `AS ENUM ('a','b')` ->
//!   `CreateEnum(CreateEnumSpec)` (duplicate label 42710 `enum label "a" already exists`? at create:
//!   `22023`? check `DefineEnum`; label longer than 63 bytes 42622 `invalid enum label "x"` DETAIL `Labels
//!   must be 63 bytes or less.`; zero labels allowed), `AS (a int, b text)` -> `CreateComposite`
//!   (attribute names unique 42701; types resolved), range/base/shell types 0A000.
//! * [`bind_create_domain`] -> `CreateDomain(Box<CreateDomainSpec>)`: `AS base [COLLATE c] [DEFAULT e]
//!   [NOT NULL|NULL] [CONSTRAINT n CHECK (expr)]...`; CHECK expressions are written in terms of `VALUE`
//!   (bind with a synthetic one-column scope `VALUE` of the base type; stored canonical text uses
//!   `VALUE`); `base` may be an array/enum/domain; `NOT VALID` on CHECK allowed at creation? (PG: only via
//!   ALTER).
//! * [`bind_create_role`] (`CreateRole` and `CreateUser`, `CREATE USER` implies LOGIN): options
//!   `SUPERUSER|NOSUPERUSER, CREATEDB, CREATEROLE, INHERIT, LOGIN, REPLICATION, BYPASSRLS, CONNECTION
//!   LIMIT n, [ENCRYPTED] PASSWORD 'p'|NULL, VALID UNTIL 'ts', IN ROLE a,b, ROLE m, ADMIN a, SYSID
//!   (ignored), USER (member list)`; role names via `env.role_oid` (42704); reserved name prefix `pg_`:
//!   42939 `role name "pg_x" is reserved` DETAIL `Role names starting with "pg_" are reserved.`
//!   `password` is stored as given here; hashing (SCRAM) is the executor's.
//! * [`bind_create_database`] (`Statement::CreateDatabase { db_name, if_not_exists, location, ..}`
//!   plus sqlparser option list): `CREATE DATABASE n [OWNER r] [TEMPLATE t] [ENCODING 'e'] [LOCALE ..]
//!   [CONNECTION LIMIT n] [IS_TEMPLATE b] [ALLOW_CONNECTIONS b]` -> `CreateDatabase(BoundCreateDatabase)`.
//!   Only UTF8 encoding is accepted (22023 `new encoding (LATIN1) is incompatible with ...` otherwise).
//!   Cannot run inside a transaction block (25001 `CREATE DATABASE cannot run inside a transaction
//!   block`) — that check belongs to the session layer, not here.
use pgarbor_catalog::ddl::spec::SequenceOptions;
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{SequenceOptions as AstSequenceOption, Statement};

use super::context::Binder;
use crate::ir::BoundStatement;

/// `Statement::CreateSchema`.
#[allow(unused_variables)]
pub fn bind_create_schema(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE SCHEMA binding (ddl_objects::bind_create_schema)"))
}

/// `Statement::CreateSequence`.
#[allow(unused_variables)]
pub fn bind_create_sequence(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE SEQUENCE binding (ddl_objects::bind_create_sequence)"))
}

/// Resolve sequence option lists (CREATE SEQUENCE, identity columns) into `SequenceOptions`,
/// applying type-dependent defaults and the range validations (module docs). `data_type` is the
/// column/sequence type oid (int2/int4/int8).
#[allow(unused_variables)]
pub fn bind_sequence_options(b: &mut Binder, options: &[AstSequenceOption], data_type: pgarbor_common::Oid, pos: Option<u32>) -> Result<SequenceOptions> {
    Err(PgError::unimplemented("SQL-35", "sequence option validation (ddl_objects::bind_sequence_options)"))
}

/// `Statement::CreateType`.
#[allow(unused_variables)]
pub fn bind_create_type(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE TYPE binding (ddl_objects::bind_create_type)"))
}

/// `Statement::CreateDomain`.
#[allow(unused_variables)]
pub fn bind_create_domain(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE DOMAIN binding (ddl_objects::bind_create_domain)"))
}

/// `Statement::CreateRole` / `CreateUser`.
#[allow(unused_variables)]
pub fn bind_create_role(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE ROLE binding (ddl_objects::bind_create_role)"))
}

/// `Statement::CreateDatabase`.
#[allow(unused_variables)]
pub fn bind_create_database(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-35", "CREATE DATABASE binding (ddl_objects::bind_create_database)"))
}
