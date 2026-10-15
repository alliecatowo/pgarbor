//! `pg_index` and `pg_constraint`. Lease CAT-31. PG sources: `catalog/pg_index.h`,
//! `pg_constraint.h`. Oracle-checked values.
//!
//! ## pg_index (user indexes only; builtin system indexes are not described by `pg_index` - documented gap)
//! One row per `Index`, ordered by `indexrelid`. Fast paths: eq on `indexrelid` (col 0) and
//! `indrelid` (col 1).
//! | column | value |
//! |---|---|
//! | indexrelid, indrelid | `oid`, `table` |
//! | indnatts / indnkeyatts | `keys.len()` / `n_key_columns` |
//! | indisunique, indnullsnotdistinct, indisprimary, indisexclusion, indimmediate, indisclustered, indisvalid | model flags |
//! | indcheckxmin | false |
//! | indisready / indislive | `valid` / true |
//! | indisreplident | `replident` |
//! | indkey | `d::int2vector(&index.indkey())` (0 for expression keys) |
//! | indcollation | `d::oidvector` of `IndexKey::collation` (0 for non-collatable; 100 for text) |
//! | indclass | `d::oidvector` of `IndexKey::opclass` (int4 1978, text 3126, ...) |
//! | indoption | `d::int2vector` of `IndexKey::indoption()` (DESC = 1, NULLS FIRST = 2) |
//! | indexprs | the expression texts (`StoredExpr::sql`) of expression keys joined with `, `, NULL if none (PG: a node-tree list; `pg_get_expr` returns the text) |
//! | indpred | predicate text or NULL |
//!
//! ## pg_constraint
//! One row per `Constraint` (table and domain constraints), ordered by `oid`. Fast paths: eq on
//! `oid` (0), `conrelid` (8), `contypid` (9), `conname` (1).
//! | column | value |
//! |---|---|
//! | oid, conname, connamespace | fields |
//! | contype | `ConstraintKind::as_char` |
//! | condeferrable, condeferred | fields |
//! | conenforced, convalidated | fields |
//! | conrelid, contypid, conindid, conparentid | `table`, `domain`, `index`, `parent` |
//! | confrelid | `fk.ref_table` or 0 |
//! | confupdtype, confdeltype, confmatchtype | FK: `FkAction::as_char` x2 and `FkMatch::as_char`; every OTHER constraint: `Char(b' ')` (a space - oracle) |
//! | conislocal, coninhcount | fields |
//! | connoinherit | `no_inherit` OR kind in {PrimaryKey, Unique, ForeignKey, Exclusion} (oracle: PK/UNIQUE/FK rows show `t`, CHECK and NOT NULL `f`) |
//! | conperiod | field |
//! | conkey | `d::int2_array(columns)`; NULL when `columns` is empty (domain constraints, constraints without columns) |
//! | confkey | FK `ref_columns` else NULL |
//! | conpfeqop, conppeqop, conffeqop | FK `pf_eq_ops`, `pp_eq_ops`, `ff_eq_ops` as `oid[]`, NULL when empty or not FK |
//! | confdelsetcols | `delete_set_columns` as `smallint[]` or NULL |
//! | conexclop | NULL |
//! | conbin | CHECK expression text (`check.sql`) or NULL |
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_index`.
#[derive(Clone, Copy, Debug)]
pub struct PgIndex;

impl VirtualRelation for PgIndex {
    fn oid(&self) -> Oid {
        relid::PG_INDEX
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-31", "pg_index rows"))
    }
}

/// Producer for `pg_constraint`.
#[derive(Clone, Copy, Debug)]
pub struct PgConstraint;

impl VirtualRelation for PgConstraint {
    fn oid(&self) -> Oid {
        relid::PG_CONSTRAINT
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-31", "pg_constraint rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgIndex, &PgConstraint];
