//! Exhaustive classification of every `sqlparser` 0.63 `Statement` variant (SQL-14, "unsupported
//! syntax inventory"). Fully implemented, frozen (v1).
//!
//! PostgreSQL's grammar accepts a fixed set of statements. `sqlparser` parses the union of many
//! dialects, so after parsing we must decide, for EVERY variant, which of three things happens. There
//! is no `_ =>` arm: adding a variant on a sqlparser upgrade fails the build here until it is
//! classified (the "none ignored" rule).
//!
//! * [`StmtClass::Handled`]: the binder (`binder::dispatch`) binds it; the family selects the binder module.
//! * [`StmtClass::Unsupported`]: PostgreSQL accepts it, PGArbor does not (yet): 0A000
//!   `<feature> is not supported by PGArbor yet` (cursors, triggers, policies, MERGE, extensions,
//!   operator DDL, ...). The feature text is part of the inventory.
//! * [`StmtClass::NotPostgres`]: syntax of another database (MySQL `SHOW TABLES`/`USE`, Snowflake
//!   stages, MSSQL `RAISERROR`, DuckDB `ATTACH`, T-SQL procedural statements, ...). PostgreSQL's own
//!   parser rejects these with 42601 `syntax error at or near "<first token>"`, NOT 0A000; reporting
//!   0A000 would be a SQLSTATE mismatch in differential tests. `check_supported` raises that.
//!   (Known divergence: PG parses `SHOW tables` as the GUC `tables` and answers 42704 `unrecognized
//!   configuration parameter "tables"`; sqlparser's PostgreSqlDialect also produces `ShowVariable`
//!   for it, which is `Handled` and gets that exact error from the SHOW binder.)
//!
//! Statements PostgreSQL has but `sqlparser` lacks are in `extra_stmt`.
use sqlparser::ast::Statement;

use pgarbor_common::Result;

use crate::diag;
use crate::parser::Parsed;
use crate::extra_stmt::ExtraStatement;

/// The binder family of a handled statement.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StmtFamily {
    /// ALTER SCHEMA/FUNCTION/TYPE/ROLE/USER (everything but ALTER TABLE/VIEW/INDEX)
    AlterOther,
    /// ALTER TABLE / ALTER VIEW / ALTER INDEX
    AlterTable,
    /// CALL proc(args)
    Call,
    /// COMMENT ON
    Comment,
    /// COPY
    Copy,
    /// CREATE DATABASE
    CreateDatabase,
    /// CREATE DOMAIN
    CreateDomain,
    /// CREATE FUNCTION / PROCEDURE
    CreateFunction,
    /// CREATE INDEX
    CreateIndex,
    /// CREATE ROLE / USER
    CreateRole,
    /// CREATE SCHEMA
    CreateSchema,
    /// CREATE SEQUENCE
    CreateSequence,
    /// CREATE TABLE (incl. AS query)
    CreateTable,
    /// CREATE TYPE (enum, composite)
    CreateType,
    /// CREATE [MATERIALIZED] VIEW
    CreateView,
    /// DEALLOCATE
    Deallocate,
    /// DELETE
    Delete,
    /// DISCARD
    Discard,
    /// DROP of every object kind, incl. DropFunction/DropDomain/DropProcedure
    Drop,
    /// EXECUTE
    Execute,
    /// EXPLAIN
    Explain,
    /// GRANT, REVOKE
    Grant,
    /// INSERT
    Insert,
    /// LOCK TABLE
    Lock,
    /// VACUUM, ANALYZE
    Maintenance,
    /// LISTEN, UNLISTEN, NOTIFY
    Notify,
    /// PREPARE
    Prepare,
    /// SELECT / VALUES / WITH ... (and SELECT INTO)
    Query,
    /// SET (all forms) and RESET
    Set,
    /// SHOW name / SHOW ALL
    Show,
    /// BEGIN/START TRANSACTION, COMMIT/END, ROLLBACK/ABORT, SAVEPOINT, RELEASE (SET TRANSACTION arrives as `Set`)
    Transaction,
    /// TRUNCATE
    Truncate,
    /// UPDATE
    Update,
    /// MERGE
    Merge,
    /// CREATE / ALTER / DROP POLICY
    Policy,
    /// Any `ExtraStatement` other than `Unsupported`.
    Extra,
}

