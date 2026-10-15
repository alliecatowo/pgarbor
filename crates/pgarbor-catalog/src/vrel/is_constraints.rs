//! information_schema, part 3 (constraints): `table_constraints`, `key_column_usage`,
//! `referential_constraints`, `constraint_column_usage`, `constraint_table_usage`,
//! `check_constraints`. Lease CAT-42. Authoritative semantics: the CREATE VIEW of each name in PG
//! 18.6 `information_schema.sql` (e.g. `check_constraints` at line ~431); visibility / conventions:
//! `is_core.rs`. Oracle facts (PG 18.6) for tables `p(id pk, b unique, x check)` and
//! `c(a not null references p(id) on delete cascade, n, constraint cc check (n<5), unique(a,n))`:
//! - **table_constraints** (`constraint_catalog/schema/name, table_catalog/schema/name,
//!   constraint_type, is_deferrable, initially_deferred, enforced, nulls_distinct`): PRIMARY KEY, UNIQUE,
//!   FOREIGN KEY, CHECK, and NOT NULL constraints reported as `CHECK` (`ConstraintKind::info_schema_type`):
//!   rows `c_a_fkey FOREIGN KEY`, `c_a_n_key UNIQUE`, `c_a_not_null CHECK`, `cc CHECK`, `p_b_key UNIQUE`,
//!   `p_id_not_null CHECK`, `p_pkey PRIMARY KEY`, `p_x_check CHECK`; `enforced` `YES`/`NO`;
//!   `nulls_distinct` `YES`/`NO` for UNIQUE (NULL otherwise).
//! - **key_column_usage**: PK/UNIQUE/FK constraints only, one row per key column in key order:
//!   `constraint_*, table_*, column_name, ordinal_position` (1-based), `position_in_unique_constraint`
//!   (FK only: position of the referenced column in the referenced key, 1-based; NULL otherwise).
//!   Oracle: `c_a_fkey | c | a | 1 | 1`, `c_a_n_key | c | a | 1 | NULL`, `c_a_n_key | c | n | 2 | NULL`.
//! - **referential_constraints**: FK constraints: `constraint_*`, `unique_constraint_catalog/schema/
//!   name` = the referenced PK/UNIQUE constraint (the constraint owning `fk.ref_index`; NULL if that
//!   index has no constraint), `match_option` (`NONE`/`FULL`/`PARTIAL`: `FkMatch::info_schema`),
//!   `update_rule`, `delete_rule` (`FkAction::sql`). Oracle: `c_a_fkey | p_pkey | NONE | NO ACTION | CASCADE`.
//! - **constraint_column_usage**: columns used by constraints, from the viewpoint of the table that
//!   OWNS the column: for CHECK/NOT NULL/PK/UNIQUE the constrained columns; for FOREIGN KEY the
//!   REFERENCED columns (oracle: `p | id | c_a_fkey`). Columns: `table_catalog/schema/name,
//!   column_name, constraint_catalog/schema/name`. Domain constraints are not listed.
//! - **constraint_table_usage**: tables that carry a constraint, from the table's viewpoint, for
//!   PK/UNIQUE/FK only (FK -> the REFERENCED table; oracle: `p | c_a_fkey`, no CHECK rows): `table_*,
//!   constraint_*`.
//! - **check_constraints**: CHECK constraints and NOT NULL constraints and DOMAIN checks:
//!   `constraint_catalog/schema/name, check_clause` = `check.sql` text (`(n < 5)`, `(VALUE > 0)`) or
//!   `<column> IS NOT NULL` / `VALUE IS NOT NULL` for NOT NULL constraints (oracle: `c_a_not_null |
//!   a IS NOT NULL`). All visible by `pg_has_role(owner of table/domain, 'USAGE')`.
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `information_schema.table_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsTableConstraints;

impl VirtualRelation for IsTableConstraints {
    fn oid(&self) -> Oid {
        relid::IS_TABLE_CONSTRAINTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.table_constraints rows"))
    }
}

/// Producer for `information_schema.key_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsKeyColumnUsage;

impl VirtualRelation for IsKeyColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_KEY_COLUMN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.key_column_usage rows"))
    }
}

/// Producer for `information_schema.referential_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsReferentialConstraints;

impl VirtualRelation for IsReferentialConstraints {
    fn oid(&self) -> Oid {
        relid::IS_REFERENTIAL_CONSTRAINTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.referential_constraints rows"))
    }
}

/// Producer for `information_schema.constraint_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsConstraintColumnUsage;

impl VirtualRelation for IsConstraintColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_CONSTRAINT_COLUMN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.constraint_column_usage rows"))
    }
}

/// Producer for `information_schema.constraint_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsConstraintTableUsage;

impl VirtualRelation for IsConstraintTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_CONSTRAINT_TABLE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.constraint_table_usage rows"))
    }
}

/// Producer for `information_schema.check_constraints`.
#[derive(Clone, Copy, Debug)]
pub struct IsCheckConstraints;

impl VirtualRelation for IsCheckConstraints {
    fn oid(&self) -> Oid {
        relid::IS_CHECK_CONSTRAINTS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-42", "information_schema.check_constraints rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsTableConstraints,
    &IsKeyColumnUsage,
    &IsReferentialConstraints,
    &IsConstraintColumnUsage,
    &IsConstraintTableUsage,
    &IsCheckConstraints,
];
