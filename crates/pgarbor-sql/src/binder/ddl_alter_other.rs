//! ALTER for everything but table columns/constraints (SQL-34): schemas, functions, types, domains,
//! sequences, roles, databases, materialized views, ownership and schema moves (SQL-34).
//! PG references: `alter.c` (`ExecRenameStmt`, `ExecAlterObjectSchemaStmt`, `ExecAlterOwnerStmt`),
//! `sequence.c` (`AlterSequence`, `init_params`), `typecmds.c` (`AlterEnum`, `AlterDomain*`),
//! `user.c` (`AlterRole`, `AlterRoleSet`), `dbcommands.c`, `functioncmds.c` (`AlterFunction`),
//! `schemacmds.c`.
//!
//! Every function returns `BoundStatement::Alter(Box<AlterAction>)` (one variant per `CatalogTxn`
//! method, see `ir::stmt::AlterAction`) or a `Noop` with the PG notice for `IF EXISTS` on a missing object
//! (`relation "x" does not exist, skipping`, `schema "x" does not exist, skipping`, ...; severity
//! NOTICE, SQLSTATE 00000; use `pgarbor_catalog::errors` constructors when present).
//!
//! | Entry point | Statements |
//! |---|---|
//! | [`bind_alter_schema`] | `ALTER SCHEMA s RENAME TO t` -> `RenameSchema`; `OWNER TO r` -> `SchemaOwner` |
//! | [`bind_alter_function`] | `ALTER FUNCTION f(args) RENAME TO / OWNER TO / SET SCHEMA / attribute changes (IMMUTABLE|STABLE|VOLATILE, [NOT] LEAKPROOF, STRICT|CALLED ON NULL INPUT, SECURITY DEFINER|INVOKER, PARALLEL .., COST n, ROWS n, SET/RESET config)` -> `RenameFunction`/`FunctionOwner`/`SetFunctionSchema`/`FunctionAttrs(FunctionChanges)`; the function is identified by name + argument TYPES (`pgarbor_catalog::ddl::function::format_signature` for messages; 42883 `function f(integer) does not exist`; 42725 `function name "f" is not unique` + HINT `Specify the argument list to select the function unambiguously.` when args omitted) |
//! | [`bind_alter_type`] | `ALTER TYPE t RENAME TO`, `OWNER TO`, `SET SCHEMA`, `ADD VALUE [IF NOT EXISTS] 'v' [BEFORE|AFTER 'w']` -> `AddEnumValue { position: EnumPosition }`, `RENAME VALUE 'a' TO 'b'` -> `RenameEnumValue`; composite `ADD/DROP/ALTER ATTRIBUTE` -> `AlterTable` command list with `keyword = Type` (ddl_alter column machinery) |
//! | [`bind_alter_role`] | `ALTER ROLE|USER r WITH options` -> `Role{changes: RoleChanges}` (SUPERUSER, CREATEDB, CREATEROLE, INHERIT, LOGIN, REPLICATION, BYPASSRLS, CONNECTION LIMIT n, [ENCRYPTED] PASSWORD 'x'|NULL, VALID UNTIL 'ts' parsed with `io::input` as timestamptz -> µs); `RENAME TO` -> `RenameRole`; `SET guc = v|TO DEFAULT|FROM CURRENT`, `RESET guc` -> `RoleConfig`; `IN DATABASE` 0A000 |
//! | extras: [`bind_alter_sequence`] | `ALTER SEQUENCE [IF EXISTS] s <options>` -> `Sequence{options: SequenceOptions, owned_by}` (value range checks as `CREATE SEQUENCE`: 22023 `MINVALUE (5) must be less than MAXVALUE (3)`, `START value (0) cannot be less than MINVALUE (1)`, `RESTART value (0) cannot be less than MINVALUE (1)`, `INCREMENT must not be zero`; `AS type` changes bounds); `RENAME TO`/`OWNER TO`/`SET SCHEMA`/`SET LOGGED|UNLOGGED` -> relation actions with `keyword = Sequence`; `OWNED BY t.c|NONE` (42809 `sequence must be in same schema as table it is linked to`, 55000 `sequence must have same owner as table it is linked to`) |
//! | extras: [`bind_alter_domain`] | `ALTER DOMAIN d SET DEFAULT e|DROP DEFAULT|SET NOT NULL|DROP NOT NULL|ADD CONSTRAINT c CHECK (..) [NOT VALID]|DROP CONSTRAINT [IF EXISTS] c|RENAME CONSTRAINT|VALIDATE CONSTRAINT|RENAME TO|OWNER TO|SET SCHEMA` -> the matching `AlterAction::Domain*` |
//! | extras: [`bind_alter_database`] | `ALTER DATABASE d RENAME TO|OWNER TO|WITH (ALLOW_CONNECTIONS, CONNECTION LIMIT, IS_TEMPLATE)|SET guc|RESET guc` -> `RenameDatabase`/`DatabaseOwner`/`DatabaseOptions`/`DatabaseConfig` |
//! | extras: [`bind_alter_matview`] | `ALTER MATERIALIZED VIEW m RENAME TO|SET SCHEMA|OWNER TO|RENAME COLUMN` |
//!
//! Role references (`OWNER TO r`, `CURRENT_USER`, `SESSION_USER`, `CURRENT_ROLE`) resolve with
//! `env.role_oid(name)` (42704 `role "x" does not exist`); permission checks are the catalog's.
//! Name resolution of the altered object uses `env.lookup_class` / `lookup_type` / schema lookup with
//! the exact PG error texts (`relation "s" does not exist` 42P01, `type "t" does not exist` 42704,
//! `schema "s" does not exist` 3F000, `sequence "s" does not exist`? -> 42P01 `relation`).
use pgarbor_catalog::ddl::function::FunctionChanges;
use pgarbor_catalog::ddl::spec::{DomainConstraintSpec, RoleChanges, SeqBound, SequenceOptions};
use pgarbor_catalog::ddl::typedef::EnumPosition;
use pgarbor_catalog::model::{FuncKind, FuncVolatility, RelKind, TypeDef};
use pgarbor_catalog::snapshot::{ClassRef, UserTypeRef};
use pgarbor_catalog::{errors, CatalogEnv, StoredExpr};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::ProcKind;
use pgarbor_types::{oids, Datum, TypeId};
use sqlparser::ast::{
    AlterFunctionAction, AlterFunctionKind, AlterFunctionOperation, AlterRoleOperation, AlterSchemaOperation,
    AlterTypeAddValuePosition, AlterTypeOperation, AlterTypeRename, AlterUser, Expr, FunctionBehavior,
    FunctionCalledOnNull, FunctionParallel, FunctionSecurity, FunctionSetValue, Ident, ObjectName, ObjectNamePart,
    Owner, ResetConfig, SetConfigValue, Statement, UnaryOperator, Value, ValueWithSpan,
};
use sqlparser::ast::{ArgMode as AstArgMode, SequenceOptions as AstSequenceOption};
use sqlparser::tokenizer::Span;

use super::context::{Binder, ExprCtx};
use super::dispatch::{ALTER_PROCEDURE_KEY, ALTER_ROUTINE_KEY};
use super::ddl_objects::bind_sequence_options;
use super::stored;
use super::types::resolve_data_type;
use crate::diag;
use crate::extra_stmt::{
    AlterDatabaseAction, AlterDomainAction, AlterMatViewAction, AlterSequenceAction, RoleOptionAst, RoleOptionItem,
    SequenceOptionAst,
};
use crate::ir::{AlterAction, AlterRelKeyword, AlterTableCmd, BoundAlterTable, BoundExpr, BoundStatement};
use crate::names::QualName;

