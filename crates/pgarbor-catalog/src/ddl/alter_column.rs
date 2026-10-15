//! ALTER TABLE column attributes and whole-relation alterations: SET/DROP DEFAULT, SET/DROP NOT
//! NULL, identity and generation, column storage/statistics options, RENAME TO, SET SCHEMA,
//! OWNER TO, SET (reloptions), SET LOGGED/UNLOGGED, REPLICA IDENTITY, ROW LEVEL SECURITY flags.
//! Lease CAT-15. PG sources: `commands/tablecmds.c` (`ATExecColumnDefault`, `ATExecSetNotNull`,
//! `ATExecDropNotNull`, `ATExecAddIdentity`, `ATExecSetIdentity`, `ATExecDropIdentity`,
//! `ATExecSetExpression`, `ATExecDropExpression`, `ATExecSetStatistics`, `ATExecSetOptions`,
//! `ATExecSetStorage`, `ATExecSetRelOptions`, `ATExecReplicaIdentity`, `RenameRelation`,
//! `AlterTableNamespace`, `ATExecChangeOwner`), `commands/sequence.c` (`DefineSequence` for
//! identity), `catalog/pg_constraint.c`.
//!
//! Every function: relation owner required (`require_owner`), relation-kind check
//! (`errors::alter_action_not_supported`), column lookup (`errors::column_of_relation_does_not_exist`,
//! system columns -> `errors::cannot_alter_system_column`).
//!
//! ## Column functions
//! - [`set_column_default`]: `Some(expr)` stores `ColumnDefault{oid (reuse existing attrdef OID if
//!   one exists, else new), expr}` with `depends_on` (self placeholder rewritten); `None` = DROP
//!   DEFAULT (no-op without error when none). Refused on identity columns (42P16 `column "id" of
//!   relation "t" is an identity column` HINT `Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY
//!   instead.` - `errors::column_is_identity`) and generated columns (`errors::column_is_generated`
//!   with HINT `Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.` - oracle). Changing the
//!   default does not touch existing rows (no follow-up) and clears nothing else.
//! - [`set_not_null`] -> `ensure_not_null(.., validate = true)` (creates `t_a_not_null` unless one
//!   exists; queues `ValidateNotNull`). Views etc. are rejected by the kind check.
//! - [`drop_not_null`]: refuses primary-key columns (`errors::column_in_primary_key`, 42P16
//!   `column "id" is in a primary key`), identity columns (`errors::column_is_identity` HINT `Use
//!   ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.`); removes the NOT NULL constraint row
//!   and clears `Column::not_null`; no-op when the column is nullable.
//! - [`add_identity`] (ADD GENERATED ALWAYS|BY DEFAULT AS IDENTITY, oracle-verified order of checks):
//!   column must already be NOT NULL (42P16 `errors::identity_requires_not_null`: `column "a" of
//!   relation "t" must be declared NOT NULL before identity can be added`), must not be identity
//!   already (55000 `errors::column_already_identity`), must have no default
//!   (`errors::column_already_has_default`), must not be generated, and its type must be
//!   smallint/integer/bigint (22023 `errors::identity_bad_type`; domains over them are rejected too).
//!   Creates the identity sequence `t_id_seq` (`identity = true`, owned_by) with
//!   `SequenceOptions` from the spec (type from the column) and sets `Column::identity`. Queues
//!   `CreateSequenceState`. Existing rows are NOT backfilled by PG here (NULLs are impossible
//!   because the column is NOT NULL, so values already exist).
//! - [`set_identity`]: SET GENERATED ALWAYS|BY DEFAULT and/or sequence options (RESTART -> `RestartSequence`
//!   follow-up); not an identity column -> `errors::column_not_identity`.
//! - [`drop_identity`]: removes the identity and its sequence (via `drop_objects`); not identity ->
//!   `errors::column_not_identity`, or with `missing_ok` NOTICE `errors::column_not_identity_skipping`.
//!   NOT NULL stays.
//! - [`set_generation_expression`] (SET EXPRESSION AS (expr), PG 17+) / [`drop_expression`]
//!   (DROP EXPRESSION: generated column becomes plain column keeping stored values; on a VIRTUAL
//!   column rows are rewritten: `RewriteTable`; STORED -> no rewrite). Non-generated column ->
//!   42P16 `errors::column_not_generated` (`column "x" of relation "t" is not a generated column`),
//!   or NOTICE `errors::column_not_generated_skipping` (`..., skipping`) with `missing_ok`.
//! - [`set_column_attribute`]: STATISTICS n (-1 = default; 22023 `statistics target 10001 is too
//!   high` DETAIL? > 10000 clamps with WARNING `lowering statistics target to 10000`), STORAGE
//!   (`plain|external|extended|main`, `Column::storage`; 42804 `column data type integer can only
//!   have storage PLAIN` when invalid for the type), COMPRESSION, per-column options `SET (n_distinct = 5)`
//!   (`Column::options`; only `n_distinct`/`n_distinct_inherited` known: 22023 `unrecognized
//!   parameter "x"`).
//!
//! ## Relation functions
//! - [`rename_relation`]: ALTER TABLE/VIEW/MATERIALIZED VIEW/SEQUENCE/INDEX/TYPE-relation RENAME TO.
//!   Owner; new name free in the namespace (42P07 `relation "x" already exists`; also the rowtype
//!   name must be free: 42710 `type "x" already exists`); builtin relations: 42501 system-catalog
//!   permission; index/sequence owned by constraint/column allowed (index rename also renames its
//!   constraint, see `ddl::index`). The array type name is re-chosen (`_new`, stored in
//!   `array_type_name` only if non-canonical). Stored expressions that mention the relation
//!   (`nextval('old_seq'::regclass)` after renaming a sequence, views selecting from a renamed table)
//!   are rewritten via `rewriter` with `NameChange::Relation`. Constraint and index names do NOT
//!   change (oracle).
//! - [`set_relation_schema`]: ALTER ... SET SCHEMA. Owner; CREATE on the target schema; name free
//!   there (42P07); not into/out of pg_catalog/pg_toast (0A000 `cannot move objects into or out
//!   of temporary schemas`/`system schemas`: use 42501 system-catalog wording); moves the
//!   relation, its indexes, owned sequences (`Sequence::owned_by`), constraints
//!   (`Constraint::namespace`) and rowtype/array type (derived). Identity sequences refuse an
//!   independent move (`errors::cannot_alter_identity_sequence`). Rewrite stored text that prints
//!   the schema qualifier through `rewriter` (`NameChange::RelationSchema`). Same-schema move is a
//!   no-op.
//! - [`alter_relation_owner`]: new owner must exist; caller must be superuser or (owner and member
//!   of the new owner); changes `owner` of the relation, its indexes and its owned/identity sequences,
//!   and rewrites ACL entries (`acl::aclnewowner`, CAT-22) if an explicit ACL exists; relation kinds:
//!   table, view, matview, sequence, composite type; NOT index (`errors::wrong_object_type...`: 42809
//!   `cannot change owner of index "i"` HINT `Change the ownership of the index's table instead.`) and
//!   not identity/owned sequences (`errors::cannot_alter_identity_sequence("change ownership", ..)`).
//! - [`set_relation_options`]: `reloptions` SET/RESET for tables: known `fillfactor` (10..100),
//!   `autovacuum_enabled` and other `autovacuum_*`/`toast.*` (accepted and stored), `parallel_workers`,
//!   `user_catalog_table`; unknown -> 22023 `unrecognized parameter "x"`; views accept
//!   `security_barrier`, `security_invoker`, `check_option`.
//! - [`set_persistence`]: SET LOGGED / UNLOGGED (stores `Persistence`; no follow-up).
//! - [`set_replica_identity`]: DEFAULT/FULL/NOTHING/USING INDEX (index must be unique, non-partial,
//!   all columns NOT NULL, belong to the table: 42809 `"i" is not an index for table "t"`, 55000
//!   `cannot use non-unique index "i" as replica identity` ...).
//! - [`set_row_security`]: ENABLE/DISABLE/FORCE/NO FORCE ROW LEVEL SECURITY flags only (policies are
//!   unsupported: `CREATE POLICY` is the binder's 0A000).
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

