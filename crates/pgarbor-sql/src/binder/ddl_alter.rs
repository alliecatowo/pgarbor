//! ALTER TABLE / ALTER VIEW / ALTER INDEX sub-commands (SQL-33). PG references: `tablecmds.c`
//! (`AlterTable`, `ATPrepCmd`, `ATExecCmd` and every `ATExec*`), `parse_utilcmd.c`
//! (`transformAlterTableStmt`, `transformColumnDefinition` reuse), `gram.y` (`alter_table_cmd`).
//!
//! `bind_alter_table(b, at)` (sqlparser `AlterTable { name, if_exists, only, operations, .. }`)
//! produces `BoundStatement::AlterTable(Box<BoundAlterTable { relation, keyword, commands }>)`:
//! * Resolve the relation (`env.lookup_class`): missing + `IF EXISTS` -> `BoundStatement::Noop { tag:
//!   "ALTER TABLE", notices: [relation "x" does not exist, skipping] }`; missing otherwise 42P01.
//!   `ALTER TABLE` on a view/index/sequence: 42809 `"v" is not a table` HINT `Use ALTER VIEW to change a
//!   view.`-style (`ATWrongRelkindError`); `keyword` = the word used.
//! * Sub-commands are bound SEQUENTIALLY against an evolving in-memory copy of the relation
//!   (list of `(name, TypeId, dropped)` columns + constraint names) so that `ADD COLUMN b int, ADD
//!   CHECK (b > 0)` binds the CHECK against the new column and `DROP COLUMN a, ADD COLUMN a text`
//!   works. Positions refer to the original statement text.
//! * `ONLY` accepted (no inheritance). `table_type`, `on_cluster`, `location`: 42601.
//!
//! ## Supported operations -> `AlterTableCmd`
//! | sqlparser | command | notes |
//! |---|---|---|
//! | `AddColumn { if_not_exists, column_def, column_position }` | `AddColumn { spec, if_not_exists }` | same ColumnDef binding as CREATE TABLE (`ddl_table::bind_column_def` equivalent, shared helper inside this module's reads); `ColumnSpec.fast_default`/`default_is_volatile` computed here: evaluate the bound default at bind time when it contains no volatile function (`BoundExpr::volatility() != Volatile`) using `io::output` of the folded constant; `FIRST`/`AFTER` positions are MySQL: 42601 |
//! | `DropColumn { column_names, if_exists, drop_behavior }` | one `DropColumn` per name | `CASCADE`/`RESTRICT` |
//! | `RenameColumn { old_column_name, new_column_name }` | `RenameColumn` | |
//! | `AlterColumn { column_name, op }` | by `AlterColumnOperation`: `SetNotNull`->`SetNotNull`; `DropNotNull`->`DropNotNull`; `SetDefault{value}`->`SetDefault{expr: Some(..)}` (bound with `ExprCtx::column_default()`, assignment-coerced); `DropDefault`->`SetDefault{expr: None}`; `SetDataType{data_type, using, had_set}`->`AlterColumnType` (see below); `AddGenerated{generated_as, sequence_options}`->`AddIdentity`; identity `SET GENERATED ALWAYS|BY DEFAULT`->`SetIdentity`; `DROP IDENTITY [IF EXISTS]`->`DropIdentity`; `SET EXPRESSION AS (e)`->`SetExpression`; `DROP EXPRESSION`->`DropExpression`; `SET STATISTICS n`/`SET STORAGE`/`SET COMPRESSION`/`SET (opts)`->`SetColumnAttribute`; others 0A000 |
//! | `AddConstraint { constraint, not_valid }` | `AddConstraint(ConstraintSpec)` via `ddl_constraint::bind_table_constraint` | `ADD COLUMN`-less `ADD PRIMARY KEY (a)`, `ADD CONSTRAINT .. FOREIGN KEY`, `ADD CHECK`, `ADD UNIQUE`; `NOT VALID` |
//! | `DropConstraint { name, if_exists, drop_behavior }` | `DropConstraint` | |
//! | `RenameConstraint { old_name, new_name }` | `RenameConstraint` | |
//! | `ValidateConstraint { name }` | `ValidateConstraint` | |
//! | `SetLogged` / `SetUnlogged` | `SetLogged(bool)` | |
//! | `OwnerTo { new_owner }` | the separate `BoundStatement::Alter(AlterAction::RelationOwner)`; if mixed with other commands bind each part in order (PG allows `ALTER TABLE t OWNER TO x, ADD COLUMN ..`; emit the owner change as `AlterTableCmd`? NOT representable: 0A000 when combined) | |
//! | `RenameTable { table_name }` | `BoundStatement::Alter(AlterAction::RenameRelation)` (only when it is the single operation) | |
//! | `EnableRowLevelSecurity`/`DisableRowLevelSecurity`/`ForceRowLevelSecurity`/`NoForceRowLevelSecurity` | `RowSecurity{..}` | |
//! | `ReplicaIdentity { .. }` | `ReplicaIdentity(..)` | |
//! | `SetOptionsParens { options }` / `SetTblProperties` | `SetOptions { set, reset }` (`RESET (..)` forms) | |
//! | others (`AttachPartition`, `DetachPartition`, triggers, rules, `ClusterBy`, `SwapWith`, Hive/MySQL/ClickHouse/Snowflake operations, `DropPrimaryKey`, `DropForeignKey`, `DropIndex`, `ChangeColumn`, `ModifyColumn`, `Algorithm`, `Lock`, `AutoIncrement`, `AddPartitions`...) | PostgreSQL-valid ones (`ATTACH PARTITION`, `ENABLE TRIGGER`, `INHERIT`, `SET TABLESPACE`, `CLUSTER ON`): 0A000 naming them; non-PG (MySQL/Hive/...): 42601 | |
//!
//! ## `ALTER COLUMN ... TYPE new_type [COLLATE c] [USING expr]`
//! Resolve the type (`types::resolve_data_type`). Bind `USING expr` (if present) with the table scope;
//! otherwise the implicit conversion is an ASSIGNMENT cast column -> new type (42804 `column "a" cannot be
//! cast automatically to type integer` HINT `You might need to specify "USING a::integer".` when no
//! assignment cast exists). `needs_rewrite`: true unless the old->new conversion is binary-coercible with
//! the same representation (`cast::is_binary_coercible` and equal typmod/array rules; `varchar(10)` ->
//! `varchar(20)`/`text` no rewrite; `int4`->`int8` rewrite since `Datum::Int4` != `Int8`). `new_default`:
//! `Some(Some(expr))` if the column default is re-bound to the new type, `None` to leave untouched (42804
//! `default for column "a" cannot be cast automatically to type X` otherwise).
//! `new_ndims` = declared `[]` count.
//!
//! The command list is `commands`; sub-commands that produce a separate `AlterAction` variant make the
//! whole statement `BoundStatement::Alter` (they cannot be mixed: 0A000 `combining this ALTER TABLE
//! action with others is not supported by PGArbor yet`).
//!
//! ## ALTER VIEW / ALTER INDEX
//! [`bind_alter_view`]: `RENAME TO`, `RENAME COLUMN`, `OWNER TO`, `SET SCHEMA`, `SET (opt=..)`/`RESET`, `ALTER COLUMN .. SET
//! DEFAULT` -> as above with `keyword = View`. [`bind_alter_index`]: `RENAME TO`, `SET (..)`/`RESET (..)`,
//! `SET TABLESPACE` (0A000), `ATTACH PARTITION` (0A000), `ALTER COLUMN SET STATISTICS`.
use pgarbor_catalog::ddl::alter_column::ReplicaIdentitySetting;
use pgarbor_catalog::ddl::spec::{
    ColumnSpec, ConstraintSpec, GeneratedSpec, IdentitySpec, SeqBound,
    SequenceOptions as CatSeqOpts, SerialKind,
};
use pgarbor_catalog::errors;
use pgarbor_catalog::model::{GeneratedKind, IdentityKind, RelKind, StoredExpr};
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::cast::apply_cast;
use pgarbor_types::catalog_data::{builtin_proc, CastContext, ImplModule, Volatility};
use pgarbor_types::context::{FuncCtx, TestSession, TypeEnv};
use pgarbor_types::io::output_opt;
use pgarbor_types::{oids, Datum, TypeId};
use sqlparser::ast::{
    AlterColumnOperation, AlterIndexOperation, AlterTable, AlterTableOperation, ColumnDef,
    ColumnOption, DataType, DropBehavior as AstDropBehavior, Expr, GeneratedAs,
    GeneratedExpressionMode, Ident, MySQLColumnPosition, ObjectName, ObjectNamePart, Owner,
    RenameTableNameKind, ReplicaIdentity, SequenceOptions as AstSeqOpt, SqlOption, Statement,
    UnaryOperator, Value,
};