/// `Statement::AlterSchema`.
pub fn bind_alter_schema(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let env = b.env;
    let (name, if_exists, operations) = match stmt {
        Statement::AlterSchema(s) => (&s.name, s.if_exists, &s.operations),
        _ => return Err(internal_wrong_stmt("bind_alter_schema")),
    };
    let qn = bind_name(b, name)?;
    if qn.schema.is_some() {
        return Err(diag::syntax_error_near(&qn.display(), qn.pos));
    }
    let schema_name = qn.name.clone();
    if env.lookup_namespace(&schema_name).is_none() {
        if if_exists {
            return Ok(noop("ALTER SCHEMA", errors::schema_does_not_exist_skipping(&schema_name)));
        }
        return Err(errors::schema_does_not_exist(&schema_name));
    }
    let op = match operations.as_slice() {
        [op] => op,
        _ => return Err(unsupported("ALTER SCHEMA with more than one action")),
    };
    match op {
        AlterSchemaOperation::Rename { name: new_name } => {
            let nq = bind_name(b, new_name)?;
            if nq.schema.is_some() {
                return Err(diag::syntax_error_near(&nq.display(), nq.pos));
            }
            if nq.name.starts_with("pg_") {
                return Err(errors::reserved_schema_name(&nq.name));
            }
            Ok(alter(AlterAction::RenameSchema { old: schema_name, new: nq.name }))
        }
        AlterSchemaOperation::OwnerTo { owner } => {
            let new_owner = owner_role(b, owner)?;
            Ok(alter(AlterAction::SchemaOwner { name: schema_name, new_owner }))
        }
        AlterSchemaOperation::SetDefaultCollate { .. } => Err(unsupported("ALTER SCHEMA ... SET DEFAULT COLLATE")),
        AlterSchemaOperation::AddReplica { .. } => Err(unsupported("ALTER SCHEMA ... ADD REPLICA")),
        AlterSchemaOperation::DropReplica { .. } => Err(unsupported("ALTER SCHEMA ... DROP REPLICA")),
        AlterSchemaOperation::SetOptionsParens { .. } => Err(unsupported("ALTER SCHEMA ... SET OPTIONS")),
    }
}

/// `Statement::AlterFunction`.
pub fn bind_alter_function(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let env = b.env;
    let af = match stmt {
        Statement::AlterFunction(af) => af,
        _ => return Err(internal_wrong_stmt("bind_alter_function")),
    };
    if matches!(af.kind, AlterFunctionKind::Aggregate) {
        return Err(unsupported("ALTER AGGREGATE"));
    }
    let qn = bind_name(b, &af.function.name)?;
    let schema = qn.schema_str();

    // Identity: input argument types (OUT arguments are not part of the signature).
    let mut given: Option<Vec<Oid>> = None;
    if let Some(args) = &af.function.args {
        let mut types = Vec::with_capacity(args.len());
        for a in args {
            if matches!(a.mode, Some(AstArgMode::Out)) {
                continue;
            }
            types.push(resolve_data_type(b, &a.data_type, None)?.id.oid);
        }
        given = Some(types);
    }

    // ALTER PROCEDURE (`alter_procedure`) takes the same grammar; PG `LookupFuncWithArgs` with
    // OBJECT_PROCEDURE: an argument list finds any routine and then rejects a non-procedure (42809),
    // while a bare name only considers procedures.
    let is_procedure = b.scratch.contains_key(ALTER_PROCEDURE_KEY);
    // ALTER ROUTINE (`OBJECT_ROUTINE`): any kind; a bare name must still be unique across kinds.
    let is_routine = b.scratch.contains_key(ALTER_ROUTINE_KEY);
    let cands = env.function_candidates(schema, &qn.name)?;
    let func_oid: Oid = match &given {
        Some(types) => match cands.iter().find(|c| c.arg_types == *types) {
            Some(c) => c.oid,
            None => {
                let shown: Vec<String> = types.iter().map(|t| b.type_name(TypeId::of(*t))).collect();
                let sig = format!("{}({})", qn.display(), shown.join(","));
                return Err(if is_procedure {
                    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("procedure {sig} does not exist"))
                } else {
                    errors::function_does_not_exist(&sig)
                });
            }
        },
        None => {
            let named: Vec<_> = cands.iter().filter(|c| !is_procedure || matches!(c.kind, ProcKind::Procedure)).collect();
            match named.len() {
                0 if is_procedure => return Err(errors::no_function_named("procedure", &qn.display())),
                0 => return Err(errors::function_does_not_exist(&qn.display())),
                1 => named[0].oid,
                _ if is_procedure => {
                    return Err(PgError::new(
                        SqlState::AMBIGUOUS_FUNCTION,
                        format!("procedure name \"{}\" is not unique", qn.name),
                    )
                    .with_hint("Specify the argument list to select the procedure unambiguously."))
                }
                _ => return Err(errors::function_name_not_unique(&qn.name)),
            }
        }
    };

    let (kind, retset, in_types) = match env.snapshot().function(func_oid) {
        Some(f) => (f.kind.clone(), f.retset, f.in_arg_types()),
        None => return Err(unsupported("ALTER FUNCTION on a built-in function")),
    };
    let shown: Vec<String> = in_types.iter().map(|t| b.type_name(TypeId::of(*t))).collect();
    let sig = format!("{}({})", qn.display(), shown.join(","));
    let is_proc_kind = matches!(kind, FuncKind::Procedure);
    match kind {
        FuncKind::Procedure if !is_procedure && !is_routine => {
            return Err(errors::wrong_routine_kind(&sig, "function"))
        }
        FuncKind::Procedure => {}
        FuncKind::Function | FuncKind::Window if is_procedure => {
            return Err(errors::wrong_routine_kind(&sig, "procedure"))
        }
        // ALTER ROUTINE may rename, reassign or move an aggregate; its actions are still rejected.
        FuncKind::Aggregate if !is_routine || matches!(af.operation, AlterFunctionOperation::Actions { .. }) => {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is an aggregate function", qn.display()),
            ))
        }
        FuncKind::Aggregate => {}
        FuncKind::Function | FuncKind::Window => {}
    }

    match &af.operation {
        AlterFunctionOperation::RenameTo { new_name } => {
            let nn = b.ident(new_name);
            Ok(alter(AlterAction::RenameFunction { func: func_oid, new_name: nn }))
        }
        AlterFunctionOperation::OwnerTo(owner) => {
            let new_owner = owner_role(b, owner)?;
            Ok(alter(AlterAction::FunctionOwner { func: func_oid, new_owner }))
        }
        AlterFunctionOperation::SetSchema { schema_name } => {
            let s = object_name_text(b, schema_name)?;
            let new_schema = env.namespace_oid(&s)?;
            Ok(alter(AlterAction::SetFunctionSchema { func: func_oid, new_schema }))
        }
        AlterFunctionOperation::DependsOnExtension { .. } => Err(unsupported(if is_procedure {
            "ALTER PROCEDURE ... DEPENDS ON EXTENSION"
        } else if is_routine {
            "ALTER ROUTINE ... DEPENDS ON EXTENSION"
        } else {
            "ALTER FUNCTION ... DEPENDS ON EXTENSION"
        })),
        AlterFunctionOperation::Actions { actions, restrict: _ } => {
            // RESTRICT is accepted by PostgreSQL and has no effect on ALTER FUNCTION.
            let changes = function_changes(b, is_proc_kind, retset, actions)?;
            Ok(alter(AlterAction::FunctionAttrs { func: func_oid, changes }))
        }
    }
}

