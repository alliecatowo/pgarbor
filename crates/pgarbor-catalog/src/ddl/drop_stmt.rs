//! Statement-level DROP: name lookup, kind checks, permission checks, IF EXISTS notices, then one
//! `drop_objects` call for all targets. Lease CAT-25. PG sources: `commands/dropcmds.c`
//! (`RemoveObjects`, `owningrel_does_not_exist_skipping`, `does_not_exist_skipping`),
//! `commands/tablecmds.c` (`RemoveRelations`, `RangeVarCallbackForDropRelation`),
//! `commands/typecmds.c` (`RemoveTypeById`), `commands/schemacmds.c`, `parser/parse_func.c`
//! (`LookupFuncWithArgs`).
//!
//! ## What gets resolved here
//! The binder passes NAMES (not OIDs) because the error and NOTICE wording depends on lookup results:
//! [`DropTarget`] = optional schema, name, and for routines the optional argument type list.
//! Lookup uses the statement's `CatalogEnv` (search_path-aware, `lookup_class` / `lookup_type` /
//! `function_candidates`). Duplicate targets are collapsed (`drop table t, t` succeeds).
//!
//! ## Messages (oracle-verified PG 18.6; `kind_word` = table|view|materialized view|index|sequence|
//! type|schema|foreign table)
//! - missing object: 42P01 `table "nosuch" does not exist` (tables, views, matviews, sequences, foreign
//!   tables: 42P01); `index "nosuch" does not exist` and `type "nosuch" does not exist` (also for
//!   DROP DOMAIN): 42704 (`errors::kind_does_not_exist`). With a qualifier the name prints qualified
//!   (`table "s.nosuch" does not exist`). Missing SCHEMA of a qualified name: error 3F000 `schema
//!   "zz_nos" does not exist`; with IF EXISTS NOTICE `schema "zz_nos" does not exist, skipping`.
//!   IF EXISTS on a missing object: NOTICE `table "nosuch" does not exist, skipping`
//!   (`errors::kind_does_not_exist_skipping`).
//! - wrong kind (`wrong_object_type_for_drop(name, expected_word, actual)`): `DROP TABLE s` on a sequence
//!   -> 42809 `"s" is not a table` HINT `Use DROP SEQUENCE to remove a sequence.`; DROP VIEW on a table ->
//!   `"t" is not a view` HINT `Use DROP TABLE to remove a table.`; composite types: `DROP TABLE c` ->
//!   HINT `Use DROP TYPE to remove a type.`; DROP INDEX on a table -> `"t" is not an index`.
//!   `DROP TABLE` also accepts partitioned tables; PG's DROP TABLE refuses views etc. as above.
//! - DROP TYPE on a table/matview rowtype -> 2BP01-class via the INTERNAL rule:
//!   `cannot drop type t because table t requires it` HINT `You can drop table t instead.`; on an
//!   array type -> `cannot drop type t[] because type t requires it`. DROP TYPE on a composite type
//!   (a `CompositeType` relation) drops the relation. DROP DOMAIN on a non-domain -> 42809
//!   `e is not a domain` (`errors::not_a_domain`, unquoted).
//! - system objects: relations in pg_catalog/information_schema -> 42501 `permission denied: "pg_class"
//!   is a system catalog`; schemas `pg_catalog`, `pg_toast`, `information_schema` -> 2BP01
//!   `cannot drop schema pg_catalog because it is required by the database system`
//!   (`errors::required_by_system`, from the pinned check in `plan_drop`).
//! - ownership: the current user must own the object (relations: the table owner; types, functions:
//!   their owner; DROP SCHEMA: the schema owner) or be a member of its owner or a superuser, else
//!   42501 `must be owner of table t` (`errors::must_be_owner`), checked per target BEFORE planning
//!   (PG `RangeVarCallbackForDropRelation`). The schema's owner has NO right to drop other roles'
//!   objects inside it.
//! - routines (`drop function/procedure/routine/aggregate`):
//!   `args = Some(types)`: exact match of IN-argument identity, none -> 42883 `function f(integer) does not
//!   exist` (`errors::function_does_not_exist(sig)`; IF EXISTS NOTICE `function nosuch(pg_catalog.int4,text)
//!   does not exist, skipping` - NOTE the internal `pg_catalog.int4` type names in the NOTICE, plain names
//!   in the error; `errors::function_does_not_exist_skipping`); kind mismatch -> 42809 `p(integer) is not a
//!   function` / `f(integer) is not a procedure` (`errors::wrong_routine_kind`; ROUTINE accepts both).
//!   `args = None` (`drop function f`): all candidates named f visible in the search path (or the
//!   qualified schema); none -> 42883 `could not find a function named "f"` (`errors::no_function_named`;
//!   procedure variant says `procedure`; IF EXISTS NOTICE `function nosuch() does not exist, skipping`,
//!   `procedure nosuch() does not exist, skipping`); more than one -> 42725 `function name "f" is not
//!   unique` HINT `Specify the argument list to select the function unambiguously.`
//!   (`errors::function_name_not_unique`). Builtin (pg_catalog) functions: system-catalog permission denied.
//! - DROP ROLE: `drop_role` per name (with `errors::role_does_not_exist` /
//!   `role_does_not_exist_skipping`), executed after resolving all names.
//!
//! After resolution: `drop_objects(&targets, behavior)` once for all targets (one CASCADE notice
//! for the whole statement). Multiple targets with IF EXISTS: every missing one gets its own NOTICE
//! in argument order, then the existing ones are dropped.
use crate::builtin::{INFORMATION_SCHEMA_NAMESPACE, PG_CATALOG_NAMESPACE};
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::snapshot::{ClassRef, UserTypeRef};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState, FIRST_NORMAL_OID};
use pgarbor_types::catalog_data::{builtin_type, ProcKind};
use pgarbor_types::names::format_type;
use pgarbor_types::resolve::func::{signature_string, FuncCandidate};
use std::collections::HashSet;

