//! Input specifications for DDL (`CatalogTxn` operations). The BINDER (pgarbor-sql) turns parse
//! trees into these structs: it resolves every name to an OID, evaluates nothing, and deparses
//! every expression into a [`StoredExpr`] (canonical text + dependency list). The catalog never
//! parses SQL. Plain data, fully implemented, frozen (v1).
//!
//! ## Conventions shared by all specs
//! - **Self placeholder** [`SELF_REL`] (`0`): inside a spec for a relation that does not exist yet
//!   (CREATE TABLE / CREATE VIEW), `ObjectAddress::column(SELF_REL, attnum)` /
//!   `ObjectAddress::relation(SELF_REL)` in a `StoredExpr::depends_on` (and `ForeignKey` targets)
//!   mean "the relation being created". The catalog rewrites them to the allocated OID before
//!   storing. For ALTER on an existing relation the binder uses the real OID.
//! - Column attnums inside specs for a NEW table are positional: `attnum = index + 1` in
//!   `CreateTableSpec::columns`.
//! - Names are exact (case-folded, unquoted), at most 63 bytes (longer: 42622 raised by the lexer).
//! - `if_not_exists` / `if_exists` flags make the catalog emit PG's `..., skipping` NOTICE (see
//!   `crate::errors`) instead of the error, and return `Ok(None)`/`Ok(false)`.
use crate::model::*;
use pgarbor_common::Oid;
use pgarbor_types::TypeId;

/// "The relation being created" placeholder OID (module docs).
pub const SELF_REL: Oid = 0;

/// One bound of a sequence (`MINVALUE n` / `NO MINVALUE`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqBound {
    /// `NO MINVALUE` / `NO MAXVALUE`: reset to the default for the type and direction.
    Default,
    Value(i64),
}

/// Sequence options as written (`None` = clause absent). Used by CREATE SEQUENCE, identity columns
/// (`GENERATED ... AS IDENTITY (START WITH 10 ...)`), serial, and ALTER SEQUENCE.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SequenceOptions {
    /// `AS smallint|integer|bigint` (type OID 21/23/20).
    pub type_oid: Option<Oid>,
    pub increment: Option<i64>,
    pub minvalue: Option<SeqBound>,
    pub maxvalue: Option<SeqBound>,
    pub start: Option<i64>,
    /// `RESTART [WITH n]`: `Some(None)` = restart at the start value (ALTER only).
    pub restart: Option<Option<i64>>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
}

/// `serial` pseudo-types.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialKind {
    /// smallserial -> smallint
    Small,
    /// serial / serial4 -> integer
    Int,
    /// bigserial / serial8 -> bigint
    Big,
}

impl SerialKind {
    /// Column type OID (21 / 23 / 20).
    pub fn type_oid(self) -> Oid {
        match self {
            SerialKind::Small => 21,
            SerialKind::Int => 23,
            SerialKind::Big => 20,
        }
    }
}

/// Identity column request.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentitySpec {
    pub kind: IdentityKind,
    pub options: SequenceOptions,
    /// `SEQUENCE NAME n`: the identity sequence's name (unqualified, in the table's schema).
    /// `None` = the PG-chosen `<table>_<column>_seq`.
    pub sequence_name: Option<String>,
}

/// Generated column request (`GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]`; PG 18 default is VIRTUAL).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedSpec {
    pub kind: GeneratedKind,
    pub expr: StoredExpr,
}

/// A column definition (CREATE TABLE, ADD COLUMN, CREATE TYPE ... AS (composite attributes use only
/// `name`, `type_id`, `collation`)).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    /// Declared type and typmod. For `serial` columns the binder passes `TypeId::of(serial.type_oid())`.
    pub type_id: TypeId,
    /// `COLLATE x`; None = the type's default collation (`TypeInfo::collation`).
    pub collation: Option<Oid>,
    /// Declared array dimensions (`attndims`).
    pub ndims: i32,
    /// Column-level `NOT NULL [name]` (the optional constraint name from `CONSTRAINT n NOT NULL`).
    pub not_null: Option<Option<String>>,
    /// `DEFAULT expr`.
    pub default: Option<StoredExpr>,
    pub serial: Option<SerialKind>,
    pub identity: Option<IdentitySpec>,
    pub generated: Option<GeneratedSpec>,
    /// `STORAGE x` (b'p','e','m','x') / `COMPRESSION x` (b'p','l').
    pub storage: Option<u8>,
    pub compression: Option<u8>,
    /// ADD COLUMN only: text output of the evaluated default when it is a non-volatile constant
    /// expression (stored as `Column::missing_value`; existing rows read it without a rewrite).
    /// `None` + a default present => the catalog reports `FollowUp::RewriteTable` instead when the
    /// default is volatile (`default_is_volatile`).
    pub fast_default: Option<String>,
    /// ADD COLUMN only: the default contains a volatile function (e.g. `random()`, `nextval()`) so
    /// every existing row needs its own value.
    pub default_is_volatile: bool,
}