/// Column attribute altered by [`CatalogTxn::set_column_attribute`].
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnAttribute {
    /// `SET STATISTICS n` (-1 = default).
    Statistics(i32),
    /// `SET STORAGE x` (b'p' plain, b'e' external, b'm' main, b'x' extended).
    Storage(u8),
    /// `SET COMPRESSION x` (b'p', b'l', or 0 = default).
    Compression(u8),
    /// `SET (opt = value, ...)`.
    Options(Vec<String>),
    /// `RESET (opt, ...)`.
    ResetOptions(Vec<String>),
}

/// New replica identity setting.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaIdentitySetting {
    Default,
    Full,
    Nothing,
    UsingIndex(Oid),
}

impl<'a> CatalogTxn<'a> {
    /// SET DEFAULT `expr` / DROP DEFAULT (`None`). See module docs.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_column_default(&mut self, table: Oid, column: &str, expr: Option<StoredExpr>) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET/DROP DEFAULT"))
    }

    /// SET NOT NULL.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_not_null(&mut self, table: Oid, column: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET NOT NULL"))
    }

    /// DROP NOT NULL.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_not_null(&mut self, table: Oid, column: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "DROP NOT NULL"))
    }

    /// ADD GENERATED ... AS IDENTITY.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_identity(&mut self, table: Oid, column: &str, identity: &IdentitySpec) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "ADD IDENTITY"))
    }

    /// SET GENERATED ALWAYS|BY DEFAULT and/or SET sequence option / RESTART.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_identity(
        &mut self,
        table: Oid,
        column: &str,
        kind: Option<IdentityKind>,
        options: &SequenceOptions,
    ) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET IDENTITY"))
    }

    /// DROP IDENTITY [IF EXISTS].
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_identity(&mut self, table: Oid, column: &str, missing_ok: bool) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "DROP IDENTITY"))
    }

    /// SET EXPRESSION AS (expr) on a generated column.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_generation_expression(&mut self, table: Oid, column: &str, expr: StoredExpr) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET EXPRESSION"))
    }

    /// DROP EXPRESSION [IF EXISTS].
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_expression(&mut self, table: Oid, column: &str, missing_ok: bool) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "DROP EXPRESSION"))
    }

    /// SET STATISTICS / STORAGE / COMPRESSION / (options).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_column_attribute(&mut self, table: Oid, column: &str, attr: &ColumnAttribute) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "ALTER COLUMN attribute"))
    }

    /// RENAME TO for table/view/matview/sequence/index/composite type relations.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_relation(&mut self, rel: Oid, new_name: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "RENAME relation"))
    }

    /// SET SCHEMA.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_relation_schema(&mut self, rel: Oid, new_schema: Oid, rewriter: &dyn ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET SCHEMA relation"))
    }

    /// OWNER TO.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_relation_owner(&mut self, rel: Oid, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "ALTER relation OWNER"))
    }

    /// SET (opts) / RESET (names) on a relation.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_relation_options(&mut self, rel: Oid, set: &[String], reset: &[String]) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "ALTER relation SET options"))
    }

    /// SET LOGGED / UNLOGGED.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_persistence(&mut self, table: Oid, persistence: Persistence) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "SET LOGGED/UNLOGGED"))
    }

    /// REPLICA IDENTITY.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_replica_identity(&mut self, table: Oid, setting: &ReplicaIdentitySetting) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "REPLICA IDENTITY"))
    }

    /// ENABLE/DISABLE/FORCE/NO FORCE ROW LEVEL SECURITY (`None` = unchanged).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_row_security(&mut self, table: Oid, enable: Option<bool>, force: Option<bool>) -> Result<()> {
        Err(PgError::unimplemented("CAT-15", "ROW LEVEL SECURITY flags"))
    }
}