use crate::diag::{self, ExprClause};
use crate::ir::{
    AlterAction, AlterRelKeyword, AlterTableCmd, BoundAlterTable, BoundExpr, BoundStatement,
    CoercionForm, ExprKind,
};
use crate::names::QualName;

use super::coerce::try_coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::ddl_constraint::{bind_column_constraint, bind_table_constraint, ConstraintTableCtx};
use super::expr::bind_expr;
use super::stored::{bind_ddl_expr, bind_stored_expr, store_expr, table_expr_scope};
use super::types::{is_pseudo_type, resolve_data_type};

// ---------------------------------------------------------------------------------------------
// Evolving relation copy
// ---------------------------------------------------------------------------------------------

/// Placeholder name of a dropped column slot (never a valid identifier, so it cannot be referenced).
fn dropped_placeholder(attnum: i16) -> String {
    format!("........pg.dropped.{attnum}........")
}

/// Columns that exist on every table but cannot be altered or dropped (PG `SystemAttributeDefinition`).
fn is_system_column(name: &str) -> bool {
    matches!(
        name,
        "tableoid" | "xmin" | "cmin" | "xmax" | "cmax" | "ctid"
    )
}

/// One column of the in-memory copy of the relation. Index `i` holds attnum `i + 1`; dropped
/// columns keep their slot so that later attnums stay aligned with the catalog.
struct EvCol {
    name: String,
    type_id: TypeId,
    dropped: bool,
    not_null: bool,
    has_default: bool,
    identity: bool,
}

/// The relation as the sub-commands leave it so far.
struct Evolving {
    oid: Oid,
    rel_name: String,
    cols: Vec<EvCol>,
}

impl Evolving {
    fn load(env: &CatalogEnv, oid: Oid, rel_name: &str) -> Evolving {
        let cols: Vec<EvCol> = match env.snapshot().relation(oid) {
            Some(rel) => rel
                .columns
                .iter()
                .map(|c| {
                    if c.dropped {
                        EvCol {
                            name: dropped_placeholder(c.attnum),
                            type_id: TypeId { oid: 0, typmod: -1 },
                            dropped: true,
                            not_null: false,
                            has_default: false,
                            identity: false,
                        }
                    } else {
                        EvCol {
                            name: c.name.clone(),
                            type_id: c.type_id,
                            dropped: false,
                            not_null: c.not_null,
                            has_default: c.default.is_some(),
                            identity: c.identity.is_some(),
                        }
                    }
                })
                .collect(),
            None => Vec::new(),
        };
        Evolving {
            oid,
            rel_name: rel_name.to_string(),
            cols,
        }
    }

    /// Index of a visible (not dropped) column.
    fn find(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| !c.dropped && c.name == name)
    }

    /// `(name, type)` of every slot in attnum order (dropped slots carry their placeholder).
    fn scope_cols(&self) -> Vec<(String, TypeId)> {
        self.cols
            .iter()
            .map(|c| (c.name.clone(), c.type_id))
            .collect()
    }

    fn constraint_ctx(&self, extra: Option<(String, TypeId)>) -> ConstraintTableCtx {
        let mut columns = self.scope_cols();
        if let Some(e) = extra {
            columns.push(e);
        }
        ConstraintTableCtx {
            table_name: self.rel_name.clone(),
            table_oid: Some(self.oid),
            columns,
        }
    }
}

/// `base` plus one more slot (the column being defined, for constraints that reference it).
fn with_new_column(base: &[(String, TypeId)], name: &str, ty: TypeId) -> Vec<(String, TypeId)> {
    let mut out = base.to_vec();
    out.push((name.to_string(), ty));
    out
}

/// Run `f` with a synthetic table scope over `cols` (named `name`), popping it afterwards.
fn with_table_scope<T>(
    b: &mut Binder,
    name: &str,
    cols: &[(String, TypeId)],
    f: impl FnOnce(&mut Binder) -> Result<T>,
) -> Result<T> {
    table_expr_scope(b, name, cols)?;
    let out = f(b);
    b.scopes.pop();
    out
}

fn no_names(_attnum: i16) -> Option<String> {
    None
}

