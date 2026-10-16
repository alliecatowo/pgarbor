//! CREATE TABLE and CREATE TABLE AS / SELECT INTO (SQL-30). PG references: `parse_utilcmd.c`
//! (`transformCreateStmt`, `transformColumnDefinition`, `transformTableConstraint`, serial handling
//! `generateSerialExtraStmts`, `transformIndexConstraints`), `tablecmds.c` (`DefineRelation`,
//! `MergeAttributes`), `createas.c`, `gram.y` (`columnDef`, `ColQualList`, `TableConstraint`).
//!
//! `bind_create_table(b, ct)` -> `BoundStatement::CreateTable(Box<CreateTableSpec>)` (or
//! `CreateTableAs` when `ct.query` is present):
//! * Name: `Binder::qual` + `env.creation_namespace_for(schema, name)` (first schema of search_path when
//!   unqualified; 3F000 `no schema has been selected to create in` / 3F000 `schema "x" does not exist`);
//!   `IF NOT EXISTS`; `OR REPLACE` not valid for tables (42601).
//! * Unsupported -> 0A000 naming the feature: `TEMP`/`TEMPORARY`/`GLOBAL`/`LOCAL` (temp tables),
//!   `INHERITS`, `PARTITION BY`, `PARTITION OF`, `OF type_name`, `LIKE` (is supported by PG; v1: 0A000
//!   `CREATE TABLE ... LIKE`), `ON COMMIT`, tablespaces, `WITHOUT OIDS`, foreign tables, non-PG options
//!   (`ENGINE=`, `COMMENT=`, `CLUSTER BY`, `ORDER BY`, `AS ... WITH NO DATA` is supported).
//!   `UNLOGGED` -> `Persistence::Unlogged`; storage `WITH (fillfactor=70)` -> `options` as `name=value`.
//! * **Columns** (`ColumnDef { name, data_type, options }`): fold + truncate names (`Binder::ident`);
//!   duplicates 42701 `column "a" specified more than once`; more than 1600 columns: 54011 `tables
//!   can have at most 1600 columns`. Type: serial pseudo-types (`serial`, `serial2`, `serial4`,
//!   `serial8`, `smallserial`, `bigserial` — arrive as `DataType::Custom`) -> `ColumnSpec.serial =
//!   Some(SerialKind)`, `type_id` = int2/int4/int8, NOT NULL implied; otherwise
//!   `types::resolve_data_type`. Pseudo-types: 42P16 `column "c" has pseudo-type anyelement`
//!   (`types::is_pseudo_type`); shell/undefined: 42704.
//! * **Column options** (`ColumnOption`): `NotNull` -> `not_null: Some(None)`; `Null` -> explicit
//!   nullable; `Default(expr)` -> `stored::bind_ddl_expr` with `ExprCtx::column_default()` coerced
//!   (Assignment) to the column type (`bind_ddl_expr` target) -> `default`; a second DEFAULT: 42601
//!   `multiple default values specified for column "c" of table "t"` (`diag::invalid_table_definition`,
//!   42601 per PG); `Collation(name)` -> `collation: Some(oid)` (42804 `collations are not supported by
//!   type integer`); `Generated { generated_as, sequence_options, generation_expr, generation_expr_mode }`:
//!   `ALWAYS`/`BY DEFAULT AS IDENTITY [(seq options)]` -> `identity: Some(IdentitySpec{kind, options})`
//!   (`ExprCtx` none; type must be smallint/integer/bigint: 22023 `identity column type must be
//!   smallint, integer, or bigint`; identity + DEFAULT: 42601 `both default and identity specified for
//!   column "c" of table "t"`), `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]` -> `generated:
//!   Some(GeneratedSpec{kind, expr})` bound with `ExprCtx::generated()` (generated columns may only use
//!   immutable functions: 42P17 `generation expression is not immutable`; may not reference other
//!   generated columns; can't have a DEFAULT: 42601 `both default and generation expression specified
//!   for column "c" of table "t"`); `PrimaryKey`/`Unique`/`ForeignKey`/`Check` column constraints ->
//!   `ddl_constraint::bind_column_constraint` (they become table `ConstraintSpec`s); `Comment`,
//!   `OnUpdate`, `CharacterSet`, `Materialized`, `Ephemeral`, `Alias`, `Options`, `Policy`, `Tags`,
//!   `Identity`(MSSQL), `OnConflict`, `DialectSpecific`: 42601.
//! * **Table constraints** (`TableConstraint`): `ddl_constraint::bind_table_constraint`. Constraint-name
//!   collisions are the catalog's job (42710 `constraint "x" for relation "t" already exists`).
//!   Multiple PRIMARY KEYs: 42P16 `multiple primary keys for table "t" are not allowed`.
//!   PRIMARY KEY columns become NOT NULL (catalog does it; binder only lists columns, 42703 `column
//!   "x" named in key does not exist`).
//! * Result: `CreateTableSpec { namespace, name, owner: env.current_user(), kind: RelKind::Table,
//!   persistence, columns, constraints, options, if_not_exists, inherits: [] }`.
//!
//! ## CREATE TABLE AS ([`bind_create_table_as`]) / SELECT INTO ([`bind_select_into`])
//! Bind the query (`select::bind_query`, `keep_unknown_outputs: false`: unknown -> text); columns = query
//! outputs renamed by the optional column list (more names than columns 42601 `too many column names
//! were specified`); no constraints; `with_data` from `WITH [NO] DATA`. Duplicate output names:
//! 42701 `column "x" specified more than once`. `IF NOT EXISTS`, `UNLOGGED` honored; TEMP 0A000;
//! `CREATE TABLE AS EXECUTE` 0A000. Result `BoundStatement::CreateTableAs`.
use std::collections::HashSet;

