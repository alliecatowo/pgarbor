//! information_schema, part 4 (routines and dependencies): `routines`, `parameters`,
//! `routine_column_usage`, `routine_routine_usage`, `routine_sequence_usage`, `routine_table_usage`,
//! `view_column_usage`, `view_table_usage`, `view_routine_usage`, `check_constraint_routine_usage`.
//! Lease CAT-43. Authoritative semantics: the CREATE VIEW of each name in PG 18.6
//! `information_schema.sql`; conventions and visibility: `is_core.rs`; type columns:
//! `is_columns::describe_type_for_info_schema`.
//!
//! - **routines**: user functions/procedures AND every builtin `pg_proc` row (oracle: 3413 rows,
//!   `WHERE pg_has_role(proowner,'USAGE') OR has_function_privilege(oid,'EXECUTE')`; PUBLIC has
//!   EXECUTE by default). Includes aggregates/window functions (the view has no `prokind` filter; only
//!   `routine_type` is NULL for them: `CASE prokind WHEN 'f' THEN 'FUNCTION' WHEN 'p' THEN 'PROCEDURE'`).
//!   Oracle values for `create function f(x int, y text default 'k') returns int language sql immutable
//!   strict as 'select 1'`: `specific_name` `f_<oid>` (name + `_` + pg_proc OID), `routine_name` `f`,
//!   `routine_type` `FUNCTION`, `data_type` `integer` (NULL for procedures), type-detail columns of the
//!   return type EXCEPT that `numeric_precision`, `character_maximum_length`, ... are NULL and only
//!   `type_udt_catalog/schema/name` (`postgres`/`pg_catalog`/`int4`) are filled, `dtd_identifier` `0`
//!   (NULL for procedures), `routine_body` `SQL` for language sql else `EXTERNAL`, `routine_definition`
//!   = `prosrc` for `sql`/`plpgsql` (NULL for builtin internal functions? oracle: builtin rows have
//!   `routine_definition` non-NULL = prosrc), `external_name` NULL (user SQL) / `prosrc` for non-SQL
//!   non-plpgsql, `external_language` = `upper(lanname)` (`SQL`, `PLPGSQL`, `INTERNAL`, `C`),
//!   `parameter_style` `GENERAL`, `is_deterministic` `YES` iff `provolatile = 'i'`, `sql_data_access`
//!   the constant `MODIFIES`, `is_null_call` `YES`/`NO` from strict (NULL for procedures),
//!   `schema_level_routine` `YES`, `max_dynamic_result_sets` 0, `security_type` `DEFINER`/`INVOKER`,
//!   `as_locator` `NO`, `is_udt_dependent` `NO`, every `result_cast_*` NULL. Port the full CASE
//!   expressions from the view (line ~1546).
//! - **parameters**: one row per declared parameter (`proargnames`/`proargmodes` expanded): `specific_*`,
//!   `ordinal_position`, `parameter_mode` (`IN`, `OUT`, `INOUT`), `is_result` `NO`, `as_locator` `NO`,
//!   `parameter_name` (NULL when unnamed), type columns, `parameter_default` (the default expr text),
//!   `dtd_identifier`. TABLE columns appear as OUT. VARIADIC shows `IN`.
//! - Usage views: `view_table_usage` / `view_column_usage` / `view_routine_usage` read
//!   `Relation::view.query.depends_on` of each view (table/column/function addresses);
//!   `routine_*_usage` and `check_constraint_routine_usage` read `Function::depends_on` and
//!   `Constraint::check.depends_on` for routine addresses (in v1 only SQL-standard `BEGIN ATOMIC`
//!   bodies and check expressions can reference routines; most are empty but MUST be computed, not
//!   assumed empty). `view_*` rows: `view_catalog/schema/name, table_catalog/schema/name[, column_name]`
//!   restricted to objects the current user owns (`pg_has_role(owner, 'USAGE')`).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `information_schema.routines`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutines;

impl VirtualRelation for IsRoutines {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.routines rows"))
    }
}

/// Producer for `information_schema.parameters`.
#[derive(Clone, Copy, Debug)]
pub struct IsParameters;

impl VirtualRelation for IsParameters {
    fn oid(&self) -> Oid {
        relid::IS_PARAMETERS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.parameters rows"))
    }
}

/// Producer for `information_schema.routine_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineColumnUsage;

impl VirtualRelation for IsRoutineColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_COLUMN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.routine_column_usage rows"))
    }
}

/// Producer for `information_schema.routine_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineRoutineUsage;

impl VirtualRelation for IsRoutineRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_ROUTINE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.routine_routine_usage rows"))
    }
}

/// Producer for `information_schema.routine_sequence_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineSequenceUsage;

impl VirtualRelation for IsRoutineSequenceUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_SEQUENCE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.routine_sequence_usage rows"))
    }
}

/// Producer for `information_schema.routine_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineTableUsage;

impl VirtualRelation for IsRoutineTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_TABLE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.routine_table_usage rows"))
    }
}

/// Producer for `information_schema.view_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewColumnUsage;

impl VirtualRelation for IsViewColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_COLUMN_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.view_column_usage rows"))
    }
}

/// Producer for `information_schema.view_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewTableUsage;

impl VirtualRelation for IsViewTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_TABLE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.view_table_usage rows"))
    }
}

/// Producer for `information_schema.view_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewRoutineUsage;

impl VirtualRelation for IsViewRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_ROUTINE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.view_routine_usage rows"))
    }
}

/// Producer for `information_schema.check_constraint_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsCheckConstraintRoutineUsage;

impl VirtualRelation for IsCheckConstraintRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_CHECK_CONSTRAINT_ROUTINE_USAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-43", "information_schema.check_constraint_routine_usage rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsRoutines,
    &IsParameters,
    &IsRoutineColumnUsage,
    &IsRoutineRoutineUsage,
    &IsRoutineSequenceUsage,
    &IsRoutineTableUsage,
    &IsViewColumnUsage,
    &IsViewTableUsage,
    &IsViewRoutineUsage,
    &IsCheckConstraintRoutineUsage,
];