/// Expression context for `ALTER COLUMN ... TYPE ... USING expr` (PG `EXPR_KIND_ALTER_COL_TRANSFORM`).
fn transform_ctx() -> ExprCtx {
    ExprCtx {
        clause: ExprClause::Other("transform expression"),
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: true,
        columns_ok: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Names and targets
// ---------------------------------------------------------------------------------------------

/// Qualified name with the cross-database check (only the connected database is allowed).
fn qualify(b: &mut Binder, name: &ObjectName) -> Result<QualName> {
    let qn = b.qual(name)?;
    if let Some(cat) = &qn.catalog {
        if *cat != b.ctx.session.database {
            return Err(diag::cross_database(&qn.display(), qn.pos));
        }
    }
    Ok(qn)
}

/// Target of `RENAME TO`: a bare name (a qualified target is a syntax error in PG).
fn rename_target(b: &mut Binder, name: &ObjectName) -> Result<String> {
    let qn = qualify(b, name)?;
    if qn.schema.is_some() {
        return Err(diag::syntax_error_near(".", None));
    }
    Ok(qn.name)
}

/// Owner role of `OWNER TO`.
fn owner_oid(b: &mut Binder, owner: &Owner) -> Result<Oid> {
    match owner {
        Owner::Ident(id) => {
            let name = b.ident(id);
            b.env.role_oid(&name)
        }
        Owner::CurrentRole | Owner::CurrentUser => Ok(b.env.current_user()),
        Owner::SessionUser => Ok(b.env.session_user()),
    }
}

/// Relation or index looked up by name: `(oid, kind)`. `None` when the name does not exist.
fn lookup_target(b: &Binder, qn: &QualName) -> Result<Option<(Oid, RelKind)>> {
    let env: &CatalogEnv = b.env;
    Ok(match env.lookup_class(qn.schema_str(), &qn.name)? {
        None => None,
        Some(ClassRef::Relation(oid)) => match env.snapshot().relation(oid) {
            Some(rel) => Some((oid, rel.kind)),
            None => {
                return Err(PgError::internal(format!(
                    "relation {oid} missing from catalog snapshot"
                )))
            }
        },
        Some(ClassRef::Index(oid)) | Some(ClassRef::SystemIndex(oid)) => {
            Some((oid, RelKind::Index))
        }
        Some(ClassRef::Sequence(oid)) => Some((oid, RelKind::Sequence)),
    })
}

/// 42P01 `relation "x" does not exist` with the statement position.
fn missing_relation(qn: &QualName) -> PgError {
    diag::with_position(
        errors::relation_does_not_exist(qn.schema_str(), &qn.name),
        qn.pos,
    )
}

/// Syntax error at a token (generic message when the token is not known).
fn syntax_at(token: &str) -> PgError {
    if token.is_empty() {
        PgError::new(SqlState::SYNTAX_ERROR, "syntax error")
    } else {
        diag::syntax_error_near(token, None)
    }
}

/// PostgreSQL-valid or foreign syntax that PGArbor does not implement yet.
fn not_implemented(what: &str) -> PgError {
    PgError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("{what} is not supported by PGArbor yet"),
    )
}

/// `quote_ident` for the HINT `USING col::type`.
fn quote_ident(name: &str) -> String {
    let mut chars = name.chars();
    let simple = match chars.next() {
        Some(first) => {
            (first.is_ascii_lowercase() || first == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        None => false,
    };
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

// ---------------------------------------------------------------------------------------------
// Relation kinds and ATSimplePermissions
// ---------------------------------------------------------------------------------------------

/// `kind_plural` of PG `errdetail_relkind_not_supported`.
fn kind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        _ => "tables",
    }
}

/// The action word of PG `alter_table_type_to_string`, used in `ALTER action X cannot be performed`.
fn action_word(op: &AlterTableOperation) -> &'static str {
    match op {
        AlterTableOperation::AddColumn { .. } => "ADD COLUMN",
        AlterTableOperation::DropColumn { .. } => "DROP COLUMN",
        AlterTableOperation::RenameColumn { .. } => "RENAME COLUMN",
        AlterTableOperation::AddConstraint { .. } => "ADD CONSTRAINT",
        AlterTableOperation::DropConstraint { .. } => "DROP CONSTRAINT",
        AlterTableOperation::RenameConstraint { .. } => "RENAME CONSTRAINT",
        AlterTableOperation::ValidateConstraint { .. } => "VALIDATE CONSTRAINT",
        AlterTableOperation::AlterColumn { op, .. } => match op {
            AlterColumnOperation::SetDataType { .. } => "ALTER COLUMN TYPE",
            AlterColumnOperation::SetNotNull => "ALTER COLUMN SET NOT NULL",
            AlterColumnOperation::DropNotNull => "ALTER COLUMN DROP NOT NULL",
            AlterColumnOperation::SetDefault { .. } => "ALTER COLUMN SET DEFAULT",
            AlterColumnOperation::DropDefault => "ALTER COLUMN DROP DEFAULT",
            AlterColumnOperation::AddGenerated { .. } => "ADD GENERATED",
        },
        AlterTableOperation::SetLogged => "SET LOGGED",
        AlterTableOperation::SetUnlogged => "SET UNLOGGED",
        AlterTableOperation::ReplicaIdentity { .. } => "REPLICA IDENTITY",
        AlterTableOperation::EnableRowLevelSecurity => "ENABLE ROW LEVEL SECURITY",
        AlterTableOperation::DisableRowLevelSecurity => "DISABLE ROW LEVEL SECURITY",
        AlterTableOperation::ForceRowLevelSecurity => "FORCE ROW LEVEL SECURITY",
        AlterTableOperation::NoForceRowLevelSecurity => "NO FORCE ROW LEVEL SECURITY",
        AlterTableOperation::SetOptionsParens { .. } => "SET",
        _ => "ALTER TABLE",
    }
}

/// `ATSimplePermissions`: the action word when `kind` does not support `op`, `None` when it does.
/// Renames and owner changes are allowed on every non-composite relation.
fn disallowed_action(kind: RelKind, op: &AlterTableOperation) -> Option<&'static str> {
    match kind {
        RelKind::Table | RelKind::PartitionedTable | RelKind::Toast => None,
        RelKind::View | RelKind::MatView => match op {
            AlterTableOperation::RenameColumn { .. }
            | AlterTableOperation::RenameTable { .. }
            | AlterTableOperation::OwnerTo { .. } => None,
            _ => Some(action_word(op)),
        },
        _ => match op {
            AlterTableOperation::RenameTable { .. } | AlterTableOperation::OwnerTo { .. } => None,
            _ => Some(action_word(op)),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Literal helpers: sequence options, reloptions, constant folding
// ---------------------------------------------------------------------------------------------

/// PG `NumericOnly`: an integer literal, optionally signed.
fn int_literal(e: &Expr) -> Result<i64> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(text, _) => text.to_string().parse::<i64>().map_err(|_| {
                PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
            }),
            _ => Err(syntax_at("")),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => {
            let v = int_literal(expr)?;
            v.checked_neg().ok_or_else(|| {
                PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
            })
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => int_literal(expr),
        _ => Err(syntax_at("")),
    }
}

/// Sequence options written on an identity column (`GENERATED ... AS IDENTITY (START WITH 10 ...)`).
fn seq_options(opts: &[AstSeqOpt]) -> Result<CatSeqOpts> {
    let mut out = CatSeqOpts::default();
    for o in opts {
        match o {
            AstSeqOpt::IncrementBy(e, _) => out.increment = Some(int_literal(e)?),
            AstSeqOpt::MinValue(None) => out.minvalue = Some(SeqBound::Default),
            AstSeqOpt::MinValue(Some(e)) => out.minvalue = Some(SeqBound::Value(int_literal(e)?)),
            AstSeqOpt::MaxValue(None) => out.maxvalue = Some(SeqBound::Default),
            AstSeqOpt::MaxValue(Some(e)) => out.maxvalue = Some(SeqBound::Value(int_literal(e)?)),
            AstSeqOpt::StartWith(e, _) => out.start = Some(int_literal(e)?),
            AstSeqOpt::Cache(e) => out.cache = Some(int_literal(e)?),
            AstSeqOpt::Cycle(flag) => out.cycle = Some(*flag),
        }
    }
    Ok(out)
}

/// Text of a reloption value (`fillfactor = 70`, `autovacuum = on`).
fn option_value_text(e: &Expr) -> Result<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(text, _) => Ok(text.to_string()),
            Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => Ok(s.clone()),
            Value::Boolean(flag) => Ok(if *flag {
                "true".to_string()
            } else {
                "false".to_string()
            }),
            _ => Err(syntax_at("")),
        },
        Expr::Identifier(id) => Ok(id.value.clone()),
        _ => Err(syntax_at("")),
    }
}

/// `SET (name = value, ...)` as `name=value` strings (the catalog's reloptions form).
fn reloption_strings(b: &mut Binder, opts: &[SqlOption]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(opts.len());
    for o in opts {
        match o {
            SqlOption::KeyValue { key, value } => {
                let k = b.ident(key);
                let v = option_value_text(value)?;
                out.push(format!("{k}={v}"));
            }
            SqlOption::Ident(id) => {
                let k = b.ident(id);
                out.push(k);
            }
            _ => return Err(syntax_at("")),
        }
    }
    Ok(out)
}

fn is_builtin_callable(oid: Oid) -> bool {
    match builtin_proc(oid) {
        Some(p) => !matches!(p.module, ImplModule::External),
        None => false,
    }
}

