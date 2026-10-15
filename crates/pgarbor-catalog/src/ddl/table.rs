//! CREATE TABLE. Lease CAT-11 (sonnet: many interacting pieces). PG sources:
//! `commands/tablecmds.c` (`DefineRelation`, `MergeAttributes`, `AddRelationNewConstraints` via
//! heap.c), `catalog/heap.c` (`heap_create_with_catalog`, `AddNewAttributeTuples`,
//! `StoreAttrDefault`, `CheckAttributeNamesTypes`), `parser/parse_utilcmd.c` (`transformColumnDefinition`:
//! serial and identity expansion, `generateSerialExtraStmts`), `catalog/pg_type.c` (`TypeCreate`,
//! `makeArrayTypeName`).
//!
//! ## Algorithm of [`CatalogTxn::create_table`]
//! 1. **Pre-checks (no writes):**
//!    - `spec.kind` must be `Table`; `PartitionedTable`, non-empty `inherits`, and
//!      `Persistence::Temp` -> `Err(PgError::unimplemented("CAT-11", "<what>"))` (partitioning,
//!      inheritance and temp schemas are later work).
//!    - `check_creatable_namespace`, `require_schema_create(namespace)`,
//!      `check_new_relation_name(namespace, name, if_not_exists)` (clash with relations -> 42P07;
//!      with types -> 42710; with `if_not_exists` -> NOTICE and `Ok(None)`).
//!    - `common::validate_column_specs`.
//!    - Constraint list sanity (`ConstraintSpec`): at most ONE primary key, else 42P16
//!      `multiple primary keys for table "t" are not allowed`; two constraints with the same
//!      explicit name -> 42710 `check constraint "c" already exists` for CHECKs
//!      (`errors::check_constraint_already_exists`), `constraint "c" for relation "t" already exists`
//!      otherwise; key columns must exist (42703 `column "x" named in key does not exist`,
//!      `errors::key_column_does_not_exist`); duplicate key columns -> 42701 `column "a" appears twice
//!      in unique constraint` / `primary key constraint` (`errors::column_appears_twice`).
//! 2. **OIDs and names:** table OID, rowtype OID, array type OID (all `new_oid`). The array type name
//!    is `naming::choose_array_type_name(snapshot, ns, name, &[])` (stored in
//!    `Relation::array_type_name` only when not canonical).
//! 3. **Columns** (attnum = position + 1; column count <= 1600): collation = explicit or the type's
//!    `TypeInfo::collation` (via `env.type_info`); `ndims` copied.
//!    - `serial`: the column type is `serial.type_oid()`, NOT NULL (+ constraint), default
//!      `nextval('<seq>'::regclass)` where `<seq>` = `naming::choose_relation_name(snapshot, ns,
//!      table, Some(col), "seq", &chosen)` (`t_id_seq`); the sequence is created with
//!      `Sequence{type_oid = column type, owned_by = (table, attnum), identity = false}` (AUTO
//!      dependency on the column) and `SequenceOptions::default()`. The default's `StoredExpr` is
//!      `nextval('t_id_seq'::regclass)` with `depends_on = [ObjectAddress::relation(seq)]`
//!      (regclass literal printed schema-qualified only if the sequence's schema is not visible in
//!      the creating session's search_path - PG prints the literal at deparse time; PGArbor fixes the
//!      text at creation: documented divergence, see contracts/crates/pgarbor-catalog.md).
//!    - `identity`: `Column::identity = Some(Identity{kind, sequence})`, NOT NULL (+ constraint
//!      `t_d_not_null`), no default; sequence `t_d_seq` with `identity = true` and the user's
//!      `SequenceOptions` (`ddl::sequence` validates; identity sequences take their type from the
//!      column unless `AS` given).
//!    - `generated`: `Column::generated = Some(kind)` and the expression stored in `Column::default`
//!      (`ColumnDefault{oid: new_oid, expr}`), as PG stores it in pg_attrdef.
//!    - plain `default`: `ColumnDefault{oid: new_oid, expr: bind_self_deps(expr, table_oid)}`.
//!    - `not_null`: `Column::not_null = true` and a `NotNull` constraint named
//!      `<table>_<col>_not_null` (or the given `CONSTRAINT name NOT NULL`).
//!    The rowtype of the table is implicit: `Relation::{rowtype, array_type}` hold the OIDs and
//!    `CatalogSnapshot::build` derives the pg_type rows.
//! 4. Build and `put` the `Relation` (kind Table, owner, persistence, options, `view = None`,
//!    `replica_identity = Default`).
//! 5. **Sequences** for serial/identity columns are created AFTER the table row exists (their
//!    `owned_by` must reference it) with `create_sequence` (in column order); then
//!    `update_relation` sets `Column::identity.sequence`. Each sequence queues
//!    `FollowUp::CreateSequenceState`.
//! 6. **Constraints** in PG's creation order: all CHECKs (in spec order), then NOT NULL constraints
//!    (columns in attnum order: explicit/implicit/identity/serial, then PRIMARY KEY columns that
//!    lack one - oracle: `create table t(id int primary key)` creates `t_id_not_null` too), then
//!    PRIMARY KEY / UNIQUE (each creates its backing index), then FOREIGN KEYs. Implemented by
//!    `CatalogTxn::add_constraints_to_new_table` (ddl::constraint, lease CAT-12) which this function
//!    calls once.
//! 7. Records `created` for the table, sequences, indexes, constraints (via the callee) and returns
//!    [`CreatedTable`]. No `BuildIndex`/validation follow-ups: the table is empty.
//!
//! ## Oracle-verified details (PG 18.6)
//! - Objects created for `create table t(id serial primary key, a int not null, b text unique,
//!   c int check (c>0), d int generated always as identity, e int generated always as (a*2) stored,
//!   f int references t(id) on delete cascade)`:
//!   relations `t_id_seq` (S), `t_d_seq` (S), `t` (r), `t_pkey` (i), `t_b_key` (i); constraints
//!   `t_c_check` (c, conkey {4}), `t_id_not_null`, `t_a_not_null`, `t_d_not_null` (n), `t_pkey` (p),
//!   `t_b_key` (u), `t_f_fkey` (f); `attnotnull`: id, a, d true; e false (generated column not
//!   NOT NULL); `attidentity` d = 'a'; `attgenerated` e = 's'.
//! - Errors: duplicate column `column "a" specified more than once` (42701); `xmin` as a column
//!   name -> 42701 `column name "xmin" conflicts with a system column name`; unknown referenced
//!   table is the binder's 42P01.
use super::spec::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// CREATE TABLE (module docs). `Ok(None)` only for `IF NOT EXISTS` on an existing relation.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_table(&mut self, spec: &CreateTableSpec) -> Result<Option<CreatedTable>> {
        Err(PgError::unimplemented("CAT-11", "CREATE TABLE"))
    }
}
