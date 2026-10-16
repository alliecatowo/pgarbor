//! DROP, TRUNCATE, COMMENT ON, DROP OWNED, REASSIGN OWNED, DROP DATABASE (SQL-38).
//! PG references: `dropcmds.c` (`RemoveObjects`, `owningrel_does_not_exist_skipping`,
//! `does_not_exist_skipping` — the NOTICE texts), `tablecmds.c` (`ExecuteTruncate`, `truncate_check_rel`),
//! `comment.c` (`CommentObject`), `user.c` (`DropOwnedObjects`, `ReassignOwnedObjects`),
//! `dbcommands.c` (`dropdb`), `gram.y` (`DropStmt`).
//!
//! **DROP carries NAMES, not OIDs** (`BoundDrop { kind, targets: Vec<DropTarget>, if_exists, cascade }`):
//! the catalog's `CatalogTxn::drop_by_names` resolves them because NOTICE/ERROR wording depends on the
//! lookup outcome. The binder only converts syntax:
//! * `Statement::Drop { object_type, if_exists, names, cascade, restrict, purge, temporary, table }`:
//!   `ObjectType::Table|View|MaterializedView|Index|Sequence|Type|Schema|Role|User|Database|...` ->
//!   `DropKind` (`User` -> `Role`; `Database` -> `BoundStatement::DropDatabase`; `Collation|Stage|Stream|
//!   Warehouse|...` -> 42601/0A000 as appropriate; `PURGE`, `TEMPORARY` 42601). `CASCADE` + `RESTRICT`
//!   together: 42601. Targets: `DropTarget { schema, name, args: None }` via `Binder::qual`
//!   (3-part names with the wrong database 0A000 `cross-database references are not implemented`).
//!   `DROP INDEX CONCURRENTLY` flag ignored. `DROP TABLE a, b` -> two targets.
//! * `DropFunction { if_exists, func_desc: Vec<FunctionDesc>, drop_behavior }`, `DropProcedure`,
//!   `DropDomain { if_exists, name, drop_behavior }`: functions/procedures carry `args: Some(Vec<Oid>)`
//!   resolved from the written argument types (`types::resolve_data_type`; arg modes OUT/INOUT handled;
//!   omitted arg list -> `args: None` = "name must be unique"); domains `DropKind::Domain`; `DROP
//!   ROUTINE`/`AGGREGATE` similar.
//! * `DROP SCHEMA s CASCADE` etc. are plain targets. `DROP OWNED BY r` -> `DropOwned`;
//!   `REASSIGN OWNED BY a TO b` -> `ReassignOwned` (roles resolved with `env.role_oid`, 42704 `role "x"
//!   does not exist`).
//! * `DROP DATABASE [IF EXISTS] n [WITH (FORCE)]` -> `DropDatabase { name, if_exists, force }` (running
//!   inside a transaction block is the session's 25001).
//!
//! ## TRUNCATE (`Statement::Truncate(Truncate { table_names, partitions, table, identity, cascade, on_cluster
//! })`) -> `BoundTruncate { tables, restart_identity, cascade }`
//! `table_names: Vec<TruncateTableTarget { name, only }>`; resolve each with `env.resolve_relation`
//! (42P01); must be tables: 42809 `"v" is not a table`; `RESTART IDENTITY`/`CONTINUE IDENTITY`;
//! `CASCADE`/`RESTRICT`; partitions/`ON CLUSTER`: 42601. Duplicates removed (PG truncates each once).
//!
//! ## COMMENT (`Statement::Comment { object_type, object_name, comment, if_exists }`)
//! -> `BoundStatement::Comment { addr: ObjectAddress, text: Option<String> }` (`IS NULL` removes).
//! Supported object types: table, view, materialized view, index, sequence, column (`COMMENT ON COLUMN
//! t.c`: addr = `ObjectAddress{class: Relation, oid, sub_id: attnum}`), schema, type, domain, function,
//! constraint (`ON constraint ON table`: sqlparser exposes `CommentObject::Constraint`? else 0A000),
//! role, database, extension (0A000). Missing object: 42P01/42704/3F000 with the normal error; IF EXISTS
//! on missing -> `Noop` with NOTICE `relation "x" does not exist, skipping`.
use pgarbor_catalog::ddl::drop_stmt::{DropKind, DropTarget};
use pgarbor_catalog::model::{ObjClass, ObjectAddress, RelKind};
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::{builtin, errors, CatalogEnv};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::catalog_data::ProcKind;
use pgarbor_types::names::format_type;
use sqlparser::ast::{
    ArgMode, CascadeOption, CommentObject, DropBehavior, FunctionDesc, Ident, ObjectName, ObjectNamePart, ObjectType,
    OperateFunctionArg, Statement, TruncateIdentityOption,
};

use super::context::Binder;
use super::dispatch::{DROP_DOMAIN_KEY, DROP_ROUTINE_KEY};
use super::types::resolve_data_type;
use crate::diag;
use crate::extra_stmt::{ExtraStatement, RoutineKind};
use crate::ir::{BoundDrop, BoundStatement, BoundTruncate};
use crate::names::QualName;

