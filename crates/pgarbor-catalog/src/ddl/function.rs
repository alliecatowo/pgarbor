//! CREATE [OR REPLACE] FUNCTION / PROCEDURE and ALTER FUNCTION (catalog side). Lease CAT-19. PG
//! sources: `commands/functioncmds.c` (`CreateFunction`, `interpret_function_parameter_list`,
//! `compute_function_attributes`, `AlterFunction`), `catalog/pg_proc.c` (`ProcedureCreate`),
//! `catalog/namespace.c` (`FuncnameGetCandidates`), `parser/parse_func.c`
//! (`LookupFuncWithArgs`, `LookupFuncName`).
//!
//! The catalog stores the definition ([`Function`]); EXECUTING SQL / PL/pgSQL bodies is
//! pgarbor-exec's business (and later work). The binder resolves parameter types, defaults
//! (`FuncArg::default` as `StoredExpr`), the language and the body text; it validates SQL-language
//! bodies by analysis before calling `create_function` (PG `check_function_bodies`).
//!
//! ## `create_function(f, or_replace)` - `f.oid` is ignored (0)
//! Checks, oracle-verified (PG 18.6):
//! - `check_creatable_namespace`, `require_schema_create`; language must be SQL (14) or PL/pgSQL
//!   (13647) - `internal` (12) / `c` (13) -> 42501 `permission denied for language c` for
//!   non-superusers and 0A000 for superusers (no loadable code in PGArbor).
//! - parameters: IN/OUT/INOUT/VARIADIC/TABLE. Duplicate names among parameters of compatible
//!   modes -> 42P13 `parameter name "a" used more than once`; VARIADIC must be the last IN
//!   parameter (42P13 `VARIADIC parameter must be the last input parameter`) and an array type
//!   (42P13 `VARIADIC parameter must be an array`); a parameter with a default followed by one
//!   without -> 42P13 `input parameters after one with a default value must also have defaults`;
//!   OUT parameters cannot have defaults (42P13 `only input parameters can have default values`).
//! - return type derivation (`ProcedureCreate`): no OUT/TABLE params -> `f.ret` as given; exactly one
//!   OUT/INOUT param -> its type (a single TABLE column too, PG `TableFuncTypeName`); two or more
//!   -> `record` (2249); any TABLE column implies `retset`; procedures -> `ret = 2278` (void) unless INOUT params make it `record`. `f.ret`
//!   given with OUT params must match (42P13 `function result type must be integer because of OUT
//!   parameters`).
//! - identity: (namespace, name, IN-argument types incl. INOUT/VARIADIC) - an existing function
//!   with the same identity -> 42723 `errors::function_already_exists(name)` (oracle text:
//!   `function "f" already exists with same argument types`; same for procedure/function clashes
//!   because they share the name space) unless `or_replace`.
//! - **OR REPLACE** keeps OID, owner, ACL, comment, dependents; requires ownership
//!   (`must be owner of function f`); rejects: kind change (42809 `"p" is a procedure` DETAIL
//!   `Use DROP PROCEDURE...`: `cannot change routine kind` 42809 + DETAIL `"p" is a procedure.`),
//!   return type change (42P13 `errors::cannot_change_return_type(signature, detail)`; detail
//!   `Row type defined by OUT parameters is different.` for OUT changes), input parameter
//!   rename (42P13 `errors::cannot_change_param_name(param, signature)`), OUT parameter rename
//!   (`cannot change name of OUT parameter`... same family), removing defaults (42P13 `cannot remove
//!   parameter defaults from existing function` HINT `Use DROP FUNCTION f(integer) first.`),
//!   changing default values' types (42P13 `cannot change data type of existing parameter default value`).
//!   Everything else (volatility, strictness, body, cost, config, security) is replaced.
//! - Stores `Function` with `cost` default 100 (sql/plpgsql), `rows` 1000 for SRFs else 0,
//!   `parallel` 'u', `depends_on` for BEGIN ATOMIC bodies. Dependencies on argument/return/default
//!   types are derived by `depend.rs`.
//!
//! ## ALTER FUNCTION
//! `alter_function_attrs`: volatility, strictness, SECURITY DEFINER/INVOKER, LEAKPROOF, PARALLEL,
//! COST, ROWS (22023 `ROWS is not applicable when function does not return a set`), SET/RESET
//! config (`Function::config`). `rename_function`: new identity must be free (42723
//! `function g(integer) already exists in schema "zz_cat"` - oracle wording for rename clash:
//! `function ff(integer) already exists in schema "zz_cat"`). `set_function_schema`, `alter_function_owner`
//! (+ ACL rewrite). All need ownership.
//!
//! ## Signature formatting
//! [`format_signature`] renders `name(argtype,argtype)` exactly like PG `format_procedure` /
//! `pg_get_function_identity_arguments` + name: types via `pgarbor_types::names::format_type`,
//! separated by `,` WITHOUT space (`g(integer,integer[])`), qualified with the schema only when the
//! function is not visible in the search path (`qualified = true` forces it).
use super::spec::{ExprRewriter, NameChange};
use crate::acl::RoleGraph;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names::format_type;
use pgarbor_types::oids;
use pgarbor_types::text::format::quote_identifier;

