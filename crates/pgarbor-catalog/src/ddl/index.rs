//! CREATE INDEX, ALTER INDEX (rename, options), index-name selection. Lease CAT-13. PG sources:
//! `commands/indexcmds.c` (`DefineIndex`, `ComputeIndexAttrs`, `ResolveOpClass`,
//! `GetDefaultOpClass`), `catalog/index.c` (`index_create`, `index_constraint_create`),
//! `commands/tablecmds.c` (`RenameRelation`).
//!
//! [`CatalogTxn::create_index`] is the user-facing CREATE INDEX; constraint-owned indexes (PRIMARY
//! KEY / UNIQUE) are created by `ddl::constraint` through the lower-level
//! [`CatalogTxn::create_index_internal`] which this file also owns.
//!
//! ## `create_index` checks (PG order), all 42xxx unless noted
//! - target must be a table or matview (partitioned later): a view/sequence/composite type ->
//!   42809 `cannot create index on relation "v"` DETAIL `This operation is not supported for views.`
//!   (`errors::operation_not_supported_for`; plural words from `RelKind`: views, sequences, composite
//!   types, indexes, foreign tables); system catalogs -> 42501 `permission denied: "pg_class" is a system catalog`;
//!   must be table owner (`require_owner`) - PG also allows MAINTAIN privilege holders? No: owner
//!   only for CREATE INDEX.
//! - method: only `btree` is implemented; `hash|gist|gin|brin|spgist` -> `Err(PgError::unimplemented(
//!   "CAT-13", "index access method <name>"))` (the binder maps unknown names to
//!   `errors::access_method_does_not_exist`, 42704).
//! - more than 32 key+include columns -> 54011 `errors::too_many_index_columns`.
//! - each key: column must exist (42703 `column "x" does not exist`, NOT the "named in key" wording
//!   used for constraints; dropped columns are not found; system columns resolve (`xmin` ->
//!   attnum -2 of type xid) and then fail on the opclass lookup below: oracle `create index on t(xmin)` ->
//!   42704 `data type xid has no default operator class for access method "btree"`); expression keys need `expr`/`expr_type`;
//!   `opclass` explicit or default btree opclass for the key type (family table:
//!   `builtin::OPCLASSES` rows with `opcdefault = true` and `opcintype`, binary-coercible matches:
//!   enums use `enum_ops` 3522, arrays `array_ops` 397, records `record_ops` 2994; domains use their
//!   base type's opclass) else 42704 `errors::no_default_opclass(type, "btree")`.
//!   Collation: explicit, else the column's collation; for a non-collatable type an explicit one is
//!   rejected: 42804 `collations are not supported by type integer` (oracle).
//! - INCLUDE columns: must exist (plain columns only). Overlap with key columns is ALLOWED (oracle:
//!   `create index on t(a) include (a)` creates `t_a_a1_idx`): PG derives index attribute names over
//!   key columns AND include columns together (`ChooseIndexColumnNames(allIndexParams)`), so pass
//!   all of them (`a`, `a1`) to `naming::choose_index_column_names` / `choose_index_name`.
//! - UNIQUE/`nulls_not_distinct` copied. Partial `predicate` stored as `StoredExpr`.
//! - name: explicit (must be free in the table's namespace: 42P07 `relation "i" already exists`, or
//!   `if_not_exists` NOTICE `relation "i" already exists, skipping` + `Ok(None)`), else
//!   `naming::choose_index_name` with `naming::choose_index_column_names` (e.g. `zz_p_a_idx`,
//!   `t_lower_idx`, `t_expr_expr1_idx`).
//! Stores `Index{oid, name, namespace = table's, table, owner = table owner, method, keys, n_key_columns,
//! unique, primary=false, immediate = true, valid = true, constraint = None, ...}` where each
//! `IndexKey.name` comes from `choose_index_column_names`, `type_id` from the column (typmod kept) or
//! `expr_type`, `opclass`/`collation` resolved, `nulls_first` = explicit or `descending` (PG
//! default NULLS FIRST iff DESC). Queues `FollowUp::BuildIndex{index, table}` (the table may hold
//! rows) and records `created`.
//!
//! ## `rename_index`
//! ALTER INDEX i RENAME TO j: owner of the table; new name free (42P07); the index must not be
//! constraint-owned? PG allows renaming a constraint index and ALSO renames the constraint
//! (`index_rename` -> `RenameConstraintById` when `indisunique`... the constraint is renamed too).
//! Implement: if `Index::constraint` is Some, rename the constraint to the same name.
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// CREATE [UNIQUE] INDEX (module docs). `Ok(None)` for `IF NOT EXISTS` skip.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_index(&mut self, spec: &IndexSpec) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-13", "CREATE INDEX"))
    }

    /// Lower-level index creation shared with constraints: `spec` is a fully validated
    /// `IndexSpec` whose `name` is `Some`; `constraint` links the owning constraint OID;
    /// `primary` marks `indisprimary`; `immediate` is `indimmediate`. Performs the key resolution
    /// of `create_index` (opclass, collation, key names) without the user-facing checks (owner,
    /// access method, IF NOT EXISTS) and without queueing `BuildIndex`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_index_internal(
        &mut self,
        spec: &IndexSpec,
        constraint: Option<Oid>,
        primary: bool,
        immediate: bool,
    ) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-13", "index object creation"))
    }

    /// ALTER INDEX `index` RENAME TO `new_name` (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_index(&mut self, index: Oid, new_name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-13", "ALTER INDEX RENAME"))
    }

    /// ALTER INDEX `index` SET (opts) / RESET: replaces `Index::options` entries (`name=value`).
    /// Unknown option names for btree -> 22023 `unrecognized parameter "x"`; known: `fillfactor`
    /// (10..100, 22023 `value 5 out of bounds for option "fillfactor"` DETAIL `Valid values are
    /// between "10" and "100".`), `deduplicate_items` (bool), `vacuum_cleanup_index_scale_factor`
    /// (accepted). `reset` removes the named options.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_index_options(&mut self, index: Oid, set: &[String], reset: &[String]) -> Result<()> {
        Err(PgError::unimplemented("CAT-13", "ALTER INDEX SET"))
    }

    /// The default btree operator class OID for `type_oid` (domain -> base type, enum -> `enum_ops`,
    /// arrays -> `array_ops`, composite -> `record_ops`), or None. Reads `builtin::OPCLASSES`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn default_btree_opclass(&mut self, type_oid: Oid) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-13", "default operator class lookup"))
    }

    /// Re-derive the `IndexKey` list entry for plain column `attnum` of `table` (type, collation,
    /// default opclass, name = column name).
    #[allow(unused_variables)]
    pub(crate) fn plain_index_key(&mut self, table: Oid, attnum: i16) -> Result<IndexKey> {
        Err(PgError::unimplemented("CAT-13", "plain index key construction"))
    }
}