/// `Statement::AlterType`.
pub fn bind_alter_type(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let env = b.env;
    let at = match stmt {
        Statement::AlterType(at) => at,
        _ => return Err(internal_wrong_stmt("bind_alter_type")),
    };
    let qn = bind_name(b, &at.name)?;
    let ty = env
        .lookup_type(qn.schema_str(), &qn.name)?
        .ok_or_else(|| errors::type_does_not_exist(&qn.display()))?;
    let target = classify_alter_type(env, ty, &qn.display())?;

    match &at.operation {
        AlterTypeOperation::Rename(AlterTypeRename { new_name }) => {
            let nn = b.ident(new_name);
            match target {
                AlterTypeTarget::Composite(rel) => {
                    Ok(alter(AlterAction::RenameRelation { rel, new_name: nn, keyword: AlterRelKeyword::Type }))
                }
                AlterTypeTarget::Enum | AlterTypeTarget::Domain => {
                    Ok(alter(AlterAction::RenameType { ty, new_name: nn }))
                }
            }
        }
        AlterTypeOperation::AddValue(av) => {
            if !matches!(target, AlterTypeTarget::Enum) {
                return Err(errors::not_an_enum(&qn.display()));
            }
            // Enum labels are string literals: they are never case-folded.
            let position = match &av.position {
                None => EnumPosition::Append,
                Some(AlterTypeAddValuePosition::Before(id)) => EnumPosition::Before(id.value.clone()),
                Some(AlterTypeAddValuePosition::After(id)) => EnumPosition::After(id.value.clone()),
            };
            Ok(alter(AlterAction::AddEnumValue {
                ty,
                label: av.value.value.clone(),
                position,
                if_not_exists: av.if_not_exists,
            }))
        }
        AlterTypeOperation::RenameValue(rv) => {
            if !matches!(target, AlterTypeTarget::Enum) {
                return Err(errors::not_an_enum(&qn.display()));
            }
            Ok(alter(AlterAction::RenameEnumValue { ty, old: rv.from.value.clone(), new: rv.to.value.clone() }))
        }
    }
}

/// `Statement::AlterRole` / `AlterUser`.
pub fn bind_alter_role(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let env = b.env;
    match stmt {
        Statement::AlterRole { name, operation } => {
            let role_name = b.ident(name);
            let role = env.role_oid(&role_name)?;
            match operation {
                AlterRoleOperation::RenameRole { role_name: new_id } => {
                    let new_name = b.ident(new_id);
                    rename_role_action(role, new_name)
                }
                AlterRoleOperation::AddMember { .. } | AlterRoleOperation::DropMember { .. } => {
                    Err(unsupported("ALTER ROLE ... ADD MEMBER / DROP MEMBER"))
                }
                // The option-list form is parsed by `parser_extra` (`ExtraStatement::AlterRole`).
                AlterRoleOperation::WithOptions { .. } => Err(unsupported("ALTER ROLE ... WITH options")),
                AlterRoleOperation::Set { config_name, config_value, in_database } => {
                    if in_database.is_some() {
                        return Err(unsupported("ALTER ROLE ... IN DATABASE"));
                    }
                    let guc = object_name_text(b, config_name)?;
                    let value = match config_value {
                        SetConfigValue::Default => None,
                        SetConfigValue::FromCurrent => return Err(unsupported("ALTER ROLE ... SET ... FROM CURRENT")),
                        SetConfigValue::Value(e) => Some(
                            value_text(b, e)
                                .ok_or_else(|| unsupported("ALTER ROLE ... SET with this value expression"))?,
                        ),
                    };
                    Ok(alter(AlterAction::RoleConfig { role, guc, value }))
                }
                AlterRoleOperation::Reset { config_name, in_database } => {
                    if in_database.is_some() {
                        return Err(unsupported("ALTER ROLE ... IN DATABASE"));
                    }
                    match config_name {
                        ResetConfig::ALL => Err(unsupported("ALTER ROLE ... RESET ALL")),
                        ResetConfig::ConfigName(n) => {
                            let guc = object_name_text(b, n)?;
                            Ok(alter(AlterAction::RoleConfig { role, guc, value: None }))
                        }
                    }
                }
            }
        }
        Statement::AlterUser(au) => alter_user(b, au),
        _ => Err(internal_wrong_stmt("bind_alter_role")),
    }
}

/// `ExtraStatement::AlterSequence`.
pub fn bind_alter_sequence(
    b: &mut Binder,
    if_exists: bool,
    name: &ObjectName,
    action: &AlterSequenceAction,
) -> Result<BoundStatement> {
    let env = b.env;
    let qn = bind_name(b, name)?;
    let schema = qn.schema_str();
    let oid = match env.lookup_class(schema, &qn.name)? {
        Some(ClassRef::Sequence(oid)) => oid,
        Some(_) => return Err(errors::not_a(&qn.name, "sequence")),
        None if if_exists => {
            return Ok(noop("ALTER SEQUENCE", errors::kind_does_not_exist_skipping("relation", schema, &qn.name)))
        }
        None => return Err(errors::relation_does_not_exist(schema, &qn.name)),
    };
    let cur = seq_cur(env, oid)?;
    match action {
        AlterSequenceAction::Options(opts) => {
            let (options, owned_by) = alter_sequence_options(b, env, &cur, opts)?;
            Ok(alter(AlterAction::Sequence { sequence: oid, options, owned_by }))
        }
        AlterSequenceAction::SetLogged(logged) => Ok(BoundStatement::AlterTable(Box::new(BoundAlterTable {
            relation: oid,
            keyword: AlterRelKeyword::Sequence,
            commands: vec![AlterTableCmd::SetLogged(*logged)],
        }))),
        AlterSequenceAction::RenameTo(id) => {
            let new_name = b.ident(id);
            Ok(alter(AlterAction::RenameRelation { rel: oid, new_name, keyword: AlterRelKeyword::Sequence }))
        }
        AlterSequenceAction::OwnerTo(id) => {
            let new_owner = ident_role(b, id)?;
            Ok(alter(AlterAction::RelationOwner { rel: oid, new_owner, keyword: AlterRelKeyword::Sequence }))
        }
        AlterSequenceAction::SetSchema(id) => {
            let s = b.ident(id);
            let new_schema = env.namespace_oid(&s)?;
            Ok(alter(AlterAction::SetRelationSchema { rel: oid, new_schema, keyword: AlterRelKeyword::Sequence }))
        }
    }
}