/// Builtin language OIDs (`pg_language`).
const LANG_INTERNAL: Oid = 12;
const LANG_C: Oid = 13;
const LANG_SQL: Oid = 14;
const LANG_PLPGSQL: Oid = 13647;

/// New attribute values for ALTER FUNCTION (`None` = unchanged).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FunctionChanges {
    pub volatility: Option<FuncVolatility>,
    pub strict: Option<bool>,
    pub security_definer: Option<bool>,
    pub leakproof: Option<bool>,
    pub parallel: Option<u8>,
    pub cost: Option<f32>,
    pub rows: Option<f32>,
    /// `SET name = value` entries (`name=value`) to add/replace.
    pub set_config: Vec<String>,
    /// `RESET name` names to remove (`RESET ALL` = every entry: pass `reset_all`).
    pub reset_config: Vec<String>,
    pub reset_all: bool,
}

/// `name(argtype,argtype)` (module docs).
// CONTRACT
pub fn format_signature(env: &CatalogEnv, namespace: Oid, name: &str, arg_types: &[Oid], qualified: bool) -> Result<String> {
    let mut s = String::with_capacity(name.len() + 2 + arg_types.len() * 8);
    if qualified || !function_visible(env, namespace, name, arg_types)? {
        let snap = env.snapshot();
        let ns = snap.namespace(namespace).ok_or_else(|| errors::cache_lookup_failed("schema", namespace))?;
        s.push_str(&quote_identifier(&ns.name)?);
        s.push('.');
    }
    s.push_str(&quote_identifier(name)?);
    s.push('(');
    for (i, t) in arg_types.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format_type(env, *t, -1)?);
    }
    s.push(')');
    Ok(s)
}

/// PG `FunctionIsVisible` for a (name, IN-argument types) pair in `namespace`: a persisted match is
/// asked through `CatalogEnv::function_is_visible`; a not-yet-persisted signature follows the same
/// rule (the first matching schema on the search path wins).
fn function_visible(env: &CatalogEnv, namespace: Oid, name: &str, arg_types: &[Oid]) -> Result<bool> {
    let snap = env.snapshot();
    let found = snap
        .functions_in_namespace(namespace, name)
        .into_iter()
        .find(|f| f.in_arg_types().as_slice() == arg_types)
        .map(|f| f.oid);
    if let Some(oid) = found {
        return env.function_is_visible(oid);
    }
    for &schema in env.search_path() {
        if schema == namespace {
            return Ok(true);
        }
        if snap.functions_in_namespace(schema, name).into_iter().any(|f| f.in_arg_types().as_slice() == arg_types) {
            return Ok(false);
        }
    }
    Ok(false)
}

/// PG `funcname_signature_string`: unquoted name, types joined by `, `, no schema (used by the
/// "already exists in schema" messages).
fn funcname_signature(env: &CatalogEnv, name: &str, arg_types: &[Oid]) -> Result<String> {
    let mut s = String::with_capacity(name.len() + 2 + arg_types.len() * 8);
    s.push_str(name);
    s.push('(');
    for (i, t) in arg_types.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format_type(env, *t, -1)?);
    }
    s.push(')');
    Ok(s)
}

fn invalid_definition(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState(*b"42P13"), msg)
}

/// Input-capable modes (PG `isinput`): IN, INOUT, VARIADIC.
fn is_input_mode(m: ArgMode) -> bool {
    matches!(m, ArgMode::In | ArgMode::InOut | ArgMode::Variadic)
}

/// Output-capable modes (PG: not IN and not VARIADIC): OUT, INOUT, TABLE.
fn is_output_mode(m: ArgMode) -> bool {
    matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table)
}