/// Attach a 1-based error position (no-op for `None`).
fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    e.position = pos;
    e
}

/// NOTICE 00000 with the given text.
fn notice(msg: String) -> PgError {
    PgError::new(SqlState::SUCCESSFUL_COMPLETION, msg).with_severity(Severity::Notice)
}

/// `schema.name` when qualified, else `name`.
fn qualified_text(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// `IS TRUE` for `CASCADE` (sqlparser's `DROP ... CASCADE|RESTRICT` option).
fn is_cascade(behavior: &Option<DropBehavior>) -> bool {
    matches!(behavior, Some(DropBehavior::Cascade))
}

/// Resolve a possibly qualified name. A 3-part name must name the connected database (PG
/// `cross-database references are not implemented`, 0A000).
fn name_of(b: &mut Binder, name: &ObjectName) -> Result<QualName> {
    let q = b.qual(name)?;
    if let Some(cat) = &q.catalog {
        if *cat != b.ctx.session.database {
            return Err(diag::cross_database(&format!("{cat}.{}", q.display()), q.pos));
        }
    }
    Ok(q)
}

/// A name PG's grammar takes unqualified (roles, databases, schemas in DROP/COMMENT): a qualified
/// name is a syntax error at its dot.
fn plain_name(b: &mut Binder, name: &ObjectName) -> Result<QualName> {
    let q = name_of(b, name)?;
    if q.schema.is_some() {
        return Err(diag::syntax_error_near(".", None));
    }
    Ok(q)
}

/// The DROP target of a schema-qualifiable name without arguments (tables, types, domains, ...).
fn plain_target(b: &mut Binder, name: &ObjectName) -> Result<DropTarget> {
    let q = name_of(b, name)?;
    Ok(DropTarget { schema: q.schema, name: q.name, args: None, all_args_lookup: false })
}

/// `ObjectType` -> `DropKind` for the object types that go through `drop_by_names`.
fn drop_kind_of(object_type: &ObjectType) -> Result<DropKind> {
    Ok(match object_type {
        ObjectType::Table => DropKind::Table,
        ObjectType::View => DropKind::View,
        ObjectType::MaterializedView => DropKind::MaterializedView,
        ObjectType::Index => DropKind::Index,
        ObjectType::Sequence => DropKind::Sequence,
        ObjectType::Type => DropKind::Type,
        ObjectType::Schema => DropKind::Schema,
        ObjectType::Role | ObjectType::User => DropKind::Role,
        ObjectType::Collation => return Err(diag::unsupported_construct("DROP COLLATION", None)),
        ObjectType::Database => return Err(PgError::internal("ddl_drop: DROP DATABASE is not a DropKind")),
        ObjectType::Stage | ObjectType::Stream | ObjectType::Warehouse => {
            // Not PostgreSQL keywords: PG reports the unknown word as a syntax error.
            return Err(diag::syntax_error_near(&object_type.to_string().to_lowercase(), None));
        }
    })
}

/// Targets of `DROP <kind> name, ...` (one entry per name, in written order).
fn drop_targets(b: &mut Binder, kind: DropKind, names: &[ObjectName]) -> Result<Vec<DropTarget>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let target = match kind {
            DropKind::Schema | DropKind::Role => {
                let q = plain_name(b, name)?;
                DropTarget { schema: None, name: q.name, args: None, all_args_lookup: false }
            }
            _ => plain_target(b, name)?,
        };
        out.push(target);
    }
    Ok(out)
}

/// Argument type OIDs of a routine's written argument list. OUT arguments are not part of the
/// identity (PG `LookupFuncWithArgs`); INOUT and VARIADIC are.
fn arg_type_oids(b: &mut Binder, list: &[OperateFunctionArg], pos: Option<u32>) -> Result<Vec<Oid>> {
    let mut oids = Vec::with_capacity(list.len());
    for arg in list {
        if matches!(arg.mode, Some(ArgMode::Out)) {
            continue;
        }
        let resolved = resolve_data_type(b, &arg.data_type, pos)?;
        oids.push(resolved.id.oid);
    }
    Ok(oids)
}

/// Targets of `DROP FUNCTION f(...)`, `DROP PROCEDURE p(...)`: `args` is `None` when the argument
/// list was omitted.
fn routine_targets(b: &mut Binder, descs: &[FunctionDesc]) -> Result<Vec<DropTarget>> {
    let mut out = Vec::with_capacity(descs.len());
    for desc in descs {
        let q = name_of(b, &desc.name)?;
        let args = match &desc.args {
            None => None,
            Some(list) => Some(arg_type_oids(b, list, q.pos)?),
        };
        // PG `LookupFuncWithArgs`: a non-empty list with no parameter mode at all (no IN/OUT/INOUT/VARIADIC).
        let all_args_lookup = desc.args.as_ref().is_some_and(|list| !list.is_empty() && list.iter().all(|a| a.mode.is_none()));
        out.push(DropTarget { schema: q.schema, name: q.name, args, all_args_lookup });
    }
    Ok(out)
}