/// `ExtraStatement::AlterDomain`.
pub fn bind_alter_domain(b: &mut Binder, name: &ObjectName, action: &AlterDomainAction) -> Result<BoundStatement> {
    let env = b.env;
    let qn = bind_name(b, name)?;
    let ty = env
        .lookup_type(qn.schema_str(), &qn.name)?
        .ok_or_else(|| errors::type_does_not_exist(&qn.display()))?;
    let base = domain_base(env, ty, &qn.display())?;
    let value_col = [("value".to_string(), base.clone())];

    match action {
        AlterDomainAction::SetDefault(e) => {
            let (_bound, stored_default) =
                bind_domain_expr(b, &qn.name, e, ExprCtx::column_default(), Some(base.clone()), &[])?;
            Ok(alter(AlterAction::DomainDefault { domain: ty, default: Some(stored_default) }))
        }
        AlterDomainAction::DropDefault => Ok(alter(AlterAction::DomainDefault { domain: ty, default: None })),
        AlterDomainAction::SetNotNull => Ok(alter(AlterAction::DomainNotNull { domain: ty, set: true })),
        AlterDomainAction::DropNotNull => Ok(alter(AlterAction::DomainNotNull { domain: ty, set: false })),
        AlterDomainAction::AddCheck { name: cname, expr, not_valid } => {
            let check_name = cname.as_ref().map(|id| b.ident(id));
            let (bound, stored_check) =
                bind_domain_expr(b, &qn.name, expr, ExprCtx::check(), None, &value_col)?;
            if bound.ty.oid != oids::BOOL {
                let actual = b.type_name(bound.ty.clone());
                return Err(diag::not_boolean("CHECK", &actual, None));
            }
            Ok(alter(AlterAction::AddDomainConstraint {
                domain: ty,
                spec: DomainConstraintSpec { name: check_name, expr: stored_check, not_valid: *not_valid },
            }))
        }
        AlterDomainAction::DropConstraint { name: cname, if_exists, cascade } => {
            let n = b.ident(cname);
            Ok(alter(AlterAction::DropDomainConstraint {
                domain: ty,
                name: n,
                missing_ok: *if_exists,
                cascade: *cascade,
            }))
        }
        AlterDomainAction::RenameConstraint { old, new } => {
            let o = b.ident(old);
            let n = b.ident(new);
            Ok(alter(AlterAction::RenameDomainConstraint { domain: ty, old: o, new: n }))
        }
        AlterDomainAction::ValidateConstraint(id) => {
            let n = b.ident(id);
            Ok(alter(AlterAction::ValidateDomainConstraint { domain: ty, name: n }))
        }
        AlterDomainAction::RenameTo(id) => {
            let new_name = b.ident(id);
            Ok(alter(AlterAction::RenameType { ty, new_name }))
        }
        AlterDomainAction::OwnerTo(id) => {
            let new_owner = ident_role(b, id)?;
            Ok(alter(AlterAction::TypeOwner { ty, new_owner }))
        }
        AlterDomainAction::SetSchema(id) => {
            let s = b.ident(id);
            let new_schema = env.namespace_oid(&s)?;
            Ok(alter(AlterAction::SetDomainSchema { ty, new_schema }))
        }
    }
}

/// `ExtraStatement::AlterTypeSetSchema`: `ALTER TYPE name SET SCHEMA new` for enums, domains and
/// composite types (PG `AlterObjectNamespace_internal`). The type is resolved first (42704), then the
/// target schema (3F000), as `ALTER DOMAIN ... SET SCHEMA` does.
pub fn bind_alter_type_set_schema(b: &mut Binder, name: &ObjectName, schema: &Ident) -> Result<BoundStatement> {
    let env = b.env;
    let qn = bind_name(b, name)?;
    let ty = env
        .lookup_type(qn.schema_str(), &qn.name)?
        .ok_or_else(|| errors::type_does_not_exist(&qn.display()))?;
    classify_alter_type(env, ty, &qn.display())?;
    let s = b.ident(schema);
    let new_schema = env.namespace_oid(&s)?;
    Ok(alter(AlterAction::SetTypeSchema { ty, new_schema }))
}

/// `ExtraStatement::AlterDatabase`.
pub fn bind_alter_database(b: &mut Binder, name: &Ident, action: &AlterDatabaseAction) -> Result<BoundStatement> {
    let env = b.env;
    let db = b.ident(name);
    if env.snapshot().database_by_name(&db).is_none() {
        return Err(errors::database_does_not_exist(&db));
    }
    match action {
        AlterDatabaseAction::RenameTo(id) => {
            let new_name = b.ident(id);
            if db == b.ctx.session.database {
                return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "current database cannot be renamed"));
            }
            Ok(alter(AlterAction::RenameDatabase { name: db, new_name }))
        }
        AlterDatabaseAction::OwnerTo(id) => {
            let new_owner = ident_role(b, id)?;
            Ok(alter(AlterAction::DatabaseOwner { name: db, new_owner }))
        }
        AlterDatabaseAction::Options { allow_connections, connection_limit, is_template } => {
            if let Some(n) = connection_limit {
                if *n < -1 {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("invalid connection limit: {n}"),
                    ));
                }
            }
            Ok(alter(AlterAction::DatabaseOptions {
                name: db,
                allow_conn: *allow_connections,
                conn_limit: *connection_limit,
                is_template: *is_template,
            }))
        }
        AlterDatabaseAction::Set { name: guc_raw, value, reset } => {
            let guc = guc_raw.to_ascii_lowercase();
            if *reset {
                return Ok(alter(AlterAction::DatabaseConfig { name: db, guc, value: None }));
            }
            match value {
                None => Err(unsupported("ALTER DATABASE ... SET ... FROM CURRENT")),
                // `TO DEFAULT` removes the setting, as RESET does.
                Some(vals) if vals.len() == 1 && vals[0].eq_ignore_ascii_case("default") => {
                    Ok(alter(AlterAction::DatabaseConfig { name: db, guc, value: None }))
                }
                Some(vals) => Ok(alter(AlterAction::DatabaseConfig { name: db, guc, value: Some(vals.join(", ")) })),
            }
        }
    }
}

/// `ExtraStatement::AlterMaterializedView`.
pub fn bind_alter_matview(
    b: &mut Binder,
    if_exists: bool,
    name: &ObjectName,
    action: &AlterMatViewAction,
) -> Result<BoundStatement> {
    let env = b.env;
    let qn = bind_name(b, name)?;
    let schema = qn.schema_str();
    let rel = match env.lookup_class(schema, &qn.name)? {
        Some(ClassRef::Relation(oid)) => {
            let r = env.snapshot().relation(oid).ok_or_else(|| errors::cache_lookup_failed("relation", oid))?;
            if !matches!(r.kind, RelKind::MatView) {
                return Err(errors::not_a(&qn.name, "materialized view"));
            }
            oid
        }
        Some(_) => return Err(errors::not_a(&qn.name, "materialized view")),
        None if if_exists => {
            return Ok(noop(
                "ALTER MATERIALIZED VIEW",
                errors::kind_does_not_exist_skipping("relation", schema, &qn.name),
            ))
        }
        None => return Err(errors::relation_does_not_exist(schema, &qn.name)),
    };
    let keyword = AlterRelKeyword::MaterializedView;
    match action {
        AlterMatViewAction::RenameTo(id) => {
            let new_name = b.ident(id);
            Ok(alter(AlterAction::RenameRelation { rel, new_name, keyword }))
        }
        AlterMatViewAction::SetSchema(id) => {
            let s = b.ident(id);
            let new_schema = env.namespace_oid(&s)?;
            Ok(alter(AlterAction::SetRelationSchema { rel, new_schema, keyword }))
        }
        AlterMatViewAction::OwnerTo(id) => {
            let new_owner = ident_role(b, id)?;
            Ok(alter(AlterAction::RelationOwner { rel, new_owner, keyword }))
        }
        AlterMatViewAction::RenameColumn { old, new } => {
            let o = b.ident(old);
            let n = b.ident(new);
            Ok(BoundStatement::AlterTable(Box::new(BoundAlterTable {
                relation: rel,
                keyword,
                commands: vec![AlterTableCmd::RenameColumn { old: o, new: n }],
            })))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Kind of object an `ALTER TYPE` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AlterTypeTarget {
    Enum,
    Domain,
    /// Composite type; payload = the pg_class OID of its relation.
    Composite(Oid),
}

/// `PgError` for a statement variant the dispatcher routed here by mistake (binder bug, XX000).
fn internal_wrong_stmt(fname: &str) -> PgError {
    PgError::internal(format!("{fname}: unexpected statement variant"))
}

/// Explicit "not supported by PGArbor yet" (0A000) for syntax this binder does not implement.
fn unsupported(what: &str) -> PgError {
    PgError::unimplemented("SQL-34", what)
}

fn alter(action: AlterAction) -> BoundStatement {
    BoundStatement::Alter(Box::new(action))
}

fn noop(tag: &str, notice: PgError) -> BoundStatement {
    BoundStatement::Noop { tag: tag.to_string(), notices: vec![notice] }
}

/// Resolve an object name, rejecting a catalog part that is not the connected database (0A000).
fn bind_name(b: &mut Binder, name: &ObjectName) -> Result<QualName> {
    let qn = b.qual(name)?;
    if let Some(cat) = &qn.catalog {
        if cat != &b.ctx.session.database {
            return Err(diag::cross_database(&format!("{}.{}", cat, qn.display()), qn.pos));
        }
    }
    Ok(qn)
}

/// Folded text of a dotted object name (`a.b.c`); function-call name parts are not allowed here.
fn object_name_text(b: &mut Binder, name: &ObjectName) -> Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(name.0.len());
    for p in &name.0 {
        match p {
            ObjectNamePart::Identifier(id) => parts.push(b.ident(id)),
            ObjectNamePart::Function(_) => return Err(unsupported("function call in an object name")),
        }
    }
    if parts.is_empty() {
        return Err(PgError::syntax("syntax error at end of input", None));
    }
    Ok(parts.join("."))
}

/// `OWNER TO` target: a role name, or CURRENT_USER / CURRENT_ROLE / SESSION_USER.
fn owner_role(b: &mut Binder, owner: &Owner) -> Result<Oid> {
    match owner {
        Owner::Ident(id) => ident_role(b, id),
        Owner::CurrentRole | Owner::CurrentUser => Ok(b.env.current_user()),
        Owner::SessionUser => Ok(b.env.session_user()),
    }
}

/// `OWNER TO` role identifier: unquoted `CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` are the
/// session's roles (PG `RoleSpec`); anything else is a role name (42704 when it does not exist).
fn ident_role(b: &mut Binder, id: &Ident) -> Result<Oid> {
    if id.quote_style.is_none() {
        match id.value.to_ascii_lowercase().as_str() {
            "current_user" | "current_role" => return Ok(b.env.current_user()),
            "session_user" => return Ok(b.env.session_user()),
            _ => {}
        }
    }
    let n = b.ident(id);
    b.env.role_oid(&n)
}

fn rename_role_action(role: Oid, new_name: String) -> Result<BoundStatement> {
    if new_name.starts_with("pg_") {
        return Err(errors::reserved_role_name(&new_name));
    }
    Ok(alter(AlterAction::RenameRole { role, new_name }))
}

/// PG `errorConflictingDefElem`: a second occurrence of the same option (42601).
fn note_option(seen: &mut Vec<&'static str>, key: &'static str) -> Result<()> {
    if seen.contains(&key) {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options"));
    }
    seen.push(key);
    Ok(())
}

/// Text of a literal option value (string, number, boolean, signed number).
fn literal_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(ValueWithSpan { value, .. }) => match value {
            Value::SingleQuotedString(s) => Some(s.clone()),
            Value::Number(n, _) => Some(n.clone()),
            Value::Boolean(v) => Some(if *v { "true".to_string() } else { "false".to_string() }),
            _ => None,
        },
        _ => number_text(e),
    }
}