/// What a DROP statement removes.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropKind {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    ForeignTable,
    /// DROP TYPE (enum, domain, composite, base).
    Type,
    Domain,
    Schema,
    Function,
    Procedure,
    /// DROP ROUTINE: function or procedure.
    Routine,
    Aggregate,
    Role,
}

/// One name in a DROP list.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub schema: Option<String>,
    pub name: String,
    /// Routines only: `Some(IN-argument types)` or `None` when the argument list was omitted.
    pub args: Option<Vec<Oid>>,
    /// Routines only: the written argument list has no parameter mode at all, so PG's second
    /// `LookupFuncWithArgs` pass (PROCEDURE and ROUTINE) also matches all argument types, OUT included.
    pub all_args_lookup: bool,
}

impl DropTarget {
    pub fn simple(name: impl Into<String>) -> DropTarget {
        DropTarget { schema: None, name: name.into(), args: None, all_args_lookup: false }
    }
}

/// All argument types of a routine candidate, OUT included (PG `proallargtypes`, or the input types when
/// every argument is IN). Built-in candidates have no entry in the snapshot, so their input types stand in.
fn all_arg_types(env: &CatalogEnv, cand: &FuncCandidate) -> Vec<Oid> {
    match env.snapshot().function(cand.oid) {
        Some(f) => f.args.iter().map(|a| a.type_oid).collect(),
        None => cand.arg_types.clone(),
    }
}

/// NOTICE 00000 with the given text (the IF EXISTS "does not exist, skipping" family).
fn skip_notice(msg: String) -> PgError {
    PgError::new(SqlState::SUCCESSFUL_COMPLETION, msg).with_severity(Severity::Notice)
}

/// Type names that gram.y spells with a `pg_catalog.` qualifier (`SystemTypeName`). A NOTICE that
/// prints an argument list reconstructs the spelling from the OID: these print qualified, every
/// other builtin prints its bare typname (`text`).
const SYSTEM_TYPE_SPELLINGS: &[&str] = &[
    "int2",
    "int4",
    "int8",
    "float4",
    "float8",
    "numeric",
    "bool",
    "bpchar",
    "varchar",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
    "interval",
    "bit",
    "varbit",
];

/// Argument type as the NOTICE of an IF EXISTS skip prints it (PG `TypeNameListToString`).
fn notice_type_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    if let Some(b) = builtin_type(oid) {
        return Ok(if SYSTEM_TYPE_SPELLINGS.contains(&b.name) {
            format!("pg_catalog.{}", b.name)
        } else {
            b.name.to_string()
        });
    }
    format_type(env, oid, -1)
}

/// `schema.name` when qualified, else `name` (the name as NameListToString prints it).
fn qualified_routine_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// The object word of a relation kind in messages.
fn relation_word(kind: DropKind) -> &'static str {
    match kind {
        DropKind::View => "view",
        DropKind::MaterializedView => "materialized view",
        DropKind::Index => "index",
        DropKind::Sequence => "sequence",
        DropKind::ForeignTable => "foreign table",
        _ => "table",
    }
}