fn fold_ctx<'x>(
    b: &'x Binder,
    ts: &'x TestSession,
    arg_types: &'x [TypeId],
    result: TypeId,
) -> FuncCtx<'x> {
    FuncCtx {
        env: b.env as &dyn TypeEnv,
        io: &b.ctx.io_config,
        session: ts,
        arg_types,
        result_type: result,
        collation: 0,
    }
}

fn fold_call(
    b: &Binder,
    ts: &TestSession,
    proc_oid: Oid,
    args: &[BoundExpr],
    result: TypeId,
) -> Result<Option<Datum>> {
    let mut vals: Vec<Datum> = Vec::with_capacity(args.len());
    let mut tys: Vec<TypeId> = Vec::with_capacity(args.len());
    for a in args {
        match fold_const(b, ts, a)? {
            Some(v) => vals.push(v),
            None => return Ok(None),
        }
        tys.push(a.ty);
    }
    let ctx = fold_ctx(b, ts, &tys, result);
    Ok(Some(pgarbor_types::func::call(proc_oid, &vals, &ctx)?))
}

/// Evaluates a bound expression that needs no row, when it is built from constants, immutable
/// built-in functions and casts. `None` when it cannot be folded here (the default is then
/// evaluated per row by the executor).
fn fold_const(b: &Binder, ts: &TestSession, e: &BoundExpr) -> Result<Option<Datum>> {
    match &e.kind {
        ExprKind::Const(d) => Ok(Some(d.clone())),
        ExprKind::Cast(c) => {
            let v = match fold_const(b, ts, &c.arg)? {
                Some(v) => v,
                None => return Ok(None),
            };
            let no_types: [TypeId; 0] = [];
            let ctx = fold_ctx(b, ts, &no_types, e.ty);
            Ok(Some(apply_cast(&c.plan, v, &ctx)?))
        }
        ExprKind::FuncCall(f) => {
            if !matches!(f.volatility, Volatility::Immutable)
                || f.retset
                || !is_builtin_callable(f.func)
            {
                return Ok(None);
            }
            fold_call(b, ts, f.func, &f.args, e.ty)
        }
        ExprKind::Op(o) => {
            if !matches!(o.op.volatility, Volatility::Immutable) || !is_builtin_callable(o.op.proc)
            {
                return Ok(None);
            }
            fold_call(b, ts, o.op.proc, &o.args, e.ty)
        }
        _ => Ok(None),
    }
}

/// `(fast_default, default_is_volatile)` for an ADD COLUMN default (PG stores a non-volatile
/// constant as the column's missing value; anything else is rewritten per row).
fn fast_default_of(b: &Binder, e: &BoundExpr) -> Result<(Option<String>, bool)> {
    if matches!(e.volatility(), Volatility::Volatile) {
        return Ok((None, true));
    }
    let ts = TestSession {
        now: b.ctx.statement_timestamp,
        ..TestSession::default()
    };
    match fold_const(b, &ts, e)? {
        None => Ok((None, true)),
        Some(Datum::Null) => Ok((None, false)),
        Some(d) => {
            let ctx = b.io_ctx();
            let text = output_opt(&ctx, e.ty, &d)?;
            Ok((text, false))
        }
    }
}

/// `serial`, `serial2/4/8`, `smallserial`, `bigserial` (arrive as custom type names).
fn serial_kind(dt: &DataType) -> Option<SerialKind> {
    if let DataType::Custom(name, mods) = dt {
        if !mods.is_empty() || name.0.len() != 1 {
            return None;
        }
        if let Some(ObjectNamePart::Identifier(id)) = name.0.first() {
            return match id.value.to_ascii_lowercase().as_str() {
                "serial" | "serial4" => Some(SerialKind::Int),
                "bigserial" | "serial8" => Some(SerialKind::Big),
                "smallserial" | "serial2" => Some(SerialKind::Small),
                _ => None,
            };
        }
    }
    None
}

/// Keyword that starts a column option, for the syntax error of a non-PG column option.
fn option_keyword(option: &ColumnOption) -> &'static str {
    match option {
        ColumnOption::Comment(_) => "COMMENT",
        ColumnOption::OnUpdate(_) => "ON",
        ColumnOption::CharacterSet(_) => "CHARACTER",
        ColumnOption::Materialized(_) => "MATERIALIZED",
        ColumnOption::Alias(_) => "ALIAS",
        ColumnOption::Invisible => "INVISIBLE",
        _ => "",
    }
}

/// Keyword of a MySQL/Hive/ClickHouse/Snowflake-only ALTER TABLE operation (for the 42601).
fn foreign_keyword(op: &AlterTableOperation) -> &'static str {
    match op {
        AlterTableOperation::ChangeColumn { .. } => "CHANGE",
        AlterTableOperation::ModifyColumn { .. } => "MODIFY",
        AlterTableOperation::ClusterBy { .. } => "CLUSTER",
        AlterTableOperation::Algorithm { .. } => "ALGORITHM",
        AlterTableOperation::Lock { .. } => "LOCK",
        AlterTableOperation::AutoIncrement { .. } => "AUTO_INCREMENT",
        AlterTableOperation::DropIndex { .. } => "INDEX",
        AlterTableOperation::DropPrimaryKey { .. } => "PRIMARY",
        AlterTableOperation::DropForeignKey { .. } => "FOREIGN",
        AlterTableOperation::SwapWith { .. } => "SWAP",
        _ => "",
    }
}

// ---------------------------------------------------------------------------------------------
// Column definitions (ADD COLUMN): the CREATE TABLE column algorithm, copied (ddl_table is another
// lease).
// ---------------------------------------------------------------------------------------------

struct BoundColDef {
    spec: ColumnSpec,
    /// Column-level constraints (PRIMARY KEY, UNIQUE, REFERENCES, CHECK); emitted after the column.
    constraints: Vec<ConstraintSpec>,
}