/// Text of a numeric literal, optionally with a leading minus sign.
fn number_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(ValueWithSpan { value: Value::Number(n, _), .. }) => Some(n.clone()),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => match expr.as_ref() {
            Expr::Value(ValueWithSpan { value: Value::Number(n, _), .. }) => Some(format!("-{n}")),
            _ => None,
        },
        _ => None,
    }
}

/// Text of a SET-style value: a literal, or a bare identifier (folded like PostgreSQL does).
fn value_text(b: &mut Binder, e: &Expr) -> Option<String> {
    if let Expr::Identifier(id) = e {
        return Some(b.ident(id));
    }
    literal_text(e)
}

fn positive_number(e: &Expr, msg: &str) -> Result<f64> {
    let text = number_text(e).ok_or_else(|| diag::syntax_error_near(&e.to_string(), None))?;
    let v: f64 = text.parse().map_err(|_| diag::syntax_error_near(&text, None))?;
    if v <= 0.0 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg));
    }
    Ok(v)
}

/// PG `AlterRole`: an empty password clears it with a NOTICE; NULL clears it silently.
fn password_value(b: &mut Binder, pw: Option<String>) -> Option<Option<String>> {
    match pw {
        None => Some(None),
        Some(s) if s.is_empty() => {
            b.notice(PgError::notice("empty string is not a valid password, clearing password"));
            Some(None)
        }
        Some(s) => Some(Some(s)),
    }
}

/// `timestamptz` input of a VALID UNTIL string, as microseconds since 2000-01-01 UTC.
fn timestamptz_input(b: &Binder, text: &str) -> Result<i64> {
    let d = pgarbor_types::io::input(&b.io_ctx(), TypeId::of(oids::TIMESTAMPTZ), text)?;
    match d {
        Datum::TimestampTz(us) => Ok(us),
        _ => Err(PgError::internal("timestamptz input produced a non-timestamp value")),
    }
}

/// ALTER ROLE option list -> `RoleChanges` (PG `AlterRole` option handling: a repeated option is
/// 42601 `conflicting or redundant options` at the repeat, `CONNECTION LIMIT` below -1 is 22023).
fn role_changes(b: &mut Binder, options: &[RoleOptionItem]) -> Result<RoleChanges> {
    let mut ch = RoleChanges::default();
    let mut seen: Vec<&'static str> = Vec::new();
    let mut conflict = |key: &'static str, item: &RoleOptionItem| -> Result<()> {
        if seen.contains(&key) {
            return Err(diag::syntax("conflicting or redundant options", item.pos));
        }
        seen.push(key);
        Ok(())
    };
    let mut valid_until: Option<(&str, Option<u32>)> = None;
    for item in options {
        match &item.option {
            RoleOptionAst::Superuser(v) => {
                conflict("superuser", item)?;
                ch.superuser = Some(*v);
            }
            RoleOptionAst::CreateDb(v) => {
                conflict("createdb", item)?;
                ch.create_db = Some(*v);
            }
            RoleOptionAst::CreateRole(v) => {
                conflict("createrole", item)?;
                ch.create_role = Some(*v);
            }
            RoleOptionAst::Inherit(v) => {
                conflict("inherit", item)?;
                ch.inherit = Some(*v);
            }
            RoleOptionAst::Login(v) => {
                conflict("canlogin", item)?;
                ch.can_login = Some(*v);
            }
            RoleOptionAst::Replication(v) => {
                conflict("isreplication", item)?;
                ch.replication = Some(*v);
            }
            RoleOptionAst::BypassRls(v) => {
                conflict("bypassrls", item)?;
                ch.bypass_rls = Some(*v);
            }
            RoleOptionAst::ConnectionLimit(v) => {
                conflict("connectionlimit", item)?;
                ch.conn_limit = Some(*v);
            }
            RoleOptionAst::Password(p) => {
                conflict("password", item)?;
                ch.password = password_value(b, p.clone());
            }
            RoleOptionAst::ValidUntil(t) => {
                conflict("validUntil", item)?;
                valid_until = Some((t.as_str(), item.pos));
            }
            RoleOptionAst::RoleMembers(_) => {
                conflict("rolemembers", item)?;
                return Err(unsupported("ALTER ROLE ... USER member list"));
            }
            // The parser does not produce the CREATE-only options for ALTER ROLE.
            RoleOptionAst::AddRoleTo(_) | RoleOptionAst::AdminMembers(_) | RoleOptionAst::Sysid => {
                return Err(diag::syntax("syntax error", item.pos));
            }
        }
    }
    if let Some(limit) = ch.conn_limit {
        if limit < -1 {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid connection limit: {limit}")));
        }
    }
    if let Some((text, pos)) = valid_until {
        let us = timestamptz_input(b, text).map_err(|e| diag::with_position(e, pos))?;
        ch.valid_until = Some(Some(us));
    }
    Ok(ch)
}