use pgarbor_catalog::ddl::spec::{
    ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec, GeneratedSpec, IdentitySpec, SerialKind,
};
use pgarbor_catalog::model::{GeneratedKind, IdentityKind, Persistence, RelKind};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::{oids, TypeEnv, TypeId};
use sqlparser::ast::{
    ArrayElemTypeDef, ColumnDef, ColumnOption, CreateTable, CreateTableOptions, DataType, Expr, GeneratedAs,
    GeneratedExpressionMode, Ident, ObjectNamePart, Query, SelectInto, SetExpr, SequenceOptions as AstSequenceOption, SqlOption,
};

use super::context::{Binder, ExprCtx};
use super::ddl_constraint::{self, ConstraintTableCtx};
use super::ddl_objects::bind_sequence_options;
use super::ddl_partition;
use super::select::{self, QueryCtx};
use super::stored;
use super::types;
use crate::diag;
use crate::ir::{BoundCreateTableAs, BoundQuery, BoundStatement};
use crate::names::{fold_ident, QualName};

/// heap.c `CheckAttributeType` with `CHKATYPE_IS_VIRTUAL`: 0A000 for a user-defined column type.
pub(super) fn check_virtual_generated_type(col_name: &str, type_oid: Oid) -> Result<()> {
    if type_oid >= pgarbor_common::FIRST_NORMAL_OID {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("virtual generated column \"{col_name}\" cannot have a user-defined type"),
        )
        .with_detail("Virtual generated columns that make use of user-defined types are not yet supported."));
    }
    Ok(())
}

/// heap.c `check_virtual_generated_security`: a virtual generation expression may only use built-in
/// functions, operators and types.
pub(super) fn check_virtual_generated_security(bound: &crate::ir::BoundExpr, pos: Option<u32>) -> Result<()> {
    use crate::ir::ExprKind;
    let first = pgarbor_common::FIRST_NORMAL_OID;
    let mut found: Option<bool> = None;
    bound.walk(&mut |e| {
        if found.is_some() {
            return false;
        }
        let user_func = match &e.kind {
            ExprKind::FuncCall(f) => f.func >= first,
            ExprKind::Op(o) => o.op.proc >= first,
            _ => false,
        };
        if user_func {
            found = Some(true);
        } else if e.ty.oid >= first {
            found = Some(false);
        }
        found.is_none()
    });
    match found {
        Some(true) => Err(diag::with_position(
            PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "generation expression uses user-defined function")
                .with_detail("Virtual generated columns that make use of user-defined functions are not yet supported."),
            pos,
        )),
        Some(false) => Err(diag::with_position(
            PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "generation expression uses user-defined type")
                .with_detail("Virtual generated columns that make use of user-defined types are not yet supported."),
            pos,
        )),
        None => Ok(()),
    }
}

/// PostgreSQL `MaxHeapAttributeNumber`.
const MAX_TABLE_COLUMNS: usize = 1600;
/// 54011 `tables can have at most 1600 columns` (heap.c `MergeAttributes`).
const TOO_MANY_COLUMNS: SqlState = SqlState(*b"54011");

/// Bind CREATE TABLE (module docs).
pub fn bind_create_table(b: &mut Binder, ct: &CreateTable) -> Result<BoundStatement> {
    if ct.query.is_some() {
        return bind_create_table_as(b, ct);
    }
    let pos = b.pos(&ct.name);
    reject_unsupported_table(ct, pos)?;
    let qn = b.qual(&ct.name)?;
    check_catalog(b, &qn)?;
    let namespace = creation_namespace(b, &qn)?;
    let options = bind_table_options(&ct.table_options)?;
    if ct.columns.len() > MAX_TABLE_COLUMNS {
        return Err(PgError::new(TOO_MANY_COLUMNS, format!("tables can have at most {MAX_TABLE_COLUMNS} columns")));
    }
    let table = qn.name.clone();
    let hierarchy = ddl_partition::bind_hierarchy(b, ct)?;
    let mut columns = bind_column_defs(b, &ct.columns, hierarchy.partition_parent)?;
    let scope: Vec<(String, TypeId)> = hierarchy.scope(&columns);
    let tctx = ConstraintTableCtx { table_name: table.clone(), table_oid: None, columns: scope.clone() };
    // Every column is declared before any clause is bound (generated expressions may reference later
    // columns). The scope is popped on every path, success or error.
    stored::table_expr_scope(b, &table, &scope)?;
    let outcome = bind_table_level(b, ct, &mut columns, &table, &tctx);
    let key = match (&outcome, &ct.partition_by) {
        (Ok(_), Some(by)) => Some(ddl_partition::bind_partition_by(b, by, &scope)),
        _ => None,
    };
    b.scopes.pop();
    let constraints = outcome?;
    let partition_key = key.transpose()?;
    let temporary = is_temporary(ct);
    let persistence = if temporary {
        Persistence::Temp
    } else if ct.unlogged {
        Persistence::Unlogged
    } else {
        Persistence::Permanent
    };
    let on_commit = ddl_partition::bind_on_commit(ct, temporary)?;
    let partition_of = match (hierarchy.partition_parent, &ct.for_values) {
        (Some(parent), Some(fv)) => {
            let after = ct.partition_of.as_ref().and_then(|n| ddl_partition::after_parent_name(b, n));
            Some(ddl_partition::bind_partition_bound(b, parent, fv, after)?)
        }
        (Some(_), None) => return Err(diag::syntax_error_near("FOR", pos)),
        _ => None,
    };
    let kind = if partition_key.is_some() { RelKind::PartitionedTable } else { RelKind::Table };
    let spec = CreateTableSpec {
        namespace,
        name: table,
        owner: b.env.current_user(),
        kind,
        persistence,
        columns,
        constraints,
        options,
        if_not_exists: ct.if_not_exists,
        inherits: hierarchy.parent_oids(),
        extra: pgarbor_catalog::ddl::spec::CreateTableExtra { partition_key, partition_of, on_commit },
    };
    Ok(BoundStatement::CreateTable(Box::new(spec)))
}