fn bind_column_def(b: &mut Binder, cd: &ColumnDef, ev: &Evolving) -> Result<BoundColDef> {
    let name = b.ident(&cd.name);
    let table = ev.rel_name.clone();
    let base_cols = ev.scope_cols();
    let (type_id, ndims, serial): (TypeId, i32, Option<SerialKind>) =
        match serial_kind(&cd.data_type) {
            Some(kind) => (
                TypeId {
                    oid: kind.type_oid(),
                    typmod: -1,
                },
                0,
                Some(kind),
            ),
            None => {
                let rt = resolve_data_type(b, &cd.data_type, None)?;
                if is_pseudo_type(rt.id.oid) {
                    return Err(PgError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        format!("column \"{name}\" has pseudo-type {}", b.type_name(rt.id)),
                    ));
                }
                (rt.id, rt.ndims, None)
            }
        };

    let mut spec = ColumnSpec::new(name.clone(), type_id);
    spec.ndims = ndims;
    spec.serial = serial;
    let mut constraints: Vec<ConstraintSpec> = Vec::new();
    let mut has_default = false;
    let mut is_identity = false;
    let mut is_generated = false;

    for od in &cd.options {
        let option = &od.option;
        match option {
            ColumnOption::NotNull => {
                let cname = match &od.name {
                    Some(id) => Some(b.ident(id)),
                    None => None,
                };
                spec.not_null = Some(cname);
            }
            ColumnOption::Null => {
                if spec.not_null.is_some() {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("conflicting NULL/NOT NULL declarations for column \"{name}\" of table \"{table}\""),
                    ));
                }
            }
            ColumnOption::Default(expr) => {
                if is_identity {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("both default and identity specified for column \"{name}\" of table \"{table}\""),
                    ));
                }
                if is_generated {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!(
                            "both default and generation expression specified for column \"{name}\" of table \"{table}\""
                        ),
                    ));
                }
                if has_default {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("multiple default values specified for column \"{name}\" of table \"{table}\""),
                    ));
                }
                let ty = type_id;
                let oid = ev.oid;
                let (bound, stored) = with_table_scope(b, &table, &base_cols, |b| {
                    bind_ddl_expr(
                        b,
                        expr,
                        ExprCtx::column_default(),
                        Some(ty),
                        Some(oid),
                        &mut Vec::new(),
                        &no_names,
                    )
                })?;
                let (fast, volatile) = fast_default_of(b, &bound)?;
                spec.default = Some(stored);
                spec.fast_default = fast;
                spec.default_is_volatile = volatile;
                has_default = true;
            }
            ColumnOption::Collation(cname) => {
                let qn = qualify(b, cname)?;
                let coll = b.env.collation_oid(qn.schema_str(), &qn.name)?;
                let info = pgarbor_types::registry::type_info(b.env as &dyn TypeEnv, type_id.oid)?;
                if info.collation == 0 {
                    return Err(PgError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "collations are not supported by type {}",
                            b.type_name(type_id)
                        ),
                    ));
                }
                spec.collation = Some(coll);
            }
            ColumnOption::Generated {
                generated_as,
                sequence_options,
                generation_expr,
                generation_expr_mode,
                ..
            } => match generation_expr {
                Some(gexpr) => {
                    if has_default {
                        return Err(PgError::new(
                                SqlState::SYNTAX_ERROR,
                                format!(
                                    "both default and generation expression specified for column \"{name}\" of table \"{table}\""
                                ),
                            ));
                    }
                    let oid = ev.oid;
                    let ty = type_id;
                    let (bound, stored) = with_table_scope(b, &table, &base_cols, |b| {
                        bind_ddl_expr(
                            b,
                            gexpr,
                            ExprCtx::generated(),
                            Some(ty),
                            Some(oid),
                            &mut Vec::new(),
                            &no_names,
                        )
                    })?;
                    if !matches!(bound.volatility(), Volatility::Immutable) {
                        return Err(PgError::new(
                            SqlState::INVALID_OBJECT_DEFINITION,
                            "generation expression is not immutable",
                        ));
                    }
                    let kind = match generation_expr_mode {
                        Some(GeneratedExpressionMode::Stored) => GeneratedKind::Stored,
                        _ => GeneratedKind::Virtual,
                    };
                    spec.generated = Some(GeneratedSpec { kind, expr: stored });
                    is_generated = true;
                }
                None => {
                    let kind = match generated_as {
                        GeneratedAs::Always => IdentityKind::Always,
                        GeneratedAs::ByDefault => IdentityKind::ByDefault,
                        GeneratedAs::ExpStored => return Err(syntax_at("STORED")),
                    };
                    if has_default {
                        return Err(PgError::new(
                                SqlState::SYNTAX_ERROR,
                                format!("both default and identity specified for column \"{name}\" of table \"{table}\""),
                            ));
                    }
                    if !matches!(type_id.oid, oids::INT2 | oids::INT4 | oids::INT8) {
                        return Err(PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            "identity column type must be smallint, integer, or bigint",
                        ));
                    }
                    let options = match sequence_options {
                        Some(v) => seq_options(v)?,
                        None => CatSeqOpts::default(),
                    };
                    spec.identity = Some(IdentitySpec { kind, options });
                    is_identity = true;
                }
            },
            ColumnOption::PrimaryKey(_)
            | ColumnOption::Unique(_)
            | ColumnOption::ForeignKey(_)
            | ColumnOption::Check(_) => {
                let tctx = ConstraintTableCtx {
                    table_name: table.clone(),
                    table_oid: Some(ev.oid),
                    columns: with_new_column(&base_cols, name.as_str(), type_id),
                };
                if let Some(c) =
                    bind_column_constraint(b, name.as_str(), od.name.as_ref(), option, &tctx)?
                {
                    constraints.push(c);
                }
            }
            other => return Err(syntax_at(option_keyword(other))),
        }
    }

    if serial.is_some() {
        spec.default_is_volatile = true;
        if spec.not_null.is_none() {
            spec.not_null = Some(None);
        }
    }
    Ok(BoundColDef { spec, constraints })
}

// ---------------------------------------------------------------------------------------------
// Sub-command binders
// ---------------------------------------------------------------------------------------------

fn cmd_add_column(
    b: &mut Binder,
    ev: &mut Evolving,
    cd: &ColumnDef,
    if_not_exists: bool,
    position: Option<&MySQLColumnPosition>,
    out: &mut Vec<AlterTableCmd>,
) -> Result<()> {
    if let Some(p) = position {
        let kw = match p {
            MySQLColumnPosition::First => "FIRST",
            MySQLColumnPosition::After(_) => "AFTER",
        };
        return Err(syntax_at(kw));
    }
    let BoundColDef { spec, constraints } = bind_column_def(b, cd, ev)?;
    let name = spec.name.clone();
    let exists = ev.find(&name).is_some();
    if exists && !if_not_exists {
        return Err(errors::column_already_exists(&name, &ev.rel_name));
    }
    let entry = EvCol {
        name: name.clone(),
        type_id: spec.type_id,
        dropped: false,
        not_null: spec.not_null.is_some(),
        has_default: spec.default.is_some() || spec.serial.is_some(),
        identity: spec.identity.is_some(),
    };
    // IF NOT EXISTS on an existing column: the catalog raises the NOTICE and nothing else changes.
    out.push(AlterTableCmd::AddColumn {
        spec,
        if_not_exists,
    });
    if exists {
        return Ok(());
    }
    ev.cols.push(entry);
    for c in constraints {
        out.push(AlterTableCmd::AddConstraint(c));
    }
    Ok(())
}

fn cmd_drop_columns(
    b: &mut Binder,
    ev: &mut Evolving,
    names: &[Ident],
    cascade: bool,
    if_exists: bool,
    out: &mut Vec<AlterTableCmd>,
) -> Result<()> {
    for id in names {
        let name = b.ident(id);
        match ev.find(&name) {
            Some(idx) => {
                ev.cols[idx] = EvCol {
                    name: dropped_placeholder((idx + 1) as i16),
                    type_id: TypeId { oid: 0, typmod: -1 },
                    dropped: true,
                    not_null: false,
                    has_default: false,
                    identity: false,
                };
                out.push(AlterTableCmd::DropColumn {
                    name,
                    cascade,
                    missing_ok: if_exists,
                });
            }
            None if is_system_column(&name) || if_exists => {
                // System columns: the catalog raises 0A000; missing with IF EXISTS: NOTICE there.
                out.push(AlterTableCmd::DropColumn {
                    name,
                    cascade,
                    missing_ok: if_exists,
                });
            }
            None => {
                return Err(errors::column_of_relation_does_not_exist(
                    &name,
                    &ev.rel_name,
                ))
            }
        }
    }
    Ok(())
}

fn cmd_rename_column(
    b: &mut Binder,
    ev: &mut Evolving,
    old_id: &Ident,
    new_id: &Ident,
    out: &mut Vec<AlterTableCmd>,
) -> Result<()> {
    let old = b.ident(old_id);
    let new = b.ident(new_id);
    if is_system_column(&old) {
        // The catalog raises 0A000 `cannot rename system column`.
        out.push(AlterTableCmd::RenameColumn { old, new });
        return Ok(());
    }
    let idx = match ev.find(&old) {
        Some(i) => i,
        None => return Err(errors::column_does_not_exist(&old)),
    };
    if ev.find(&new).is_some() {
        return Err(errors::column_already_exists(&new, &ev.rel_name));
    }
    ev.cols[idx].name = new.clone();
    out.push(AlterTableCmd::RenameColumn { old, new });
    Ok(())
}

