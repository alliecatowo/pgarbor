//! Per-relation statistics views: `pg_stat_{all,user,sys}_{tables,indexes}`,
//! `pg_statio_{all,user,sys}_{tables,indexes,sequences}`, `pg_stat_xact_{all,user,sys}_tables`.
//! Lease CAT-39. PGArbor does not collect these counters (documented gap): every counter column is
//! reported as zero / NULL exactly like a freshly created PG table that was never touched, so tools
//! that list tables with statistics (pgAdmin, DBeaver, Grafana dashboards) work. Row SETS are real:
//! - `*_tables`: one row per relation of relkind r, m, t, p: `relid, schemaname, relname` + the
//!   counters; `n_live_tup`/`n_dead_tup` etc. `Int8(0)`; timestamp columns NULL; `total_*_time`
//!   `Float8(0.0)`. `user` = relations outside pg_catalog/information_schema/pg_toast; `sys` = the rest;
//!   `all` = both.
//! - `*_indexes`: one row per index (`indexrelid, relid` = table, `schemaname, relname, indexrelname`),
//!   counters 0 / NULL. System indexes are omitted (not linked to tables, gap).
//! - `pg_statio_*_sequences`: `relid, schemaname, relname, blks_read 0, blks_hit 0`.
//! - `pg_stat_xact_*_tables`: same shape as `pg_stat_*_tables` minus the vacuum columns (use the
//!   column list of `builtin::system_rel`), zero counters.
//! Column names/types are authoritative in `builtin::SYSTEM_RELS`; build rows with `RowBuilder`
//! (unset = NULL) and set every non-nullable counter to 0.
//! A future epoch may feed real counters through `SessionInfo`; until then these relations must
//! not pretend to measure anything: this paragraph is the compatibility statement.
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_stat_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatAllTables;

impl VirtualRelation for PgStatAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ALL_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_all_tables rows"))
    }
}

/// Producer for `pg_stat_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserTables;

impl VirtualRelation for PgStatUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_user_tables rows"))
    }
}

/// Producer for `pg_stat_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysTables;

impl VirtualRelation for PgStatSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_sys_tables rows"))
    }
}

/// Producer for `pg_stat_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatAllIndexes;

impl VirtualRelation for PgStatAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ALL_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_all_indexes rows"))
    }
}

/// Producer for `pg_stat_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserIndexes;

impl VirtualRelation for PgStatUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_user_indexes rows"))
    }
}

/// Producer for `pg_stat_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysIndexes;

impl VirtualRelation for PgStatSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_sys_indexes rows"))
    }
}

/// Producer for `pg_statio_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllTables;

impl VirtualRelation for PgStatioAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_all_tables rows"))
    }
}

/// Producer for `pg_statio_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserTables;

impl VirtualRelation for PgStatioUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_user_tables rows"))
    }
}

/// Producer for `pg_statio_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysTables;

impl VirtualRelation for PgStatioSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_sys_tables rows"))
    }
}

/// Producer for `pg_statio_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllIndexes;

impl VirtualRelation for PgStatioAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_all_indexes rows"))
    }
}

/// Producer for `pg_statio_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserIndexes;

impl VirtualRelation for PgStatioUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_user_indexes rows"))
    }
}

/// Producer for `pg_statio_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysIndexes;

impl VirtualRelation for PgStatioSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_INDEXES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_sys_indexes rows"))
    }
}

/// Producer for `pg_statio_all_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllSequences;

impl VirtualRelation for PgStatioAllSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_SEQUENCES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_all_sequences rows"))
    }
}

/// Producer for `pg_statio_user_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserSequences;

impl VirtualRelation for PgStatioUserSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_SEQUENCES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_user_sequences rows"))
    }
}

/// Producer for `pg_statio_sys_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysSequences;

impl VirtualRelation for PgStatioSysSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_SEQUENCES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_statio_sys_sequences rows"))
    }
}

/// Producer for `pg_stat_xact_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactAllTables;

impl VirtualRelation for PgStatXactAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_ALL_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_xact_all_tables rows"))
    }
}

/// Producer for `pg_stat_xact_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactUserTables;

impl VirtualRelation for PgStatXactUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_USER_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_xact_user_tables rows"))
    }
}

/// Producer for `pg_stat_xact_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactSysTables;

impl VirtualRelation for PgStatXactSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_SYS_TABLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "pg_stat_xact_sys_tables rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &PgStatAllTables,
    &PgStatUserTables,
    &PgStatSysTables,
    &PgStatAllIndexes,
    &PgStatUserIndexes,
    &PgStatSysIndexes,
    &PgStatioAllTables,
    &PgStatioUserTables,
    &PgStatioSysTables,
    &PgStatioAllIndexes,
    &PgStatioUserIndexes,
    &PgStatioSysIndexes,
    &PgStatioAllSequences,
    &PgStatioUserSequences,
    &PgStatioSysSequences,
    &PgStatXactAllTables,
    &PgStatXactUserTables,
    &PgStatXactSysTables,
];