fn drop_bound(kind: DropKind, targets: Vec<DropTarget>, if_exists: bool, cascade: bool) -> BoundStatement {
    BoundStatement::Drop(Box::new(BoundDrop { kind, targets, if_exists, cascade }))
}

/// `DROP DATABASE [IF EXISTS] name` (one name, no CASCADE/RESTRICT in PG's grammar).
fn bind_drop_database(
    b: &mut Binder,
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
    restrict: bool,
) -> Result<BoundStatement> {
    if cascade {
        return Err(diag::syntax_error_near("CASCADE", None));
    }
    if restrict {
        return Err(diag::syntax_error_near("RESTRICT", None));
    }
    let [single] = names else {
        return Err(diag::syntax_error_near(",", None));
    };
    let q = plain_name(b, single)?;
    Ok(BoundStatement::DropDatabase { name: q.name, if_exists, force: false })
}

/// `Statement::Drop`, `DropFunction`, `DropProcedure`, `DropDomain`.
pub fn bind_drop(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::Drop { object_type, if_exists, names, cascade, restrict, purge, temporary, table } => {
            if *purge {
                return Err(diag::syntax_error_near("PURGE", None));
            }
            if *temporary {
                return Err(diag::syntax_error_near("TEMPORARY", None));
            }
            if table.is_some() {
                // MySQL-only `DROP INDEX i ON t`.
                return Err(diag::syntax_error_near("ON", None));
            }
            if matches!(object_type, ObjectType::Database) {
                return bind_drop_database(b, names, *if_exists, *cascade, *restrict);
            }
            if *cascade && *restrict {
                return Err(diag::syntax_error_near("RESTRICT", None));
            }
            // A multi-name `DROP DOMAIN` arrives as `DROP TYPE` (see `parser::drop_domain_list_as_type`).
            let kind = if b.scratch.contains_key(DROP_DOMAIN_KEY) { DropKind::Domain } else { drop_kind_of(object_type)? };
            let targets = drop_targets(b, kind, names)?;
            Ok(drop_bound(kind, targets, *if_exists, *cascade))
        }
        Statement::DropFunction(d) => {
            let targets = routine_targets(b, &d.func_desc)?;
            // `DROP ROUTINE` (PG `OBJECT_ROUTINE`) matches a function or a procedure of any kind.
            let kind = if b.scratch.contains_key(DROP_ROUTINE_KEY) { DropKind::Routine } else { DropKind::Function };
            Ok(drop_bound(kind, targets, d.if_exists, is_cascade(&d.drop_behavior)))
        }
        Statement::DropProcedure { if_exists, proc_desc, drop_behavior } => {
            let targets = routine_targets(b, proc_desc)?;
            Ok(drop_bound(DropKind::Procedure, targets, *if_exists, is_cascade(drop_behavior)))
        }
        Statement::DropDomain(d) => {
            let target = plain_target(b, &d.name)?;
            Ok(drop_bound(DropKind::Domain, vec![target], d.if_exists, is_cascade(&d.drop_behavior)))
        }
        _ => Err(PgError::internal("ddl_drop::bind_drop: not a DROP statement")),
    }
}

/// Resolve one TRUNCATE target to a table OID (PG `RangeVarCallbackForTruncate` + `truncate_check_rel`).
fn truncate_target(env: &CatalogEnv, q: &QualName) -> Result<Oid> {
    let schema = q.schema.as_deref();
    let class = env.lookup_class(schema, &q.name).map_err(|e| at(e, q.pos))?;
    let oid = match class {
        None => return Err(at(errors::relation_does_not_exist(schema, &q.name), q.pos)),
        Some(ClassRef::Relation(oid)) => oid,
        // Indexes and sequences are not tables (PG checks the relkind after the lookup).
        Some(_) => return Err(at(errors::not_a(&q.name, "table"), q.pos)),
    };
    let snap = env.snapshot();
    let rel = snap.relation(oid).ok_or_else(|| at(errors::cache_lookup_failed("relation", oid), q.pos))?;
    match rel.kind {
        RelKind::Table | RelKind::PartitionedTable => {}
        RelKind::ForeignTable => {
            // PGArbor has no foreign data wrappers, so no FDW supports TRUNCATE.
            return Err(at(
                PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot truncate foreign table \"{}\"", q.name)),
                q.pos,
            ));
        }
        _ => return Err(at(errors::not_a(&q.name, "table"), q.pos)),
    }
    if rel.namespace == builtin::PG_CATALOG_NAMESPACE || rel.namespace == builtin::INFORMATION_SCHEMA_NAMESPACE {
        return Err(at(errors::system_catalog_permission(&q.name), q.pos));
    }
    Ok(oid)
}