/// `ExtraStatement::AlterRole`: `ALTER ROLE|USER r [WITH] options`. The target role is resolved after
/// the option list is checked (PG `AlterRole`); `CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` are
/// the keyword roles.
pub fn bind_alter_role_options(b: &mut Binder, name: &Ident, options: &[RoleOptionItem]) -> Result<BoundStatement> {
    let changes = role_changes(b, options)?;
    let role = if name.quote_style.is_none() {
        match name.value.to_ascii_lowercase().as_str() {
            "current_user" | "current_role" => b.env.current_user(),
            "session_user" => b.env.session_user(),
            _ => ident_role(b, name)?,
        }
    } else {
        ident_role(b, name)?
    };
    Ok(alter(AlterAction::Role { role, changes }))
}

/// ALTER USER (sqlparser `AlterUser`): the PostgreSQL subset (rename, password) is bound; the
/// Snowflake/MFA/policy/tag clauses are not PostgreSQL and are reported as unsupported.
fn alter_user(b: &mut Binder, au: &AlterUser) -> Result<BoundStatement> {
    let env = b.env;
    let uname = b.ident(&au.name);
    if au.reset_password
        || au.abort_all_queries
        || au.enroll_mfa
        || au.add_role_delegation.is_some()
        || au.remove_role_delegation.is_some()
        || au.set_default_mfa_method.is_some()
        || au.remove_mfa_method.is_some()
        || au.modify_mfa_method.is_some()
        || au.add_mfa_method_otp.is_some()
        || au.set_policy.is_some()
        || au.unset_policy.is_some()
        || !au.set_tag.options.is_empty()
        || !au.unset_tag.is_empty()
        || !au.set_props.options.is_empty()
        || !au.unset_props.is_empty()
    {
        return Err(unsupported("ALTER USER with this clause"));
    }
    if env.snapshot().role_by_name(&uname).is_none() {
        if au.if_exists {
            return Ok(noop("ALTER ROLE", errors::role_does_not_exist_skipping(&uname)));
        }
        return Err(errors::role_does_not_exist(&uname));
    }
    let role = env.role_oid(&uname)?;
    if let Some(new_id) = &au.rename_to {
        if au.password.is_some() {
            return Err(unsupported("ALTER USER ... RENAME TO combined with PASSWORD"));
        }
        let new_name = b.ident(new_id);
        return rename_role_action(role, new_name);
    }
    let mut changes = RoleChanges::default();
    if let Some(p) = &au.password {
        changes.password = password_value(b, p.password.clone());
    }
    Ok(alter(AlterAction::Role { role, changes }))
}

/// Dependency-free output of `bind_domain_expr`: bound + stored forms.
fn bind_domain_expr(
    b: &mut Binder,
    domain: &str,
    e: &Expr,
    ec: ExprCtx,
    target: Option<TypeId>,
    cols: &[(String, TypeId)],
) -> Result<(BoundExpr, StoredExpr)> {
    stored::table_expr_scope(b, domain, cols)?;
    let mut refs: Vec<i16> = Vec::new();
    let res = stored::bind_ddl_expr(b, e, ec, target, None, &mut refs, &|_: i16| Some("value".to_string()));
    b.scopes.pop();
    res
}

/// Base type of a domain (42809 `x is not a domain` otherwise).
fn domain_base(env: &CatalogEnv, ty: Oid, name: &str) -> Result<TypeId> {
    let snap = env.snapshot();
    if let Some(UserTypeRef::Type(t)) = snap.user_type(ty) {
        if let Some(obj) = snap.type_obj(t) {
            if let TypeDef::Domain { base, .. } = &obj.def {
                return Ok(base.clone());
            }
        }
    }
    Err(errors::not_a_domain(name))
}

/// Classify the target of ALTER TYPE (PG `RenameType` / `AlterEnum` checks).
fn classify_alter_type(env: &CatalogEnv, ty: Oid, name: &str) -> Result<AlterTypeTarget> {
    let snap = env.snapshot();
    match snap.user_type(ty) {
        Some(UserTypeRef::Type(t)) => {
            let obj = snap.type_obj(t).ok_or_else(|| errors::cache_lookup_failed("type", t))?;
            match &obj.def {
                TypeDef::Enum { .. } => Ok(AlterTypeTarget::Enum),
                TypeDef::Domain { .. } => Ok(AlterTypeTarget::Domain),
                _ => Err(unsupported(&format!("ALTER TYPE on {name}"))),
            }
        }
        Some(UserTypeRef::RowType(rel)) => {
            let r = snap.relation(rel).ok_or_else(|| errors::cache_lookup_failed("relation", rel))?;
            if matches!(r.kind, RelKind::CompositeType) {
                Ok(AlterTypeTarget::Composite(rel))
            } else {
                Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{name} is a table's row type"))
                    .with_hint("Use ALTER TABLE instead."))
            }
        }
        Some(UserTypeRef::TypeArray(elem)) => Err(array_alter_error(env, name, elem)),
        Some(UserTypeRef::RowTypeArray(rel)) => {
            let r = snap.relation(rel).ok_or_else(|| errors::cache_lookup_failed("relation", rel))?;
            Err(array_alter_error(env, name, r.rowtype))
        }
        None => Err(errors::cannot_rename_system_object(name)),
    }
}

fn array_alter_error(env: &CatalogEnv, array_name: &str, elem: Oid) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot alter array type {array_name}")).with_hint(format!(
        "You can alter type {}, which will alter the array type as well.",
        pgarbor_types::names::display_name(env, elem)
    ))
}

/// PG `errdetail_relkind_not_supported` wording (plural form) for a relation kind.
fn rel_kind_plural(kind: &RelKind) -> &'static str {
    match kind {
        RelKind::Table => "tables",
        RelKind::Index => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::PartitionedTable => "partitioned tables",
        RelKind::PartitionedIndex => "partitioned indexes",
        RelKind::Toast => "TOAST tables",
    }
}