/// `DROP FUNCTION` / `DROP PROCEDURE` / `DROP AGGREGATE` hint word for a routine.
fn drop_command(f: &Function) -> &'static str {
    match f.kind {
        FuncKind::Procedure => "DROP PROCEDURE",
        FuncKind::Aggregate => "DROP AGGREGATE",
        FuncKind::Function | FuncKind::Window => "DROP FUNCTION",
    }
}

/// The result row layout implied by the output parameters (name, type) in declaration order.
fn output_row(args: &[FuncArg]) -> Vec<(&str, Oid)> {
    args.iter().filter(|a| is_output_mode(a.mode)).map(|a| (a.name.as_str(), a.type_oid)).collect()
}

/// `name` part of a stored `name=value` configuration entry.
fn config_name(entry: &str) -> &str {
    entry.split_once('=').map_or(entry, |(k, _)| k)
}

/// 42723 `function f(integer) already exists in schema "s"` for a rename / SET SCHEMA clash.
fn schema_clash_error(env: &CatalogEnv, snap: &CatalogSnapshot, namespace: Oid, name: &str, in_types: &[Oid]) -> Result<PgError> {
    let sig = funcname_signature(env, name, in_types)?;
    let schema = snap.namespace(namespace).map(|n| n.name.clone()).ok_or_else(|| errors::cache_lookup_failed("schema", namespace))?;
    Ok(PgError::new(SqlState::DUPLICATE_FUNCTION, format!("function {sig} already exists in schema \"{schema}\"")))
}

/// PG `interpret_function_parameter_list` checks, in PG's per-parameter order. Returns the required
/// result type when there are output parameters (`None` otherwise): the first output type for one
/// output parameter, `record` for two or more, and `record` for any output parameter of a procedure.
fn check_parameters(env: &CatalogEnv, is_procedure: bool, args: &[FuncArg]) -> Result<Option<Oid>> {
    let mut var_count = 0usize;
    let mut out_count = 0usize;
    let mut first_out: Oid = 0;
    let mut have_defaults = false;
    for (i, a) in args.iter().enumerate() {
        let is_input = is_input_mode(a.mode);
        if is_input && var_count > 0 {
            return Err(invalid_definition("VARIADIC parameter must be the last input parameter"));
        }
        if is_output_mode(a.mode) {
            if is_procedure {
                if var_count > 0 {
                    return Err(invalid_definition("VARIADIC parameter must be the last parameter"));
                }
                first_out = oids::RECORD;
            } else if out_count == 0 {
                first_out = a.type_oid;
            }
            out_count += 1;
        }
        if a.mode == ArgMode::Variadic {
            var_count += 1;
            let polymorphic = matches!(a.type_oid, oids::ANY | oids::ANYARRAY | oids::ANYCOMPATIBLEARRAY);
            let is_array = env.type_info(a.type_oid).is_some_and(|t| t.elem != 0);
            if !polymorphic && !is_array {
                return Err(invalid_definition("VARIADIC parameter must be an array"));
            }
        }
        if !a.name.is_empty() {
            for prev in &args[..i] {
                if prev.name.is_empty() {
                    continue;
                }
                // Pure IN does not conflict with pure OUT / TABLE, in either order.
                let cur_pure_in = matches!(a.mode, ArgMode::In | ArgMode::Variadic);
                let cur_pure_out = matches!(a.mode, ArgMode::Out | ArgMode::Table);
                let prev_pure_in = matches!(prev.mode, ArgMode::In | ArgMode::Variadic);
                let prev_pure_out = matches!(prev.mode, ArgMode::Out | ArgMode::Table);
                if (cur_pure_in && prev_pure_out) || (prev_pure_in && cur_pure_out) {
                    continue;
                }
                if prev.name == a.name {
                    return Err(invalid_definition(format!("parameter name \"{}\" used more than once", a.name)));
                }
            }
        }
        if a.default.is_some() {
            if !is_input {
                return Err(invalid_definition("only input parameters can have default values"));
            }
            have_defaults = true;
        } else {
            if is_input && have_defaults {
                return Err(invalid_definition("input parameters after one with a default value must also have defaults"));
            }
            if is_procedure && have_defaults {
                return Err(invalid_definition("procedure OUT parameters cannot appear after one with a default value"));
            }
        }
    }
    Ok(match out_count {
        0 => None,
        1 => Some(first_out),
        _ => Some(oids::RECORD),
    })
}