impl ColumnSpec {
    /// A plain nullable column; `collation` None.
    pub fn new(name: impl Into<String>, type_id: TypeId) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            type_id,
            collation: None,
            ndims: 0,
            not_null: None,
            default: None,
            serial: None,
            identity: None,
            generated: None,
            storage: None,
            compression: None,
            fast_default: None,
            default_is_volatile: false,
        }
    }
}

/// What kind of table constraint.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ConstraintSpecKind {
    PrimaryKey {
        columns: Vec<String>,
        /// `INCLUDE (...)` columns of the backing index.
        include: Vec<String>,
    },
    Unique {
        columns: Vec<String>,
        include: Vec<String>,
        nulls_not_distinct: bool,
    },
    Check {
        expr: StoredExpr,
        /// Names of the columns the expression references, in order of first reference (drives
        /// `conkey` and the automatic name `<table>_<first column>_check`).
        columns: Vec<String>,
        no_inherit: bool,
    },
    ForeignKey {
        columns: Vec<String>,
        /// Referenced table; `SELF_REL` = the table being created / altered.
        ref_table: Oid,
        /// Empty = the referenced table's primary key.
        ref_columns: Vec<String>,
        on_update: FkAction,
        on_delete: FkAction,
        match_type: FkMatch,
        /// `ON DELETE SET NULL (a, b)` / `SET DEFAULT (a, b)` column list.
        delete_set_columns: Vec<String>,
    },
    NotNull {
        column: String,
    },
    /// EXCLUDE constraints are not supported in v1 (0A000).
    Exclusion,
}

/// A table constraint as written.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ConstraintSpec {
    /// `CONSTRAINT name`; None = automatic name (`crate::naming`).
    pub name: Option<String>,
    pub kind: ConstraintSpecKind,
    pub deferrable: bool,
    pub initially_deferred: bool,
    /// `NOT VALID` (CHECK / FOREIGN KEY only; others: 0A000 `cannot be marked NOT VALID`).
    pub not_valid: bool,
    /// PG 18 `NOT ENFORCED` is `enforced = false`.
    pub enforced: bool,
    /// `USING INDEX TABLESPACE` etc. are ignored; `WITH (fillfactor=..)` for the backing index.
    pub index_options: Vec<String>,
}

impl ConstraintSpec {
    /// An unnamed, immediate, validated, enforced constraint.
    pub fn new(kind: ConstraintSpecKind) -> ConstraintSpec {
        ConstraintSpec {
            name: None,
            kind,
            deferrable: false,
            initially_deferred: false,
            not_valid: false,
            enforced: true,
            index_options: Vec::new(),
        }
    }
}

/// CREATE TABLE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTableSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    /// `Table` (or `PartitionedTable`: 0A000 in v1).
    pub kind: RelKind,
    pub persistence: Persistence,
    pub columns: Vec<ColumnSpec>,
    pub constraints: Vec<ConstraintSpec>,
    /// `WITH (fillfactor=70)` as `name=value`.
    pub options: Vec<String>,
    pub if_not_exists: bool,
    /// Parent tables (`INHERITS`): 0A000 in v1 (reserved).
    pub inherits: Vec<Oid>,
}