/// `Statement::Truncate`.
pub fn bind_truncate(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let t = match stmt {
        Statement::Truncate(t) => t,
        _ => return Err(PgError::internal("ddl_drop::bind_truncate: not a TRUNCATE statement")),
    };
    // PG's TRUNCATE grammar has none of these.
    if t.if_exists {
        return Err(diag::syntax_error_near("IF", None));
    }
    if t.partitions.is_some() {
        return Err(diag::syntax_error_near("PARTITION", None));
    }
    if t.on_cluster.is_some() {
        return Err(diag::syntax_error_near("ON", None));
    }
    let restart_identity = matches!(t.identity, Some(TruncateIdentityOption::Restart));
    let cascade = matches!(t.cascade, Some(CascadeOption::Cascade));
    let env = b.env;
    let mut tables: Vec<Oid> = Vec::with_capacity(t.table_names.len());
    for target in &t.table_names {
        // `BoundTruncate` carries no ONLY flag, so `TRUNCATE ONLY` would also truncate descendants;
        // refuse it rather than run the wrong statement.
        if target.only {
            return Err(diag::unsupported_construct("TRUNCATE ONLY", b.pos(&target.name)));
        }
        let q = name_of(b, &target.name)?;
        let oid = truncate_target(env, &q)?;
        if !tables.contains(&oid) {
            tables.push(oid);
        }
    }
    Ok(BoundStatement::Truncate(BoundTruncate { tables, restart_identity, cascade }))
}

/// Result of resolving a COMMENT target: the address to comment, or a skipped object (IF EXISTS).
enum Resolved {
    Addr(ObjectAddress),
    Skip(PgError),
}

/// The relation kind a COMMENT ON <relation kind> names.
#[derive(Clone, Copy)]
enum RelWant {
    Table,
    View,
    MatView,
    Index,
    Sequence,
}

/// COMMENT ON TABLE / VIEW / MATERIALIZED VIEW / INDEX / SEQUENCE (PG `get_object_address_relobject`:
/// `"t" is not a table` for a kind mismatch).
fn comment_relation(b: &mut Binder, name: &ObjectName, want: RelWant, if_exists: bool) -> Result<Resolved> {
    let q = name_of(b, name)?;
    let env = b.env;
    let schema = q.schema.as_deref();
    let found = env.lookup_class(schema, &q.name).map_err(|e| at(e, q.pos))?;
    let class = match found {
        Some(c) => c,
        None => {
            if if_exists {
                let msg = format!("relation \"{}\" does not exist, skipping", q.display());
                return Ok(Resolved::Skip(notice(msg)));
            }
            return Err(at(errors::relation_does_not_exist(schema, &q.name), q.pos));
        }
    };
    let (oid, kind_ok) = match class {
        ClassRef::Relation(oid) => {
            let kind = env.snapshot().relation(oid).map(|r| r.kind);
            let ok = match (want, kind) {
                (RelWant::Table, Some(RelKind::Table | RelKind::PartitionedTable)) => true,
                (RelWant::View, Some(RelKind::View)) => true,
                (RelWant::MatView, Some(RelKind::MatView)) => true,
                _ => false,
            };
            (oid, ok)
        }
        ClassRef::Index(oid) | ClassRef::SystemIndex(oid) => (oid, matches!(want, RelWant::Index)),
        ClassRef::Sequence(oid) => (oid, matches!(want, RelWant::Sequence)),
    };
    if !kind_ok {
        let word = match want {
            RelWant::Table => "table",
            RelWant::View => "view",
            RelWant::MatView => "materialized view",
            RelWant::Index => "index",
            RelWant::Sequence => "sequence",
        };
        return Err(at(errors::not_a(&q.name, word), q.pos));
    }
    Ok(Resolved::Addr(ObjectAddress::relation(oid)))
}

