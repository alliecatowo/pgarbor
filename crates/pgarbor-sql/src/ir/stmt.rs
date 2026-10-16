//! Bound statements: the typed, name-resolved form of every SQL statement PGArbor accepts.
//! Plain data, fully implemented, frozen (v1). Produced by `binder::bind`, consumed by
//! `pgarbor_plan::plan` (queries and DML) and `pgarbor_exec` (DDL, utility statements).
//!
//! ## Design rules
//! * **DDL carries the catalog's own spec structs.** `CreateTable(CreateTableSpec)`,
//!   `CreateIndex(IndexSpec)`, ... are exactly the inputs of the matching `CatalogTxn` methods
//!   (`pgarbor_catalog::ddl::spec`). There is no parallel DDL IR; ALTER actions map one-to-one onto
//!   `CatalogTxn` methods (the doc of every [`AlterTableCmd`] / [`AlterAction`] variant names the
//!   method the executor calls).
//! * **DROP carries names, not OIDs** (`DropTarget`): the catalog's `drop_by_names` resolves them
//!   because NOTICE/ERROR wording depends on the lookup outcome.
//! * **A statement is bound against the catalog snapshot current at ITS start** and nothing else:
//!   binding is pure (no catalog writes, no side effects), so READ COMMITTED DDL can re-bind and
//!   retry (catalog protocol step 2). In `CREATE TABLE t(..); INSERT INTO t ..` the INSERT is bound
//!   only after the CREATE executed.
//! * Statements the engine accepts but treats as a no-op (`SET CONSTRAINTS`, `CHECKPOINT`, ...)
//!   are `Noop`/`Maintenance`, never silently dropped: the tag and notices still reach the client.
//! * Result shape for Describe: [`BoundStatement::result_columns`].
use super::expr::BoundExpr;
use super::query::{BoundCte, BoundQuery, FromItem, OutputColumn, RangeEntry, TargetEntry};
use pgarbor_catalog::ddl::alter_column::{ColumnAttribute, ReplicaIdentitySetting};
use pgarbor_catalog::ddl::drop_stmt::{DropKind, DropTarget};
use pgarbor_catalog::ddl::function::FunctionChanges;
use pgarbor_catalog::ddl::spec::*;
use pgarbor_catalog::ddl::typedef::EnumPosition;
use pgarbor_catalog::model::{
    Function, GeneratedKind, IdentityKind, ObjectAddress, RelKind, StoredExpr,
};
use pgarbor_common::{Oid, PgError};
use pgarbor_types::TypeId;

// ---------------------------------------------------------------------------------------------
// DML target description
// ---------------------------------------------------------------------------------------------

/// A column of a DML target table, with everything INSERT/UPDATE needs bound ahead of time.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct TargetColumn {
    pub attnum: i16,
    pub name: String,
    pub ty: TypeId,
    pub dropped: bool,
    pub not_null: bool,
    pub collation: Oid,
    /// Effective default for this column when the statement does not supply a value: the column's
    /// `DEFAULT` expression, or `nextval('seq')` for IDENTITY columns; `None` = NULL.
    /// Expressions reference columns of the target as `Column{levels_up:0, rt_index: 1, attnum}`.
    pub default: Option<BoundExpr>,
    pub identity: Option<IdentityKind>,
    /// Generated column: kind and the (bound) generation expression.
    pub generated: Option<(GeneratedKind, BoundExpr)>,
}

/// A CHECK constraint to enforce on every new row version.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct TargetCheck {
    pub constraint: Oid,
    pub name: String,
    /// Boolean expression over the target's own columns (`Column{rt_index: 1, ..}`), parsed from
    /// `Constraint::check` and bound once. NOT ENFORCED constraints are omitted.
    pub expr: BoundExpr,
}

/// The relation modified by INSERT / UPDATE / DELETE / COPY FROM.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct TargetTable {
    pub oid: Oid,
    pub namespace: Oid,
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    /// One entry per attnum slot (index = attnum - 1), dropped columns included.
    pub columns: Vec<TargetColumn>,
    pub checks: Vec<TargetCheck>,
}

impl TargetTable {
    /// Non-dropped columns.
    pub fn live_columns(&self) -> impl Iterator<Item = &TargetColumn> {
        self.columns.iter().filter(|c| !c.dropped)
    }
    pub fn column(&self, attnum: i16) -> Option<&TargetColumn> {
        if attnum < 1 {
            return None;
        }
        self.columns.get(attnum as usize - 1)
    }
}

/// `INSERT ... OVERRIDING {SYSTEM|USER} VALUE`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overriding {
    System,
    User,
}

/// Row source of an INSERT.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    /// `VALUES (..), (..)`: cells already coerced (assignment context) to the target column types
    /// in `BoundInsert::columns` order; may contain `SetToDefault`.
    Values(Vec<Vec<BoundExpr>>),
    /// `INSERT ... SELECT ...`: query output columns, coerced via casts inside the query where
    /// needed (the binder appends a coercing projection when types differ).
    Query(Box<BoundQuery>),
    /// `DEFAULT VALUES`.
    DefaultValues,
}

