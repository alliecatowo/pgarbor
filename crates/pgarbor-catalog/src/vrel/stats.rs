//! Statistics relation schemas are exposed, but scans fail closed until counters are collected.
//! Lease CAT-39. See accepted amendment pgarbor-catalog-07-statistics-fail-closed.
//! Returning synthetic zero counters would misrepresent an unmeasured database.
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
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserTables;

impl VirtualRelation for PgStatUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysTables;

impl VirtualRelation for PgStatSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatAllIndexes;

impl VirtualRelation for PgStatAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_ALL_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatUserIndexes;

impl VirtualRelation for PgStatUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_USER_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatSysIndexes;

impl VirtualRelation for PgStatSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STAT_SYS_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllTables;

impl VirtualRelation for PgStatioAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserTables;

impl VirtualRelation for PgStatioUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysTables;

impl VirtualRelation for PgStatioSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_all_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllIndexes;

impl VirtualRelation for PgStatioAllIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_user_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserIndexes;

impl VirtualRelation for PgStatioUserIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_sys_indexes`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysIndexes;

impl VirtualRelation for PgStatioSysIndexes {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_INDEXES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_all_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioAllSequences;

impl VirtualRelation for PgStatioAllSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_ALL_SEQUENCES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_user_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioUserSequences;

impl VirtualRelation for PgStatioUserSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_USER_SEQUENCES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_statio_sys_sequences`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatioSysSequences;

impl VirtualRelation for PgStatioSysSequences {
    fn oid(&self) -> Oid {
        relid::PG_STATIO_SYS_SEQUENCES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_xact_all_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactAllTables;

impl VirtualRelation for PgStatXactAllTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_ALL_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_xact_user_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactUserTables;

impl VirtualRelation for PgStatXactUserTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_USER_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
    }
}

/// Producer for `pg_stat_xact_sys_tables`.
#[derive(Clone, Copy, Debug)]
pub struct PgStatXactSysTables;

impl VirtualRelation for PgStatXactSysTables {
    fn oid(&self) -> Oid {
        relid::PG_STAT_XACT_SYS_TABLES
    }
    fn scan(&self, _ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-39", "statistics counter collection"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use crate::env::CatalogEnv;
    use crate::model::{Persistence, RelKind};
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::{IoConfig, TypeId};

    /// `CREATE TABLE <name> (id int4 [, PRIMARY KEY (id)])` in `public`.
    fn table_spec(name: &str, primary_key: bool) -> CreateTableSpec {
        let constraints = if primary_key {
            vec![ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
                columns: vec!["id".to_string()],
                include: Vec::new(),
            })]
        } else {
            Vec::new()
        };
        CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("id", TypeId { oid: 23, typmod: -1 })],
            constraints,
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    fn scan_rows(env: &CatalogEnv, oid: Oid) -> Result<Vec<Row>> {
        let io = IoConfig::default();
        let ctx = ScanContext { env, session: &NullSessionInfo, io_config: &io };
        crate::vrel::scan(&ctx, oid, &ScanRequest::all())
    }


    #[test]
    fn every_statistics_scan_fails_closed_with_or_without_relations() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, with_table) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1", true)).map(|_| ()))?;
        for env in [&env, &with_table] {
            for relation in RELATIONS {
                assert!(!relation.schema()?.columns.is_empty());
                let error = scan_rows(env, relation.oid()).expect_err("uncollected counters must not produce rows");
                assert_eq!(error.sqlstate.code(), "0A000");
                assert!(error.detail.as_deref().unwrap_or_default().contains("CAT-39"));
            }
        }
        Ok(())
    }
}