/// `CREATE TEMP|TEMPORARY|LOCAL TEMP TABLE` (GLOBAL TEMP is accepted with PG's warning elsewhere).
fn is_temporary(ct: &CreateTable) -> bool {
    ct.temporary
}

/// `CREATE TABLE name [(cols)] AS query [WITH [NO] DATA]` (called by `bind_create_table` when
/// `ct.query` is set).
/// The identity option list of the next `AS IDENTITY` in the statement: the parsed group from
/// `identity_opts` (its options and `SEQUENCE NAME`), or, when the statement was not extracted, the
/// sqlparser option list with no name.
pub(super) fn identity_option_list(
    b: &mut Binder,
    ast_options: Option<&[AstSequenceOption]>,
) -> Result<(Vec<AstSequenceOption>, Option<String>)> {
    match b.identity_groups.pop_front() {
        Some(g) => {
            if g.sequence_name_qualified {
                return Err(PgError::unimplemented("SQL-30", "SEQUENCE NAME with a schema-qualified name"));
            }
            Ok((g.options, g.sequence_name))
        }
        None => Ok((ast_options.map(|o| o.to_vec()).unwrap_or_default(), None)),
    }
}

pub fn bind_create_table_as(b: &mut Binder, ct: &CreateTable) -> Result<BoundStatement> {
    let Some(query) = ct.query.as_deref() else {
        return Err(PgError::internal("CREATE TABLE AS bound without a query"));
    };
    let pos = b.pos(&ct.name);
    reject_unsupported_table(ct, pos)?;
    for (set, what) in [
        (ct.inherits.is_some(), "INHERITS"),
        (ct.partition_by.is_some(), "PARTITION BY"),
        (ct.partition_of.is_some(), "PARTITION OF"),
    ] {
        if set {
            return Err(diag::syntax_error_near(what, pos));
        }
    }
    if let Some(tc) = ct.constraints.first() {
        return Err(syntax_near(&tc.to_string(), pos));
    }
    for def in &ct.columns {
        if !matches!(def.data_type, DataType::Unspecified) || !def.options.is_empty() {
            let npos = b.pos_span(def.name.span.clone());
            return Err(diag::syntax_error_near(&def.name.value, npos));
        }
    }
    let qn = b.qual(&ct.name)?;
    check_catalog(b, &qn)?;
    let namespace = ctas_creation_namespace(b, &qn)?;
    let options = bind_table_options(&ct.table_options)?;
    let aliases: Vec<String> = ct.columns.iter().map(|c| b.ident(&c.name)).collect();
    let bq = select::bind_query(
        b,
        query,
        &QueryCtx { column_aliases: Vec::new(), top_level: false, keep_unknown_outputs: false },
    )?;
    let columns = ctas_columns(&bq, &aliases)?;
    let with_data = ct.with_data.as_ref().map_or(true, |w| w.data);
    let persistence = if ct.unlogged { Persistence::Unlogged } else { Persistence::Permanent };
    let table = CreateTableSpec {
        namespace,
        name: qn.name,
        owner: b.env.current_user(),
        kind: RelKind::Table,
        persistence,
        columns,
        constraints: Vec::new(),
        options,
        if_not_exists: ct.if_not_exists,
        inherits: Vec::new(),
        extra: Default::default(),
    };
    Ok(BoundStatement::CreateTableAs(Box::new(BoundCreateTableAs { table, query: Box::new(bq), with_data })))
}

/// A copy of `q` whose top SELECT block no longer carries its `INTO` clause. The statement-level
/// `INTO` names the new table and is taken by [`bind_select_into`]; the block itself must bind as a
/// plain query, otherwise `reject_select_extensions` would refuse it. Nested `INTO` still fails there.
fn query_without_into(q: &Query) -> Query {
    let mut inner = q.clone();
    clear_leftmost_into(&mut inner.body);
    inner
}

/// Clears the `INTO` of the leftmost SELECT leaf of a set-operation body (see
/// `select::leftmost_select_into`); other leaves keep theirs so they are still rejected.
fn clear_leftmost_into(body: &mut SetExpr) {
    match body {
        SetExpr::Select(sel) => sel.into = None,
        SetExpr::SetOperation { left, .. } => clear_leftmost_into(&mut **left),
        _ => {}
    }
}