/// One assignment target (`SET col = ...`). Subscript / field assignment (`SET a[1] = ..`,
/// `SET c.f = ..`) is not representable yet (sqlparser's `AssignmentTarget` cannot express it).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct AssignTarget {
    pub attnum: i16,
    pub name: String,
    pub ty: TypeId,
}

/// `SET a = expr` or `SET (a, b) = (e1, e2)` / `= (SELECT ..)`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub targets: Vec<AssignTarget>,
    /// One target: the value coerced to the column type (may be `SetToDefault`). Several targets:
    /// a `Row` of per-target coerced expressions, or a `SubLink{kind: Row}` returning that many
    /// columns (each already coerced by the binder inside the sub-query's projection).
    pub value: BoundExpr,
}

/// `ON CONFLICT` arbiter selection (PG `InferClause`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Arbiter {
    /// Unique / primary-key indexes inferred from `ON CONFLICT (cols)` or named by `ON CONSTRAINT`.
    /// Empty together with `any_unique = true` for a bare `ON CONFLICT DO NOTHING`.
    pub indexes: Vec<Oid>,
    pub any_unique: bool,
    /// `ON CONFLICT (cols) WHERE pred` partial-index predicate (over the target columns).
    pub index_predicate: Option<BoundExpr>,
}

/// The `DO NOTHING` / `DO UPDATE` action.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictAction {
    DoNothing,
    DoUpdate {
        assignments: Vec<Assignment>,
        /// `WHERE` over the existing row (`rt_index` = target) and `excluded` (`excluded_rt`).
        where_: Option<BoundExpr>,
    },
}

/// INSERT ... ON CONFLICT.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundOnConflict {
    pub arbiter: Arbiter,
    pub action: ConflictAction,
    /// Range-table index of the `excluded` pseudo relation.
    pub excluded_rt: u32,
}

/// Bound INSERT.
///
/// `range_table[0]` (rt_index 1) is the target relation; with ON CONFLICT DO UPDATE
/// `range_table[1]` (rt_index 2) is `excluded`. `returning` expressions and `ON CONFLICT` expressions
/// reference those entries.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundInsert {
    pub target: TargetTable,
    pub range_table: Vec<RangeEntry>,
    pub ctes: Vec<BoundCte>,
    /// Target attnums in source-column order (explicit column list, or all non-dropped columns in
    /// attnum order; for `DefaultValues` empty).
    pub columns: Vec<i16>,
    pub source: InsertSource,
    pub overriding: Option<Overriding>,
    pub on_conflict: Option<BoundOnConflict>,
    pub returning: Vec<TargetEntry>,
}

/// Bound UPDATE. `range_table[target_rt - 1]` is the target; FROM items follow.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundUpdate {
    pub target: TargetTable,
    pub target_rt: u32,
    pub range_table: Vec<RangeEntry>,
    pub ctes: Vec<BoundCte>,
    /// `UPDATE .. FROM a, b` items (join tree over the range table).
    pub from: Vec<FromItem>,
    pub assignments: Vec<Assignment>,
    pub where_: Option<BoundExpr>,
    pub returning: Vec<TargetEntry>,
}

/// Bound DELETE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDelete {
    pub target: TargetTable,
    pub target_rt: u32,
    pub range_table: Vec<RangeEntry>,
    pub ctes: Vec<BoundCte>,
    /// `DELETE .. USING a, b`.
    pub using: Vec<FromItem>,
    pub where_: Option<BoundExpr>,
    pub returning: Vec<TargetEntry>,
}

/// `CREATE TABLE ... AS query` / `SELECT ... INTO`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCreateTableAs {
    /// Columns from the query output (names after any column list), no constraints.
    pub table: CreateTableSpec,
    pub query: Box<BoundQuery>,
    /// `WITH NO DATA` = false.
    pub with_data: bool,
}

/// CREATE [MATERIALIZED] VIEW.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCreateView {
    /// `spec.query` holds the canonical deparsed text (`deparse::deparse_query`) and the
    /// dependency list; `spec.columns` the output columns.
    pub spec: ViewSpec,
    /// The bound query (for `WITH DATA` population and for validating view definition).
    pub query: Box<BoundQuery>,
}

/// TRUNCATE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundTruncate {
    /// Resolved tables (permission/kind checked by the executor), in written order.
    pub tables: Vec<Oid>,
    pub restart_identity: bool,
    pub cascade: bool,
}

// ---------------------------------------------------------------------------------------------
// ALTER
// ---------------------------------------------------------------------------------------------