/// COMMENT ON COLUMN [schema.]table.column (`sub_id` = attnum).
fn comment_column(b: &mut Binder, name: &ObjectName, if_exists: bool) -> Result<Resolved> {
    let pos = b.pos(name);
    let mut parts: Vec<String> = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            ObjectNamePart::Identifier(id) => parts.push(b.ident(id)),
            ObjectNamePart::Function(_) => return Err(at(PgError::syntax("syntax error", None), pos)),
        }
    }
    let (schema, table, column): (Option<&str>, &str, &str) = match parts.as_slice() {
        [t, c] => (None, t.as_str(), c.as_str()),
        [s, t, c] => (Some(s.as_str()), t.as_str(), c.as_str()),
        [cat, s, t, c] => {
            if *cat != b.ctx.session.database {
                let full = format!("{cat}.{s}.{t}");
                return Err(at(diag::cross_database(&full, pos), pos));
            }
            (Some(s.as_str()), t.as_str(), c.as_str())
        }
        [_] => return Err(at(PgError::syntax("column name must be qualified", None), pos)),
        _ => {
            let dotted = parts.join(".");
            return Err(at(
                PgError::syntax(format!("improper qualified name (too many dotted names): {dotted}"), None),
                pos,
            ));
        }
    };
    let env = b.env;
    let found = env.lookup_class(schema, table).map_err(|e| at(e, pos))?;
    let oid = match found {
        Some(ClassRef::Relation(oid)) => oid,
        Some(ClassRef::Index(_)) | Some(ClassRef::SystemIndex(_)) => {
            let what = format!("cannot open relation \"{table}\"");
            return Err(at(errors::operation_not_supported_for(&what, "indexes"), pos));
        }
        Some(ClassRef::Sequence(_)) => {
            // A sequence's own columns are last_value, log_cnt and is_called; any other name is missing.
            if !matches!(column, "last_value" | "log_cnt" | "is_called") {
                return Err(at(errors::column_of_relation_does_not_exist(column, table), pos));
            }
            let what = format!("cannot open relation \"{table}\"");
            return Err(at(errors::operation_not_supported_for(&what, "sequences"), pos));
        }
        None => {
            if if_exists {
                let msg = format!("relation \"{}\" does not exist, skipping", qualified_text(schema, table));
                return Ok(Resolved::Skip(notice(msg)));
            }
            return Err(at(errors::relation_does_not_exist(schema, table), pos));
        }
    };
    let snap = env.snapshot();
    let rel = snap.relation(oid).ok_or_else(|| at(errors::cache_lookup_failed("relation", oid), pos))?;
    match rel.column_by_name(column) {
        Some(col) => Ok(Resolved::Addr(ObjectAddress::column(oid, col.attnum))),
        None => {
            if if_exists {
                return Ok(Resolved::Skip(errors::column_of_relation_does_not_exist_skipping(column, &rel.name)));
            }
            Err(at(errors::column_of_relation_does_not_exist(column, &rel.name), pos))
        }
    }
}

/// COMMENT ON SCHEMA name (never qualified in PG's grammar).
fn comment_schema(b: &mut Binder, name: &ObjectName, if_exists: bool) -> Result<Resolved> {
    let q = plain_name(b, name)?;
    let env = b.env;
    match env.lookup_namespace(&q.name) {
        Some(ns) => Ok(Resolved::Addr(ObjectAddress::namespace(ns))),
        None => {
            if if_exists {
                return Ok(Resolved::Skip(errors::schema_does_not_exist_skipping(&q.name)));
            }
            Err(at(errors::schema_does_not_exist(&q.name), q.pos))
        }
    }
}

/// COMMENT ON TYPE / DOMAIN (the comment goes on the pg_type row).
fn comment_type(b: &mut Binder, name: &ObjectName, domain: bool, if_exists: bool) -> Result<Resolved> {
    let q = name_of(b, name)?;
    let env = b.env;
    let schema = q.schema.as_deref();
    let found = env.lookup_type(schema, &q.name).map_err(|e| at(e, q.pos))?;
    let oid = match found {
        Some(oid) => oid,
        None => {
            if if_exists {
                return Ok(Resolved::Skip(errors::kind_does_not_exist_skipping("type", schema, &q.name)));
            }
            return Err(at(errors::kind_does_not_exist("type", schema, &q.name), q.pos));
        }
    };
    if domain {
        let is_domain = env.snapshot().type_obj(oid).is_some_and(|t| t.is_domain());
        if !is_domain {
            return Err(at(errors::not_a_domain(&q.display()), q.pos));
        }
    }
    Ok(Resolved::Addr(ObjectAddress::type_(oid)))
}

/// COMMENT ON FUNCTION / PROCEDURE / ROUTINE name [(argument list)] (PG `get_object_address` and
/// `LookupFuncWithArgs`). With an argument list the routine is identified by its input argument types alone
/// (OUT arguments, names and defaults do not take part). Without one the name must be unambiguous.
/// A routine of the other kind is 42809.
fn comment_routine(
    b: &mut Binder,
    name: &ObjectName,
    args: Option<&[OperateFunctionArg]>,
    kind: RoutineKind,
    if_exists: bool,
) -> Result<Resolved> {
    let q = name_of(b, name)?;
    let env = b.env;
    let schema = q.schema.as_deref();
    let cands = env.function_candidates(schema, &q.name).map_err(|e| at(e, q.pos))?;
    let word = match kind {
        RoutineKind::Procedure => "procedure",
        RoutineKind::Function | RoutineKind::Routine => "function",
    };
    let Some(list) = args else {
        let matching: Vec<_> = cands.iter().filter(|c| kind_matches(kind, &c.kind)).collect();
        return match matching.as_slice() {
            [] => {
                if if_exists {
                    let msg = format!("{word} {}() does not exist, skipping", q.display());
                    return Ok(Resolved::Skip(notice(msg)));
                }
                Err(at(errors::no_function_named(word, &q.display()), q.pos))
            }
            [one] => Ok(Resolved::Addr(ObjectAddress::proc(one.oid))),
            _ => Err(at(errors::function_name_not_unique(&q.display()), q.pos)),
        };
    };
    let types = arg_type_oids(b, list, q.pos)?;
    let Some(cand) = cands.iter().filter(|c| c.arg_types == types).min_by_key(|c| c.path_pos) else {
        let sig = format!("{}({})", q.display(), oid_type_names(env, &types)?);
        if if_exists {
            return Ok(Resolved::Skip(notice(format!("{word} {sig} does not exist, skipping"))));
        }
        let err = match kind {
            RoutineKind::Procedure => PgError::new(SqlState::UNDEFINED_FUNCTION, format!("procedure {sig} does not exist")),
            RoutineKind::Function | RoutineKind::Routine => errors::function_does_not_exist(&sig),
        };
        return Err(at(err, q.pos));
    };
    if !kind_matches(kind, &cand.kind) {
        let sig = format!("{}({})", q.display(), oid_type_names(env, &types)?);
        return Err(at(errors::wrong_routine_kind(&sig, word), q.pos));
    }
    Ok(Resolved::Addr(ObjectAddress::proc(cand.oid)))
}