/// Result of [`classify`].
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmtClass {
    Handled(StmtFamily),
    /// PostgreSQL syntax PGArbor does not implement; the text names the feature.
    Unsupported(&'static str),
    /// Not PostgreSQL syntax at all.
    NotPostgres,
}

/// Classify a `sqlparser` statement (exhaustive match, no wildcard arm).
pub fn classify(stmt: &Statement) -> StmtClass {
    match stmt {
        // Handled
        Statement::Analyze { .. } => StmtClass::Handled(StmtFamily::Maintenance),
        Statement::Set { .. } => StmtClass::Handled(StmtFamily::Set),
        Statement::Truncate { .. } => StmtClass::Handled(StmtFamily::Truncate),
        Statement::Query { .. } => StmtClass::Handled(StmtFamily::Query),
        Statement::Insert { .. } => StmtClass::Handled(StmtFamily::Insert),
        Statement::Call { .. } => StmtClass::Handled(StmtFamily::Call),
        Statement::Copy { .. } => StmtClass::Handled(StmtFamily::Copy),
        Statement::Update { .. } => StmtClass::Handled(StmtFamily::Update),
        Statement::Delete { .. } => StmtClass::Handled(StmtFamily::Delete),
        Statement::CreateView { .. } => StmtClass::Handled(StmtFamily::CreateView),
        Statement::CreateTable { .. } => StmtClass::Handled(StmtFamily::CreateTable),
        Statement::CreateIndex { .. } => StmtClass::Handled(StmtFamily::CreateIndex),
        Statement::CreateRole { .. } => StmtClass::Handled(StmtFamily::CreateRole),
        Statement::AlterTable { .. } => StmtClass::Handled(StmtFamily::AlterTable),
        Statement::AlterSchema { .. } => StmtClass::Handled(StmtFamily::AlterOther),
        Statement::AlterIndex { .. } => StmtClass::Handled(StmtFamily::AlterTable),
        Statement::AlterView { .. } => StmtClass::Handled(StmtFamily::AlterTable),
        Statement::AlterFunction { .. } => StmtClass::Handled(StmtFamily::AlterOther),
        Statement::AlterType { .. } => StmtClass::Handled(StmtFamily::AlterOther),
        Statement::AlterRole { .. } => StmtClass::Handled(StmtFamily::AlterOther),
        Statement::Drop { .. } => StmtClass::Handled(StmtFamily::Drop),
        Statement::DropFunction { .. } => StmtClass::Handled(StmtFamily::Drop),
        Statement::DropDomain { .. } => StmtClass::Handled(StmtFamily::Drop),
        Statement::DropProcedure { .. } => StmtClass::Handled(StmtFamily::Drop),
        Statement::Discard { .. } => StmtClass::Handled(StmtFamily::Discard),
        Statement::ShowVariable { .. } => StmtClass::Handled(StmtFamily::Show),
        Statement::StartTransaction { .. } => StmtClass::Handled(StmtFamily::Transaction),
        Statement::Comment { .. } => StmtClass::Handled(StmtFamily::Comment),
        Statement::Commit { .. } => StmtClass::Handled(StmtFamily::Transaction),
        Statement::Rollback { .. } => StmtClass::Handled(StmtFamily::Transaction),
        Statement::CreateSchema { .. } => StmtClass::Handled(StmtFamily::CreateSchema),
        Statement::CreateDatabase { .. } => StmtClass::Handled(StmtFamily::CreateDatabase),
        Statement::CreateFunction { .. } => StmtClass::Handled(StmtFamily::CreateFunction),
        Statement::CreateProcedure { .. } => StmtClass::Handled(StmtFamily::CreateFunction),
        Statement::Grant { .. } => StmtClass::Handled(StmtFamily::Grant),
        Statement::Revoke { .. } => StmtClass::Handled(StmtFamily::Grant),
        Statement::Deallocate { .. } => StmtClass::Handled(StmtFamily::Deallocate),
        Statement::Execute { .. } => StmtClass::Handled(StmtFamily::Execute),
        Statement::Prepare { .. } => StmtClass::Handled(StmtFamily::Prepare),
        Statement::Explain { .. } => StmtClass::Handled(StmtFamily::Explain),
        Statement::Savepoint { .. } => StmtClass::Handled(StmtFamily::Transaction),
        Statement::ReleaseSavepoint { .. } => StmtClass::Handled(StmtFamily::Transaction),
        Statement::CreateSequence { .. } => StmtClass::Handled(StmtFamily::CreateSequence),
        Statement::CreateDomain { .. } => StmtClass::Handled(StmtFamily::CreateDomain),
        Statement::CreateType { .. } => StmtClass::Handled(StmtFamily::CreateType),
        Statement::Lock { .. } => StmtClass::Handled(StmtFamily::Lock),
        Statement::LISTEN { .. } => StmtClass::Handled(StmtFamily::Notify),
        Statement::UNLISTEN { .. } => StmtClass::Handled(StmtFamily::Notify),
        Statement::NOTIFY { .. } => StmtClass::Handled(StmtFamily::Notify),
        Statement::CreateUser { .. } => StmtClass::Handled(StmtFamily::CreateRole),
        Statement::AlterUser { .. } => StmtClass::Handled(StmtFamily::AlterOther),
        Statement::Vacuum { .. } => StmtClass::Handled(StmtFamily::Maintenance),
        Statement::Reset { .. } => StmtClass::Handled(StmtFamily::Set),
        // PostgreSQL syntax PGArbor does not implement (0A000)
        Statement::Load { .. } => StmtClass::Unsupported("LOAD"),
        Statement::Close { .. } => StmtClass::Unsupported("CLOSE (cursors)"),
        Statement::CreateServer { .. } => StmtClass::Unsupported("CREATE SERVER"),
        Statement::CreatePolicy { .. } => StmtClass::Handled(StmtFamily::Policy),
        Statement::CreateOperator { .. } => StmtClass::Unsupported("CREATE OPERATOR"),
        Statement::CreateOperatorFamily { .. } => StmtClass::Unsupported("CREATE OPERATOR FAMILY"),
        Statement::CreateOperatorClass { .. } => StmtClass::Unsupported("CREATE OPERATOR CLASS"),
        Statement::CreateTextSearch { .. } => StmtClass::Unsupported("CREATE TEXT SEARCH"),
        Statement::AlterCollation { .. } => StmtClass::Unsupported("ALTER COLLATION"),
        Statement::AlterOperator { .. } => StmtClass::Unsupported("ALTER OPERATOR"),
        Statement::AlterOperatorFamily { .. } => StmtClass::Unsupported("ALTER OPERATOR FAMILY"),
        Statement::AlterOperatorClass { .. } => StmtClass::Unsupported("ALTER OPERATOR CLASS"),
        Statement::AlterTextSearch { .. } => StmtClass::Unsupported("ALTER TEXT SEARCH"),
        Statement::AlterPolicy { .. } => StmtClass::Handled(StmtFamily::Policy),
        Statement::DropPolicy { .. } => StmtClass::Handled(StmtFamily::Policy),
        Statement::Declare { .. } => StmtClass::Unsupported("DECLARE (cursors)"),
        Statement::CreateExtension { .. } => StmtClass::Unsupported("CREATE EXTENSION"),
        Statement::CreateCollation { .. } => StmtClass::Unsupported("CREATE COLLATION"),
        Statement::DropExtension { .. } => StmtClass::Unsupported("DROP EXTENSION"),
        Statement::DropOperator { .. } => StmtClass::Unsupported("DROP OPERATOR"),
        Statement::DropOperatorFamily { .. } => StmtClass::Unsupported("DROP OPERATOR FAMILY"),
        Statement::DropOperatorClass { .. } => StmtClass::Unsupported("DROP OPERATOR CLASS"),
        Statement::Fetch { .. } => StmtClass::Unsupported("FETCH (cursors)"),
        Statement::CreateTrigger { .. } => StmtClass::Unsupported("CREATE TRIGGER"),
        Statement::DropTrigger { .. } => StmtClass::Unsupported("DROP TRIGGER"),
        Statement::Merge { .. } => StmtClass::Handled(StmtFamily::Merge),
        // Syntax of other database systems: PostgreSQL itself reports 42601
        Statement::Msck { .. } => StmtClass::NotPostgres,
        Statement::Install { .. } => StmtClass::NotPostgres,
        Statement::Directory { .. } => StmtClass::NotPostgres,
        Statement::Case { .. } => StmtClass::NotPostgres,
        Statement::If { .. } => StmtClass::NotPostgres,
        Statement::While { .. } => StmtClass::NotPostgres,
        Statement::Raise { .. } => StmtClass::NotPostgres,
        Statement::CopyIntoSnowflake { .. } => StmtClass::NotPostgres,
        Statement::Open { .. } => StmtClass::NotPostgres,
        Statement::CreateVirtualTable { .. } => StmtClass::NotPostgres,
        Statement::CreateSecret { .. } => StmtClass::NotPostgres,
        Statement::CreateConnector { .. } => StmtClass::NotPostgres,
        Statement::AlterConnector { .. } => StmtClass::NotPostgres,
        Statement::AlterSession { .. } => StmtClass::NotPostgres,
        Statement::AttachDatabase { .. } => StmtClass::NotPostgres,
        Statement::AttachDuckDBDatabase { .. } => StmtClass::NotPostgres,
        Statement::DetachDuckDBDatabase { .. } => StmtClass::NotPostgres,
        Statement::DropSecret { .. } => StmtClass::NotPostgres,
        Statement::DropConnector { .. } => StmtClass::NotPostgres,
        Statement::Flush { .. } => StmtClass::NotPostgres,
        Statement::ShowFunctions { .. } => StmtClass::NotPostgres,
        Statement::ShowStatus { .. } => StmtClass::NotPostgres,
        Statement::ShowVariables { .. } => StmtClass::NotPostgres,
        Statement::ShowCreate { .. } => StmtClass::NotPostgres,
        Statement::ShowColumns { .. } => StmtClass::NotPostgres,
        Statement::ShowCatalogs { .. } => StmtClass::NotPostgres,
        Statement::ShowDatabases { .. } => StmtClass::NotPostgres,
        Statement::ShowProcessList { .. } => StmtClass::NotPostgres,
        Statement::ShowSchemas { .. } => StmtClass::NotPostgres,
        Statement::ShowCharset { .. } => StmtClass::NotPostgres,
        Statement::ShowObjects { .. } => StmtClass::NotPostgres,
        Statement::ShowTables { .. } => StmtClass::NotPostgres,
        Statement::ShowViews { .. } => StmtClass::NotPostgres,
        Statement::ShowCollation { .. } => StmtClass::NotPostgres,
        Statement::Use { .. } => StmtClass::NotPostgres,
        Statement::CreateMacro { .. } => StmtClass::NotPostgres,
        Statement::CreateStage { .. } => StmtClass::NotPostgres,
        Statement::CreateFileFormat { .. } => StmtClass::NotPostgres,
        Statement::CreateWarehouse { .. } => StmtClass::NotPostgres,
        Statement::Assert { .. } => StmtClass::NotPostgres,
        Statement::Deny { .. } => StmtClass::NotPostgres,
        Statement::Kill { .. } => StmtClass::NotPostgres,
        Statement::ExplainTable { .. } => StmtClass::NotPostgres,
        Statement::Cache { .. } => StmtClass::NotPostgres,
        Statement::UNCache { .. } => StmtClass::NotPostgres,
        Statement::Pragma { .. } => StmtClass::NotPostgres,
        Statement::LockTables { .. } => StmtClass::NotPostgres,
        Statement::UnlockTables { .. } => StmtClass::NotPostgres,
        Statement::Unload { .. } => StmtClass::NotPostgres,
        Statement::OptimizeTable { .. } => StmtClass::NotPostgres,
        Statement::LoadData { .. } => StmtClass::NotPostgres,
        Statement::RenameTable { .. } => StmtClass::NotPostgres,
        Statement::List { .. } => StmtClass::NotPostgres,
        Statement::Put { .. } => StmtClass::NotPostgres,
        Statement::Remove { .. } => StmtClass::NotPostgres,
        Statement::RaisError { .. } => StmtClass::NotPostgres,
        Statement::Throw { .. } => StmtClass::NotPostgres,
        Statement::Print { .. } => StmtClass::NotPostgres,
        Statement::WaitFor { .. } => StmtClass::NotPostgres,
        Statement::Return { .. } => StmtClass::NotPostgres,
        Statement::ExportData { .. } => StmtClass::NotPostgres,
    }
}

/// Classify a parsed statement of either kind.
pub fn classify_parsed(p: &Parsed) -> StmtClass {
    match p {
        Parsed::Ast(s) => classify(s),
        Parsed::Extra(ExtraStatement::Unsupported { .. }) => StmtClass::Unsupported("recognized PostgreSQL command"),
        Parsed::Extra(_) => StmtClass::Handled(StmtFamily::Extra),
    }
}

/// Raise the error for a statement that cannot be bound, or return its family.
///
/// * `Handled(f)` -> `Ok(f)`.
/// * `Unsupported(feature)` -> 0A000 `diag::unsupported_construct(feature, pos)`
///   (for `ExtraStatement::Unsupported { feature }` the stored feature text is used).
/// * `NotPostgres` -> 42601 `diag::syntax_error_near(<first token text>, pos)`; `first_token` is the
///   statement's first token text as written (`parser` passes it).
pub fn check_supported(p: &Parsed, first_token: &str, pos: Option<u32>) -> Result<StmtFamily> {
    match p {
        Parsed::Extra(ExtraStatement::Unsupported { feature }) => Err(diag::unsupported_construct(feature, pos)),
        _ => match classify_parsed(p) {
            StmtClass::Handled(f) => Ok(f),
            StmtClass::Unsupported(feature) => Err(diag::unsupported_construct(feature, pos)),
            StmtClass::NotPostgres => Err(diag::syntax_error_near(first_token, pos)),
        },
    }
}