/// One ALTER TABLE sub-command, in written order. Each variant maps to exactly one `CatalogTxn`
/// method (named in the variant docs). Sub-commands are bound sequentially against an evolving
/// in-memory copy of the relation (see `binder::ddl_alter`), so
/// `ADD COLUMN b int, ADD CHECK (b > 0)` binds the CHECK against the new column.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterTableCmd {
    /// `add_column(table, &spec, if_not_exists)`.
    AddColumn { spec: ColumnSpec, if_not_exists: bool },
    /// `drop_column(table, name, behavior, missing_ok)`.
    DropColumn { name: String, cascade: bool, missing_ok: bool },
    /// `rename_column(table, old, new, rewriter)`.
    RenameColumn { old: String, new: String },
    /// `alter_column_type(table, column, new_type, new_collation, new_ndims, new_default, needs_rewrite)`.
    /// `using` is the bound USING expression (or the implicit/assignment cast) the executor
    /// evaluates per row when `needs_rewrite`.
    AlterColumnType {
        column: String,
        new_type: TypeId,
        new_collation: Option<Oid>,
        new_ndims: i32,
        using: Option<BoundExpr>,
        new_default: Option<Option<StoredExpr>>,
        needs_rewrite: bool,
    },
    /// `set_column_default(table, column, expr)` (`None` = DROP DEFAULT).
    SetDefault { column: String, expr: Option<StoredExpr> },
    /// `set_not_null`.
    SetNotNull { column: String },
    /// `drop_not_null`.
    DropNotNull { column: String },
    /// `add_identity`.
    AddIdentity { column: String, identity: IdentitySpec },
    /// `set_identity(table, column, kind, &options)`.
    SetIdentity { column: String, kind: Option<IdentityKind>, options: SequenceOptions },
    /// `drop_identity(table, column, missing_ok)`.
    DropIdentity { column: String, missing_ok: bool },
    /// `set_generation_expression`.
    SetExpression { column: String, expr: StoredExpr },
    /// `drop_expression(table, column, missing_ok)`.
    DropExpression { column: String, missing_ok: bool },
    /// `set_column_attribute`.
    SetColumnAttribute { column: String, attr: ColumnAttribute },
    /// `add_constraint(table, &spec)`.
    AddConstraint(ConstraintSpec),
    /// `drop_constraint(table, name, behavior, missing_ok)`.
    DropConstraint { name: String, cascade: bool, missing_ok: bool },
    /// `rename_constraint`.
    RenameConstraint { old: String, new: String },
    /// `validate_constraint`.
    ValidateConstraint { name: String },
    /// `alter_constraint(table, name, deferrable, initially_deferred, enforced)`.
    AlterConstraint { name: String, deferrable: Option<bool>, initially_deferred: Option<bool>, enforced: Option<bool> },
    /// `set_relation_options(rel, set, reset)`.
    SetOptions { set: Vec<String>, reset: Vec<String> },
    /// `set_persistence`.
    SetLogged(bool),
    /// `set_replica_identity`.
    ReplicaIdentity(ReplicaIdentitySetting),
    /// `set_row_security(table, enable, force)`.
    RowSecurity { enable: Option<bool>, force: Option<bool> },
}

/// `ALTER TABLE`/`VIEW`/`MATERIALIZED VIEW`/`TYPE (composite)` with a command list.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAlterTable {
    pub relation: Oid,
    /// Which keyword was used (decides the error wording and the command tag).
    pub keyword: AlterRelKeyword,
    pub commands: Vec<AlterTableCmd>,
}

/// The object-kind word of an ALTER statement over a relation.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlterRelKeyword {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    ForeignTable,
    /// `ALTER TYPE composite ADD/DROP/ALTER ATTRIBUTE`.
    Type,
}