/// `SELECT ... INTO [TEMP] t FROM ...` (statement-level; `into` taken from the top SELECT block).
pub fn bind_select_into(b: &mut Binder, q: &Query, into: &SelectInto) -> Result<BoundStatement> {
    let target = match into.targets.as_slice() {
        [one] => one,
        _ => return Err(diag::syntax_error_near("INTO", None)),
    };
    let pos = b.pos(target);
    if into.temporary {
        return Err(diag::unsupported_construct("temporary tables", pos));
    }
    let qn = into_name(b, target, pos)?;
    check_catalog(b, &qn)?;
    let namespace = ctas_creation_namespace(b, &qn)?;
    let inner = query_without_into(q);
    let bq = select::bind_query(
        b,
        &inner,
        &QueryCtx { column_aliases: Vec::new(), top_level: false, keep_unknown_outputs: false },
    )?;
    let columns = ctas_columns(&bq, &[])?;
    let persistence = if into.unlogged { Persistence::Unlogged } else { Persistence::Permanent };
    let table = CreateTableSpec {
        namespace,
        name: qn.name,
        owner: b.env.current_user(),
        kind: RelKind::Table,
        persistence,
        columns,
        constraints: Vec::new(),
        options: Vec::new(),
        if_not_exists: false,
        inherits: Vec::new(),
        extra: Default::default(),
    };
    Ok(BoundStatement::CreateTableAs(Box::new(BoundCreateTableAs { table, query: Box::new(bq), with_data: true })))
}

// ---------------------------------------------------------------------------------------------
// Table level
// ---------------------------------------------------------------------------------------------

/// Column clauses, then table constraints in written order, then the PRIMARY KEY multiplicity check.
fn bind_table_level(
    b: &mut Binder,
    ct: &CreateTable,
    columns: &mut [ColumnSpec],
    table: &str,
    tctx: &ConstraintTableCtx,
) -> Result<Vec<ConstraintSpec>> {
    let mut constraints = bind_column_clauses(b, &ct.columns, columns, table, tctx)?;
    for tc in &ct.constraints {
        constraints.push(ddl_constraint::bind_table_constraint(b, tc, tctx)?);
    }
    check_single_primary_key(table, &constraints)?;
    Ok(constraints)
}

fn check_single_primary_key(table: &str, constraints: &[ConstraintSpec]) -> Result<()> {
    let count = constraints.iter().filter(|c| matches!(c.kind, ConstraintSpecKind::PrimaryKey { .. })).count();
    if count > 1 {
        return Err(PgError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("multiple primary keys for table \"{table}\" are not allowed"),
        ));
    }
    Ok(())
}

/// Rejects CREATE TABLE forms PGArbor does not implement (0A000) and syntax PostgreSQL does not have.
fn reject_unsupported_table(ct: &CreateTable, pos: Option<u32>) -> Result<()> {
    if ct.or_replace {
        return Err(diag::syntax_error_near("TABLE", pos));
    }
    if ct.temporary || ct.global.is_some() {
        return Err(diag::unsupported_construct("temporary tables", pos));
    }
    let checks: &[(bool, &str)] = &[
        (ct.external, "EXTERNAL TABLE"),
        (ct.dynamic, "DYNAMIC TABLE"),
        (ct.transient, "TRANSIENT TABLE"),
        (ct.volatile, "VOLATILE TABLE"),
        (ct.iceberg, "ICEBERG TABLE"),
        (ct.snapshot, "SNAPSHOT TABLE"),
        (ct.like.is_some(), "CREATE TABLE ... LIKE"),
        (ct.clone.is_some(), "CREATE TABLE ... CLONE"),
        (ct.for_values.is_some() && ct.partition_of.is_none(), "FOR VALUES"),
        (ct.comment.is_some(), "COMMENT"),
        (ct.on_cluster.is_some(), "ON CLUSTER"),
        (ct.order_by.is_some(), "ORDER BY"),
        (ct.cluster_by.is_some() || ct.clustered_by.is_some(), "CLUSTER BY"),
        (ct.primary_key.is_some(), "PRIMARY KEY clause"),
        (ct.location.is_some() || ct.file_format.is_some() || ct.hive_formats.is_some(), "external storage clauses"),
        (ct.without_rowid, "WITHOUT ROWID"),
        (ct.version.is_some(), "table VERSION clause"),
    ];
    for (set, what) in checks {
        if *set {
            return Err(diag::unsupported_construct(what, pos));
        }
    }
    Ok(())
}

/// `WITH (name=value, ...)` storage parameters, rendered as `name=value`.
fn bind_table_options(opts: &CreateTableOptions) -> Result<Vec<String>> {
    match opts {
        CreateTableOptions::None => Ok(Vec::new()),
        CreateTableOptions::With(list) => list.iter().map(storage_option).collect(),
        _ => Err(diag::unsupported_construct("table options other than WITH (...)", None)),
    }
}

/// One storage parameter. Only `fillfactor` exists for heap tables in this engine (reloptions.c
/// messages: 22023 `unrecognized parameter`, `invalid value for integer option`, `value N out of bounds`).
fn storage_option(opt: &SqlOption) -> Result<String> {
    let SqlOption::KeyValue { key, value } = opt else {
        return Err(syntax_near(&opt.to_string(), None));
    };
    let name = fold_ident(key);
    if name != "fillfactor" {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("unrecognized parameter \"{name}\"")));
    }
    let text = value.to_string();
    let n: i64 = match text.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid value for integer option \"fillfactor\": {text}"),
            ))
        }
    };
    if !(10..=100).contains(&n) {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("value {n} out of bounds for option \"fillfactor\""),
        )
        .with_detail("Valid values are between \"10\" and \"100\"."));
    }
    Ok(format!("fillfactor={n}"))
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// The name must be in this database (3-part names); cross-database references are 0A000.
fn check_catalog(b: &Binder, qn: &QualName) -> Result<()> {
    match &qn.catalog {
        Some(cat) if cat != &b.ctx.session.database => {
            Err(diag::cross_database(&format!("{cat}.{}", qn.display()), qn.pos))
        }
        _ => Ok(()),
    }
}