/// `function_changes` for `ALTER FUNCTION ... <actions>` (PG `compute_function_attributes` with
/// `AlterFunction`). `retset` = the function returns a set (ROWS applies only then).
fn function_changes(
    b: &mut Binder,
    is_procedure: bool,
    retset: bool,
    actions: &[AlterFunctionAction],
) -> Result<FunctionChanges> {
    let mut ch = FunctionChanges::default();
    let mut seen: Vec<&'static str> = Vec::new();
    for a in actions {
        // PG `compute_common_attribute` for procedures: only SECURITY, SET and RESET are allowed.
        if is_procedure
            && !matches!(a, AlterFunctionAction::Security { .. } | AlterFunctionAction::Set(_) | AlterFunctionAction::Reset(_))
        {
            return Err(PgError::new(SqlState(*b"42P13"), "invalid attribute in procedure definition"));
        }
        match a {
            AlterFunctionAction::CalledOnNull(c) => {
                note_option(&mut seen, "strict")?;
                ch.strict = Some(matches!(
                    c,
                    FunctionCalledOnNull::Strict | FunctionCalledOnNull::ReturnsNullOnNullInput
                ));
            }
            AlterFunctionAction::Behavior(v) => {
                note_option(&mut seen, "volatility")?;
                ch.volatility = Some(match v {
                    FunctionBehavior::Immutable => FuncVolatility::Immutable,
                    FunctionBehavior::Stable => FuncVolatility::Stable,
                    FunctionBehavior::Volatile => FuncVolatility::Volatile,
                });
            }
            AlterFunctionAction::Leakproof(v) => {
                note_option(&mut seen, "leakproof")?;
                ch.leakproof = Some(*v);
            }
            AlterFunctionAction::Security { security, .. } => {
                note_option(&mut seen, "security")?;
                ch.security_definer = Some(matches!(security, FunctionSecurity::Definer));
            }
            AlterFunctionAction::Parallel(p) => {
                note_option(&mut seen, "parallel")?;
                // pg_proc.proparallel: 'u' unsafe, 'r' restricted, 's' safe.
                ch.parallel = Some(match p {
                    FunctionParallel::Unsafe => b'u',
                    FunctionParallel::Restricted => b'r',
                    FunctionParallel::Safe => b's',
                });
            }
            AlterFunctionAction::Cost(e) => {
                note_option(&mut seen, "cost")?;
                let v = positive_number(e, "COST must be positive")?;
                ch.cost = Some(v as f32);
            }
            AlterFunctionAction::Rows(e) => {
                note_option(&mut seen, "rows")?;
                let v = positive_number(e, "ROWS must be positive")?;
                if !retset {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "ROWS is not applicable when function does not return a set",
                    ));
                }
                ch.rows = Some(v as f32);
            }
            AlterFunctionAction::Support(_) => return Err(unsupported("ALTER FUNCTION ... SUPPORT")),
            AlterFunctionAction::Set(p) => {
                let guc = object_name_text(b, &p.name)?;
                match &p.value {
                    FunctionSetValue::Default => ch.reset_config.push(guc),
                    FunctionSetValue::Values(vals) => {
                        let mut texts: Vec<String> = Vec::with_capacity(vals.len());
                        for v in vals {
                            let t = value_text(b, v)
                                .ok_or_else(|| unsupported("ALTER FUNCTION ... SET with this value expression"))?;
                            texts.push(t);
                        }
                        ch.set_config.push(format!("{guc}={}", texts.join(", ")));
                    }
                    FunctionSetValue::FromCurrent => {
                        return Err(unsupported("ALTER FUNCTION ... SET ... FROM CURRENT"))
                    }
                }
            }
            AlterFunctionAction::Reset(r) => match r {
                ResetConfig::ALL => ch.reset_all = true,
                ResetConfig::ConfigName(n) => {
                    let guc = object_name_text(b, n)?;
                    ch.reset_config.push(guc);
                }
            },
        }
    }
    Ok(ch)
}

// ---- sequences -------------------------------------------------------------------------------

/// Current catalog values of a sequence, copied out of the snapshot.
struct SeqCur {
    oid: Oid,
    name: String,
    owner: Oid,
    namespace: Oid,
    type_oid: Oid,
    increment: i64,
    min: i64,
    max: i64,
    start: i64,
    cache: i64,
    cycle: bool,
    identity: bool,
    owned_table: Option<Oid>,
}

fn seq_cur(env: &CatalogEnv, oid: Oid) -> Result<SeqCur> {
    let s = env.snapshot().sequence(oid).ok_or_else(|| errors::cache_lookup_failed("sequence", oid))?;
    Ok(SeqCur {
        oid,
        name: s.name.clone(),
        owner: s.owner,
        namespace: s.namespace,
        type_oid: s.type_oid,
        increment: s.increment,
        min: s.min,
        max: s.max,
        start: s.start,
        cache: s.cache,
        cycle: s.cycle,
        identity: s.identity,
        owned_table: s.owned_by.map(|(t, _)| t),
    })
}

fn num_expr(text: &str) -> Expr {
    Expr::Value(ValueWithSpan { value: Value::Number(text.to_string(), false), span: Span::empty() })
}

/// PG `init_params` defaults: ascending sequences start at 1 / end at the type maximum, descending
/// ones end at -1 / start at the type minimum.
fn int_type_min(oid: Oid) -> i64 {
    match oid {
        oids::INT2 => -32768,
        oids::INT4 => i32::MIN as i64,
        _ => i64::MIN,
    }
}

fn int_type_max(oid: Oid) -> i64 {
    match oid {
        oids::INT2 => 32767,
        oids::INT4 => i32::MAX as i64,
        _ => i64::MAX,
    }
}

fn default_min(type_oid: Oid, increment: i64) -> i64 {
    if increment > 0 {
        1
    } else {
        int_type_min(type_oid)
    }
}

fn default_max(type_oid: Oid, increment: i64) -> i64 {
    if increment > 0 {
        int_type_max(type_oid)
    } else {
        -1
    }
}

fn parse_bigint(text: &str) -> Result<i64> {
    text.parse::<i64>().map_err(|_| {
        PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{text}\" is out of range for type bigint"))
    })
}

/// Refuse a change of ownership of an identity sequence (PG 55000 / 0A000 `cannot change ownership`).
fn check_identity(env: &CatalogEnv, cur: &SeqCur) -> Result<()> {
    if cur.identity {
        let table = cur
            .owned_table
            .and_then(|t| env.snapshot().relation(t))
            .map(|r| r.name.clone())
            .unwrap_or_default();
        return Err(errors::cannot_alter_identity_sequence("change ownership", &cur.name, &table));
    }
    Ok(())
}

/// `OWNED BY table.column` (PG `process_owned_by`): table kind, owner, schema, column, identity.
fn resolve_owned_by(b: &mut Binder, env: &CatalogEnv, cur: &SeqCur, name: &ObjectName) -> Result<(Oid, i16)> {
    let (col_part, tbl_parts) = match name.0.split_last() {
        Some((ObjectNamePart::Identifier(col), rest)) if !rest.is_empty() => (col, rest),
        _ => {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "invalid OWNED BY option")
                .with_hint("Specify OWNED BY table.column or OWNED BY NONE."))
        }
    };
    let tbl_name = ObjectName(tbl_parts.to_vec());
    let tqn = bind_name(b, &tbl_name)?;
    let col = b.ident(col_part);
    let tschema = tqn.schema_str();
    let (rel_oid, kind) = match env.lookup_class(tschema, &tqn.name)? {
        None => return Err(errors::relation_does_not_exist(tschema, &tqn.name)),
        Some(ClassRef::Relation(o)) => {
            let r = env.snapshot().relation(o).ok_or_else(|| errors::cache_lookup_failed("relation", o))?;
            (o, r.kind.clone())
        }
        Some(ClassRef::Index(o)) | Some(ClassRef::SystemIndex(o)) => (o, RelKind::Index),
        Some(ClassRef::Sequence(o)) => (o, RelKind::Sequence),
    };
    match kind {
        RelKind::Table | RelKind::View | RelKind::ForeignTable | RelKind::PartitionedTable => {}
        other => {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("sequence cannot be owned by relation \"{}\"", tqn.name),
            )
            .with_detail(format!("This operation is not supported for {}.", rel_kind_plural(&other))))
        }
    }
    let rel = env.snapshot().relation(rel_oid).ok_or_else(|| errors::cache_lookup_failed("relation", rel_oid))?;
    if rel.owner != cur.owner {
        return Err(errors::sequence_owner_mismatch());
    }
    if rel.namespace != cur.namespace {
        return Err(errors::sequence_schema_mismatch());
    }
    let attnum = rel
        .columns
        .iter()
        .find(|c| !c.dropped && c.name == col)
        .map(|c| c.attnum)
        .ok_or_else(|| errors::column_of_relation_does_not_exist(&col, &tqn.name))?;
    check_identity(env, cur)?;
    Ok((rel_oid, attnum))
}