/// Every other ALTER (and ownership / schema / rename) action. One variant per `CatalogTxn`
/// method; the executor wraps each in one catalog statement.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum AlterAction {
    /// `rename_relation(rel, new_name, rewriter)` (table/view/matview/sequence/index/composite).
    RenameRelation { rel: Oid, new_name: String, keyword: AlterRelKeyword },
    /// `rename_index` is used instead for indexes when the executor needs constraint renames:
    /// the executor chooses by `keyword == Index`.
    /// `set_relation_schema(rel, new_schema, rewriter)`.
    SetRelationSchema { rel: Oid, new_schema: Oid, keyword: AlterRelKeyword },
    /// `alter_relation_owner(rel, new_owner)`.
    RelationOwner { rel: Oid, new_owner: Oid, keyword: AlterRelKeyword },
    /// `alter_index_options(index, set, reset)`.
    IndexOptions { index: Oid, set: Vec<String>, reset: Vec<String> },
    /// `set_view_options(view, set, reset)`.
    ViewOptions { view: Oid, set: Vec<String>, reset: Vec<String> },
    /// `alter_sequence(seq, &options, owned_by)`.
    Sequence { sequence: Oid, options: SequenceOptions, owned_by: Option<Option<(Oid, i16)>> },
    /// `rename_schema(old, new)`.
    RenameSchema { old: String, new: String },
    /// `alter_schema_owner(name, new_owner)`.
    SchemaOwner { name: String, new_owner: Oid },
    /// `rename_type(ty, new_name, rewriter)` (enum, domain).
    RenameType { ty: Oid, new_name: String },
    /// `set_type_schema(ty, new_schema, rewriter)`: `ALTER TYPE ... SET SCHEMA` (enum, composite).
    SetTypeSchema { ty: Oid, new_schema: Oid },
    /// `set_type_schema(ty, new_schema, rewriter)` for `ALTER DOMAIN ... SET SCHEMA` (same effect,
    /// command tag `ALTER DOMAIN`).
    SetDomainSchema { ty: Oid, new_schema: Oid },
    /// `alter_type_owner(ty, new_owner)`.
    TypeOwner { ty: Oid, new_owner: Oid },
    /// `add_enum_value(ty, label, &position, if_not_exists)`.
    AddEnumValue { ty: Oid, label: String, position: EnumPosition, if_not_exists: bool },
    /// `rename_enum_value(ty, old, new)`.
    RenameEnumValue { ty: Oid, old: String, new: String },
    /// `alter_domain_default(domain, default)`.
    DomainDefault { domain: Oid, default: Option<StoredExpr> },
    /// `alter_domain_not_null(domain, set)`; the executor validates the returned columns.
    DomainNotNull { domain: Oid, set: bool },
    /// `add_domain_constraint(domain, &spec)`.
    AddDomainConstraint { domain: Oid, spec: DomainConstraintSpec },
    /// `drop_domain_constraint(domain, name, missing_ok, behavior)`.
    DropDomainConstraint { domain: Oid, name: String, missing_ok: bool, cascade: bool },
    /// `rename_domain_constraint(domain, old, new)`.
    RenameDomainConstraint { domain: Oid, old: String, new: String },
    /// `validate_domain_constraint(domain, name)`.
    ValidateDomainConstraint { domain: Oid, name: String },
    /// `alter_function_attrs(func, &changes)`.
    FunctionAttrs { func: Oid, changes: FunctionChanges },
    /// `rename_function(func, new_name, rewriter)`.
    RenameFunction { func: Oid, new_name: String },
    /// `set_function_schema(func, new_schema, rewriter)`.
    SetFunctionSchema { func: Oid, new_schema: Oid },
    /// `alter_function_owner(func, new_owner)`.
    FunctionOwner { func: Oid, new_owner: Oid },
    /// `alter_role(role, &changes)`.
    Role { role: Oid, changes: RoleChanges },
    /// `rename_role(role, new_name)`.
    RenameRole { role: Oid, new_name: String },
    /// `set_role_config(role, guc, value)` (`None` = RESET).
    RoleConfig { role: Oid, guc: String, value: Option<String> },
    /// `rename_database(name, new_name)`.
    RenameDatabase { name: String, new_name: String },
    /// `alter_database_owner(name, new_owner)`.
    DatabaseOwner { name: String, new_owner: Oid },
    /// `set_database_config(name, guc, value)`.
    DatabaseConfig { name: String, guc: String, value: Option<String> },
    /// `alter_database_options(name, allow_conn, conn_limit, is_template)`.
    DatabaseOptions { name: String, allow_conn: Option<bool>, conn_limit: Option<i32>, is_template: Option<bool> },
    /// Generic `alter_owner(addr, new_owner)` for objects without a dedicated variant.
    Owner { addr: ObjectAddress, new_owner: Oid },
}

impl AlterAction {
    /// PG command tag (`ALTER TABLE`, `ALTER SEQUENCE`, `ALTER ROLE`, ...).
    pub fn tag(&self) -> &'static str {
        fn kw(k: &AlterRelKeyword) -> &'static str {
            match k {
                AlterRelKeyword::Table => "ALTER TABLE",
                AlterRelKeyword::View => "ALTER VIEW",
                AlterRelKeyword::MaterializedView => "ALTER MATERIALIZED VIEW",
                AlterRelKeyword::Index => "ALTER INDEX",
                AlterRelKeyword::Sequence => "ALTER SEQUENCE",
                AlterRelKeyword::ForeignTable => "ALTER FOREIGN TABLE",
                AlterRelKeyword::Type => "ALTER TYPE",
            }
        }
        match self {
            AlterAction::RenameRelation { keyword, .. }
            | AlterAction::SetRelationSchema { keyword, .. }
            | AlterAction::RelationOwner { keyword, .. } => kw(keyword),
            AlterAction::IndexOptions { .. } => "ALTER INDEX",
            AlterAction::ViewOptions { .. } => "ALTER VIEW",
            AlterAction::Sequence { .. } => "ALTER SEQUENCE",
            AlterAction::RenameSchema { .. } | AlterAction::SchemaOwner { .. } => "ALTER SCHEMA",
            AlterAction::RenameType { .. }
            | AlterAction::SetTypeSchema { .. }
            | AlterAction::TypeOwner { .. }
            | AlterAction::AddEnumValue { .. }
            | AlterAction::RenameEnumValue { .. } => "ALTER TYPE",
            AlterAction::DomainDefault { .. }
            | AlterAction::DomainNotNull { .. }
            | AlterAction::AddDomainConstraint { .. }
            | AlterAction::DropDomainConstraint { .. }
            | AlterAction::RenameDomainConstraint { .. }
            | AlterAction::ValidateDomainConstraint { .. }
            | AlterAction::SetDomainSchema { .. } => "ALTER DOMAIN",
            AlterAction::FunctionAttrs { .. }
            | AlterAction::RenameFunction { .. }
            | AlterAction::SetFunctionSchema { .. }
            | AlterAction::FunctionOwner { .. } => "ALTER FUNCTION",
            AlterAction::Role { .. } | AlterAction::RenameRole { .. } | AlterAction::RoleConfig { .. } => "ALTER ROLE",
            AlterAction::RenameDatabase { .. }
            | AlterAction::DatabaseOwner { .. }
            | AlterAction::DatabaseConfig { .. }
            | AlterAction::DatabaseOptions { .. } => "ALTER DATABASE",
            AlterAction::Owner { .. } => "ALTER OWNER",
        }
    }
}