/// The object word of a routine kind in messages.
fn routine_word(kind: DropKind) -> &'static str {
    match kind {
        DropKind::Procedure => "procedure",
        DropKind::Aggregate => "aggregate",
        DropKind::Routine => "routine",
        _ => "function",
    }
}

/// Whether a DROP of `kind` accepts a pg_class object of kind `actual` (PG `RemoveRelations`
/// callback: `DROP TABLE` also takes partitioned tables, `DROP INDEX` also partitioned indexes).
fn accepts_relation(kind: DropKind, actual: RelKind) -> bool {
    match kind {
        DropKind::Table => matches!(actual, RelKind::Table | RelKind::PartitionedTable),
        DropKind::View => actual == RelKind::View,
        DropKind::MaterializedView => actual == RelKind::MatView,
        DropKind::Index => matches!(actual, RelKind::Index | RelKind::PartitionedIndex),
        DropKind::Sequence => actual == RelKind::Sequence,
        DropKind::ForeignTable => actual == RelKind::ForeignTable,
        _ => false,
    }
}

/// PG `RemoveObjects` / `LookupFuncWithArgs` kind checks for a found routine. `sig` is the
/// `name(argtypes)` text PG prints in these errors, `qualified` the name as written.
fn check_routine_kind(kind: DropKind, cand: &FuncCandidate, qualified: &str, sig: &str) -> Result<()> {
    match kind {
        DropKind::Function => {
            if matches!(cand.kind, ProcKind::Procedure) {
                return Err(errors::wrong_routine_kind(sig, "function"));
            }
            if matches!(cand.kind, ProcKind::Aggregate) {
                return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{qualified}\" is an aggregate function"))
                    .with_hint("Use DROP AGGREGATE to drop aggregate functions."));
            }
        }
        DropKind::Procedure => {
            if !matches!(cand.kind, ProcKind::Procedure) {
                return Err(errors::wrong_routine_kind(sig, "procedure"));
            }
        }
        DropKind::Aggregate => {
            if !matches!(cand.kind, ProcKind::Aggregate) {
                return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("function {sig} is not an aggregate")));
            }
        }
        _ => {}
    }
    Ok(())
}

impl<'a> CatalogTxn<'a> {
    /// Execute a DROP statement (module docs).
    // CONTRACT
    pub fn drop_by_names(
        &mut self,
        kind: DropKind,
        targets: &[DropTarget],
        if_exists: bool,
        behavior: DropBehavior,
    ) -> Result<()> {
        let env = self.env()?;
        if kind == DropKind::Role {
            return self.drop_role_names(&env, targets, if_exists);
        }
        let mut addrs: Vec<ObjectAddress> = Vec::with_capacity(targets.len());
        let mut seen: HashSet<ObjectAddress> = HashSet::with_capacity(targets.len());
        for t in targets {
            let found = match kind {
                DropKind::Schema => self.resolve_schema_target(&env, t, if_exists)?,
                DropKind::Type | DropKind::Domain => self.resolve_type_target(&env, kind, t, if_exists)?,
                DropKind::Function | DropKind::Procedure | DropKind::Routine | DropKind::Aggregate => {
                    self.resolve_routine_target(&env, kind, t, if_exists)?
                }
                _ => self.resolve_relation_target(&env, kind, t, if_exists)?,
            };
            if let Some(addr) = found {
                if seen.insert(addr) {
                    addrs.push(addr);
                }
            }
        }
        if addrs.is_empty() {
            return Ok(());
        }
        self.drop_objects(&addrs, behavior)
    }

    /// Qualifier check of a possibly-qualified target: a missing schema is 3F000, or with IF EXISTS
    /// the NOTICE `schema "x" does not exist, skipping`. Returns `false` when the target is skipped.
    fn qualifier_exists(&mut self, env: &CatalogEnv, schema: Option<&str>, if_exists: bool) -> Result<bool> {
        match schema {
            Some(s) if env.lookup_namespace(s).is_none() => {
                if if_exists {
                    self.notice(errors::schema_does_not_exist_skipping(s));
                    Ok(false)
                } else {
                    Err(errors::schema_does_not_exist(s))
                }
            }
            _ => Ok(true),
        }
    }

