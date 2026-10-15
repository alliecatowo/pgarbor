//! DDL operations on [`crate::txn::CatalogTxn`] (one `impl CatalogTxn` block per file). The
//! binder resolves names and builds the [`spec`] structs; these functions do everything PostgreSQL
//! does to the CATALOG for the statement (validation, automatic naming, OID allocation,
//! dependency-correct writes, privilege and ownership checks, NOTICEs) and queue the DATA-level
//! consequences as [`spec::FollowUp`]s for the executor.
//!
//! | file | statements |
//! |---|---|
//! | [`common`] | shared helpers: ownership/privilege checks, name-clash checks, system-column names |
//! | [`schema`] | CREATE/ALTER SCHEMA, ALTER DATABASE |
//! | [`role`] | CREATE/ALTER/DROP ROLE, GRANT/REVOKE role, ALTER ROLE SET |
//! | [`table`] | CREATE TABLE |
//! | [`constraint`] | ALTER TABLE ADD/DROP/RENAME/VALIDATE CONSTRAINT, key lookups |
//! | [`index`] | CREATE INDEX, ALTER INDEX |
//! | [`alter_table`] | ADD/DROP/RENAME COLUMN, ALTER COLUMN TYPE |
//! | [`alter_column`] | SET/DROP DEFAULT/NOT NULL/IDENTITY/EXPRESSION, table RENAME/SET SCHEMA/OWNER/options |
//! | [`sequence`] | CREATE/ALTER SEQUENCE |
//! | [`view`] | CREATE [OR REPLACE] [MATERIALIZED] VIEW |
//! | [`typedef`] | CREATE/ALTER TYPE (enum, composite), CREATE/ALTER DOMAIN |
//! | [`function`] | CREATE [OR REPLACE] FUNCTION/PROCEDURE, ALTER FUNCTION |
//! | [`comment`] | COMMENT ON, generic ALTER ... OWNER TO, REASSIGN/DROP OWNED |
//! | [`grant`] | GRANT/REVOKE privileges |
//! | [`drop_stmt`] | DROP TABLE/VIEW/INDEX/SEQUENCE/TYPE/DOMAIN/SCHEMA/FUNCTION/ROLE statements |
//!
//! ## Rules common to every operation
//! - Every function takes the statement's already-resolved inputs, performs ALL validation before
//!   its first write when practical, and writes through `CatalogTxn::put/remove`.
//! - Ownership/privilege checks use [`common`]'s helpers (superuser bypasses; the owner's
//!   implicit rights; `pg_database_owner` owns `public`). Error wording: `errors::must_be_owner`,
//!   `errors::permission_denied_for`.
//! - A name clash in the pg_class namespace (tables, views, indexes, sequences, matviews, composite
//!   types) is 42P07 `relation "x" already exists`; in the pg_type namespace (types, rowtypes,
//!   array types) 42710 `type "x" already exists`. CREATE TABLE/VIEW/SEQUENCE/INDEX/TYPE check both
//!   (a table also claims its rowtype name).
//! - Automatic names come from `crate::naming`; every created object gets the PG-style dependency
//!   structure that `crate::depend` derives (so `model` fields must be filled consistently: e.g.
//!   `Sequence::owned_by`, `Index::constraint`, `Relation::rowtype`).
//! - Created objects are recorded with `CatalogTxn::record_created`.
pub mod alter_column;
pub mod alter_table;
pub mod comment;
pub mod common;
pub mod constraint;
pub mod drop_stmt;
pub mod function;
pub mod grant;
pub mod index;
pub mod role;
pub mod schema;
pub mod sequence;
pub mod spec;
pub mod table;
pub mod typedef;
pub mod view;