/// CREATE FUNCTION / PROCEDURE (definition stored, execution later).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCreateFunction {
    /// `oid` is 0; the catalog allocates it.
    pub function: Function,
    pub or_replace: bool,
}

/// DROP [IF EXISTS] kind names [CASCADE|RESTRICT] (`DropKind::Role` included).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDrop {
    pub kind: DropKind,
    pub targets: Vec<DropTarget>,
    pub if_exists: bool,
    pub cascade: bool,
}

/// CREATE DATABASE (PGArbor databases are catalog rows; a single container hosts `postgres`).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCreateDatabase {
    pub name: String,
    pub owner: Oid,
    pub template: Option<String>,
    pub encoding: Option<String>,
    pub locale: Option<String>,
    pub connection_limit: i32,
    pub is_template: bool,
    pub allow_connections: bool,
    pub if_not_exists: bool,
}

// ---------------------------------------------------------------------------------------------
// Utility statements
// ---------------------------------------------------------------------------------------------

/// Transaction isolation levels.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// Transaction characteristics (`None` = not specified).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnModes {
    pub isolation: Option<IsolationLevel>,
    pub read_only: Option<bool>,
    pub deferrable: Option<bool>,
}

/// Transaction control.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionStmt {
    /// BEGIN / START TRANSACTION.
    Begin(TxnModes),
    /// COMMIT / END. `chain` = AND CHAIN.
    Commit { chain: bool },
    /// ROLLBACK / ABORT.
    Rollback { chain: bool },
    Savepoint(String),
    ReleaseSavepoint(String),
    RollbackToSavepoint(String),
    /// `SET TRANSACTION ...` (current transaction).
    SetTransaction(TxnModes),
    /// `SET SESSION CHARACTERISTICS AS TRANSACTION ...`.
    SetSessionCharacteristics(TxnModes),
}

/// `SET` scope.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetScope {
    Session,
    Local,
}

/// Value of a `SET`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetValue {
    /// `SET x TO DEFAULT` (same as RESET).
    Default,
    /// `SET x FROM CURRENT`.
    FromCurrent,
    /// One or more values, each already rendered as PG renders a `SET` argument (identifier or
    /// string literal contents, numbers as written; `SET search_path = a, "B"` -> `["a","B"]`).
    Values(Vec<String>),
}

/// `SET [SESSION|LOCAL] name = value`. `name` is lower-cased; special forms are normalized:
/// `SET TIME ZONE x` -> `timezone`, `SET NAMES x` -> `client_encoding`, `SET SCHEMA 'x'` ->
/// `search_path`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetStmt {
    pub scope: SetScope,
    pub name: String,
    pub value: SetValue,
}

/// `COPY` direction.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyDirection {
    From,
    To,
}

/// Where COPY data comes from / goes to.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyEndpoint {
    Stdin,
    Stdout,
    /// Server-side file (superuser / pg_read_server_files only).
    File(String),
    Program(String),
}

/// COPY format.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    Text,
    Csv,
    Binary,
}

/// COPY options with PG defaults resolved by the binder (`delimiter` tab for text, comma for CSV...).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyOptions {
    pub format: CopyFormat,
    pub delimiter: char,
    pub null_string: String,
    pub header: bool,
    pub quote: char,
    pub escape: char,
    pub force_quote_all: bool,
    pub force_quote: Vec<i16>,
    pub force_not_null: Vec<i16>,
    pub force_null: Vec<i16>,
    pub encoding: Option<String>,
    pub freeze: bool,
    pub default_string: Option<String>,
}

/// COPY target: a table (with column list) or a query.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum CopyTarget {
    Table { table: Box<TargetTable>, columns: Vec<i16> },
    Query(Box<BoundQuery>),
}

/// Bound COPY.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCopy {
    pub direction: CopyDirection,
    pub target: CopyTarget,
    pub endpoint: CopyEndpoint,
    pub options: CopyOptions,
    /// `COPY FROM ... WHERE cond` over the target columns.
    pub where_: Option<BoundExpr>,
}

/// EXPLAIN output format.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainFormat {
    Text,
    Xml,
    Json,
    Yaml,
}