    /// DROP TABLE / VIEW / MATERIALIZED VIEW / INDEX / SEQUENCE / FOREIGN TABLE.
    fn resolve_relation_target(
        &mut self,
        env: &CatalogEnv,
        kind: DropKind,
        t: &DropTarget,
        if_exists: bool,
    ) -> Result<Option<ObjectAddress>> {
        let word = relation_word(kind);
        let schema = t.schema.as_deref();
        if !self.qualifier_exists(env, schema, if_exists)? {
            return Ok(None);
        }
        let class = match env.lookup_class(schema, &t.name)? {
            Some(c) => c,
            None => {
                if if_exists {
                    self.notice(errors::kind_does_not_exist_skipping(word, schema, &t.name));
                    return Ok(None);
                }
                return Err(errors::kind_does_not_exist(word, schema, &t.name));
            }
        };
        let snap = env.snapshot();
        let (oid, actual) = match class {
            ClassRef::Relation(oid) => {
                let rel = snap.relation(oid).ok_or_else(|| errors::cache_lookup_failed("relation", oid))?;
                (oid, rel.kind)
            }
            ClassRef::Index(oid) | ClassRef::SystemIndex(oid) => (oid, RelKind::Index),
            ClassRef::Sequence(oid) => (oid, RelKind::Sequence),
        };
        if !accepts_relation(kind, actual) {
            return Err(errors::wrong_object_type_for_drop(&t.name, word, actual));
        }
        let addr = ObjectAddress::relation(oid);
        self.require_owner(&addr)?;
        let (namespace, _, _) =
            snap.class_identity(oid).ok_or_else(|| errors::cache_lookup_failed("relation", oid))?;
        if namespace == PG_CATALOG_NAMESPACE || namespace == INFORMATION_SCHEMA_NAMESPACE {
            return Err(errors::system_catalog_permission("pg_class"));
        }
        Ok(Some(addr))
    }

    /// DROP TYPE / DROP DOMAIN.
    fn resolve_type_target(
        &mut self,
        env: &CatalogEnv,
        kind: DropKind,
        t: &DropTarget,
        if_exists: bool,
    ) -> Result<Option<ObjectAddress>> {
        let schema = t.schema.as_deref();
        if !self.qualifier_exists(env, schema, if_exists)? {
            return Ok(None);
        }
        let oid = match env.lookup_type(schema, &t.name)? {
            Some(o) => o,
            None => {
                if if_exists {
                    self.notice(errors::kind_does_not_exist_skipping("type", schema, &t.name));
                    return Ok(None);
                }
                return Err(errors::kind_does_not_exist("type", schema, &t.name));
            }
        };
        let snap = env.snapshot();
        if kind == DropKind::Domain {
            let is_domain = snap.type_obj(oid).is_some_and(|ty| ty.is_domain());
            if !is_domain {
                return Err(errors::not_a_domain(&format_type(env, oid, -1)?));
            }
        }
        // DROP TYPE on a composite type's rowtype drops the relation itself (PG RemoveTypeById).
        let addr = match snap.user_type(oid) {
            Some(UserTypeRef::RowType(rel))
                if snap.relation(rel).is_some_and(|r| r.kind == RelKind::CompositeType) =>
            {
                ObjectAddress::relation(rel)
            }
            _ => ObjectAddress::type_(oid),
        };
        if addr.is_pinned() {
            // Built-in types: the pinned check in `plan_drop` reports them (2BP01); they have no
            // owner row in the snapshot to check against.
            return Ok(Some(addr));
        }
        self.require_owner(&addr)?;
        Ok(Some(addr))
    }