/// `ALTER COLUMN x TYPE t [USING e]` (module docs). Returns the new column type.
fn cmd_alter_column_type(
    b: &mut Binder,
    ev: &Evolving,
    idx: usize,
    col: &str,
    data_type: &DataType,
    using: Option<&Expr>,
    out: &mut Vec<AlterTableCmd>,
) -> Result<TypeId> {
    let old_ty = ev.cols[idx].type_id;
    let attnum = (idx + 1) as i16;
    let rt = resolve_data_type(b, data_type, None)?;
    if is_pseudo_type(rt.id.oid) {
        return Err(PgError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("column \"{col}\" has pseudo-type {}", b.type_name(rt.id)),
        ));
    }
    let new_ty = rt.id;
    let scope = ev.scope_cols();

    // The USING expression (bound against the table) or the implicit conversion of the old value.
    let (using_bound, plain): (Option<BoundExpr>, bool) = match using {
        None => (None, true),
        Some(e) => {
            let bound = with_table_scope(b, &ev.rel_name, &scope, |b| {
                bind_expr(b, e, transform_ctx())
            })?;
            let plain = matches!(&bound.kind, ExprKind::Column(c) if c.levels_up == 0 && c.rt_index == 1 && c.attnum == attnum);
            (Some(bound), plain)
        }
    };
    let had_using = using.is_some();
    let arg_expr = match using_bound {
        Some(x) => x,
        None => BoundExpr::column(0, 1, attnum, old_ty),
    };
    let arg = BoundArg::new(arg_expr, None);
    let coerced = match try_coerce(
        b,
        &arg,
        new_ty,
        CastContext::Assignment,
        CoercionForm::Implicit,
    )? {
        Some(e) => e,
        None => {
            let tn = b.type_name(new_ty);
            return Err(if had_using {
                PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("result of USING clause for column \"{col}\" cannot be cast automatically to type {tn}"),
                )
                .with_hint("You might need to add an explicit cast.")
            } else {
                PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("column \"{col}\" cannot be cast automatically to type {tn}"),
                )
                .with_hint(format!(
                    "You might need to specify \"USING {}::{}\".",
                    quote_ident(col),
                    b.type_name_typmod(new_ty)
                ))
            });
        }
    };

    let binary = if old_ty.oid == new_ty.oid {
        true
    } else {
        pgarbor_types::cast::lookup::is_binary_coercible(
            b.env as &dyn TypeEnv,
            old_ty.oid,
            new_ty.oid,
        )?
    };
    let needs_rewrite = rewrite_needed(old_ty, new_ty, binary, plain);

    // The column default is re-bound to the new type (PG `RememberAllDependentForRebuilding`).
    let new_default: Option<Option<StoredExpr>> = if ev.cols[idx].has_default && old_ty != new_ty {
        let stored_sql = match b
            .env
            .snapshot()
            .relation(ev.oid)
            .and_then(|r| r.columns.get(idx))
            .and_then(|c| c.default.as_ref())
        {
            Some(d) => Some(d.expr.sql.clone()),
            None => None,
        };
        match stored_sql {
            None => None,
            Some(sql) => {
                let bound_default = bind_stored_expr(b, &sql, ev.oid, ExprCtx::column_default())?;
                let darg = BoundArg::new(bound_default, None);
                match try_coerce(
                    b,
                    &darg,
                    new_ty,
                    CastContext::Assignment,
                    CoercionForm::Implicit,
                )? {
                    Some(e) => {
                        let stored = store_expr(b, &e, Some(ev.oid), &no_names)?;
                        Some(Some(stored))
                    }
                    None => {
                        return Err(PgError::new(
                            SqlState::DATATYPE_MISMATCH,
                            format!(
                                "default for column \"{col}\" cannot be cast automatically to type {}",
                                b.type_name(new_ty)
                            ),
                        ));
                    }
                }
            }
        }
    } else {
        None
    };

    out.push(AlterTableCmd::AlterColumnType {
        column: col.to_string(),
        new_type: new_ty,
        new_collation: None,
        new_ndims: rt.ndims,
        using: Some(coerced),
        new_default,
        needs_rewrite,
    });
    Ok(new_ty)
}

/// Whether the ALTER TYPE rewrites the table (PG `ATColumnChangeRequiresRewrite`, simplified).
/// `plain`: no USING, or USING is the bare column itself.
fn rewrite_needed(old: TypeId, new: TypeId, binary: bool, plain: bool) -> bool {
    if !plain {
        return true;
    }
    if old.oid == new.oid {
        let typmod_ok = new.typmod == -1 || (old.typmod != -1 && new.typmod >= old.typmod);
        return !typmod_ok;
    }
    if new.typmod != -1 {
        return true;
    }
    !binary
}

fn cmd_alter_column(
    b: &mut Binder,
    ev: &mut Evolving,
    column: &Ident,
    op: &AlterColumnOperation,
    out: &mut Vec<AlterTableCmd>,
) -> Result<()> {
    let col = b.ident(column);
    let idx = match ev.find(&col) {
        Some(i) => i,
        None if is_system_column(&col) => {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot alter system column \"{col}\""),
            ));
        }
        None => {
            return Err(errors::column_of_relation_does_not_exist(
                &col,
                &ev.rel_name,
            ))
        }
    };
    match op {
        AlterColumnOperation::SetNotNull => {
            out.push(AlterTableCmd::SetNotNull { column: col });
            ev.cols[idx].not_null = true;
        }
        AlterColumnOperation::DropNotNull => {
            out.push(AlterTableCmd::DropNotNull { column: col });
            ev.cols[idx].not_null = false;
        }
        AlterColumnOperation::SetDefault { value } => {
            let ty = ev.cols[idx].type_id;
            let oid = ev.oid;
            let scope = ev.scope_cols();
            let (_bound, stored) = with_table_scope(b, &ev.rel_name, &scope, |b| {
                bind_ddl_expr(
                    b,
                    value,
                    ExprCtx::column_default(),
                    Some(ty),
                    Some(oid),
                    &mut Vec::new(),
                    &no_names,
                )
            })?;
            out.push(AlterTableCmd::SetDefault {
                column: col,
                expr: Some(stored),
            });
            ev.cols[idx].has_default = true;
        }
        AlterColumnOperation::DropDefault => {
            out.push(AlterTableCmd::SetDefault {
                column: col,
                expr: None,
            });
            ev.cols[idx].has_default = false;
        }
        AlterColumnOperation::SetDataType {
            data_type, using, ..
        } => {
            let new_ty = cmd_alter_column_type(b, ev, idx, &col, data_type, using.as_ref(), out)?;
            ev.cols[idx].type_id = new_ty;
        }
        AlterColumnOperation::AddGenerated {
            generated_as,
            sequence_options,
        } => {
            let kind = match generated_as {
                Some(GeneratedAs::Always) => IdentityKind::Always,
                Some(GeneratedAs::ByDefault) => IdentityKind::ByDefault,
                _ => return Err(syntax_at("AS")),
            };
            let options = match sequence_options {
                Some(v) => seq_options(v)?,
                None => CatSeqOpts::default(),
            };
            out.push(AlterTableCmd::AddIdentity {
                column: col,
                identity: IdentitySpec { kind, options },
            });
            ev.cols[idx].identity = true;
        }
    }
    Ok(())
}