/// EXPLAIN options (PG defaults: COSTS on, others off).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainOptions {
    pub analyze: bool,
    pub verbose: bool,
    pub costs: bool,
    pub buffers: bool,
    pub timing: bool,
    pub summary: bool,
    pub settings: bool,
    pub wal: bool,
    pub format: ExplainFormat,
}

impl Default for ExplainOptions {
    fn default() -> ExplainOptions {
        ExplainOptions {
            analyze: false,
            verbose: false,
            costs: true,
            buffers: false,
            timing: false,
            summary: false,
            settings: false,
            wal: false,
            format: ExplainFormat::Text,
        }
    }
}

/// EXPLAIN [options] statement.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundExplain {
    pub options: ExplainOptions,
    pub statement: Box<BoundStatement>,
}

/// PREPARE name [(types)] AS statement.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundPrepare {
    pub name: String,
    /// Parameter types after inference (declared types first, the rest inferred).
    pub param_types: Vec<TypeId>,
    pub statement: Box<BoundStatement>,
    /// Source text of the inner statement (for `pg_prepared_statements.statement`).
    pub text: String,
}

/// EXECUTE name (args). Arguments are bound in an empty scope; the SESSION coerces them to the
/// prepared statement's parameter types with `binder::utility::coerce_execute_args`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundExecute {
    pub name: String,
    pub args: Vec<BoundExpr>,
}

/// `DISCARD {ALL|PLANS|SEQUENCES|TEMP}`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardKind {
    All,
    Plans,
    Sequences,
    Temp,
}

/// Table lock modes.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

/// LOCK TABLE.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundLock {
    pub tables: Vec<Oid>,
    pub mode: LockMode,
    pub nowait: bool,
}

/// `DO [LANGUAGE x] 'code'` (execution of procedural languages is later work).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundDo {
    pub language: String,
    pub code: String,
}

/// `CALL proc(args)`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCall {
    pub procedure: Oid,
    pub name: String,
    pub args: Vec<BoundExpr>,
}

/// Maintenance commands PGArbor accepts without doing physical work.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceKind {
    Vacuum,
    Analyze,
    Reindex,
    Cluster,
    Checkpoint,
}

/// The statement variants.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum BoundStatement {
    /// Empty statement (`;`): EmptyQueryResponse.
    Empty,
    /// Accepted statement with no effect (e.g. `ALTER TABLE IF EXISTS missing ...`, `SET
    /// CONSTRAINTS`): only `tag` and `notices` are reported.
    Noop { tag: String, notices: Vec<PgError> },

    // --- queries and DML ---
    Query(Box<BoundQuery>),
    Insert(Box<BoundInsert>),
    Update(Box<BoundUpdate>),
    Delete(Box<BoundDelete>),
    CreateTableAs(Box<BoundCreateTableAs>),
    Truncate(BoundTruncate),

    // --- DDL ---
    CreateSchema { name: String, owner: Oid, if_not_exists: bool },
    CreateTable(Box<CreateTableSpec>),
    CreateIndex(Box<IndexSpec>),
    CreateView(Box<BoundCreateView>),
    CreateSequence(Box<CreateSequenceSpec>),
    CreateEnum(CreateEnumSpec),
    CreateDomain(Box<CreateDomainSpec>),
    CreateComposite(CreateCompositeSpec),
    CreateFunction(Box<BoundCreateFunction>),
    CreateRole(Box<CreateRoleSpec>),
    CreateDatabase(Box<BoundCreateDatabase>),
    AlterTable(Box<BoundAlterTable>),
    Alter(Box<AlterAction>),
    Drop(Box<BoundDrop>),
    DropDatabase { name: String, if_exists: bool, force: bool },
    Comment { addr: ObjectAddress, text: Option<String> },
    /// `GRANT privileges ON ...` / `REVOKE` (`revoke = true`). One spec per (object, ...) pair of the
    /// statement: `GRANT SELECT ON a, b TO x, y` is two specs (each listing both grantees).
    Grant { specs: Vec<GrantSpec>, revoke: bool },
    /// `GRANT role [, role] TO member [, member]`: one spec per (role, member) pair.
    GrantRole(Vec<MembershipSpec>),
    /// `REVOKE [ADMIN OPTION FOR] role FROM member`: one spec per pair.
    RevokeRole { specs: Vec<MembershipSpec>, admin_option_only: bool, cascade: bool },
    ReassignOwned { old_roles: Vec<Oid>, new_owner: Oid },
    DropOwned { roles: Vec<Oid>, cascade: bool },
    RefreshMatView { view: Oid, with_data: bool, concurrently: bool },
    Maintenance { kind: MaintenanceKind, targets: Vec<Oid> },

    // --- session / transaction / utility ---
    Transaction(TransactionStmt),
    Set(SetStmt),
    /// `SET [LOCAL] ROLE name` (`None` = `ROLE NONE` / RESET ROLE).
    SetRole { role: Option<String>, local: bool },
    /// `SET [LOCAL] SESSION AUTHORIZATION name` (`None` = DEFAULT).
    SetSessionAuthorization { user: Option<String>, local: bool },
    /// `RESET name`; `None` = `RESET ALL`.
    Reset(Option<String>),
    /// `SHOW name`; `None` = `SHOW ALL`.
    Show(Option<String>),
    Prepare(Box<BoundPrepare>),
    Execute(Box<BoundExecute>),
    /// `DEALLOCATE name`; `None` = `DEALLOCATE ALL`.
    Deallocate(Option<String>),
    Copy(Box<BoundCopy>),
    Explain(Box<BoundExplain>),
    Listen(String),
    /// `UNLISTEN channel`; `None` = `UNLISTEN *`.
    Unlisten(Option<String>),
    Notify { channel: String, payload: Option<String> },
    Discard(DiscardKind),
    Lock(BoundLock),
    Do(BoundDo),
    Call(Box<BoundCall>),
}

