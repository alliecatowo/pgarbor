//! AST for the PostgreSQL statements that `sqlparser` 0.63 cannot parse. Plain data, fully
//! implemented, frozen (v1). Produced by `parser_extra::parse_extra` (lease SQL-02), bound by
//! `binder::utility` / `binder::ddl_alter_other` / `binder::ddl_objects`.
//!
//! `sqlparser`'s `Statement` has no variant for these PostgreSQL commands, and its parser turns
//! them into a generic "Expected: an SQL statement, found: REFRESH" error. PostgreSQL accepts them,
//! and ORMs/clients send some of them (`ALTER SEQUENCE .. OWNED BY` from `pg_dump`-style schemas,
//! `REFRESH MATERIALIZED VIEW`, `CHECKPOINT`, `SET CONSTRAINTS ALL DEFERRED`), so `parse()` first
//! looks at the leading keywords of every statement chunk and routes these to `parse_extra`,
//! which hand-parses them with `sqlparser::parser::Parser`'s public token helpers.
//!
//! Only statements PGArbor will (eventually) bind get a typed variant. Recognized PostgreSQL
//! commands PGArbor deliberately does not implement (`CREATE RULE`, `CREATE CAST`, `ALTER DEFAULT
//! PRIVILEGES`, `SECURITY LABEL`, `CREATE TABLESPACE`, `ALTER SYSTEM`, `CREATE EVENT TRIGGER`,
//! `CREATE FOREIGN TABLE`, `CREATE PUBLICATION`, `CREATE SUBSCRIPTION`, `CREATE STATISTICS`,
//! `CREATE LANGUAGE`, `CREATE ACCESS METHOD`, `CREATE TRANSFORM`, `IMPORT FOREIGN SCHEMA`,
//! `CREATE USER MAPPING`, `CREATE AGGREGATE`, `CREATE CONVERSION`, `ALTER DEFAULT PRIVILEGES`,
//! `SECURITY LABEL`, `PREPARE TRANSACTION`, `COMMIT PREPARED`, `ROLLBACK PREPARED`, `LOAD`,
//! `START`-less `DECLARE`...) become [`ExtraStatement::Unsupported`] so the binder reports 0A000
//! instead of a bogus 42601.
use sqlparser::ast::{Ident, ObjectName};

/// What `REINDEX` targets.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReindexKind {
    Index,
    Table,
    Schema,
    Database,
    System,
}

/// One action of `ALTER SEQUENCE`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterSequenceAction {
    /// `INCREMENT [BY] n`, `MINVALUE n | NO MINVALUE`, `MAXVALUE`, `START [WITH] n`, `RESTART [[WITH] n]`,
    /// `CACHE n`, `[NO] CYCLE`, `AS type`, `OWNED BY ...`, in written order. Numeric option values
    /// keep their text (`-5`, `9223372036854775807`) so the binder range-checks them as PG does.
    Options(Vec<SequenceOptionAst>),
    /// `SET { LOGGED | UNLOGGED }`.
    SetLogged(bool),
    /// `RENAME TO new`.
    RenameTo(Ident),
    /// `OWNER TO role` (role may be `CURRENT_USER`/`SESSION_USER` keywords kept as idents).
    OwnerTo(Ident),
    /// `SET SCHEMA schema`.
    SetSchema(Ident),
}

/// One `ALTER SEQUENCE` option.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum SequenceOptionAst {
    As(sqlparser::ast::DataType),
    IncrementBy(String),
    MinValue(Option<String>),
    MaxValue(Option<String>),
    StartWith(String),
    /// `RESTART` (None) / `RESTART WITH n` (Some).
    Restart(Option<String>),
    Cache(String),
    Cycle(bool),
    /// `OWNED BY NONE` = None; `OWNED BY t.c` = Some(name parts).
    OwnedBy(Option<ObjectName>),
}

/// One action of `ALTER DOMAIN name ...`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterDomainAction {
    SetDefault(sqlparser::ast::Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    /// `ADD [CONSTRAINT n] CHECK (expr) [NOT VALID]`.
    AddCheck { name: Option<Ident>, expr: sqlparser::ast::Expr, not_valid: bool },
    DropConstraint { name: Ident, if_exists: bool, cascade: bool },
    RenameConstraint { old: Ident, new: Ident },
    ValidateConstraint(Ident),
    RenameTo(Ident),
    OwnerTo(Ident),
    SetSchema(Ident),
}

/// One action of `ALTER DATABASE name ...`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterDatabaseAction {
    RenameTo(Ident),
    OwnerTo(Ident),
    /// `WITH [ALLOW_CONNECTIONS b] [CONNECTION LIMIT n] [IS_TEMPLATE b]` (any subset, any order).
    Options { allow_connections: Option<bool>, connection_limit: Option<i32>, is_template: Option<bool> },
    /// `SET guc { TO | = } value | FROM CURRENT` (`value` None) or `RESET guc` (`reset = true`).
    Set { name: String, value: Option<Vec<String>>, reset: bool },
}

/// Statements parsed by `parser_extra`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ExtraStatement {
    /// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`.
    RefreshMaterializedView { name: ObjectName, concurrently: bool, with_data: bool },
    /// `REINDEX [(options)] {INDEX|TABLE|SCHEMA|DATABASE|SYSTEM} [CONCURRENTLY] [name]`.
    Reindex { kind: ReindexKind, name: Option<ObjectName>, concurrently: bool },
    /// `CLUSTER [VERBOSE] [table [USING index]]`.
    Cluster { table: Option<ObjectName>, index: Option<Ident> },
    /// `CHECKPOINT`.
    Checkpoint,
    /// `SET CONSTRAINTS { ALL | name, ... } { DEFERRED | IMMEDIATE }` (`names` None = ALL).
    SetConstraints { names: Option<Vec<ObjectName>>, deferred: bool },
    /// `DO [LANGUAGE lang] 'code'` (code = the string literal contents, dollar-quotes removed).
    Do { language: Option<Ident>, code: String },
    /// `ALTER SEQUENCE [IF EXISTS] name action`.
    AlterSequence { if_exists: bool, name: ObjectName, action: AlterSequenceAction },
    /// `ALTER DOMAIN name action`.
    AlterDomain { name: ObjectName, action: AlterDomainAction },
    /// `ALTER DATABASE name action`.
    AlterDatabase { name: Ident, action: AlterDatabaseAction },
    /// `DROP OWNED BY role, ... [CASCADE|RESTRICT]`.
    DropOwned { roles: Vec<Ident>, cascade: bool },
    /// `REASSIGN OWNED BY old, ... TO new`.
    ReassignOwned { old_roles: Vec<Ident>, new_role: Ident },
    /// `ALTER MATERIALIZED VIEW [IF EXISTS] name RENAME TO new` / `SET SCHEMA` / `OWNER TO` (the only
    /// forms bound in v1); other actions are `Unsupported`.
    AlterMaterializedView { if_exists: bool, name: ObjectName, action: AlterMatViewAction },
    /// A PostgreSQL command PGArbor recognizes but does not implement: reported as 0A000
    /// `<feature> is not supported by PGArbor yet`. Examples of `feature`: `CREATE RULE`,
    /// `CREATE CAST`, `ALTER SYSTEM`, `ALTER DEFAULT PRIVILEGES`, `SECURITY LABEL`.
    Unsupported { feature: String },
}

/// Supported `ALTER MATERIALIZED VIEW` actions.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterMatViewAction {
    RenameTo(Ident),
    SetSchema(Ident),
    OwnerTo(Ident),
    /// `RENAME [COLUMN] old TO new`.
    RenameColumn { old: Ident, new: Ident },
}