fn replica_setting(
    b: &mut Binder,
    ev: &Evolving,
    identity: &ReplicaIdentity,
) -> Result<ReplicaIdentitySetting> {
    Ok(match identity {
        ReplicaIdentity::Nothing => ReplicaIdentitySetting::Nothing,
        ReplicaIdentity::Full => ReplicaIdentitySetting::Full,
        ReplicaIdentity::Default => ReplicaIdentitySetting::Default,
        ReplicaIdentity::Index(id) => {
            let idx_name = b.ident(id);
            match b.env.lookup_class(None, &idx_name)? {
                Some(ClassRef::Index(oid)) | Some(ClassRef::SystemIndex(oid)) => {
                    ReplicaIdentitySetting::UsingIndex(oid)
                }
                _ => {
                    return Err(PgError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "index \"{idx_name}\" for table \"{}\" does not exist",
                            ev.rel_name
                        ),
                    ));
                }
            }
        }
    })
}

/// One sub-command other than RENAME TO / OWNER TO (those are whole-statement actions).
fn bind_op(
    b: &mut Binder,
    ev: &mut Evolving,
    op: &AlterTableOperation,
    out: &mut Vec<AlterTableCmd>,
) -> Result<()> {
    match op {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => cmd_add_column(
            b,
            ev,
            column_def,
            *if_not_exists,
            column_position.as_ref(),
            out,
        ),
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            let cascade = matches!(drop_behavior, Some(AstDropBehavior::Cascade));
            cmd_drop_columns(b, ev, column_names, cascade, *if_exists, out)
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => cmd_rename_column(b, ev, old_column_name, new_column_name, out),
        AlterTableOperation::AlterColumn { column_name, op } => {
            cmd_alter_column(b, ev, column_name, op, out)
        }
        AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } => {
            let tctx = ev.constraint_ctx(None);
            let mut spec = bind_table_constraint(b, constraint, &tctx)?;
            if *not_valid {
                spec.not_valid = true;
            }
            out.push(AlterTableCmd::AddConstraint(spec));
            Ok(())
        }
        AlterTableOperation::DropConstraint {
            if_exists,
            name,
            drop_behavior,
        } => {
            let cname = b.ident(name);
            let cascade = matches!(drop_behavior, Some(AstDropBehavior::Cascade));
            out.push(AlterTableCmd::DropConstraint {
                name: cname,
                cascade,
                missing_ok: *if_exists,
            });
            Ok(())
        }
        AlterTableOperation::RenameConstraint { old_name, new_name } => {
            let old = b.ident(old_name);
            let new = b.ident(new_name);
            out.push(AlterTableCmd::RenameConstraint { old, new });
            Ok(())
        }
        AlterTableOperation::ValidateConstraint { name } => {
            let cname = b.ident(name);
            out.push(AlterTableCmd::ValidateConstraint { name: cname });
            Ok(())
        }
        AlterTableOperation::SetOptionsParens { options } => {
            let set = reloption_strings(b, options)?;
            out.push(AlterTableCmd::SetOptions {
                set,
                reset: Vec::new(),
            });
            Ok(())
        }
        AlterTableOperation::SetTblProperties { table_properties } => {
            let set = reloption_strings(b, table_properties)?;
            out.push(AlterTableCmd::SetOptions {
                set,
                reset: Vec::new(),
            });
            Ok(())
        }
        AlterTableOperation::SetLogged => {
            out.push(AlterTableCmd::SetLogged(true));
            Ok(())
        }
        AlterTableOperation::SetUnlogged => {
            out.push(AlterTableCmd::SetLogged(false));
            Ok(())
        }
        AlterTableOperation::ReplicaIdentity { identity } => {
            let setting = replica_setting(b, ev, identity)?;
            out.push(AlterTableCmd::ReplicaIdentity(setting));
            Ok(())
        }
        AlterTableOperation::EnableRowLevelSecurity => {
            out.push(AlterTableCmd::RowSecurity {
                enable: Some(true),
                force: None,
            });
            Ok(())
        }
        AlterTableOperation::DisableRowLevelSecurity => {
            out.push(AlterTableCmd::RowSecurity {
                enable: Some(false),
                force: None,
            });
            Ok(())
        }
        AlterTableOperation::ForceRowLevelSecurity => {
            out.push(AlterTableCmd::RowSecurity {
                enable: None,
                force: Some(true),
            });
            Ok(())
        }
        AlterTableOperation::NoForceRowLevelSecurity => {
            out.push(AlterTableCmd::RowSecurity {
                enable: None,
                force: Some(false),
            });
            Ok(())
        }
        // PostgreSQL-valid, not implemented: 0A000 naming the operation.
        AlterTableOperation::AttachPartition { .. } => Err(not_implemented("ATTACH PARTITION")),
        AlterTableOperation::DetachPartition { .. } => Err(not_implemented("DETACH PARTITION")),
        AlterTableOperation::EnableTrigger { .. } => Err(not_implemented("ENABLE TRIGGER")),
        AlterTableOperation::DisableTrigger { .. } => Err(not_implemented("DISABLE TRIGGER")),
        AlterTableOperation::EnableAlwaysTrigger { .. } => {
            Err(not_implemented("ENABLE ALWAYS TRIGGER"))
        }
        AlterTableOperation::EnableReplicaTrigger { .. } => {
            Err(not_implemented("ENABLE REPLICA TRIGGER"))
        }
        AlterTableOperation::EnableRule { .. } => Err(not_implemented("ENABLE RULE")),
        AlterTableOperation::DisableRule { .. } => Err(not_implemented("DISABLE RULE")),
        AlterTableOperation::EnableAlwaysRule { .. } => Err(not_implemented("ENABLE ALWAYS RULE")),
        AlterTableOperation::EnableReplicaRule { .. } => {
            Err(not_implemented("ENABLE REPLICA RULE"))
        }
        AlterTableOperation::RenameTable { .. } | AlterTableOperation::OwnerTo { .. } => Err(
            PgError::internal("ddl_alter: whole-statement action reached the sub-command binder"),
        ),
        // MySQL / Hive / ClickHouse / Snowflake operations: not PostgreSQL syntax.
        _ => Err(syntax_at(foreign_keyword(op))),
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

/// ALTER TABLE.
pub fn bind_alter_table(b: &mut Binder, at: &AlterTable) -> Result<BoundStatement> {
    if at.location.is_some() {
        return Err(syntax_at("LOCATION"));
    }
    if at.on_cluster.is_some() {
        return Err(syntax_at("ON"));
    }
    if at.table_type.is_some() {
        return Err(syntax_at(""));
    }
    let qn = qualify(b, &at.name)?;
    let (oid, kind) = match lookup_target(b, &qn)? {
        Some(found) => found,
        None => {
            if at.if_exists {
                return Ok(BoundStatement::Noop {
                    tag: "ALTER TABLE".to_string(),
                    notices: vec![errors::kind_does_not_exist_skipping(
                        "relation",
                        qn.schema_str(),
                        &qn.name,
                    )],
                });
            }
            return Err(missing_relation(&qn));
        }
    };
    if kind == RelKind::CompositeType {
        return Err(PgError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{}\" is a composite type", qn.name),
        )
        .with_hint("Use ALTER TYPE instead."));
    }

    let standalone = at.operations.iter().any(|op| {
        matches!(
            op,
            AlterTableOperation::RenameTable { .. } | AlterTableOperation::OwnerTo { .. }
        )
    });
    if standalone && at.operations.len() > 1 {
        return Err(not_implemented(
            "combining this ALTER TABLE action with others",
        ));
    }

    let mut ev = Evolving::load(b.env, oid, &qn.name);
    let mut cmds: Vec<AlterTableCmd> = Vec::new();
    for op in &at.operations {
        match op {
            AlterTableOperation::RenameTable { table_name } => {
                let new_name = match table_name {
                    RenameTableNameKind::To(n) => rename_target(b, n)?,
                    RenameTableNameKind::As(_) => return Err(syntax_at("AS")),
                };
                return Ok(BoundStatement::Alter(Box::new(
                    AlterAction::RenameRelation {
                        rel: oid,
                        new_name,
                        keyword: AlterRelKeyword::Table,
                    },
                )));
            }
            AlterTableOperation::OwnerTo { new_owner } => {
                let new_owner = owner_oid(b, new_owner)?;
                return Ok(BoundStatement::Alter(Box::new(
                    AlterAction::RelationOwner {
                        rel: oid,
                        new_owner,
                        keyword: AlterRelKeyword::Table,
                    },
                )));
            }
            other => {
                if let Some(action) = disallowed_action(kind, other) {
                    return Err(errors::alter_action_not_supported(
                        action,
                        &qn.name,
                        kind_plural(kind),
                    ));
                }
                bind_op(b, &mut ev, other, &mut cmds)?;
            }
        }
    }
    Ok(BoundStatement::AlterTable(Box::new(BoundAlterTable {
        relation: oid,
        keyword: AlterRelKeyword::Table,
        commands: cmds,
    })))
}