impl BoundStatement {
    /// Base command tag as PG prints it in CommandComplete (row counts are appended by the
    /// executor: `SELECT 5`, `INSERT 0 1`, `UPDATE 2`, `DELETE 1`, `COPY 3`, `MOVE`, ...).
    pub fn tag(&self) -> String {
        match self {
            BoundStatement::Empty => String::new(),
            BoundStatement::Noop { tag, .. } => tag.clone(),
            BoundStatement::Query(_) => "SELECT".into(),
            BoundStatement::Insert(_) => "INSERT".into(),
            BoundStatement::Update(_) => "UPDATE".into(),
            BoundStatement::Delete(_) => "DELETE".into(),
            BoundStatement::CreateTableAs(_) => "SELECT".into(),
            BoundStatement::Truncate(_) => "TRUNCATE TABLE".into(),
            BoundStatement::CreateSchema { .. } => "CREATE SCHEMA".into(),
            BoundStatement::CreateTable(_) => "CREATE TABLE".into(),
            BoundStatement::CreateIndex(_) => "CREATE INDEX".into(),
            BoundStatement::CreateView(v) => {
                if v.spec.materialized { "CREATE MATERIALIZED VIEW".into() } else { "CREATE VIEW".into() }
            }
            BoundStatement::CreateSequence(_) => "CREATE SEQUENCE".into(),
            BoundStatement::CreateEnum(_) | BoundStatement::CreateComposite(_) => "CREATE TYPE".into(),
            BoundStatement::CreateDomain(_) => "CREATE DOMAIN".into(),
            BoundStatement::CreateFunction(f) => {
                if matches!(f.function.kind, pgarbor_catalog::model::FuncKind::Procedure) {
                    "CREATE PROCEDURE".into()
                } else {
                    "CREATE FUNCTION".into()
                }
            }
            BoundStatement::CreateRole(_) => "CREATE ROLE".into(),
            BoundStatement::CreateDatabase(_) => "CREATE DATABASE".into(),
            BoundStatement::AlterTable(a) => match a.keyword {
                AlterRelKeyword::Table => "ALTER TABLE".into(),
                AlterRelKeyword::View => "ALTER VIEW".into(),
                AlterRelKeyword::MaterializedView => "ALTER MATERIALIZED VIEW".into(),
                AlterRelKeyword::Index => "ALTER INDEX".into(),
                AlterRelKeyword::Sequence => "ALTER SEQUENCE".into(),
                AlterRelKeyword::ForeignTable => "ALTER FOREIGN TABLE".into(),
                AlterRelKeyword::Type => "ALTER TYPE".into(),
            },
            BoundStatement::Alter(a) => a.tag().into(),
            BoundStatement::Drop(d) => format!("DROP {}", drop_kind_word(d.kind)),
            BoundStatement::DropDatabase { .. } => "DROP DATABASE".into(),
            BoundStatement::Comment { .. } => "COMMENT".into(),
            BoundStatement::Grant { revoke, .. } => if *revoke { "REVOKE".into() } else { "GRANT".into() },
            BoundStatement::GrantRole(_) => "GRANT ROLE".into(),
            BoundStatement::RevokeRole { .. } => "REVOKE ROLE".into(),
            BoundStatement::ReassignOwned { .. } => "REASSIGN OWNED".into(),
            BoundStatement::DropOwned { .. } => "DROP OWNED".into(),
            BoundStatement::RefreshMatView { .. } => "REFRESH MATERIALIZED VIEW".into(),
            BoundStatement::Maintenance { kind, .. } => match kind {
                MaintenanceKind::Vacuum => "VACUUM",
                MaintenanceKind::Analyze => "ANALYZE",
                MaintenanceKind::Reindex => "REINDEX",
                MaintenanceKind::Cluster => "CLUSTER",
                MaintenanceKind::Checkpoint => "CHECKPOINT",
            }
            .into(),
            BoundStatement::Transaction(t) => match t {
                TransactionStmt::Begin(_) => "BEGIN".into(),
                TransactionStmt::Commit { .. } => "COMMIT".into(),
                TransactionStmt::Rollback { .. } => "ROLLBACK".into(),
                TransactionStmt::Savepoint(_) => "SAVEPOINT".into(),
                TransactionStmt::ReleaseSavepoint(_) => "RELEASE".into(),
                TransactionStmt::RollbackToSavepoint(_) => "ROLLBACK".into(),
                TransactionStmt::SetTransaction(_) | TransactionStmt::SetSessionCharacteristics(_) => "SET".into(),
            },
            BoundStatement::Set(_) | BoundStatement::SetRole { .. } | BoundStatement::SetSessionAuthorization { .. } => {
                "SET".into()
            }
            BoundStatement::Reset(_) => "RESET".into(),
            BoundStatement::Show(_) => "SHOW".into(),
            BoundStatement::Prepare(_) => "PREPARE".into(),
            BoundStatement::Execute(_) => "EXECUTE".into(),
            BoundStatement::Deallocate(_) => "DEALLOCATE".into(),
            BoundStatement::Copy(_) => "COPY".into(),
            BoundStatement::Explain(_) => "EXPLAIN".into(),
            BoundStatement::Listen(_) => "LISTEN".into(),
            BoundStatement::Unlisten(_) => "UNLISTEN".into(),
            BoundStatement::Notify { .. } => "NOTIFY".into(),
            BoundStatement::Discard(d) => match d {
                DiscardKind::All => "DISCARD ALL".into(),
                DiscardKind::Plans => "DISCARD PLANS".into(),
                DiscardKind::Sequences => "DISCARD SEQUENCES".into(),
                DiscardKind::Temp => "DISCARD TEMP".into(),
            },
            BoundStatement::Lock(_) => "LOCK TABLE".into(),
            BoundStatement::Do(_) => "DO".into(),
            BoundStatement::Call(_) => "CALL".into(),
        }
    }

