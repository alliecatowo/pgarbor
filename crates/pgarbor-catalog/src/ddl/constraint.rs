//! Table constraints: ADD / DROP / RENAME / VALIDATE / ALTER CONSTRAINT, NOT NULL constraints and
//! referenced-key lookup. Lease CAT-12 (sonnet). PG sources: `commands/tablecmds.c`
//! (`ATExecAddConstraint`, `ATAddCheckNNConstraint`, `ATAddForeignKeyConstraint`,
//! `ATExecDropConstraint`, `ATExecValidateConstraint`, `ATExecAlterConstraint`,
//! `transformIndexConstraint` in parse_utilcmd.c, `ChooseConstraintName` in pg_constraint.c),
//! `catalog/heap.c` (`StoreRelCheck`, `AddRelationNotNullConstraints`), `catalog/index.c`
//! (`index_constraint_create`), `catalog/pg_constraint.c` (`CreateConstraintEntry`).
//!
//! ## What each constraint kind creates
//! | spec kind | objects written |
//! |---|---|
//! | PrimaryKey | NOT NULL constraints for key columns lacking one (`t_id_not_null`), `Index{primary, unique}` named `t_pkey` (or the constraint name), `Constraint{kind PrimaryKey, columns = key attnums, index}` named `t_pkey`; sets `Column::not_null` |
//! | Unique | `Index{unique}` + `Constraint{Unique}` named `t_a_b_key` |
//! | Check | `Constraint{Check, check: StoredExpr, columns = referenced attnums}` named `t_<firstcol>_check` (`t_check` when no columns) |
//! | NotNull | `Constraint{NotNull, columns=[attnum]}` named `t_a_not_null`; sets `Column::not_null` |
//! | ForeignKey | `Constraint{ForeignKey, columns, fk: ForeignKey{ref_table, ref_columns, ref_index, actions, match}}` named `t_a_fkey` |
//! | Exclusion | `Err(PgError::unimplemented("CAT-12", "EXCLUDE constraints"))` |
//! The constraint's `namespace` is the table's. Names: `naming::choose_constraint_name` for
//! automatic ones, checked against ALL constraints of the namespace (PG's pg_constraint unique
//! index is (conrelid, contypid, conname) for table constraints, but ChooseConstraintName avoids
//! clashes namespace-wide). Explicit name clash on the same table -> 42710 `constraint "c" for
//! relation "t" already exists` (`errors::constraint_already_exists`) - EXCEPT inside CREATE TABLE
//! where two CHECKs with one name give `check constraint "c" already exists`. An explicit
//! PK/UNIQUE constraint name that equals an existing RELATION name in the namespace (its index
//! would clash) -> 42P07 `relation "x" already exists`.
//!
//! ## Validation rules (oracle-verified messages)
//! - key columns must exist: 42703 `column "nosuch" named in key does not exist`;
//!   duplicated key column: 42701 `column "a" appears twice in unique constraint` /
//!   `primary key constraint`.
//! - second PRIMARY KEY: 42P16 `multiple primary keys for table "t" are not allowed`.
//! - NOT NULL on a view/index/... is rejected earlier by the table-kind check
//!   (`errors::alter_action_not_supported`).
//! - **Foreign keys** (`ATAddForeignKeyConstraint`):
//!   referencing and referenced column COUNTS must match: 42830 `number of referencing and
//!   referenced columns for foreign key disagree`; > 32 columns: 54011 `cannot have more than 32
//!   keys in a foreign key`; referenced column duplicates: 42701 `foreign key referenced-columns
//!   list must not contain duplicates`; empty `ref_columns` -> the referenced table's PRIMARY KEY
//!   columns, else 42704 `there is no primary key for referenced table "t"`; the referenced
//!   columns must be exactly (as a set, any order) the key of a non-partial, non-expression UNIQUE
//!   index (`find_referenced_index`), else 42830 `there is no unique constraint matching given keys
//!   for referenced table "t"`. Column type compatibility per pair: identical types, or an `=`
//!   operator resolves for (fk type, pk type) (`pgarbor_types::ops`), or an implicit cast exists;
//!   otherwise 42804 `foreign key constraint "f_x_fkey" cannot be implemented` DETAIL `Key columns
//!   "x" of the referencing table and "a" of the referenced table are of incompatible types: text and integer.`
//!   (`errors::fk_cannot_be_implemented`, types via `pgarbor_types::names::format_type`).
//!   MATCH PARTIAL -> 0A000 `MATCH PARTIAL not yet implemented`. `SET NULL/DEFAULT (cols)` columns
//!   must be among the FK columns (42P10 `column "x" referenced in ON DELETE SET action must be part
//!   of foreign key`). Referenced table must be a table (42809 `referenced relation "v" is not a
//!   table`). Privilege: REFERENCES on the referenced table (owner check suffices in v1).
//!   `ForeignKey::pf_eq_ops/pp_eq_ops/ff_eq_ops`: fill with the `=` operator OIDs when found, else leave empty.
//! - CHECK: `columns` from the spec; `enforced = false` (`NOT ENFORCED`) allowed for CHECK and FK.
//! - `not_valid` is only grammatical for CHECK and FOREIGN KEY (the parser rejects it elsewhere);
//!   on any other kind return `PgError::syntax("NOT VALID is only supported for CHECK and FOREIGN KEY constraints", None)`.
//!
//! ## Follow-ups for ALTER TABLE ADD (`creating = false`)
//! A new index -> `FollowUp::BuildIndex`; NOT NULL / PRIMARY KEY columns -> `ValidateNotNull` per
//! newly NOT NULL column; CHECK (validated) -> `ValidateCheck`; FK (validated) ->
//! `ValidateForeignKey`. `NOT VALID` constraints get `validated = false` and no validation
//! follow-up (VALIDATE CONSTRAINT queues it later). For `add_constraints_to_new_table` (empty
//! table) none are queued.
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// Create all constraints of a NEW (empty) table in PG's order: CHECKs, NOT NULLs (explicit,
    /// serial/identity, then PRIMARY KEY columns lacking one), PRIMARY KEY/UNIQUE, FOREIGN KEYs.
    /// `NotNull` specs and `ColumnSpec::not_null` are already folded by `create_table` into `specs`
    /// (it appends a `ConstraintSpecKind::NotNull` per not-null/identity/serial column). Returns
    /// the created constraint OIDs in creation order.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_constraints_to_new_table(&mut self, table: Oid, specs: &[ConstraintSpec]) -> Result<Vec<Oid>> {
        Err(PgError::unimplemented("CAT-12", "constraints of a new table"))
    }

    /// ALTER TABLE `table` ADD CONSTRAINT (module docs). Requires table ownership and a
    /// table-like relation (`Table`); queues follow-ups. Returns the created constraint OIDs
    /// (PK: the NOT NULL constraints it created, then the PK constraint).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_constraint(&mut self, table: Oid, spec: &ConstraintSpec) -> Result<Vec<Oid>> {
        Err(PgError::unimplemented("CAT-12", "ADD CONSTRAINT"))
    }

    /// Add (or find) the NOT NULL constraint of column `attnum` and set `Column::not_null`.
    /// Returns `(constraint_oid, created)`: when a NOT NULL constraint already exists it is reused
    /// (`created = false`, no follow-up). When created and `validate`, queues
    /// `FollowUp::ValidateNotNull`. `name = None` -> `t_a_not_null`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn ensure_not_null(&mut self, table: Oid, attnum: i16, name: Option<&str>, validate: bool) -> Result<(Oid, bool)> {
        Err(PgError::unimplemented("CAT-12", "NOT NULL constraint"))
    }

    /// ALTER TABLE DROP CONSTRAINT `name`: owner check; missing -> 42704 `constraint "c" of
    /// relation "t" does not exist` or (`missing_ok`) NOTICE `constraint "c" of relation "t" does
    /// not exist, skipping` and `Ok(false)`. Drops through `drop_objects([constraint], behavior)`:
    /// the backing index goes with PK/UNIQUE (INTERNAL dep); an FK from another table that depends
    /// on this PK/UNIQUE index blocks under RESTRICT (oracle: 2BP01 `cannot drop constraint t_pkey
    /// on table t because other objects depend on it` DETAIL `constraint t2_x_fkey on table t2
    /// depends on index t_pkey` HINT `Use DROP ... CASCADE to drop the dependent objects too.`).
    /// Dropping a PRIMARY KEY leaves its NOT NULL constraints (oracle: `t_id_not_null` remains and
    /// `attnotnull` stays true). Dropping a NOT NULL constraint clears `Column::not_null` (same as
    /// `DROP NOT NULL`, which refuses identity columns and primary-key columns:
    /// `errors::column_is_identity`, `errors::column_in_primary_key`).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_constraint(&mut self, table: Oid, name: &str, behavior: DropBehavior, missing_ok: bool) -> Result<bool> {
        Err(PgError::unimplemented("CAT-12", "DROP CONSTRAINT"))
    }

    /// ALTER TABLE RENAME CONSTRAINT old TO new: owner; new name free on the table (42710 as
    /// above); for PK/UNIQUE also renames the backing index to the new name (PG
    /// `RenameConstraintById` + `RenameRelationInternal`; index name clash -> 42P07). Missing old
    /// name -> 42704 `constraint "x" for table "t" does not exist` (this PG message differs from
    /// DROP: `constraint \"%s\" for table \"%s\" does not exist`).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_constraint(&mut self, table: Oid, old_name: &str, new_name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-12", "RENAME CONSTRAINT"))
    }

    /// ALTER TABLE VALIDATE CONSTRAINT: only CHECK and FOREIGN KEY (`errors::constraint_not_fk_or_check`,
    /// 42809); already valid -> no-op. Sets `validated = true` and queues `ValidateCheck` /
    /// `ValidateForeignKey` (the executor raises 23514/23503 if rows violate; the statement then
    /// rolls back, restoring `validated = false`). Missing -> 42704 `constraint "c" of relation
    /// "t" does not exist`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn validate_constraint(&mut self, table: Oid, name: &str) -> Result<()> {
        Err(PgError::unimplemented("CAT-12", "VALIDATE CONSTRAINT"))
    }

    /// ALTER TABLE ALTER CONSTRAINT c [NOT] DEFERRABLE / INITIALLY ... / [NOT] ENFORCED: FK only for
    /// deferrability (`errors::constraint_not_foreign_key`, 42809); `initially_deferred` without
    /// `deferrable` -> 42601? (binder). Updates the flags.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_constraint(
        &mut self,
        table: Oid,
        name: &str,
        deferrable: Option<bool>,
        initially_deferred: Option<bool>,
        enforced: Option<bool>,
    ) -> Result<()> {
        Err(PgError::unimplemented("CAT-12", "ALTER CONSTRAINT"))
    }

    /// Find the UNIQUE/PRIMARY index of `ref_table` whose key column set equals `ref_attnums` (any
    /// order; non-partial, no expressions, no INCLUDE columns counted, valid). Prefers the primary
    /// key, then the lowest OID. Returns `(index_oid, ref_attnums reordered?)` - PG keeps the
    /// order given by the FK, so only the index OID is returned. None if no match.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn find_referenced_index(&mut self, ref_table: Oid, ref_attnums: &[i16]) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-12", "referenced index lookup"))
    }
}