    /// DROP FUNCTION / PROCEDURE / ROUTINE / AGGREGATE.
    fn resolve_routine_target(
        &mut self,
        env: &CatalogEnv,
        kind: DropKind,
        t: &DropTarget,
        if_exists: bool,
    ) -> Result<Option<ObjectAddress>> {
        let schema = t.schema.as_deref();
        if !self.qualifier_exists(env, schema, if_exists)? {
            return Ok(None);
        }
        let word = routine_word(kind);
        let qualified = qualified_routine_name(schema, &t.name);
        let cands = env.function_candidates(schema, &t.name)?;
        let (cand, sig): (&FuncCandidate, String) = match &t.args {
            Some(types) => {
                // PG LookupFuncWithArgs: input-argument types first; a PROCEDURE or ROUTINE written without
                // argmodes is then looked up again with all argument types (OUT included).
                let mut found: Option<&FuncCandidate> = cands
                    .iter()
                    .filter(|c| c.arg_types.as_slice() == types.as_slice())
                    .min_by_key(|c| c.path_pos);
                if t.all_args_lookup && matches!(kind, DropKind::Procedure | DropKind::Routine) && !types.is_empty() {
                    let by_all_types = cands
                        .iter()
                        .filter(|c| all_arg_types(env, c).as_slice() == types.as_slice())
                        .min_by_key(|c| c.path_pos);
                    match (found, by_all_types) {
                        (Some(a), Some(b)) if a.oid != b.oid => {
                            let what = if matches!(kind, DropKind::Routine) { "routine" } else { "procedure" };
                            return Err(PgError::new(SqlState::AMBIGUOUS_FUNCTION, format!("{what} name \"{qualified}\" is not unique")));
                        }
                        (None, Some(b)) => found = Some(b),
                        _ => {}
                    }
                }
                match found {
                    Some(c) => (c, signature_string(env, &qualified, types)?),
                    None => {
                        if if_exists {
                            let mut names = Vec::with_capacity(types.len());
                            for o in types {
                                names.push(notice_type_name(env, *o)?);
                            }
                            self.notice(skip_notice(format!(
                                "{word} {qualified}({}) does not exist, skipping",
                                names.join(",")
                            )));
                            return Ok(None);
                        }
                        let sig = signature_string(env, &qualified, types)?;
                        return Err(match kind {
                            DropKind::Procedure => {
                                PgError::new(SqlState::UNDEFINED_FUNCTION, format!("procedure {sig} does not exist"))
                            }
                            DropKind::Aggregate if types.is_empty() => PgError::new(
                                SqlState::UNDEFINED_FUNCTION,
                                format!("aggregate {qualified}(*) does not exist"),
                            ),
                            DropKind::Aggregate => {
                                PgError::new(SqlState::UNDEFINED_FUNCTION, format!("aggregate {sig} does not exist"))
                            }
                            _ => errors::function_does_not_exist(&sig),
                        });
                    }
                }
            }
            None => {
                if cands.is_empty() {
                    if if_exists {
                        self.notice(skip_notice(format!("{word} {qualified}() does not exist, skipping")));
                        return Ok(None);
                    }
                    return Err(match kind {
                        DropKind::Aggregate => PgError::new(
                            SqlState::UNDEFINED_FUNCTION,
                            format!("could not find an aggregate named \"{qualified}\""),
                        ),
                        DropKind::Procedure => errors::no_function_named("procedure", &qualified),
                        _ => errors::no_function_named("function", &qualified),
                    });
                }
                if cands.len() > 1 {
                    // PG LookupFuncWithArgs: the bare name of a procedure or routine is "not unique" in its own words.
                    return Err(match kind {
                        DropKind::Procedure | DropKind::Routine => {
                            let what = if matches!(kind, DropKind::Routine) { "routine" } else { "procedure" };
                            PgError::new(SqlState::AMBIGUOUS_FUNCTION, format!("{what} name \"{qualified}\" is not unique"))
                                .with_hint(format!("Specify the argument list to select the {what} unambiguously."))
                        }
                        _ => errors::function_name_not_unique(&qualified),
                    });
                }
                (&cands[0], format!("{qualified}()"))
            }
        };
        check_routine_kind(kind, cand, &qualified, &sig)?;
        if cand.oid < FIRST_NORMAL_OID {
            // Built-in routines are never dropped by a user statement.
            return Err(errors::system_catalog_permission("pg_proc"));
        }
        let addr = ObjectAddress::proc(cand.oid);
        self.require_owner(&addr)?;
        Ok(Some(addr))
    }

    /// DROP SCHEMA (the name is never qualified in PG's grammar).
    fn resolve_schema_target(
        &mut self,
        env: &CatalogEnv,
        t: &DropTarget,
        if_exists: bool,
    ) -> Result<Option<ObjectAddress>> {
        let ns = match env.lookup_namespace(&t.name) {
            Some(ns) => ns,
            None => {
                if if_exists {
                    self.notice(errors::schema_does_not_exist_skipping(&t.name));
                    return Ok(None);
                }
                return Err(errors::schema_does_not_exist(&t.name));
            }
        };
        let addr = ObjectAddress::namespace(ns);
        self.require_owner(&addr)?;
        Ok(Some(addr))
    }