/// Plain CREATE TABLE reports the RangeVar location (`transformCreateStmt` installs a parser
/// errposition callback); CREATE TABLE AS / SELECT INTO resolve the namespace in `DefineRelation`
/// at execution time, without a position.
fn creation_namespace(b: &Binder, qn: &QualName) -> Result<Oid> {
    let ns = b.env.creation_namespace_for(qn.schema.as_deref(), &qn.name).map_err(|e| diag::with_position(e, qn.pos))?;
    b.env.check_schema_create(ns).map_err(|e| diag::with_position(e, qn.pos))?;
    Ok(ns)
}

fn ctas_creation_namespace(b: &Binder, qn: &QualName) -> Result<Oid> {
    let ns = b.env.creation_namespace_for(qn.schema.as_deref(), &qn.name)?;
    b.env.check_schema_create(ns)?;
    Ok(ns)
}

/// Target of `SELECT ... INTO name`: one to three identifiers.
fn into_name(b: &mut Binder, target: &Expr, pos: Option<u32>) -> Result<QualName> {
    let idents: Vec<&Ident> = match target {
        Expr::Identifier(id) => vec![id],
        Expr::CompoundIdentifier(ids) => ids.iter().collect(),
        _ => return Err(diag::syntax_error_near("INTO", pos)),
    };
    if idents.is_empty() || idents.len() > 3 {
        return Err(diag::syntax_error_near("INTO", pos));
    }
    let mut parts: Vec<String> = idents.iter().map(|id| b.ident(*id)).collect();
    let n = parts.len();
    let name = parts.pop().unwrap_or_default();
    let schema = if n >= 2 { parts.pop() } else { None };
    let catalog = if n == 3 { parts.pop() } else { None };
    Ok(QualName { catalog, schema, name, pos })
}

// ---------------------------------------------------------------------------------------------
// Columns: pass 1 (names, types) and pass 2 (options and column-level constraints)
// ---------------------------------------------------------------------------------------------

/// Pass 1: fold and check names (42701), resolve types (serial pseudo-types, pseudo-types 42P16).
fn bind_column_defs(b: &mut Binder, defs: &[ColumnDef], partition_parent: Option<&pgarbor_catalog::Relation>) -> Result<Vec<ColumnSpec>> {
    let mut columns: Vec<ColumnSpec> = Vec::with_capacity(defs.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(defs.len());
    for def in defs {
        let name = b.ident(&def.name);
        if !seen.insert(name.clone()) {
            return Err(PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{name}\" specified more than once")));
        }
        if let Some(parent) = partition_parent {
            // `PARTITION OF` column entries carry no type (parser_hier marker): the parent's type.
            let ty = ddl_partition::partition_column_type(parent, &name)?;
            columns.push(ColumnSpec::new(name, ty));
            continue;
        }
        if is_serial_array(&def.data_type) {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "array of serial is not implemented"));
        }
        if let Some(kind) = serial_kind(&def.data_type) {
            let mut column = ColumnSpec::new(name, TypeId::of(kind.type_oid()));
            column.serial = Some(kind);
            column.not_null = Some(None);
            columns.push(column);
            continue;
        }
        let pos = b.pos_span(def.name.span.clone());
        let resolved = types::resolve_data_type(b, &def.data_type, pos)?;
        if types::is_pseudo_type(resolved.id.oid) {
            let tn = b.type_name(resolved.id.clone());
            return Err(PgError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("column \"{name}\" has pseudo-type {tn}"),
            ));
        }
        let mut column = ColumnSpec::new(name, resolved.id.clone());
        column.ndims = resolved.ndims;
        columns.push(column);
    }
    Ok(columns)
}