/// What CREATE TABLE created.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreatedTable {
    pub table: Oid,
    /// Serial/identity sequences, in column order.
    pub sequences: Vec<Oid>,
    /// Backing indexes (PK, UNIQUE) in creation order. A new table is empty, so exec does not need
    /// to build them; `FollowUp::BuildIndex` is emitted only for ALTER TABLE ADD ... on existing data.
    pub indexes: Vec<Oid>,
    /// All constraints created (including NOT NULL constraints), in creation order.
    pub constraints: Vec<Oid>,
}

/// One key of CREATE INDEX.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexKeySpec {
    /// Plain column key (name) or None for an expression key.
    pub column: Option<String>,
    pub expr: Option<StoredExpr>,
    /// For expression keys: Some(function name) when the expression is a top-level function call
    /// (drives the index attribute name `lower`), else None (`expr`, `expr1`, ...).
    pub expr_func_name: Option<String>,
    /// Type of an expression key (typmod -1); ignored for column keys.
    pub expr_type: Option<TypeId>,
    /// Explicit operator class OID; None = default btree opclass of the key type
    /// (42704 `data type X has no default operator class for access method "btree"`).
    pub opclass: Option<Oid>,
    /// Explicit `COLLATE`; None = column's / expression's collation.
    pub collation: Option<Oid>,
    pub descending: bool,
    /// `NULLS FIRST`/`LAST` explicitly given; None = PG default (NULLS LAST for ASC, FIRST for DESC).
    pub nulls_first: Option<bool>,
}

/// CREATE [UNIQUE] INDEX.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct IndexSpec {
    pub table: Oid,
    /// None = automatic name (`naming::choose_index_name`).
    pub name: Option<String>,
    pub unique: bool,
    pub nulls_not_distinct: bool,
    pub method: IndexMethod,
    pub keys: Vec<IndexKeySpec>,
    /// `INCLUDE (cols)`.
    pub include: Vec<String>,
    /// `WHERE predicate`.
    pub predicate: Option<StoredExpr>,
    pub options: Vec<String>,
    pub if_not_exists: bool,
    /// Accepted and ignored (`CREATE INDEX CONCURRENTLY` behaves as a plain build).
    pub concurrently: bool,
}

/// CREATE [OR REPLACE] [MATERIALIZED] VIEW.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ViewSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    /// Output columns (names after any column-alias list, types after analysis).
    pub columns: Vec<ColumnSpec>,
    /// `pg_get_viewdef` text + dependencies (relations, columns, functions, types referenced).
    pub query: StoredExpr,
    pub or_replace: bool,
    pub materialized: bool,
    /// Materialized views: false for `WITH NO DATA`.
    pub populated: bool,
    /// `WITH (security_barrier=true)` etc.
    pub options: Vec<String>,
    /// `WITH [LOCAL|CASCADED] CHECK OPTION`.
    pub check_option: Option<String>,
    pub if_not_exists: bool,
}

/// CREATE SEQUENCE.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CreateSequenceSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    pub persistence: Persistence,
    pub options: SequenceOptions,
    /// `OWNED BY table.column` (checked: same owner and schema, 55000).
    pub owned_by: Option<(Oid, i16)>,
    pub if_not_exists: bool,
}

/// CREATE TYPE ... AS ENUM.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateEnumSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    pub labels: Vec<String>,
}

/// One domain constraint.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct DomainConstraintSpec {
    pub name: Option<String>,
    pub expr: StoredExpr,
    pub not_valid: bool,
}

/// CREATE DOMAIN.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CreateDomainSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    pub base: TypeId,
    pub collation: Option<Oid>,
    pub default: Option<StoredExpr>,
    /// `NOT NULL` (optionally named).
    pub not_null: Option<Option<String>>,
    pub checks: Vec<DomainConstraintSpec>,
    pub ndims: i32,
}

/// CREATE TYPE name AS (attributes).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CreateCompositeSpec {
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    /// Only `name`, `type_id`, `collation` are used.
    pub attributes: Vec<ColumnSpec>,
}

/// CREATE ROLE / USER / GROUP.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateRoleSpec {
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub can_login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub conn_limit: i32,
    /// Already-hashed verifier (`SCRAM-SHA-256$...` or `md5...`); None = no password. The catalog
    /// rejects anything else (22023 `password is not hashed`: PGArbor-specific guard).
    pub password: Option<String>,
    pub valid_until: Option<i64>,
    /// `IN ROLE a, b` (make the new role a member of these).
    pub in_roles: Vec<Oid>,
    /// `ROLE a, b` (make these members of the new role).
    pub members: Vec<Oid>,
    /// `ADMIN a, b` (members WITH ADMIN OPTION).
    pub admins: Vec<Oid>,
}