/// Whether a routine of kind `proc_kind` is named by COMMENT ON `kind` (FUNCTION names a non-procedure).
fn kind_matches(kind: RoutineKind, proc_kind: &ProcKind) -> bool {
    match kind {
        RoutineKind::Routine => true,
        RoutineKind::Procedure => matches!(proc_kind, ProcKind::Procedure),
        RoutineKind::Function => !matches!(proc_kind, ProcKind::Procedure),
    }
}

/// Type names of an argument list for messages (`integer, text`).
fn oid_type_names(env: &CatalogEnv, oids: &[Oid]) -> Result<String> {
    let mut names = Vec::with_capacity(oids.len());
    for oid in oids {
        names.push(format_type(env, *oid, -1)?);
    }
    Ok(names.join(", "))
}

/// COMMENT ON ROLE / USER name.
fn comment_role(b: &mut Binder, name: &ObjectName, if_exists: bool) -> Result<Resolved> {
    let q = plain_name(b, name)?;
    let env = b.env;
    match env.snapshot().role_by_name(&q.name) {
        Some(role) => Ok(Resolved::Addr(ObjectAddress::new(ObjClass::Role, role.oid))),
        None => {
            if if_exists {
                return Ok(Resolved::Skip(errors::role_does_not_exist_skipping(&q.name)));
            }
            Err(at(errors::role_does_not_exist(&q.name), q.pos))
        }
    }
}

/// COMMENT ON DATABASE name.
fn comment_database(b: &mut Binder, name: &ObjectName, if_exists: bool) -> Result<Resolved> {
    let q = plain_name(b, name)?;
    let env = b.env;
    match env.snapshot().database_by_name(&q.name) {
        Some(db) => Ok(Resolved::Addr(ObjectAddress::new(ObjClass::Database, db.oid))),
        None => {
            if if_exists {
                return Ok(Resolved::Skip(notice(format!("database \"{}\" does not exist, skipping", q.name))));
            }
            Err(at(errors::database_does_not_exist(&q.name), q.pos))
        }
    }
}

/// COMMENT resolves its object through `get_object_address`, which has no parse location: only syntax
/// errors carry a position.
fn drop_comment_position(mut e: PgError) -> PgError {
    if e.sqlstate != SqlState::SYNTAX_ERROR {
        e.position = None;
    }
    e
}

/// `Statement::Comment`.
pub fn bind_comment(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    bind_comment_inner(b, stmt).map_err(drop_comment_position)
}