/// Pass 2: the column options, in written order, with the PostgreSQL conflict rules
/// (`transformColumnDefinition`). Returns the column-level constraints (PK / UNIQUE / FK / CHECK).
fn bind_column_clauses(
    b: &mut Binder,
    defs: &[ColumnDef],
    columns: &mut [ColumnSpec],
    table: &str,
    tctx: &ConstraintTableCtx,
) -> Result<Vec<ConstraintSpec>> {
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let generated_flags: Vec<bool> = defs
        .iter()
        .map(|d| d.options.iter().any(|o| matches!(o.option, ColumnOption::Generated { generation_expr: Some(_), .. })))
        .collect();
    // Dependencies use binder positions in the table scope (inherited columns follow the local ones).
    let scope_names: Vec<String> = tctx.columns.iter().map(|(n, _)| n.clone()).collect();
    let names_fn = |attnum: i16| -> Option<String> { attnum_index(attnum).and_then(|i| scope_names.get(i)).cloned() };

    let mut out: Vec<ConstraintSpec> = Vec::new();
    for (i, def) in defs.iter().enumerate() {
        let col_name = names[i].clone();
        let mut saw_default = false;
        let mut saw_null = false;
        let mut saw_not_null = false;
        let mut saw_identity = false;
        let mut saw_generated = false;
        let clause_starts = column_clause_starts(b, def);
        let mut clause_idx = 0usize;
        for opt in &def.options {
            // Where PG's `Constraint.location` (the DEFAULT / GENERATED keyword) of this clause is.
            let clause_pos = if matches!(opt.option, ColumnOption::Default(_) | ColumnOption::Generated { .. }) {
                let p = clause_starts.get(clause_idx).copied();
                clause_idx += 1;
                p
            } else {
                None
            };
            match &opt.option {
                ColumnOption::Null => {
                    if saw_not_null || columns[i].serial.is_some() || saw_identity {
                        return Err(column_error("conflicting NULL/NOT NULL declarations", &col_name, table));
                    }
                    saw_null = true;
                }
                ColumnOption::NotNull => {
                    if saw_null {
                        return Err(column_error("conflicting NULL/NOT NULL declarations", &col_name, table));
                    }
                    saw_not_null = true;
                    let constraint_name = opt.name.as_ref().map(|n| b.ident(n));
                    columns[i].not_null = Some(constraint_name);
                }
                ColumnOption::Default(expr) => {
                    if saw_default || columns[i].serial.is_some() {
                        return Err(column_error("multiple default values specified", &col_name, table));
                    }
                    saw_default = true;
                    let mut refs: Vec<i16> = Vec::new();
                    let ty = columns[i].type_id.clone();
                    let (bound_default, stored_default) =
                        stored::bind_ddl_expr_with_source(
                            b,
                            expr,
                            ExprCtx::column_default(),
                            Some(ty),
                            None,
                            &mut refs,
                            &names_fn,
                        )?;
                    stored::reject_regrole_constants(&bound_default)?;
                    columns[i].default = Some(stored_default);
                }
                ColumnOption::Collation(name) => {
                    let qn = b.qual(name)?;
                    let ty = columns[i].type_id.clone();
                    let collatable = TypeEnv::type_info(b.env, ty.oid).is_some_and(|info| info.is_collatable());
                    if !collatable {
                        return Err(diag::with_position(
                            PgError::new(
                                SqlState::DATATYPE_MISMATCH,
                                format!("collations are not supported by type {}", b.type_name(ty)),
                            ),
                            super::expr::collate_keyword_before(b.src, name),
                        ));
                    }
                    let oid = b.env.collation_oid(qn.schema.as_deref(), &qn.name)?;
                    columns[i].collation = Some(oid);
                }
                ColumnOption::Generated { generated_as, sequence_options, generation_expr, generation_expr_mode, .. } => {
                    match generation_expr {
                        Some(gen_expr) => {
                            if saw_generated {
                                return Err(column_error("multiple generation clauses specified", &col_name, table));
                            }
                            saw_generated = true;
                            let ty = columns[i].type_id.clone();
                            let is_virtual = !matches!(generation_expr_mode, Some(GeneratedExpressionMode::Stored));
                            if is_virtual {
                                check_virtual_generated_type(&col_name, ty.oid)?;
                            }
                            let mut refs: Vec<i16> = Vec::new();
                            // The table does not exist yet: column references are recorded against the
                            // SELF_REL placeholder, rewritten to the new OID by the catalog (PG records
                            // the generation expression's column dependencies as Normal edges).
                            let (bound, stored_gen) = stored::bind_ddl_expr(
                                b,
                                gen_expr,
                                ExprCtx::generated(),
                                Some(ty),
                                Some(pgarbor_catalog::ddl::spec::SELF_REL),
                                &mut refs,
                                &names_fn,
                            )?;
                            if let Some(&referenced) = refs.iter().find(|a| flag_at(&generated_flags, **a)) {
                                let rname = names_fn(referenced).unwrap_or_default();
                                // Error position is the offending column reference (PG ParseState location).
                                let mut ref_pos: Option<u32> = None;
                                bound.walk(&mut |x| {
                                    if ref_pos.is_none() {
                                        if let crate::ir::ExprKind::Column(c) = &x.kind {
                                            if c.levels_up == 0 && c.attnum == referenced && c.pos > 0 {
                                                ref_pos = Some(c.pos);
                                            }
                                        }
                                    }
                                    ref_pos.is_none()
                                });
                                return Err(diag::with_position(
                                    PgError::new(
                                        SqlState::INVALID_OBJECT_DEFINITION,
                                        format!("cannot use generated column \"{rname}\" in column generation expression"),
                                    )
                                    .with_detail("A generated column cannot reference another generated column."),
                                    ref_pos,
                                ));
                            }
                            if bound.volatility() != Volatility::Immutable {
                                return Err(PgError::new(
                                    SqlState::INVALID_OBJECT_DEFINITION,
                                    "generation expression is not immutable",
                                ));
                            }
                            if is_virtual {
                                let mut e: &Expr = gen_expr;
                                while let Expr::Nested(inner) = e {
                                    e = inner;
                                }
                                check_virtual_generated_security(&bound, b.pos(e))?;
                            }
                            let kind = match generation_expr_mode {
                                Some(GeneratedExpressionMode::Stored) => GeneratedKind::Stored,
                                _ => GeneratedKind::Virtual,
                            };
                            columns[i].generated = Some(GeneratedSpec { kind, expr: stored_gen });
                        }
                        None => {
                            if saw_identity {
                                return Err(column_error("multiple identity specifications", &col_name, table));
                            }
                            saw_identity = true;
                            let oid: Oid = columns[i].type_id.oid;
                            if oid != oids::INT2 && oid != oids::INT4 && oid != oids::INT8 {
                                return Err(PgError::new(
                                    SqlState::INVALID_PARAMETER_VALUE,
                                    "identity column type must be smallint, integer, or bigint",
                                ));
                            }
                            let kind = match generated_as {
                                GeneratedAs::Always => IdentityKind::Always,
                                GeneratedAs::ByDefault => IdentityKind::ByDefault,
                                GeneratedAs::ExpStored => return Err(syntax_near("GENERATED", None)),
                            };
                            let (seq, sequence_name) = identity_option_list(b, sequence_options.as_deref())?;
                            let options = bind_sequence_options(b, &seq, oid, None)?;
                            columns[i].identity = Some(IdentitySpec { kind, options, sequence_name });
                            set_implied_not_null(&mut columns[i]);
                        }
                    }
                }
                ColumnOption::PrimaryKey(_)
                | ColumnOption::Unique(_)
                | ColumnOption::ForeignKey(_)
                | ColumnOption::Check(_) => {
                    if matches!(opt.option, ColumnOption::PrimaryKey(_)) && saw_null {
                        return Err(column_error("conflicting NULL/NOT NULL declarations", &col_name, table));
                    }
                    if let Some(spec) =
                        ddl_constraint::bind_column_constraint(b, &col_name, opt.name.as_ref(), &opt.option, tctx)?
                    {
                        out.push(spec);
                    }
                }
                // COMMENT, ON UPDATE, CHARACTER SET, MATERIALIZED, EPHEMERAL, ALIAS, MySQL/ClickHouse/BigQuery
                // options: PostgreSQL's grammar has no such column clause.
                _ => return Err(syntax_near(&opt.option.to_string(), None)),
            }
            // transformColumnDefinition checks the conflicts after every clause, at that clause.
            if clause_pos.is_some() {
                let conflict = if saw_default && saw_identity {
                    Some("both default and identity specified")
                } else if saw_default && saw_generated {
                    Some("both default and generation expression specified")
                } else if saw_identity && saw_generated {
                    Some("both identity and generation expression specified")
                } else {
                    None
                };
                if let Some(msg) = conflict {
                    return Err(diag::with_position(column_error(msg, &col_name, table), clause_pos));
                }
            }
        }
        let has_default = saw_default || columns[i].serial.is_some();
        if has_default && saw_identity {
            return Err(column_error("both default and identity specified", &col_name, table));
        }
        if has_default && saw_generated {
            return Err(column_error("both default and generation expression specified", &col_name, table));
        }
        if saw_identity && saw_generated {
            return Err(column_error("both identity and generation expression specified", &col_name, table));
        }
        if saw_null && (saw_identity || columns[i].serial.is_some()) {
            return Err(column_error("conflicting NULL/NOT NULL declarations", &col_name, table));
        }
    }
    Ok(out)
}