/// Position of the `AS` keyword that follows the view name in `ALTER VIEW v [(cols)] AS query`.
fn as_keyword_pos(b: &Binder, name: &ObjectName) -> Option<u32> {
    let first = match name.0.first() {
        Some(ObjectNamePart::Identifier(id)) => id,
        _ => return None,
    };
    let start = b.pos_span(first.span)?;
    let text = b.src.text();
    let chars: Vec<char> = text.chars().collect();
    let is_sep = |c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == ';';
    let mut i = (start as usize).checked_sub(1)?;
    // Skip the name token itself.
    while i < chars.len() && !is_sep(chars[i]) {
        i += 1;
    }
    while i < chars.len() {
        if is_sep(chars[i]) {
            i += 1;
            continue;
        }
        let word_start = i;
        while i < chars.len() && !is_sep(chars[i]) {
            i += 1;
        }
        let word: String = chars[word_start..i].iter().collect();
        if word.eq_ignore_ascii_case("AS") {
            return Some((word_start + 1) as u32);
        }
    }
    None
}

/// `Statement::AlterView`. sqlparser 0.63 only represents `ALTER VIEW v [(cols)] [WITH ..] AS query`,
/// which is not PostgreSQL grammar: PG reports the `AS` keyword as a syntax error. The ALTER VIEW
/// RENAME / OWNER / SET forms do not reach this function (see the contract break notes).
pub fn bind_alter_view(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::AlterView { name, .. } => {
            let pos = as_keyword_pos(b, name);
            Err(diag::syntax_error_near("AS", pos))
        }
        _ => Err(PgError::internal(
            "ddl_alter::bind_alter_view called with a non-ALTER VIEW statement",
        )),
    }
}

/// `Statement::AlterIndex`: `ALTER INDEX i RENAME TO j` (the only form sqlparser represents).
pub fn bind_alter_index(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::AlterIndex { name, operation } => match operation {
            AlterIndexOperation::RenameIndex { index_name } => {
                let qn = qualify(b, name)?;
                let oid = match lookup_target(b, &qn)? {
                    Some((oid, RelKind::Index)) | Some((oid, RelKind::PartitionedIndex)) => oid,
                    Some(_) => return Err(errors::not_a(&qn.name, "index")),
                    None => return Err(missing_relation(&qn)),
                };
                let new_name = rename_target(b, index_name)?;
                Ok(BoundStatement::Alter(Box::new(
                    AlterAction::RenameRelation {
                        rel: oid,
                        new_name,
                        keyword: AlterRelKeyword::Index,
                    },
                )))
            }
        },
        _ => Err(PgError::internal(
            "ddl_alter::bind_alter_index called with a non-ALTER INDEX statement",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_keeps_simple_names_and_quotes_the_rest() {
        assert_eq!(quote_ident("a"), "a");
        assert_eq!(quote_ident("col_2"), "col_2");
        assert_eq!(quote_ident("Mixed"), "\"Mixed\"");
        assert_eq!(quote_ident("a b"), "\"a b\"");
        assert_eq!(quote_ident("q\"x"), "\"q\"\"x\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn rewrite_rules_follow_pg_examples() {
        let varchar10 = TypeId {
            oid: 1043,
            typmod: 14,
        };
        let varchar20 = TypeId {
            oid: 1043,
            typmod: 24,
        };
        let text = TypeId {
            oid: 25,
            typmod: -1,
        };
        let int4 = TypeId {
            oid: 23,
            typmod: -1,
        };
        let int8 = TypeId {
            oid: 20,
            typmod: -1,
        };
        // varchar(10) -> varchar(20): binary-compatible, length grows: no rewrite.
        assert!(!rewrite_needed(varchar10, varchar20, true, true));
        // varchar(20) -> varchar(10): length shrinks: rewrite.
        assert!(rewrite_needed(varchar20, varchar10, true, true));
        // varchar(10) -> text: binary-compatible, unlimited target: no rewrite.
        assert!(!rewrite_needed(varchar10, text, true, true));
        // integer -> bigint: representation changes: rewrite.
        assert!(rewrite_needed(int4, int8, false, true));
        // A USING expression other than the bare column always rewrites.
        assert!(rewrite_needed(int4, int4, true, false));
    }

    #[test]
    fn serial_pseudo_names_map_to_kinds() {
        let big = DataType::Custom(ObjectName::from(vec![Ident::new("bigserial")]), vec![]);
        assert_eq!(serial_kind(&big), Some(SerialKind::Big));
        let small = DataType::Custom(ObjectName::from(vec![Ident::new("SERIAL2")]), vec![]);
        assert_eq!(serial_kind(&small), Some(SerialKind::Small));
        let other = DataType::Custom(ObjectName::from(vec![Ident::new("mood")]), vec![]);
        assert_eq!(serial_kind(&other), None);
    }

    #[test]
    fn relkind_permissions_follow_atsimplepermissions() {
        let logged = AlterTableOperation::SetLogged;
        assert_eq!(disallowed_action(RelKind::Table, &logged), None);
        assert_eq!(
            disallowed_action(RelKind::Sequence, &logged),
            Some("SET LOGGED")
        );
        assert_eq!(
            disallowed_action(RelKind::View, &logged),
            Some("SET LOGGED")
        );
        let rename_col = AlterTableOperation::RenameColumn {
            old_column_name: Ident::new("a"),
            new_column_name: Ident::new("b"),
        };
        assert_eq!(disallowed_action(RelKind::View, &rename_col), None);
        assert_eq!(
            disallowed_action(RelKind::Index, &rename_col),
            Some("RENAME COLUMN")
        );
    }

    #[test]
    fn dropped_placeholders_are_not_identifiers() {
        let p = dropped_placeholder(3);
        assert!(p.starts_with('.'));
        assert!(!is_system_column(&p));
        assert!(is_system_column("ctid"));
        assert!(!is_system_column("ctid2"));
    }
}
