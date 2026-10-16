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
use sqlparser::ast::{Ident, ObjectName, OperateFunctionArg};

/// One option of `VACUUM` / `ANALYZE` (PG `DefElem` in `VacuumStmt.options`). `name` is upper-cased.
/// `value` is the value as written (a boolean word, a number with its sign, or a string's contents);
/// `None` when the option has no value.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct MaintenanceOption {
    pub name: String,
    pub value: Option<String>,
    /// 1-based character position of the option name (for 42601 errors).
    pub pos: Option<u32>,
}

/// Object kind named by `COMMENT ON FUNCTION`, `PROCEDURE` or `ROUTINE`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
    Routine,
}

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
    /// `LOGGED` (true) / `UNLOGGED` (false) inside a CREATE SEQUENCE option list. PG's grammar
    /// accepts them there and `init_params` rejects them (XX000 `option "logged" not recognized`).
    /// ALTER SEQUENCE never produces this variant (it reports 0A000 at parse time).
    Logged(bool),
    /// `SEQUENCE NAME n`: legal grammar, rejected for CREATE SEQUENCE (42601 `invalid sequence option
    /// SEQUENCE NAME`). Only produced for CREATE SEQUENCE.
    SequenceName,
}

/// `CREATE ROLE`, `CREATE USER` or `CREATE GROUP`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleStmtKind {
    Role,
    /// Implies `LOGIN` unless the options say otherwise.
    User,
    /// Same as `Role`.
    Group,
}

/// One role option (PG `CreateOptRoleElem` / `AlterOptRoleElem`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum RoleOptionAst {
    /// `[ENCRYPTED] PASSWORD 'x'` (`Some`) or `PASSWORD NULL` (`None`).
    Password(Option<String>),
    Superuser(bool),
    Inherit(bool),
    CreateRole(bool),
    CreateDb(bool),
    Login(bool),
    Replication(bool),
    BypassRls(bool),
    /// `CONNECTION LIMIT n`; the sign is folded in. Range checking (`< -1`) is the binder's.
    ConnectionLimit(i32),
    /// `VALID UNTIL 'timestamp'` (the text; the binder parses it).
    ValidUntil(String),
    /// `ROLE r, ...` or `USER r, ...` (same PG defname `rolemembers`).
    RoleMembers(Vec<Ident>),
    /// `IN ROLE r, ...` or `IN GROUP r, ...` (CREATE only).
    AddRoleTo(Vec<Ident>),
    /// `ADMIN r, ...` (CREATE only).
    AdminMembers(Vec<Ident>),
    /// `SYSID n` (CREATE only, ignored by PG with a NOTICE).
    Sysid,
}

/// A role option with the 1-based character position of its first token (for 42601 conflicts).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RoleOptionItem {
    pub option: RoleOptionAst,
    pub pos: Option<u32>,
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
    /// `VACUUM` (`vacuum` true) or `ANALYZE` written with an option list `(...)` or a legacy option
    /// keyword (`VACUUM FULL|FREEZE|VERBOSE|ANALYZE`, `ANALYZE VERBOSE`), then `[table [(column, ...)]]`.
    /// Legacy keywords become options without a value, as PG's grammar does.
    Maintenance { vacuum: bool, options: Vec<MaintenanceOption>, table: Option<ObjectName>, columns: Vec<Ident> },
    /// `COMMENT ON {FUNCTION|PROCEDURE|ROUTINE} name [(argument list)] IS {'text' | NULL}`. `args` is
    /// `None` when the argument list was omitted; argument modes and names are kept (the binder ignores
    /// them for identity).
    CommentRoutine { kind: RoutineKind, name: ObjectName, args: Option<Vec<OperateFunctionArg>>, comment: Option<String> },
    /// `SET CONSTRAINTS { ALL | name, ... } { DEFERRED | IMMEDIATE }` (`names` None = ALL).
    SetConstraints { names: Option<Vec<ObjectName>>, deferred: bool },
    /// `DO [LANGUAGE lang] 'code'` (code = the string literal contents, dollar-quotes removed).
    Do { language: Option<Ident>, code: String },
    /// `CREATE [TEMP|TEMPORARY|UNLOGGED] SEQUENCE [IF NOT EXISTS] name [options in any order]`.
    CreateSequence { temporary: bool, unlogged: bool, if_not_exists: bool, name: ObjectName, options: Vec<SequenceOptionAst> },
    /// `CREATE {ROLE|USER|GROUP} name [WITH] [options in any order]`.
    CreateRole { kind: RoleStmtKind, name: Ident, options: Vec<RoleOptionItem> },
    /// `ALTER {ROLE|USER} name [WITH] [options in any order]` (the option-list form only; RENAME,
    /// SET, RESET and IN DATABASE stay with sqlparser).
    AlterRole { name: Ident, options: Vec<RoleOptionItem> },
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
    /// `ALTER INDEX [IF EXISTS] name action [, action ...]` for the storage-parameter and tablespace
    /// subcommands (`SET (..)`, `RESET (..)`, `SET TABLESPACE`). `RENAME TO` stays with sqlparser.
    AlterIndex { if_exists: bool, name: ObjectName, actions: Vec<RelOptionAction> },
    /// `ALTER INDEX ALL IN TABLESPACE from SET TABLESPACE to [NOWAIT]`.
    AlterIndexAllInTablespace { from: Ident, to: Ident },
    /// `ALTER VIEW [IF EXISTS] name action [, action ...]` with `SET (..)` / `RESET (..)`.
    AlterView { if_exists: bool, name: ObjectName, actions: Vec<RelOptionAction> },
    /// `ALTER VIEW [IF EXISTS] name ALTER [COLUMN] c SET DEFAULT e | DROP DEFAULT` (PG `AT_ColumnDefault`
    /// on a view; `default` is `None` for DROP DEFAULT). sqlparser 0.63 parses only `ALTER VIEW .. AS`.
    AlterViewColumnDefault { if_exists: bool, name: ObjectName, column: Ident, default: Option<sqlparser::ast::Expr> },
    /// `ALTER TYPE name SET SCHEMA new` (`RENAME TO` stays with sqlparser).
    AlterTypeSetSchema { name: ObjectName, schema: Ident },
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

/// One subcommand of `ALTER INDEX` / `ALTER VIEW` (PG `alter_table_cmds` subset), in source order.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum RelOptionAction {
    /// `SET (name [= value], ...)` as `name=value` strings; a bare name is kept as `name` (PG reads
    /// it as the boolean `true`). Values follow PG `def_arg`: strings as written, numbers with sign,
    /// unquoted words case-folded.
    SetOptions(Vec<String>),
    /// `RESET (name, ...)`.
    ResetOptions(Vec<String>),
    /// `SET TABLESPACE name` (indexes only).
    SetTablespace(Ident),
}