fn set_implied_not_null(col: &mut ColumnSpec) {
    if col.not_null.is_none() {
        col.not_null = Some(None);
    }
}

// ---------------------------------------------------------------------------------------------
// CREATE TABLE AS / SELECT INTO helpers
// ---------------------------------------------------------------------------------------------

/// Output columns of a query, renamed by the optional column list (42601 when there are more names
/// than columns; 42701 for duplicate output names).
fn ctas_columns(bq: &BoundQuery, aliases: &[String]) -> Result<Vec<ColumnSpec>> {
    if aliases.len() > bq.output.len() {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "too many column names were specified"));
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(bq.output.len());
    let mut columns: Vec<ColumnSpec> = Vec::with_capacity(bq.output.len());
    for (i, out) in bq.output.iter().enumerate() {
        let name: &str = match aliases.get(i) {
            Some(alias) => alias.as_str(),
            None => out.name.as_str(),
        };
        if !seen.insert(name) {
            return Err(PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{name}\" specified more than once")));
        }
        columns.push(ColumnSpec::new(name, out.ty.clone()));
    }
    Ok(columns)
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// `serial`, `bigserial`, `smallserial` and the `serialN` spellings (only an unqualified,
/// unparameterized name, as the grammar would tokenize it).
fn serial_kind(dt: &DataType) -> Option<SerialKind> {
    let DataType::Custom(name, modifiers) = dt else {
        return None;
    };
    if !modifiers.is_empty() {
        return None;
    }
    let [part] = name.0.as_slice() else {
        return None;
    };
    let ObjectNamePart::Identifier(id) = part else {
        return None;
    };
    match fold_ident(id).as_str() {
        "serial" | "serial4" => Some(SerialKind::Int),
        "bigserial" | "serial8" => Some(SerialKind::Big),
        "smallserial" | "serial2" => Some(SerialKind::Small),
        _ => None,
    }
}

fn array_element(dt: &DataType) -> Option<&DataType> {
    let DataType::Array(def) = dt else {
        return None;
    };
    match def {
        ArrayElemTypeDef::SquareBracket(inner, _) | ArrayElemTypeDef::Qualified(inner, _) => Some(inner.as_ref()),
        ArrayElemTypeDef::AngleBracket(inner) | ArrayElemTypeDef::Parenthesis(inner) => Some(inner.as_ref()),
        _ => None,
    }
}

fn is_serial_array(dt: &DataType) -> bool {
    array_element(dt).is_some_and(|inner| serial_kind(inner).is_some())
}

/// 1-based attnum to 0-based index (system columns and 0 have none).
fn attnum_index(attnum: i16) -> Option<usize> {
    attnum.checked_sub(1).and_then(|v| usize::try_from(v).ok())
}

fn flag_at(flags: &[bool], attnum: i16) -> bool {
    attnum_index(attnum).and_then(|i| flags.get(i)).copied().unwrap_or(false)
}