/// ALTER SEQUENCE option list (PG `init_params` with `isInit = false`).
///
/// The merged list handed to `bind_sequence_options` carries the CURRENT values for every option
/// the statement leaves out, so the range checks see the effective sequence, as PG does. Only the
/// options the statement actually names are returned as `Some` in the result.
fn alter_sequence_options(
    b: &mut Binder,
    env: &CatalogEnv,
    cur: &SeqCur,
    opts: &[SequenceOptionAst],
) -> Result<(SequenceOptions, Option<Option<(Oid, i16)>>)> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut as_type: Option<TypeId> = None;
    let mut inc: Option<String> = None;
    let mut min_opt: Option<Option<String>> = None;
    let mut max_opt: Option<Option<String>> = None;
    let mut start_opt: Option<String> = None;
    let mut restart_opt: Option<Option<String>> = None;
    let mut cache_opt: Option<String> = None;
    let mut cycle_opt: Option<bool> = None;
    let mut owned_opt: Option<Option<ObjectName>> = None;
    for o in opts {
        match o {
            SequenceOptionAst::As(dt) => {
                note_option(&mut seen, "as")?;
                as_type = Some(resolve_data_type(b, dt, None)?.id);
            }
            SequenceOptionAst::IncrementBy(s) => {
                note_option(&mut seen, "increment")?;
                inc = Some(s.clone());
            }
            SequenceOptionAst::MinValue(v) => {
                note_option(&mut seen, "minvalue")?;
                min_opt = Some(v.clone());
            }
            SequenceOptionAst::MaxValue(v) => {
                note_option(&mut seen, "maxvalue")?;
                max_opt = Some(v.clone());
            }
            SequenceOptionAst::StartWith(s) => {
                note_option(&mut seen, "start")?;
                start_opt = Some(s.clone());
            }
            SequenceOptionAst::Restart(v) => {
                note_option(&mut seen, "restart")?;
                restart_opt = Some(v.clone());
            }
            SequenceOptionAst::Cache(s) => {
                note_option(&mut seen, "cache")?;
                cache_opt = Some(s.clone());
            }
            SequenceOptionAst::Cycle(c) => {
                note_option(&mut seen, "cycle")?;
                cycle_opt = Some(*c);
            }
            SequenceOptionAst::OwnedBy(v) => {
                note_option(&mut seen, "owned_by")?;
                owned_opt = Some(v.clone());
            }
            // Produced only for CREATE SEQUENCE; the ALTER parser reports these as unsupported.
            SequenceOptionAst::Logged(_) | SequenceOptionAst::SequenceName => {
                return Err(unsupported("ALTER SEQUENCE ... LOGGED / UNLOGGED / SEQUENCE NAME"));
            }
        }
    }

    // AS type: only smallint / integer / bigint (PG `init_params`).
    let old_type = cur.type_oid;
    let type_oid = match as_type {
        Some(t) => {
            if !matches!(t.oid, oids::INT2 | oids::INT4 | oids::INT8) {
                return Err(errors::sequence_bad_type());
            }
            t.oid
        }
        None => old_type,
    };
    // A bound equal to the old type's extreme value follows the new type (PG `reset_min/max_value`).
    let (reset_min, reset_max) = if as_type.is_some() {
        (cur.min == int_type_min(old_type), cur.max == int_type_max(old_type))
    } else {
        (false, false)
    };

    let inc_text = inc.clone().unwrap_or_else(|| cur.increment.to_string());
    let start_text = start_opt.clone().unwrap_or_else(|| cur.start.to_string());
    let cache_text = cache_opt.clone().unwrap_or_else(|| cur.cache.to_string());
    let mut merged: Vec<AstSequenceOption> = Vec::with_capacity(6);
    merged.push(AstSequenceOption::IncrementBy(num_expr(&inc_text), true));
    merged.push(AstSequenceOption::MinValue(match &min_opt {
        Some(v) => v.as_deref().map(num_expr),
        None if reset_min => None,
        None => Some(num_expr(&cur.min.to_string())),
    }));
    merged.push(AstSequenceOption::MaxValue(match &max_opt {
        Some(v) => v.as_deref().map(num_expr),
        None if reset_max => None,
        None => Some(num_expr(&cur.max.to_string())),
    }));
    merged.push(AstSequenceOption::StartWith(num_expr(&start_text), true));
    merged.push(AstSequenceOption::Cache(num_expr(&cache_text)));
    merged.push(AstSequenceOption::Cycle(cycle_opt.unwrap_or(cur.cycle)));

    let r = bind_sequence_options(b, &merged, type_oid, None)?;

    // Effective values after the merge (what RESTART is checked against).
    let inc_eff = r.increment.unwrap_or(cur.increment);
    let eff_min = match r.minvalue {
        Some(SeqBound::Value(v)) => v,
        Some(SeqBound::Default) => default_min(type_oid, inc_eff),
        None => cur.min,
    };
    let eff_max = match r.maxvalue {
        Some(SeqBound::Value(v)) => v,
        Some(SeqBound::Default) => default_max(type_oid, inc_eff),
        None => cur.max,
    };
    let eff_start = r.start.unwrap_or(cur.start);

    let restart = match restart_opt {
        None => None,
        Some(None) => Some(None),
        Some(Some(t)) => {
            let v = parse_bigint(&t)?;
            if v < eff_min {
                return Err(errors::sequence_restart_below_min(v, eff_min));
            }
            if v > eff_max {
                return Err(errors::sequence_restart_above_max(v, eff_max));
            }
            Some(Some(v))
        }
    };

    let owned_by = match owned_opt {
        None => None,
        Some(None) => {
            check_identity(env, cur)?;
            Some(None)
        }
        Some(Some(name)) => Some(Some(resolve_owned_by(b, env, cur, &name)?)),
    };

    let minvalue = if min_opt.is_some() || reset_min { Some(SeqBound::Value(eff_min)) } else { None };
    let maxvalue = if max_opt.is_some() || reset_max { Some(SeqBound::Value(eff_max)) } else { None };
    let options = SequenceOptions {
        type_oid: as_type.map(|t| t.oid),
        increment: if inc.is_some() { Some(inc_eff) } else { None },
        minvalue,
        maxvalue,
        start: if start_opt.is_some() { Some(eff_start) } else { None },
        restart,
        cache: if cache_opt.is_some() { r.cache.or(Some(cur.cache)) } else { None },
        cycle: cycle_opt,
    };
    Ok((options, owned_by))
}

// ---- tests -----------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_defaults_follow_init_params() {
        assert_eq!(default_min(oids::INT4, 1), 1);
        assert_eq!(default_min(oids::INT4, -1), i32::MIN as i64);
        assert_eq!(default_max(oids::INT2, 1), 32767);
        assert_eq!(default_max(oids::INT8, -1), -1);
        assert_eq!(default_max(oids::INT8, 1), i64::MAX);
    }

    #[test]
    fn rel_kind_words_match_pg_errdetail() {
        assert_eq!(rel_kind_plural(&RelKind::MatView), "materialized views");
        assert_eq!(rel_kind_plural(&RelKind::Toast), "TOAST tables");
    }

    #[test]
    fn signed_numbers_keep_their_sign() {
        let neg = Expr::UnaryOp { op: UnaryOperator::Minus, expr: Box::new(num_expr("5")) };
        assert_eq!(number_text(&neg).as_deref(), Some("-5"));
        assert_eq!(number_text(&num_expr("9223372036854775807")).as_deref(), Some("9223372036854775807"));
    }

    #[test]
    fn duplicate_options_are_rejected() {
        let mut seen: Vec<&'static str> = Vec::new();
        assert!(note_option(&mut seen, "cache").is_ok());
        assert!(note_option(&mut seen, "cache").is_err());
        assert!(note_option(&mut seen, "cycle").is_ok());
    }

    #[test]
    fn restart_value_out_of_bigint_range_is_rejected() {
        assert_eq!(parse_bigint("-7").ok(), Some(-7));
        assert!(parse_bigint("99999999999999999999").is_err());
    }
}