fn bind_comment_inner(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let (object_type, object_name, comment, if_exists) = match stmt {
        Statement::Comment { object_type, object_name, comment, if_exists } => {
            (object_type, object_name, comment, *if_exists)
        }
        _ => return Err(PgError::internal("ddl_drop::bind_comment: not a COMMENT statement")),
    };
    // PG treats an empty string like NULL: both remove the comment.
    let text = match comment {
        Some(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let resolved = match object_type {
        CommentObject::Table => comment_relation(b, object_name, RelWant::Table, if_exists)?,
        CommentObject::View => comment_relation(b, object_name, RelWant::View, if_exists)?,
        CommentObject::MaterializedView => comment_relation(b, object_name, RelWant::MatView, if_exists)?,
        CommentObject::Index => comment_relation(b, object_name, RelWant::Index, if_exists)?,
        CommentObject::Sequence => comment_relation(b, object_name, RelWant::Sequence, if_exists)?,
        CommentObject::Column => comment_column(b, object_name, if_exists)?,
        CommentObject::Schema => comment_schema(b, object_name, if_exists)?,
        CommentObject::Type => comment_type(b, object_name, false, if_exists)?,
        CommentObject::Domain => comment_type(b, object_name, true, if_exists)?,
        CommentObject::Function => comment_routine(b, object_name, None, RoutineKind::Function, if_exists)?,
        CommentObject::Procedure => comment_routine(b, object_name, None, RoutineKind::Procedure, if_exists)?,
        CommentObject::Role | CommentObject::User => comment_role(b, object_name, if_exists)?,
        CommentObject::Database => comment_database(b, object_name, if_exists)?,
        CommentObject::Collation | CommentObject::Extension => {
            return Err(diag::unsupported_construct(&format!("COMMENT ON {object_type}"), None));
        }
    };
    Ok(match resolved {
        Resolved::Addr(addr) => BoundStatement::Comment { addr, text },
        Resolved::Skip(n) => BoundStatement::Noop { tag: "COMMENT".to_string(), notices: vec![n] },
    })
}

/// `ExtraStatement::CommentRoutine`: COMMENT ON FUNCTION / PROCEDURE / ROUTINE with an optional argument list.
pub fn bind_comment_routine(
    b: &mut Binder,
    kind: RoutineKind,
    name: &ObjectName,
    args: Option<&[OperateFunctionArg]>,
    comment: Option<&str>,
) -> Result<BoundStatement> {
    // PG treats an empty string like NULL: both remove the comment.
    let text = match comment {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    let resolved = comment_routine(b, name, args, kind, false).map_err(drop_comment_position)?;
    Ok(match resolved {
        Resolved::Addr(addr) => BoundStatement::Comment { addr, text },
        Resolved::Skip(n) => BoundStatement::Noop { tag: "COMMENT".to_string(), notices: vec![n] },
    })
}

/// A `RoleSpec` of DROP OWNED / REASSIGN OWNED: unquoted `CURRENT_USER` / `CURRENT_ROLE` are the current
/// user, `SESSION_USER` the session user (PG `get_rolespec_oid`); anything else is a role name.
fn owned_role_oid(b: &mut Binder, id: &Ident) -> Result<Oid> {
    if id.quote_style.is_none() {
        match id.value.to_ascii_lowercase().as_str() {
            "current_user" | "current_role" => return Ok(b.env.current_user()),
            "session_user" => return Ok(b.env.session_user()),
            _ => {}
        }
    }
    let name = b.ident(id);
    b.env.role_oid(&name)
}

/// `ExtraStatement::DropOwned` / `ReassignOwned`.
pub fn bind_owned(b: &mut Binder, stmt: &ExtraStatement) -> Result<BoundStatement> {
    match stmt {
        ExtraStatement::DropOwned { roles, cascade } => {
            let mut oids = Vec::with_capacity(roles.len());
            for role in roles {
                oids.push(owned_role_oid(b, role)?);
            }
            Ok(BoundStatement::DropOwned { roles: oids, cascade: *cascade })
        }
        ExtraStatement::ReassignOwned { old_roles, new_role } => {
            let mut oids = Vec::with_capacity(old_roles.len());
            for role in old_roles {
                oids.push(owned_role_oid(b, role)?);
            }
            let new_owner = owned_role_oid(b, new_role)?;
            Ok(BoundStatement::ReassignOwned { old_roles: oids, new_owner })
        }
        _ => Err(PgError::internal("ddl_drop::bind_owned: not a DROP OWNED / REASSIGN OWNED statement")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec};
    use pgarbor_catalog::model::Persistence;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::{oids, TypeId};
    use sqlparser::ast::Ident;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse(sql: &str) -> Statement {
        let mut v = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL must parse");
        v.remove(0)
    }

    /// Catalog with one `public.<name>(a int4)` table; returns the env after its creation.
    fn env_with_table(name: &str) -> Result<CatalogEnv> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::of(oids::INT4))],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        };
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        Ok(env)
    }

    /// Run `f` with a binder over `sql` (statement parsed by sqlparser directly).
    fn with_binder<T>(
        env: &CatalogEnv,
        sql: &str,
        f: impl FnOnce(&mut Binder<'_>, &Statement) -> Result<T>,
    ) -> Result<T> {
        let stmt = parse(sql);
        let src = SourceMap::new(sql);
        let ctx = BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let mut b = Binder::new(&ctx, &src);
        f(&mut b, &stmt)
    }

    fn table_oid(env: &CatalogEnv, name: &str) -> Oid {
        match env.lookup_class(Some("public"), name) {
            Ok(Some(ClassRef::Relation(oid))) => oid,
            _ => panic!("test table {name} must exist"),
        }
    }

    #[test]
    fn drop_table_if_exists_cascade() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let out = with_binder(&env, "drop table if exists a, b cascade", bind_drop)?;
        let BoundStatement::Drop(d) = out else { panic!("expected a Drop statement") };
        assert_eq!(d.kind, DropKind::Table);
        assert_eq!(d.targets, vec![DropTarget::simple("a"), DropTarget::simple("b")]);
        assert!(d.if_exists);
        assert!(d.cascade);
        Ok(())
    }

    #[test]
    fn drop_index_user_and_database_kinds() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let out = with_binder(&env, "drop index i", bind_drop)?;
        let BoundStatement::Drop(d) = out else { panic!("expected a Drop statement") };
        assert_eq!(d.kind, DropKind::Index);
        let out = with_binder(&env, "drop user r", bind_drop)?;
        let BoundStatement::Drop(d) = out else { panic!("expected a Drop statement") };
        assert_eq!(d.kind, DropKind::Role);
        assert_eq!(d.targets, vec![DropTarget::simple("r")]);
        let out = with_binder(&env, "drop database d", bind_drop)?;
        match out {
            BoundStatement::DropDatabase { name, if_exists, force } => {
                assert_eq!(name, "d");
                assert!(!if_exists);
                assert!(!force);
            }
            _ => panic!("expected DropDatabase"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-07"]
    fn drop_routine_arg_types_need_type_binder() -> Result<()> {
        // Argument OIDs come from `types::resolve_data_type` (SQL-07): f(int, text) -> [23, 25].
        let (_kv, env) = testing::bootstrapped()?;
        let out = with_binder(&env, "drop function f(int, text)", bind_drop)?;
        let BoundStatement::Drop(d) = out else { panic!("expected a Drop statement") };
        assert_eq!(d.kind, DropKind::Function);
        assert_eq!(d.targets[0].args, Some(vec![23, 25]));
        Ok(())
    }

    #[test]
    fn drop_unsupported_kinds_are_0a000() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let err = with_binder(&env, "drop collation c", bind_drop).err().expect("collation drop is unsupported");
        assert_eq!(err.sqlstate.code(), "0A000");
        Ok(())
    }

    #[test]
    fn truncate_restart_identity_cascade_resolves_oids() -> Result<()> {
        let env = env_with_table("t")?;
        let expected = table_oid(&env, "t");
        let out = with_binder(&env, "truncate t, t restart identity cascade", bind_truncate)?;
        match out {
            BoundStatement::Truncate(t) => {
                assert_eq!(t.tables, vec![expected]);
                assert!(t.restart_identity);
                assert!(t.cascade);
            }
            _ => panic!("expected Truncate"),
        }
        Ok(())
    }

    #[test]
    fn truncate_missing_table_is_42p01() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let err = with_binder(&env, "truncate nosuch", bind_truncate).err().expect("missing table must fail");
        assert_eq!(err.sqlstate.code(), "42P01");
        assert_eq!(err.message, "relation \"nosuch\" does not exist");
        Ok(())
    }

    #[test]
    fn comment_on_column_uses_attnum_as_sub_id() -> Result<()> {
        let env = env_with_table("t")?;
        let oid = table_oid(&env, "t");
        let out = with_binder(&env, "comment on column t.a is 'x'", bind_comment)?;
        match out {
            BoundStatement::Comment { addr, text } => {
                assert_eq!(addr, ObjectAddress::column(oid, 1));
                assert_eq!(text, Some("x".to_string()));
            }
            _ => panic!("expected Comment"),
        }
        let out = with_binder(&env, "comment on column t.a is null", bind_comment)?;
        match out {
            BoundStatement::Comment { text, .. } => assert_eq!(text, None),
            _ => panic!("expected Comment"),
        }
        Ok(())
    }

    #[test]
    fn comment_on_missing_column_is_42703() -> Result<()> {
        let env = env_with_table("t")?;
        let err = with_binder(&env, "comment on column t.zz is 'x'", bind_comment)
            .err()
            .expect("missing column must fail");
        assert_eq!(err.sqlstate.code(), "42703");
        Ok(())
    }

    #[test]
    fn comment_if_exists_on_missing_table_is_noop_with_notice() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let out = with_binder(&env, "comment if exists on table nosuch is 'x'", bind_comment)?;
        match out {
            BoundStatement::Noop { notices, .. } => {
                assert_eq!(notices.len(), 1);
                assert_eq!(notices[0].message, "relation \"nosuch\" does not exist, skipping");
                assert_eq!(notices[0].severity, Severity::Notice);
            }
            _ => panic!("missing table with IF EXISTS must be a Noop"),
        }
        Ok(())
    }

    #[test]
    fn comment_without_if_exists_on_missing_table_is_42p01() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let err = with_binder(&env, "comment on table nosuch is 'x'", bind_comment)
            .err()
            .expect("missing table must fail");
        assert_eq!(err.sqlstate.code(), "42P01");
        Ok(())
    }

    #[test]
    fn truncate_only_is_refused_with_0a000() -> Result<()> {
        let env = env_with_table("t")?;
        let err = with_binder(&env, "truncate only t", bind_truncate).err().expect("TRUNCATE ONLY must fail");
        assert_eq!(err.sqlstate.code(), "0A000");
        Ok(())
    }

    #[test]
    fn drop_owned_resolves_roles() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let expected = env.role_oid("postgres")?;
        let stmt = ExtraStatement::DropOwned { roles: vec![Ident::new("postgres")], cascade: true };
        let src = SourceMap::new("drop owned by postgres cascade");
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let mut b = Binder::new(&ctx, &src);
        match bind_owned(&mut b, &stmt)? {
            BoundStatement::DropOwned { roles, cascade } => {
                assert_eq!(roles, vec![expected]);
                assert!(cascade);
            }
            _ => panic!("expected DropOwned"),
        }
        Ok(())
    }
}