/// Positions of the top-level `DEFAULT` / `GENERATED` keywords of a column definition, in
/// source order (`BY DEFAULT` of an identity clause is not a DEFAULT clause). Scanning starts at the
/// column name and stops at the comma or parenthesis that ends the definition.
fn column_clause_starts(b: &Binder, def: &ColumnDef) -> Vec<u32> {
    let mut out = Vec::new();
    let Some(start) = b.src.byte_offset(def.name.span.start) else { return out };
    let Some(text) = b.src.text().get(start..) else { return out };
    let bytes = text.as_bytes();
    let (mut i, mut depth) = (0usize, 0usize);
    let mut prev = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == c {
                        if bytes.get(i + 1) == Some(&c) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    i += 1;
                }
                i += 1;
                prev.clear();
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'(' => {
                depth += 1;
                i += 1;
                prev.clear();
            }
            b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                i += 1;
                prev.clear();
            }
            b',' if depth == 0 => break,
            _ if c.is_ascii_alphanumeric() || c == b'_' => {
                let w0 = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                    i += 1;
                }
                let word = text[w0..i].to_ascii_uppercase();
                if depth == 0 && ((word == "DEFAULT" && prev != "BY") || word == "GENERATED") {
                    out.push(b.src.pos_of_byte(start + w0));
                }
                prev = word;
            }
            _ => i += 1,
        }
    }
    out
}

/// 42601 for a per-column rule: `<msg> for column "c" of table "t"`.
fn column_error(msg: &str, col: &str, table: &str) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, format!("{msg} for column \"{col}\" of table \"{table}\""))
}

/// `syntax error at or near "<first word of text>"` (the grammar rejects the construct).
fn syntax_near(text: &str, pos: Option<u32>) -> PgError {
    let word = text.split_whitespace().next().unwrap_or("");
    diag::syntax_error_near(word, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::ObjectName;

    fn custom(name: &str) -> DataType {
        DataType::Custom(ObjectName::from(vec![Ident::new(name)]), Vec::new())
    }

    fn kv(key: &str, value: &str) -> SqlOption {
        SqlOption::KeyValue { key: Ident::new(key), value: Expr::Identifier(Ident::new(value)) }
    }

    #[test]
    fn serial_pseudo_types_map_to_kinds() {
        assert_eq!(serial_kind(&custom("serial")), Some(SerialKind::Int));
        assert_eq!(serial_kind(&custom("serial4")), Some(SerialKind::Int));
        assert_eq!(serial_kind(&custom("bigserial")), Some(SerialKind::Big));
        assert_eq!(serial_kind(&custom("serial8")), Some(SerialKind::Big));
        assert_eq!(serial_kind(&custom("smallserial")), Some(SerialKind::Small));
        assert_eq!(serial_kind(&custom("serial2")), Some(SerialKind::Small));
        assert_eq!(serial_kind(&custom("text")), None);
    }

    #[test]
    fn serial_arrays_are_detected() {
        let arr = DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(custom("serial")), None));
        assert!(is_serial_array(&arr));
        assert!(!is_serial_array(&custom("serial")));
    }

    #[test]
    fn fillfactor_option_is_range_checked() {
        assert_eq!(storage_option(&kv("fillfactor", "70")).ok().as_deref(), Some("fillfactor=70"));
        let err = storage_option(&kv("fillfactor", "5")).err();
        assert_eq!(err.as_ref().map(|e| e.sqlstate), Some(SqlState::INVALID_PARAMETER_VALUE));
        assert_eq!(err.map(|e| e.message).as_deref(), Some("value 5 out of bounds for option \"fillfactor\""));
        let err = storage_option(&kv("autovacuum_enabled", "on")).err();
        assert_eq!(err.map(|e| e.message).as_deref(), Some("unrecognized parameter \"autovacuum_enabled\""));
    }

    #[test]
    fn column_errors_use_pg_text() {
        let e = column_error("multiple default values specified", "a", "t");
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "multiple default values specified for column \"a\" of table \"t\"");
    }

    #[test]
    fn attnum_index_maps_one_based_positions() {
        assert_eq!(attnum_index(0), None);
        assert_eq!(attnum_index(-1), None);
        assert_eq!(attnum_index(1), Some(0));
        assert_eq!(attnum_index(3), Some(2));
        assert!(flag_at(&[false, true], 2));
        assert!(!flag_at(&[false, true], 1));
    }

    #[test]
    fn query_without_into_drops_only_the_statement_target() {
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let mut p = sqlparser::parser::Parser::new(&dialect).try_with_sql("SELECT DISTINCT 1 AS z INTO t FROM generate_series(1, 2) n ORDER BY z LIMIT 1").expect("tokenizes");
        let q = p.parse_query().expect("parses");
        let inner = query_without_into(&q);
        assert!(matches!(&*inner.body, SetExpr::Select(sel) if sel.into.is_none() && sel.distinct.is_some()));
        assert!(matches!(&*q.body, SetExpr::Select(sel) if sel.into.is_some()));
        assert_eq!(inner.limit_clause, q.limit_clause);
    }

    #[test]
    fn query_without_into_drops_the_leftmost_set_operation_target() {
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let mut p = sqlparser::parser::Parser::new(&dialect).try_with_sql("SELECT 1 AS x INTO su UNION SELECT 2").expect("tokenizes");
        let q = p.parse_query().expect("parses");
        let inner = query_without_into(&q);
        assert!(select::leftmost_select_into(&inner.body).is_none());
        assert!(select::leftmost_select_into(&q.body).is_some());
        assert!(matches!(&*inner.body, SetExpr::SetOperation { right, .. } if matches!(&**right, SetExpr::Select(sel) if sel.into.is_none())));
    }
}