    /// Result columns a client sees for this statement (Describe / RowDescription), or `None` when
    /// the statement returns no rows. `EXECUTE` returns `None` here: the session resolves it through
    /// the prepared statement. `SHOW name` returns one `text` column named `name`; `SHOW ALL`
    /// (name, setting, description); `EXPLAIN` one `text` column `QUERY PLAN` (`XML`/`JSON`/`YAML`
    /// formats use the matching type name as column name and type `xml`/`json`/`text`).
    pub fn result_columns(&self) -> Option<Vec<OutputColumn>> {
        fn text_col(name: &str) -> OutputColumn {
            OutputColumn { name: name.to_string(), ty: TypeId::TEXT, origin: None }
        }
        match self {
            BoundStatement::Query(q) => Some(q.output.clone()),
            BoundStatement::Insert(i) => returning_columns(&i.returning),
            BoundStatement::Update(u) => returning_columns(&u.returning),
            BoundStatement::Delete(d) => returning_columns(&d.returning),
            BoundStatement::Show(None) => Some(vec![text_col("name"), text_col("setting"), text_col("description")]),
            BoundStatement::Show(Some(n)) => Some(vec![text_col(n)]),
            BoundStatement::Explain(e) => Some(vec![match e.options.format {
                ExplainFormat::Text => text_col("QUERY PLAN"),
                ExplainFormat::Json => {
                    OutputColumn { name: "QUERY PLAN".into(), ty: TypeId::JSON, origin: None }
                }
                ExplainFormat::Xml | ExplainFormat::Yaml => text_col("QUERY PLAN"),
            }]),
            BoundStatement::Call(_) => None,
            _ => None,
        }
    }

    /// Statements that read or write relation data (go through `pgarbor_plan::plan`).
    pub fn is_plannable(&self) -> bool {
        matches!(
            self,
            BoundStatement::Query(_)
                | BoundStatement::Insert(_)
                | BoundStatement::Update(_)
                | BoundStatement::Delete(_)
                | BoundStatement::CreateTableAs(_)
        )
    }
}

fn returning_columns(r: &[TargetEntry]) -> Option<Vec<OutputColumn>> {
    if r.is_empty() {
        return None;
    }
    Some(
        r.iter()
            .filter(|t| !t.junk)
            .map(|t| OutputColumn { name: t.name.clone(), ty: t.expr.ty, origin: t.origin })
            .collect(),
    )
}

fn drop_kind_word(k: DropKind) -> &'static str {
    match k {
        DropKind::Table => "TABLE",
        DropKind::View => "VIEW",
        DropKind::MaterializedView => "MATERIALIZED VIEW",
        DropKind::Index => "INDEX",
        DropKind::Sequence => "SEQUENCE",
        DropKind::ForeignTable => "FOREIGN TABLE",
        DropKind::Type => "TYPE",
        DropKind::Domain => "DOMAIN",
        DropKind::Schema => "SCHEMA",
        DropKind::Function => "FUNCTION",
        DropKind::Procedure => "PROCEDURE",
        DropKind::Routine => "ROUTINE",
        DropKind::Aggregate => "AGGREGATE",
        DropKind::Role => "ROLE",
    }
}