impl CreateRoleSpec {
    /// PG defaults: NOSUPERUSER INHERIT NOCREATEROLE NOCREATEDB NOLOGIN NOREPLICATION NOBYPASSRLS,
    /// CONNECTION LIMIT -1.
    pub fn new(name: impl Into<String>) -> CreateRoleSpec {
        CreateRoleSpec {
            name: name.into(),
            superuser: false,
            inherit: true,
            create_role: false,
            create_db: false,
            can_login: false,
            replication: false,
            bypass_rls: false,
            conn_limit: -1,
            password: None,
            valid_until: None,
            in_roles: Vec::new(),
            members: Vec::new(),
            admins: Vec::new(),
        }
    }
}

/// ALTER ROLE changes (`None` = unchanged).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleChanges {
    pub superuser: Option<bool>,
    pub inherit: Option<bool>,
    pub create_role: Option<bool>,
    pub create_db: Option<bool>,
    pub can_login: Option<bool>,
    pub replication: Option<bool>,
    pub bypass_rls: Option<bool>,
    pub conn_limit: Option<i32>,
    /// `Some(None)` = `PASSWORD NULL`.
    pub password: Option<Option<String>>,
    pub valid_until: Option<Option<i64>>,
}

/// Privilege request set.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivilegeSet {
    /// `ALL [PRIVILEGES]`: every privilege applicable to the object kind.
    All,
    /// Explicit `acl_mode` bits.
    Bits(u32),
}

/// Target of GRANT/REVOKE.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantTarget {
    /// One object (table/view/sequence/function/schema/type/database ...). Columns: see
    /// `GrantSpec::columns`.
    Object(ObjectAddress),
    AllTablesInSchema(Oid),
    AllSequencesInSchema(Oid),
    AllFunctionsInSchema(Oid),
}

/// GRANT / REVOKE privileges.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantSpec {
    pub target: GrantTarget,
    pub privileges: PrivilegeSet,
    /// Grantee role OIDs; `0` = PUBLIC.
    pub grantees: Vec<Oid>,
    /// `WITH GRANT OPTION` (GRANT) / `GRANT OPTION FOR` (REVOKE).
    pub grant_option: bool,
    /// Role issuing the grant (`GRANTED BY`, default current user; ownership-implied grants are
    /// recorded with the owner as grantor, PG `select_best_grantor`).
    pub grantor: Oid,
    /// `GRANT SELECT (a, b) ON t`: column attnums (empty = whole object).
    pub columns: Vec<i16>,
    /// REVOKE ... CASCADE.
    pub cascade: bool,
}

/// `GRANT role TO member` / `REVOKE role FROM member`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MembershipSpec {
    pub role: Oid,
    pub member: Oid,
    pub grantor: Oid,
    pub admin_option: bool,
    /// None = default (inherit = the member's `rolinherit`).
    pub inherit_option: Option<bool>,
    pub set_option: bool,
}