    /// DROP ROLE: resolve every name first, then drop each role (`drop_role` checks the rest).
    fn drop_role_names(&mut self, env: &CatalogEnv, targets: &[DropTarget], if_exists: bool) -> Result<()> {
        let mut oids: Vec<Oid> = Vec::with_capacity(targets.len());
        for t in targets {
            match env.snapshot().role_by_name(&t.name) {
                Some(role) => {
                    if !oids.contains(&role.oid) {
                        oids.push(role.oid);
                    }
                }
                None => {
                    if if_exists {
                        self.notice(errors::role_does_not_exist_skipping(&t.name));
                    } else {
                        return Err(errors::role_does_not_exist(&t.name));
                    }
                }
            }
        }
        for oid in oids {
            self.drop_role(oid)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec};
    use crate::testing;
    use pgarbor_common::MemKv;
    use pgarbor_types::oids;
    use pgarbor_types::TypeId;

    /// Bootstrapped catalog plus one `public.<name>(a int4)` table.
    fn with_table(name: &str) -> Result<(MemKv, CatalogEnv)> {
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
        };
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&spec).map(|_| ()))?;
        Ok((kv, env))
    }

    #[test]
    fn duplicate_targets_collapse() -> Result<()> {
        let (mut kv, env) = with_table("t")?;
        let targets = [DropTarget::simple("t"), DropTarget::simple("t")];
        let (res, _) = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Table, &targets, false, DropBehavior::Restrict)
        })?;
        assert!(res.snapshot.class_in_namespace(PUBLIC_NAMESPACE, "t").is_none());
        Ok(())
    }

    #[test]
    fn missing_table_is_42p01() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Table, &[DropTarget::simple("nosuch")], false, DropBehavior::Restrict)
        })
        .err()
        .expect("missing table must fail");
        assert_eq!(err.sqlstate.code(), "42P01");
        assert_eq!(err.message, "table \"nosuch\" does not exist");
        Ok(())
    }

    #[test]
    fn missing_schema_if_exists_is_notice() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let target = DropTarget { schema: Some("zz_nos".to_string()), name: "a".to_string(), args: None, all_args_lookup: false };
        let (res, _) = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Table, &[target], true, DropBehavior::Restrict)
        })?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "schema \"zz_nos\" does not exist, skipping");
        Ok(())
    }

    #[test]
    fn missing_schema_without_if_exists_is_3f000() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let target = DropTarget { schema: Some("zz_nos".to_string()), name: "a".to_string(), args: None, all_args_lookup: false };
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Table, &[target], false, DropBehavior::Restrict)
        })
        .err()
        .expect("missing schema must fail");
        assert_eq!(err.sqlstate.code(), "3F000");
        assert_eq!(err.message, "schema \"zz_nos\" does not exist");
        Ok(())
    }

    #[test]
    fn missing_index_and_type_are_42704() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Index, &[DropTarget::simple("nosuch")], false, DropBehavior::Restrict)
        })
        .err()
        .expect("missing index must fail");
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "index \"nosuch\" does not exist");
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Type, &[DropTarget::simple("nosuch")], false, DropBehavior::Restrict)
        })
        .err()
        .expect("missing type must fail");
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "type \"nosuch\" does not exist");
        Ok(())
    }

    #[test]
    fn routine_with_args_if_exists_notice_uses_internal_names() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let target = DropTarget { schema: None, name: "nosuch".to_string(), args: Some(vec![oids::INT4]), all_args_lookup: false };
        let (res, _) = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Function, &[target], true, DropBehavior::Restrict)
        })?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "function nosuch(pg_catalog.int4) does not exist, skipping");
        Ok(())
    }

    #[test]
    fn routine_without_args_missing_is_42883() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Function, &[DropTarget::simple("nosuch")], false, DropBehavior::Restrict)
        })
        .err()
        .expect("missing function must fail");
        assert_eq!(err.sqlstate.code(), "42883");
        assert_eq!(err.message, "could not find a function named \"nosuch\"");
        Ok(())
    }

    #[test]
    fn system_relation_is_permission_denied() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let target = DropTarget { schema: Some("pg_catalog".to_string()), name: "pg_class".to_string(), args: None, all_args_lookup: false };
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_by_names(DropKind::Table, &[target], false, DropBehavior::Restrict)
        })
        .err()
        .expect("system relation must not drop");
        assert_eq!(err.sqlstate.code(), "42501");
        assert_eq!(err.message, "permission denied: \"pg_class\" is a system catalog");
        Ok(())
    }
}