/// Rewrites one stored expression when it depends on `target`; returns whether it changed.
fn rewrite_if_depends(
    expr: &mut StoredExpr,
    target: ObjectAddress,
    change: &NameChange,
    rewriter: &dyn ExprRewriter,
    env: &CatalogEnv,
) -> Result<bool> {
    if !expr.depends_on.contains(&target) {
        return Ok(false);
    }
    let rewritten = rewriter.rewrite(expr, change, env)?;
    *expr = rewritten;
    Ok(true)
}

impl<'a> CatalogTxn<'a> {
    /// CREATE [OR REPLACE] FUNCTION/PROCEDURE (module docs). Returns the function OID.
    // CONTRACT
    pub fn create_function(&mut self, f: &Function, or_replace: bool) -> Result<Oid> {
        self.check_creatable_namespace(f.namespace, &f.name)?;
        self.require_schema_create(f.namespace)?;
        let cur_user = self.current_user();
        match f.language {
            LANG_SQL | LANG_PLPGSQL => {}
            LANG_INTERNAL | LANG_C => {
                let lang = if f.language == LANG_INTERNAL { "internal" } else { "c" };
                if !self.is_superuser(cur_user)? {
                    return Err(errors::permission_denied_for("language", lang));
                }
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("language \"{lang}\" functions cannot be loaded: PGArbor has no loadable code"),
                ));
            }
            other => return Err(PgError::internal(format!("cache lookup failed for language {other}"))),
        }

        let env = self.env()?;
        let proc_kind = f.kind == FuncKind::Procedure;
        let required = check_parameters(&env, proc_kind, &f.args)?;
        let has_table = f.args.iter().any(|a| a.mode == ArgMode::Table);
        let (ret, retset) = if proc_kind {
            (required.unwrap_or(oids::VOID), false)
        } else {
            let ret = match required {
                Some(req) => {
                    if f.ret == 0 || f.ret == req {
                        req
                    } else {
                        return Err(invalid_definition(format!(
                            "function result type must be {} because of OUT parameters",
                            format_type(&env, req, -1)?
                        )));
                    }
                }
                None => {
                    if f.ret == 0 {
                        return Err(invalid_definition("function result type must be specified"));
                    }
                    f.ret
                }
            };
            (ret, f.retset || has_table)
        };

        let cost = if f.cost == 0.0 {
            100.0
        } else if f.cost > 0.0 {
            f.cost
        } else {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "COST must be positive"));
        };
        let rows_given = f.rows != 0.0;
        if rows_given && !(f.rows > 0.0) {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "ROWS must be positive"));
        }
        if rows_given && !retset {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "ROWS is not applicable when function does not return a set",
            ));
        }
        let rows = if !retset {
            0.0
        } else if rows_given {
            f.rows
        } else {
            1000.0
        };
        let parallel = match f.parallel {
            0 => b'u',
            p @ (b's' | b'r' | b'u') => p,
            p => return Err(PgError::internal(format!("invalid parallel safety value {p}"))),
        };

        let mut func = f.clone();
        func.ret = ret;
        func.retset = retset;
        func.cost = cost;
        func.rows = rows;
        func.parallel = parallel;

        let in_types = func.in_arg_types();
        let existing: Option<Function> = self
            .snapshot()?
            .functions_in_namespace(func.namespace, &func.name)
            .into_iter()
            .find(|g| g.in_arg_types() == in_types)
            .cloned();

        let oid = match existing {
            Some(old) => {
                if !or_replace {
                    return Err(errors::function_already_exists(&func.name));
                }
                self.require_owner(&ObjectAddress::proc(old.oid))?;
                if old.kind != func.kind {
                    let detail = match old.kind {
                        FuncKind::Aggregate => "is an aggregate function.",
                        FuncKind::Function => "is a function.",
                        FuncKind::Procedure => "is a procedure.",
                        FuncKind::Window => "is a window function.",
                    };
                    return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, "cannot change routine kind")
                        .with_detail(format!("\"{}\" {detail}", func.name)));
                }
                let sig = self.routine_signature(&old)?;
                let hint_err = |msg: &str, detail: Option<&str>| -> PgError {
                    let mut e = invalid_definition(msg).with_hint(format!("Use {} {sig} first.", drop_command(&old)));
                    if let Some(d) = detail {
                        e = e.with_detail(d);
                    }
                    e
                };
                if func.ret != old.ret || func.retset != old.retset {
                    if proc_kind {
                        return Err(hint_err("cannot change whether a procedure has output parameters", None));
                    }
                    return Err(errors::cannot_change_return_type(&sig, None));
                }
                if func.ret == oids::RECORD && output_row(&old.args) != output_row(&func.args) {
                    if proc_kind {
                        return Err(hint_err(
                            "cannot change return type of existing function",
                            Some("Row type defined by OUT parameters is different."),
                        ));
                    }
                    return Err(errors::cannot_change_return_type(
                        &sig,
                        Some("Row type defined by OUT parameters is different."),
                    ));
                }
                let old_in: Vec<&FuncArg> = old.args.iter().filter(|a| is_input_mode(a.mode)).collect();
                let new_in: Vec<&FuncArg> = func.args.iter().filter(|a| is_input_mode(a.mode)).collect();
                for (j, oa) in old_in.iter().enumerate() {
                    if oa.name.is_empty() {
                        continue;
                    }
                    let same = new_in.get(j).is_some_and(|na| na.name == oa.name);
                    if !same {
                        if proc_kind {
                            let msg = format!("cannot change name of input parameter \"{}\"", oa.name);
                            return Err(hint_err(&msg, None));
                        }
                        return Err(errors::cannot_change_param_name(&oa.name, &sig));
                    }
                }
                let old_defaults = old.n_defaults() as usize;
                if old_defaults > 0 {
                    let new_defaults = func.n_defaults() as usize;
                    if new_defaults < old_defaults {
                        return Err(hint_err("cannot remove parameter defaults from existing function", None));
                    }
                    // Defaults always cover the last input parameters; compare the types of the
                    // defaulted parameters pairwise (PG compares the coerced default expressions).
                    let old_base = old_in.len().saturating_sub(old_defaults);
                    let new_base = new_in.len().saturating_sub(old_defaults);
                    for k in 0..old_defaults {
                        if let (Some(o), Some(n)) = (old_in.get(old_base + k), new_in.get(new_base + k)) {
                            if o.type_oid != n.type_oid {
                                return Err(hint_err("cannot change data type of existing parameter default value", None));
                            }
                        }
                    }
                }
                // OID, owner and ACL are kept; everything else is replaced.
                let mut stored = func.clone();
                stored.oid = old.oid;
                stored.owner = old.owner;
                stored.acl = old.acl.clone();
                let oid = old.oid;
                self.put(CatalogObject::Function(stored))?;
                oid
            }
            None => {
                let oid = self.new_oid()?;
                func.oid = oid;
                func.owner = cur_user;
                func.acl = None;
                self.put(CatalogObject::Function(func))?;
                self.record_created(ObjectAddress::proc(oid));
                oid
            }
        };
        Ok(oid)
    }

    /// ALTER FUNCTION/PROCEDURE ... attribute changes.
    // CONTRACT
    pub fn alter_function_attrs(&mut self, func: Oid, changes: &FunctionChanges) -> Result<()> {
        let old = self.function_or_err(func)?;
        self.require_owner(&ObjectAddress::proc(func))?;
        let mut f = old;
        if let Some(c) = changes.cost {
            if !(c > 0.0) {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "COST must be positive"));
            }
            f.cost = c;
        }
        if let Some(r) = changes.rows {
            if !(r > 0.0) {
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "ROWS must be positive"));
            }
            if !f.retset {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "ROWS is not applicable when function does not return a set",
                ));
            }
            f.rows = r;
        }
        if let Some(v) = changes.volatility {
            f.volatility = v;
        }
        if let Some(s) = changes.strict {
            f.strict = s;
        }
        if let Some(s) = changes.security_definer {
            f.security_definer = s;
        }
        if let Some(l) = changes.leakproof {
            f.leakproof = l;
        }
        if let Some(p) = changes.parallel {
            f.parallel = match p {
                b's' | b'r' | b'u' => p,
                other => return Err(PgError::internal(format!("invalid parallel safety value {other}"))),
            };
        }
        if changes.reset_all {
            f.config.clear();
        } else {
            for name in &changes.reset_config {
                f.config.retain(|c| !config_name(c).eq_ignore_ascii_case(name));
            }
        }
        for entry in &changes.set_config {
            let key = config_name(entry);
            match f.config.iter_mut().find(|c| config_name(c).eq_ignore_ascii_case(key)) {
                Some(slot) => *slot = entry.clone(),
                None => f.config.push(entry.clone()),
            }
        }
        self.put(CatalogObject::Function(f))
    }

    /// ALTER FUNCTION ... RENAME TO.
    // CONTRACT
    pub fn rename_function(&mut self, func: Oid, new_name: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        let old = self.function_or_err(func)?;
        self.require_owner(&ObjectAddress::proc(func))?;
        let in_types = old.in_arg_types();
        let snap = self.snapshot()?;
        let clash = snap
            .functions_in_namespace(old.namespace, new_name)
            .into_iter()
            .any(|g| g.in_arg_types() == in_types);
        if clash {
            let env = self.env()?;
            return Err(schema_clash_error(&env, &snap, old.namespace, new_name, &in_types)?);
        }
        let mut renamed = old.clone();
        renamed.name = new_name.to_string();
        self.put(CatalogObject::Function(renamed))?;
        self.rewrite_function_dependents(
            ObjectAddress::proc(func),
            &NameChange::Function { oid: func, old: old.name.clone(), new: new_name.to_string() },
            rewriter,
        )
    }

    /// ALTER FUNCTION ... SET SCHEMA.
    // CONTRACT
    pub fn set_function_schema(&mut self, func: Oid, new_schema: Oid, rewriter: &dyn ExprRewriter) -> Result<()> {
        let old = self.function_or_err(func)?;
        self.require_owner(&ObjectAddress::proc(func))?;
        if old.namespace == new_schema {
            return Ok(());
        }
        self.require_schema_create(new_schema)?;
        self.check_creatable_namespace(new_schema, &old.name)?;
        let in_types = old.in_arg_types();
        let snap = self.snapshot()?;
        let clash = snap
            .functions_in_namespace(new_schema, &old.name)
            .into_iter()
            .any(|g| g.in_arg_types() == in_types);
        if clash {
            let env = self.env()?;
            return Err(schema_clash_error(&env, &snap, new_schema, &old.name, &in_types)?);
        }
        let old_schema_name = snap
            .namespace(old.namespace)
            .map(|n| n.name.clone())
            .ok_or_else(|| errors::cache_lookup_failed("schema", old.namespace))?;
        let new_schema_name = snap
            .namespace(new_schema)
            .map(|n| n.name.clone())
            .ok_or_else(|| errors::cache_lookup_failed("schema", new_schema))?;
        let mut moved = old.clone();
        moved.namespace = new_schema;
        self.put(CatalogObject::Function(moved))?;
        // The function has no schema-specific NameChange: the schema names are passed as old/new.
        self.rewrite_function_dependents(
            ObjectAddress::proc(func),
            &NameChange::Function { oid: func, old: old_schema_name, new: new_schema_name },
            rewriter,
        )
    }

    /// ALTER FUNCTION ... OWNER TO.
    // CONTRACT
    pub fn alter_function_owner(&mut self, func: Oid, new_owner: Oid) -> Result<()> {
        let old = self.function_or_err(func)?;
        self.require_owner(&ObjectAddress::proc(func))?;
        if old.owner == new_owner {
            return Ok(());
        }
        let snap = self.snapshot()?;
        let new_owner_name = snap
            .role(new_owner)
            .map(|r| r.name.clone())
            .ok_or_else(|| errors::cache_lookup_failed("role", new_owner))?;
        let cur = self.current_user();
        if !self.is_superuser(cur)? {
            let env = self.env()?;
            let graph = RoleGraph::build(&snap, env.database())?;
            if !graph.is_member_of_role(cur, new_owner)? {
                return Err(PgError::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("must be member of role \"{new_owner_name}\""),
                ));
            }
        }
        let mut owned = old.clone();
        owned.owner = new_owner;
        owned.acl = match &old.acl {
            Some(a) => Some(crate::acl::aclnewowner(a, old.owner, new_owner)?),
            None => None,
        };
        self.put(CatalogObject::Function(owned))
    }

    /// The persisted function `func`, or XX000 `cache lookup failed`.
    fn function_or_err(&self, func: Oid) -> Result<Function> {
        self.data().functions.get(&func).cloned().ok_or_else(|| errors::cache_lookup_failed("function", func))
    }

    /// `name(argtype,...)` of a persisted routine (for hints): PG `format_procedure`.
    fn routine_signature(&mut self, f: &Function) -> Result<String> {
        let env = self.env()?;
        format_signature(&env, f.namespace, &f.name, &f.in_arg_types(), false)
    }

    /// Rewrites every stored expression that depends on `target` (a function) through `rewriter`:
    /// relation column defaults and generation expressions, view queries, partition key
    /// expressions, index keys and predicates, CHECK constraints, domain defaults, and function
    /// argument defaults and BEGIN ATOMIC bodies. Runs after the renamed object is written, so the
    /// rewriter sees the catalog after the change.
    fn rewrite_function_dependents(
        &mut self,
        target: ObjectAddress,
        change: &NameChange,
        rewriter: &dyn ExprRewriter,
    ) -> Result<()> {
        let env = self.env()?;
        let mut updates: Vec<CatalogObject> = Vec::new();
        let data = self.data();
        for rel in data.relations.values() {
            let mut r = rel.clone();
            let mut hit = false;
            for col in r.columns.iter_mut() {
                if let Some(d) = col.default.as_mut() {
                    hit |= rewrite_if_depends(&mut d.expr, target, change, rewriter, &env)?;
                }
            }
            if let Some(v) = r.view.as_mut() {
                hit |= rewrite_if_depends(&mut v.query, target, change, rewriter, &env)?;
            }
            if let Some(pk) = r.partition_key.as_mut() {
                for e in pk.exprs.iter_mut() {
                    hit |= rewrite_if_depends(e, target, change, rewriter, &env)?;
                }
            }
            if hit {
                updates.push(CatalogObject::Relation(r));
            }
        }
        for idx in data.indexes.values() {
            let mut x = idx.clone();
            let mut hit = false;
            for key in x.keys.iter_mut() {
                if let Some(e) = key.expr.as_mut() {
                    hit |= rewrite_if_depends(e, target, change, rewriter, &env)?;
                }
            }
            if let Some(p) = x.predicate.as_mut() {
                hit |= rewrite_if_depends(p, target, change, rewriter, &env)?;
            }
            if hit {
                updates.push(CatalogObject::Index(x));
            }
        }
        for c in data.constraints.values() {
            let mut x = c.clone();
            let changed = match x.check.as_mut() {
                Some(e) => rewrite_if_depends(e, target, change, rewriter, &env)?,
                None => false,
            };
            if changed {
                updates.push(CatalogObject::Constraint(x));
            }
        }
        for t in data.types.values() {
            let mut x = t.clone();
            let changed = match &mut x.def {
                TypeDef::Domain { default: Some(e), .. } => rewrite_if_depends(e, target, change, rewriter, &env)?,
                _ => false,
            };
            if changed {
                updates.push(CatalogObject::Type(x));
            }
        }
        for g in data.functions.values() {
            let mut x = g.clone();
            let mut hit = false;
            if let Some(e) = x.sql_body.as_mut() {
                hit |= rewrite_if_depends(e, target, change, rewriter, &env)?;
            }
            for a in x.args.iter_mut() {
                if let Some(e) = a.default.as_mut() {
                    hit |= rewrite_if_depends(e, target, change, rewriter, &env)?;
                }
            }
            if hit {
                updates.push(CatalogObject::Function(x));
            }
        }
        for obj in updates {
            self.put(obj)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::PUBLIC_NAMESPACE;
    use crate::testing;
    use pgarbor_types::oids::{INT4, INT4_ARRAY, TEXT};

    fn arg(name: &str, type_oid: Oid, mode: ArgMode) -> FuncArg {
        FuncArg { name: name.to_string(), type_oid, mode, default: None }
    }

    fn sql_fn(name: &str, args: Vec<FuncArg>, ret: Oid) -> Function {
        Function {
            oid: 0,
            name: name.to_string(),
            namespace: PUBLIC_NAMESPACE,
            owner: 10,
            kind: FuncKind::Function,
            language: LANG_SQL,
            args,
            ret,
            retset: false,
            volatility: FuncVolatility::Volatile,
            strict: false,
            security_definer: false,
            leakproof: false,
            parallel: b'u',
            cost: 0.0,
            rows: 0.0,
            body: "select 1".to_string(),
            sql_body: None,
            config: Vec::new(),
            acl: None,
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn signature_has_no_spaces() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let sig = format_signature(&env, PUBLIC_NAMESPACE, "g", &[INT4, INT4_ARRAY], false)?;
        assert_eq!(sig, "g(integer,integer[])");
        Ok(())
    }

    #[test]
    fn duplicate_identity_is_42723() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("f", vec![arg("a", INT4, ArgMode::In)], INT4);
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))
            .err()
            .expect("second CREATE FUNCTION with the same identity must fail");
        assert_eq!(err.sqlstate.code(), "42723");
        assert_eq!(err.message, "function \"f\" already exists with same argument types");
        Ok(())
    }

    #[test]
    fn replace_cannot_change_return_type() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("f", vec![arg("a", INT4, ArgMode::In)], INT4);
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))?;
        let g = sql_fn("f", vec![arg("a", INT4, ArgMode::In)], TEXT);
        let err = testing::run(&mut kv, &env, |t| t.create_function(&g, true).map(|_| ()))
            .err()
            .expect("OR REPLACE with a new return type must fail");
        assert_eq!(err.sqlstate.code(), "42P13");
        assert_eq!(err.message, "cannot change return type of existing function");
        assert_eq!(err.hint.as_deref(), Some("Use DROP FUNCTION f(integer) first."));
        Ok(())
    }

    #[test]
    fn replace_cannot_rename_input_parameter() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("f", vec![arg("a", INT4, ArgMode::In)], INT4);
        let (_res, env) = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))?;
        let g = sql_fn("f", vec![arg("b", INT4, ArgMode::In)], INT4);
        let err = testing::run(&mut kv, &env, |t| t.create_function(&g, true).map(|_| ()))
            .err()
            .expect("OR REPLACE renaming an input parameter must fail");
        assert_eq!(err.sqlstate.code(), "42P13");
        assert_eq!(err.message, "cannot change name of input parameter \"a\"");
        Ok(())
    }

    #[test]
    fn two_out_params_make_record() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("two", vec![arg("x", INT4, ArgMode::Out), arg("y", TEXT, ArgMode::Out)], oids::RECORD);
        let mut oid = 0;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            oid = t.create_function(&f, false)?;
            Ok(())
        })?;
        let stored = res.snapshot.function(oid).expect("created function is in the snapshot");
        assert_eq!(stored.ret, oids::RECORD);
        assert!(!stored.retset);
        Ok(())
    }

    #[test]
    fn table_sets_retset_and_record() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("t", vec![arg("a", INT4, ArgMode::Table), arg("b", TEXT, ArgMode::Table)], oids::RECORD);
        let mut oid = 0;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            oid = t.create_function(&f, false)?;
            Ok(())
        })?;
        let stored = res.snapshot.function(oid).expect("created function is in the snapshot");
        assert_eq!(stored.ret, oids::RECORD);
        assert!(stored.retset);
        assert_eq!(stored.rows, 1000.0);
        Ok(())
    }

    #[test]
    fn procedure_without_out_params_returns_void() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut p = sql_fn("p", vec![arg("a", INT4, ArgMode::In)], oids::VOID);
        p.kind = FuncKind::Procedure;
        let mut oid = 0;
        let (res, _env) = testing::run(&mut kv, &env, |t| {
            oid = t.create_function(&p, false)?;
            Ok(())
        })?;
        let stored = res.snapshot.function(oid).expect("created procedure is in the snapshot");
        assert_eq!(stored.ret, oids::VOID);
        assert_eq!(stored.kind, FuncKind::Procedure);
        Ok(())
    }

    #[test]
    fn variadic_must_be_last_input() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("v", vec![arg("xs", INT4_ARRAY, ArgMode::Variadic), arg("b", INT4, ArgMode::In)], INT4);
        let err = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))
            .err()
            .expect("VARIADIC before another input parameter must fail");
        assert_eq!(err.sqlstate.code(), "42P13");
        assert_eq!(err.message, "VARIADIC parameter must be the last input parameter");
        Ok(())
    }

    #[test]
    fn variadic_must_be_array() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("v", vec![arg("x", INT4, ArgMode::Variadic)], INT4);
        let err = testing::run(&mut kv, &env, |t| t.create_function(&f, false).map(|_| ()))
            .err()
            .expect("VARIADIC over a non-array must fail");
        assert_eq!(err.sqlstate.code(), "42P13");
        assert_eq!(err.message, "VARIADIC parameter must be an array");
        Ok(())
    }

    #[test]
    fn rows_not_applicable_to_scalar_function() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let f = sql_fn("s", vec![arg("a", INT4, ArgMode::In)], INT4);
        let mut oid = 0;
        let (_res, env) = testing::run(&mut kv, &env, |t| {
            oid = t.create_function(&f, false)?;
            Ok(())
        })?;
        let changes = FunctionChanges { rows: Some(5.0), ..Default::default() };
        let err = testing::run(&mut kv, &env, |t| t.alter_function_attrs(oid, &changes))
            .err()
            .expect("ROWS on a non-set function must fail");
        assert_eq!(err.sqlstate.code(), "22023");
        assert_eq!(err.message, "ROWS is not applicable when function does not return a set");
        Ok(())
    }
}