/// What to do about stored data, reported to the executor after DDL (`DdlOutcome::follow_ups`).
/// The catalog never touches relation data keys or sequence state (spaces it does not own).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    /// Populate a NEW index of a table that may already hold rows. Uniqueness failure -> 23505
    /// `could not create unique index "i"` + DETAIL `Key (a)=(1) is duplicated.` (exec).
    BuildIndex { index: Oid, table: Oid },
    /// SET NOT NULL / ADD PRIMARY KEY on existing rows: scan for NULLs; violation -> 23502
    /// `column "a" of relation "t" contains null values`.
    ValidateNotNull { table: Oid, attnum: i16 },
    /// ADD CHECK (not NOT VALID) on existing rows -> 23514 `check constraint "c" of relation "t" is violated by some row`.
    ValidateCheck { table: Oid, constraint: Oid },
    /// ADD FOREIGN KEY (not NOT VALID) -> 23503 `insert or update on table "t" violates foreign key constraint "c"`.
    ValidateForeignKey { table: Oid, constraint: Oid },
    /// Rewrite every row of the table (ALTER COLUMN TYPE with a real conversion, ADD COLUMN with a
    /// volatile default, STORED generated column added/changed) then rebuild all indexes.
    RewriteTable { table: Oid },
    /// Delete all data keys of this relation or index OID (DROP TABLE/INDEX/MATERIALIZED VIEW).
    DropRelationData { relation: Oid },
    /// Create sequence state (`pgarbor_core::sequence::store::create`) for the sequence definition.
    CreateSequenceState { sequence: Oid },
    /// Drop sequence state.
    DropSequenceState { sequence: Oid },
    /// ALTER SEQUENCE ... RESTART [WITH n]: set state so the next `nextval` returns `value`.
    RestartSequence { sequence: Oid, value: i64 },
    /// REFRESH MATERIALIZED VIEW / WITH DATA: (re)populate the matview from its query.
    PopulateMatView { relation: Oid },
    /// ALTER TYPE (composite) ADD/DROP/ALTER ATTRIBUTE while `table.attnum` is declared with that
    /// composite type: PG allows it only when the table holds no rows (its
    /// `find_composite_type_dependencies` skips empty tables; oracle-verified). The executor raises
    /// 0A000 `cannot alter type "c" because column "u.c" uses it` (`errors::cannot_alter_type_column_uses_it`)
    /// when the table is not empty.
    RequireEmptyTable { table: Oid, attnum: i16, type_oid: Oid },
}

/// A name change that invalidates the CANONICAL TEXT of stored expressions (defaults, CHECK bodies,
/// index expressions/predicates, view queries, generation expressions, function bodies of
/// BEGIN ATOMIC functions). PostgreSQL stores parsed trees and re-deparses on demand, so a rename
/// is free there; PGArbor stores deparsed text (`StoredExpr::sql`) and therefore asks the SQL layer
/// to re-deparse every affected expression. Passed to the rename operations as an [`ExprRewriter`].
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameChange {
    /// `ALTER TABLE t RENAME COLUMN old TO new` (also view columns): expressions whose
    /// `depends_on` contains `ObjectAddress::column(table, attnum)`.
    Column { table: Oid, attnum: i16, old: String, new: String },
    /// Rename of a table/view/sequence/index/matview (`nextval('old'::regclass)` defaults,
    /// view queries mentioning the table, ...): expressions depending on `ObjectAddress::relation(oid)`.
    Relation { oid: Oid, old: String, new: String },
    /// `ALTER ... SET SCHEMA` of a relation: qualification of regclass literals / view queries.
    RelationSchema { oid: Oid, old_schema: Oid, new_schema: Oid },
    /// Schema rename: expressions that print the schema qualifier (`s.f(x)`, `'s.seq'::regclass`).
    Schema { oid: Oid, old: String, new: String },
    /// Type rename / SET SCHEMA (`'a'::mood`).
    Type { oid: Oid, old: String, new: String },
    /// Function rename / SET SCHEMA.
    Function { oid: Oid, old: String, new: String },
}

/// Supplied by the SQL layer (pgarbor-sql owns the deparser). Given a stored expression that
/// depends on the changed object and the change, returns the expression with the SAME meaning and
/// canonical text under the new names (`CHECK ((bb <> ''::text))` after renaming `b` to `bb`;
/// view query ` SELECT a,\n    bb AS b\n   FROM t;`). It may update `bound` and `depends_on`. The
/// catalog calls it once per affected expression, with the catalog snapshot AFTER the rename
/// available through `env` if the rewriter wants to re-bind.
// CONTRACT
pub trait ExprRewriter {
    fn rewrite(
        &self,
        expr: &StoredExpr,
        change: &NameChange,
        env: &crate::env::CatalogEnv,
    ) -> pgarbor_common::Result<StoredExpr>;
}

/// Returns every expression unchanged (tests, and statements that provably affect no expression).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRewrite;

impl ExprRewriter for NoRewrite {
    fn rewrite(
        &self,
        expr: &StoredExpr,
        _change: &NameChange,
        _env: &crate::env::CatalogEnv,
    ) -> pgarbor_common::Result<StoredExpr> {
        Ok(expr.clone())
    }
}
