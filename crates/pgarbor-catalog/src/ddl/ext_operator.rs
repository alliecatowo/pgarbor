//! CREATE / ALTER / DROP OPERATOR (amendment pgarbor-catalog-09) and the option helpers shared by
//! the other DefineStmt-style commands. PG sources: `commands/operatorcmds.c` (`DefineOperator`,
//! `AlterOperator`, `ValidateRestrictionEstimator`, `ValidateJoinEstimator`,
//! `ValidateOperatorReference`), `catalog/pg_operator.c` (`OperatorCreate`, `OperatorShellMake`,
//! `OperatorUpd`, `validOperatorName`, `OperatorValidateParams`), `commands/define.c`
//! (`defGetQualifiedName`, `defGetTypeName`, `defGetBoolean`), `parser/parse_oper.c`
//! (`LookupOperWithArgs`, `op_signature_string`), `parser/parse_func.c` (`LookupFuncName`).
use crate::ddl::ext_spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::ext::OperInfo;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::names::{display_name, resolve_type_name, TypeName};
use pgarbor_types::oids;

/// 42P13 invalid_function_definition.
pub const INVALID_FUNCTION_DEFINITION: SqlState = SqlState(*b"42P13");

// ------------------------------------------------------------------ define.c helpers

/// `defGetQualifiedName`.
pub fn def_qname(d: &DefItem) -> Result<QName> {
    match &d.value {
        None => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires a parameter", d.name))),
        Some(DefVal::Type { qname: Some(q), .. }) => Ok(q.clone()),
        Some(DefVal::Type { tn, .. }) => Ok(QName { schema: tn.schema.clone(), name: tn.name.clone() }),
        Some(DefVal::Oper(q)) => Ok(q.clone()),
        Some(DefVal::Str(s)) => Ok(QName { schema: None, name: s.clone() }),
        Some(DefVal::Number(_)) => {
            Err(PgError::new(SqlState::SYNTAX_ERROR, format!("argument of {} must be a name", d.name)))
        }
    }
}

/// `defGetTypeName` (with the SETOF flag).
pub fn def_typename(d: &DefItem) -> Result<(TypeName, bool)> {
    match &d.value {
        None => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires a parameter", d.name))),
        Some(DefVal::Type { tn, setof, .. }) => Ok((tn.clone(), *setof)),
        Some(DefVal::Str(s)) => Ok((TypeName { name: s.clone(), quoted: true, ..Default::default() }, false)),
        Some(_) => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("argument of {} must be a type name", d.name))),
    }
}

/// `defGetBoolean`.
pub fn def_bool(d: &DefItem) -> Result<bool> {
    let bad = || PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires a Boolean value", d.name));
    match &d.value {
        None => Ok(true),
        Some(DefVal::Number(n)) => match n.as_str() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(bad()),
        },
        Some(DefVal::Str(s)) => parse_bool_word(s).ok_or_else(bad),
        Some(DefVal::Type { tn, .. }) if tn.schema.is_none() && tn.typmods.is_empty() && tn.array_dims == 0 => {
            parse_bool_word(&tn.name).ok_or_else(bad)
        }
        Some(_) => Err(bad()),
    }
}

fn parse_bool_word(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" => Some(true),
        "false" | "off" => Some(false),
        _ => None,
    }
}

/// `defGetString`.
pub fn def_string(d: &DefItem) -> Result<String> {
    match &d.value {
        None => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires a parameter", d.name))),
        Some(DefVal::Str(s)) | Some(DefVal::Number(s)) => Ok(s.clone()),
        Some(DefVal::Type { tn, qname, .. }) => Ok(match qname {
            Some(q) => q.display(),
            None => tn.name.clone(),
        }),
        Some(DefVal::Oper(q)) => Ok(q.display()),
    }
}

/// `defGetInt32`.
pub fn def_int32(d: &DefItem) -> Result<i32> {
    match &d.value {
        None => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires an integer value", d.name))),
        Some(DefVal::Number(n)) => n
            .parse::<i32>()
            .map_err(|_| PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires an integer value", d.name))),
        Some(_) => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("{} requires an integer value", d.name))),
    }
}

/// `typenameTypeId(NULL, tn)`.
pub fn resolve_tn(env: &CatalogEnv, tn: &TypeName) -> Result<Oid> {
    resolve_type_name(env, tn).map(|t| t.oid)
}

/// PG `func_signature_string`: `name(type, type)`.
pub fn func_signature(env: &CatalogEnv, name: &QName, args: &[Oid]) -> String {
    let types: Vec<String> = args.iter().map(|t| display_name(env, *t)).collect();
    format!("{}({})", name.display(), types.join(", "))
}

/// PG `LookupFuncName(names, nargs, argtypes, missing_ok)`: exact argument types; 42883
/// `function f(integer) does not exist` (3F000 for a missing schema). With `missing_ok` a missing
/// function (but not a missing schema) is `Ok(None)`.
pub fn lookup_func_exact(env: &CatalogEnv, name: &QName, args: &[Oid], missing_ok: bool) -> Result<Option<Oid>> {
    let cands = env.function_candidates(name.schema.as_deref(), &name.name)?;
    if let Some(c) = cands.iter().find(|c| c.arg_types.as_slice() == args && c.kind != pgarbor_types::catalog_data::ProcKind::Procedure) {
        return Ok(Some(c.oid));
    }
    if missing_ok {
        return Ok(None);
    }
    Err(errors::function_does_not_exist(&func_signature(env, name, args)))
}

/// Return type of a builtin or user function.
pub fn func_rettype(env: &CatalogEnv, f: Oid) -> Oid {
    if let Some(p) = pgarbor_types::catalog_data::builtin_proc(f) {
        return p.ret;
    }
    env.snapshot().function(f).map(|f| f.ret).unwrap_or(0)
}

/// PG `op_signature_string`.
pub fn op_signature(env: &CatalogEnv, name: &QName, left: Oid, right: Oid) -> String {
    let mut s = String::new();
    if left != 0 {
        s.push_str(&display_name(env, left));
        s.push(' ');
    }
    s.push_str(&name.display());
    if right != 0 {
        s.push(' ');
        s.push_str(&display_name(env, right));
    }
    s
}

/// PG `validOperatorName`.
pub fn valid_operator_name(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len >= 64 {
        return false;
    }
    if !name.chars().all(|c| "~!@#^&|`?+-*/%<>=".contains(c)) {
        return false;
    }
    if name.contains("/*") || name.contains("--") {
        return false;
    }
    let b = name.as_bytes();
    if len > 1 && (b[len - 1] == b'+' || b[len - 1] == b'-') && !name[..len - 1].chars().any(|c| "~!@#^&|`?%".contains(c)) {
        return false;
    }
    name != "!="
}

/// The two operand types of an operator reference.
fn oper_ref_types(env: &CatalogEnv, op: &OperRef) -> Result<(Oid, Oid)> {
    let l = match &op.left {
        Some(t) => resolve_tn(env, t)?,
        None => 0,
    };
    let r = match &op.right {
        Some(t) => resolve_tn(env, t)?,
        None => 0,
    };
    Ok((l, r))
}

/// PG `LookupOperWithArgs(oper, missing_ok)`: postfix forms are rejected (42601 `postfix operators
/// are not supported`), a missing operator is 42883 `operator does not exist: integer ### bigint`.
pub fn lookup_oper_with_args(env: &CatalogEnv, op: &OperRef, missing_ok: bool) -> Result<Option<Oid>> {
    let (l, r) = oper_ref_types(env, op)?;
    if r == 0 {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "postfix operators are not supported"));
    }
    let found = if missing_ok {
        match env.lookup_operator(op.name.schema.as_deref(), &op.name.name, l, r) {
            Ok(f) => f,
            Err(e) if e.sqlstate == SqlState::INVALID_SCHEMA_NAME => None,
            Err(e) => return Err(e),
        }
    } else {
        env.lookup_operator(op.name.schema.as_deref(), &op.name.name, l, r)?
    };
    match found {
        Some(o) => Ok(Some(o)),
        None if missing_ok => Ok(None),
        None => Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("operator does not exist: {}", op_signature(env, &op.name, l, r)),
        )),
    }
}

/// `OperatorValidateParams`.
fn validate_params(
    left: Oid,
    right: Oid,
    result: Oid,
    commutator: bool,
    negator: bool,
    restrict: bool,
    join: bool,
    merges: bool,
    hashes: bool,
) -> Result<()> {
    let e = |m: &str| Err(PgError::new(INVALID_FUNCTION_DEFINITION, m));
    if !(left != 0 && right != 0) {
        if commutator {
            return e("only binary operators can have commutators");
        }
        if join {
            return e("only binary operators can have join selectivity");
        }
        if merges {
            return e("only binary operators can merge join");
        }
        if hashes {
            return e("only binary operators can hash");
        }
    }
    if result != oids::BOOL {
        if negator {
            return e("only boolean operators can have negators");
        }
        if restrict {
            return e("only boolean operators can have restriction selectivity");
        }
        if join {
            return e("only boolean operators can have join selectivity");
        }
        if merges {
            return e("only boolean operators can merge join");
        }
        if hashes {
            return e("only boolean operators can hash");
        }
    }
    Ok(())
}

impl<'a> CatalogTxn<'a> {
    /// `ValidateRestrictionEstimator`.
    fn restriction_estimator(&mut self, name: &QName) -> Result<Oid> {
        let env = self.env()?;
        let types = [oids::INTERNAL, oids::OID, oids::INTERNAL, oids::INT4];
        let f = lookup_func_exact(&env, name, &types, false)?.unwrap_or(0);
        if func_rettype(&env, f) != oids::FLOAT8 {
            return Err(PgError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("restriction estimator function {} must return type float8", name.display()),
            ));
        }
        if f >= pgarbor_common::FIRST_NORMAL_OID && !self.current_is_superuser_ext()? {
            return Err(PgError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to specify a non-built-in restriction estimator function",
            ));
        }
        Ok(f)
    }

    /// `ValidateJoinEstimator`.
    fn join_estimator(&mut self, name: &QName) -> Result<Oid> {
        let env = self.env()?;
        let t5 = [oids::INTERNAL, oids::OID, oids::INTERNAL, oids::INT2, oids::INTERNAL];
        let f5 = lookup_func_exact(&env, name, &t5, true)?;
        let f4 = lookup_func_exact(&env, name, &t5[..4], true)?;
        let f = match (f5, f4) {
            (Some(_), Some(_)) => {
                return Err(PgError::new(
                    SqlState::AMBIGUOUS_FUNCTION,
                    format!("join estimator function {} has multiple matches", name.display()),
                ))
            }
            (Some(f), None) | (None, Some(f)) => f,
            (None, None) => lookup_func_exact(&env, name, &t5, false)?.unwrap_or(0),
        };
        if func_rettype(&env, f) != oids::FLOAT8 {
            return Err(PgError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("join estimator function {} must return type float8", name.display()),
            ));
        }
        if f >= pgarbor_common::FIRST_NORMAL_OID && !self.current_is_superuser_ext()? {
            return Err(PgError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to specify a non-built-in join estimator function",
            ));
        }
        Ok(f)
    }

    /// Ownership of an operator by the current user (`object_ownercheck`); 42501 `must be owner of
    /// operator <name>`.
    fn require_operator_owner(&mut self, oid: Oid, shown: &str) -> Result<()> {
        let addr = ObjectAddress::new(ObjClass::Operator, oid);
        match self.require_owner(&addr) {
            Ok(()) => Ok(()),
            Err(e) if e.sqlstate == SqlState::INSUFFICIENT_PRIVILEGE => Err(errors::must_be_owner("operator", shown)),
            Err(e) => Err(e),
        }
    }

    /// CREATE OPERATOR (`DefineOperator` + `OperatorCreate`).
    pub fn create_operator(&mut self, name: &QName, defs: &[DefItem]) -> Result<Oid> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;

        let mut can_merge = false;
        let mut can_hash = false;
        let mut func: Option<QName> = None;
        let mut t1: Option<TypeName> = None;
        let mut t2: Option<TypeName> = None;
        let mut commutator: Option<QName> = None;
        let mut negator: Option<QName> = None;
        let mut restrict: Option<QName> = None;
        let mut join: Option<QName> = None;
        for d in defs {
            match d.name.as_str() {
                "leftarg" | "rightarg" => {
                    let (tn, setof) = def_typename(d)?;
                    if setof {
                        return Err(PgError::new(
                            INVALID_FUNCTION_DEFINITION,
                            "SETOF type not allowed for operator argument",
                        ));
                    }
                    if d.name == "leftarg" {
                        t1 = Some(tn);
                    } else {
                        t2 = Some(tn);
                    }
                }
                "function" | "procedure" => func = Some(def_qname(d)?),
                "commutator" => commutator = Some(def_qname(d)?),
                "negator" => negator = Some(def_qname(d)?),
                "restrict" => restrict = Some(def_qname(d)?),
                "join" => join = Some(def_qname(d)?),
                "hashes" => can_hash = def_bool(d)?,
                "merges" => can_merge = def_bool(d)?,
                "sort1" | "sort2" | "ltcmp" | "gtcmp" => can_merge = true,
                other => self.notice(
                    PgError::new(SqlState::SYNTAX_ERROR, format!("operator attribute \"{other}\" not recognized"))
                        .with_severity(Severity::Warning),
                ),
            }
        }
        let Some(func) = func else {
            return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "operator function must be specified"));
        };
        let left = match &t1 {
            Some(t) => resolve_tn(&env, t)?,
            None => 0,
        };
        let right = match &t2 {
            Some(t) => resolve_tn(&env, t)?,
            None => 0,
        };
        if left == 0 && right == 0 {
            return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "operator argument types must be specified"));
        }
        if right == 0 {
            return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "operator right argument type must be specified")
                .with_detail("Postfix operators are not supported."));
        }
        let fargs: Vec<Oid> = if left == 0 { vec![right] } else { vec![left, right] };
        let proc = lookup_func_exact(&env, &func, &fargs, false)?.unwrap_or(0);
        let restrict_oid = match &restrict {
            Some(r) => self.restriction_estimator(r)?,
            None => 0,
        };
        let join_oid = match &join {
            Some(j) => self.join_estimator(j)?,
            None => 0,
        };

        // OperatorCreate.
        if !crate::ddl::ext_operator::valid_operator_name(&name.name) {
            return Err(PgError::new(SqlState::INVALID_NAME, format!("\"{}\" is not a valid operator name", name.name)));
        }
        let env = self.env()?;
        let result = func_rettype(&env, proc);
        validate_params(
            left,
            right,
            result,
            commutator.is_some(),
            negator.is_some(),
            restrict_oid != 0,
            join_oid != 0,
            can_merge,
            can_hash,
        )?;
        let ns_name = env.snapshot().namespace(ns).map(|n| n.name.clone()).unwrap_or_default();
        let existing = env.lookup_operator_opt(Some(&ns_name), &name.name, left, right);
        let existing_info = existing.and_then(|o| env.operator_info(o));
        if let Some(info) = &existing_info {
            if info.proc != 0 {
                return Err(PgError::new(SqlState::DUPLICATE_FUNCTION, format!("operator {} already exists", name.name)));
            }
            self.require_operator_owner(info.oid, &name.name)?;
        }
        let oid = match &existing_info {
            Some(i) => i.oid,
            None => self.new_oid()?,
        };

        let mut self_commutator = false;
        let commutator_oid = match &commutator {
            Some(c) => {
                let o = self.other_operator(c, right, left, &name.name, ns, left, right)?;
                if o != 0 {
                    let shown = c.display();
                    self.require_operator_owner(o, &shown)?;
                } else {
                    self_commutator = true;
                }
                o
            }
            None => 0,
        };
        let negator_oid = match &negator {
            Some(n) => {
                let o = self.other_operator(n, left, right, &name.name, ns, left, right)?;
                if o != 0 {
                    let shown = n.display();
                    self.require_operator_owner(o, &shown)?;
                }
                if o == 0 || o == oid && existing_info.is_some() {
                    return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "operator cannot be its own negator"));
                }
                o
            }
            None => 0,
        };
        let op = Operator {
            oid,
            name: name.name.clone(),
            namespace: ns,
            owner: cur,
            kind: if left != 0 { b'b' } else { b'l' },
            can_merge,
            can_hash,
            left,
            right,
            result,
            commutator: if self_commutator { oid } else { commutator_oid },
            negator: negator_oid,
            proc,
            restrict: restrict_oid,
            join: join_oid,
        };
        self.put_ext(ExtObject::Operator(op))?;
        if existing_info.is_none() {
            self.record_created(ObjectAddress::new(ObjClass::Operator, oid));
        }
        let comm = if self_commutator { oid } else { commutator_oid };
        if comm != 0 || negator_oid != 0 {
            self.operator_upd(oid, comm, negator_oid)?;
        }
        Ok(oid)
    }

    /// `get_other_operator`: an existing operator, 0 for a self-link, or a new shell.
    fn other_operator(
        &mut self,
        other: &QName,
        other_left: Oid,
        other_right: Oid,
        op_name: &str,
        op_ns: Oid,
        left: Oid,
        right: Oid,
    ) -> Result<Oid> {
        let env = self.env()?;
        if let Some(o) = env.lookup_operator(other.schema.as_deref(), &other.name, other_left, other_right)? {
            return Ok(o);
        }
        let other_ns = env.creation_namespace_for(other.schema.as_deref(), &other.name)?;
        if other.name == op_name && other_ns == op_ns && other_left == left && other_right == right {
            return Ok(0);
        }
        let cur = self.current_user();
        self.require_schema_create_for(cur, other_ns)?;
        if !valid_operator_name(&other.name) {
            return Err(PgError::new(SqlState::INVALID_NAME, format!("\"{}\" is not a valid operator name", other.name)));
        }
        let oid = self.new_oid()?;
        let shell = Operator {
            oid,
            name: other.name.clone(),
            namespace: other_ns,
            owner: cur,
            kind: if other_left != 0 { b'b' } else { b'l' },
            can_merge: false,
            can_hash: false,
            left: other_left,
            right: other_right,
            result: 0,
            commutator: 0,
            negator: 0,
            proc: 0,
            restrict: 0,
            join: 0,
        };
        self.put_ext(ExtObject::Operator(shell))?;
        self.record_created(ObjectAddress::new(ObjClass::Operator, oid));
        Ok(oid)
    }

    /// `OperatorUpd(base, comm, neg, false)`: point the partners back at `base`.
    fn operator_upd(&mut self, base: Oid, comm: Oid, neg: Oid) -> Result<()> {
        let env = self.env()?;
        for (other, is_comm) in [(comm, true), (neg, false)] {
            if other == 0 {
                continue;
            }
            let Some(info) = env.operator_info(other) else { continue };
            let current = if is_comm { info.commutator } else { info.negator };
            let refreshed = self.snapshot()?.user_operator(other).cloned();
            let current = refreshed.as_ref().map(|o| if is_comm { o.commutator } else { o.negator }).unwrap_or(current);
            if current == base {
                continue;
            }
            if current != 0 {
                let third = env.operator_info(current).map(|o| o.name).unwrap_or_else(|| current.to_string());
                let (what, which) = if is_comm { ("commutator", "commutator") } else { ("negator", "negator") };
                return Err(PgError::new(
                    INVALID_FUNCTION_DEFINITION,
                    format!("{what} operator {} is already the {which} of operator {third}", info.name),
                ));
            }
            match refreshed {
                Some(mut o) => {
                    if is_comm {
                        o.commutator = base;
                    } else {
                        o.negator = base;
                    }
                    self.put_ext(ExtObject::Operator(o))?;
                }
                None => {
                    return Err(PgError::unimplemented(
                        "CAT-EXT-01",
                        "linking a user operator as commutator or negator of a built-in operator",
                    ))
                }
            }
        }
        Ok(())
    }

    /// ALTER OPERATOR ... SET (...) (`AlterOperator`).
    pub fn alter_operator_set(&mut self, op_ref: &OperRef, defs: &[DefItem]) -> Result<()> {
        let env = self.env()?;
        let oid = lookup_oper_with_args(&env, op_ref, false)?.unwrap_or(0);
        let info = env.operator_info(oid).ok_or_else(|| errors::cache_lookup_failed("operator", oid))?;
        let mut restrict: Option<Option<QName>> = None;
        let mut join: Option<Option<QName>> = None;
        let mut commutator: Option<QName> = None;
        let mut negator: Option<QName> = None;
        let mut merges: Option<bool> = None;
        let mut hashes: Option<bool> = None;
        for d in defs {
            let param = if d.value.is_none() { None } else { Some(def_qname(d)?) };
            match d.name.as_str() {
                "restrict" => restrict = Some(param),
                "join" => join = Some(param),
                "commutator" => commutator = Some(def_qname(d)?),
                "negator" => negator = Some(def_qname(d)?),
                "merges" => merges = Some(def_bool(d)?),
                "hashes" => hashes = Some(def_bool(d)?),
                "leftarg" | "rightarg" | "function" | "procedure" => {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("operator attribute \"{}\" cannot be changed", d.name),
                    ))
                }
                other => {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, format!("operator attribute \"{other}\" not recognized")))
                }
            }
        }
        self.require_operator_owner(oid, &info.name)?;
        let restrict_oid = match &restrict {
            Some(Some(r)) => self.restriction_estimator(r)?,
            _ => 0,
        };
        let join_oid = match &join {
            Some(Some(j)) => self.join_estimator(j)?,
            _ => 0,
        };
        let commutator_oid = match &commutator {
            Some(c) => self.validate_operator_reference(c, info.right, info.left)?,
            None => 0,
        };
        let negator_oid = match &negator {
            Some(n) => {
                let o = self.validate_operator_reference(n, info.left, info.right)?;
                if o == info.oid {
                    return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "operator cannot be its own negator"));
                }
                o
            }
            None => 0,
        };
        let already = |attr: &str| {
            Err(PgError::new(
                INVALID_FUNCTION_DEFINITION,
                format!("operator attribute \"{attr}\" cannot be changed if it has already been set"),
            ))
        };
        if commutator_oid != 0 && info.commutator != 0 && commutator_oid != info.commutator {
            return already("commutator");
        }
        if negator_oid != 0 && info.negator != 0 && negator_oid != info.negator {
            return already("negator");
        }
        if merges == Some(false) && info.can_merge {
            return already("merges");
        }
        if hashes == Some(false) && info.can_hash {
            return already("hashes");
        }
        validate_params(
            info.left,
            info.right,
            info.result,
            commutator_oid != 0,
            negator_oid != 0,
            restrict_oid != 0,
            join_oid != 0,
            merges.unwrap_or(false),
            hashes.unwrap_or(false),
        )?;
        let Some(mut op) = self.snapshot()?.user_operator(oid).cloned() else {
            return Err(PgError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{}\" is a system catalog", info.name),
            ));
        };
        if restrict.is_some() {
            op.restrict = restrict_oid;
        }
        if join.is_some() {
            op.join = join_oid;
        }
        if commutator_oid != 0 {
            op.commutator = commutator_oid;
        }
        if negator_oid != 0 {
            op.negator = negator_oid;
        }
        if let Some(m) = merges {
            op.can_merge = m;
        }
        if let Some(h) = hashes {
            op.can_hash = h;
        }
        self.put_ext(ExtObject::Operator(op))?;
        if commutator_oid != 0 || negator_oid != 0 {
            self.operator_upd(oid, commutator_oid, negator_oid)?;
        }
        Ok(())
    }

    /// `ValidateOperatorReference`.
    fn validate_operator_reference(&mut self, name: &QName, left: Oid, right: Oid) -> Result<Oid> {
        let env = self.env()?;
        let found = env.lookup_operator(name.schema.as_deref(), &name.name, left, right)?;
        let Some(oid) = found else {
            return Err(PgError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("operator does not exist: {}", op_signature(&env, name, left, right)),
            ));
        };
        if env.operator_info(oid).is_some_and(|o| o.proc == 0) {
            return Err(PgError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("operator is only a shell: {}", op_signature(&env, name, left, right)),
            ));
        }
        let shown = name.display();
        self.require_operator_owner(oid, &shown)?;
        Ok(oid)
    }

    /// ALTER OPERATOR ... OWNER TO.
    pub fn alter_operator_owner(&mut self, op_ref: &OperRef, owner: Oid) -> Result<()> {
        let env = self.env()?;
        let oid = lookup_oper_with_args(&env, op_ref, false)?.unwrap_or(0);
        self.alter_ext_owner(&ObjectAddress::new(ObjClass::Operator, oid), owner)
    }

    /// ALTER OPERATOR ... SET SCHEMA.
    pub fn alter_operator_schema(&mut self, op_ref: &OperRef, schema: &str) -> Result<()> {
        let env = self.env()?;
        let oid = lookup_oper_with_args(&env, op_ref, false)?.unwrap_or(0);
        let ns = env.namespace_oid(schema)?;
        self.set_ext_schema(&ObjectAddress::new(ObjClass::Operator, oid), ns)
    }

    /// DROP OPERATOR (`RemoveObjects`): missing operators error, or with IF EXISTS give the NOTICE
    /// `operator ### does not exist, skipping` (`schema "s" does not exist, skipping`).
    pub fn drop_operators(&mut self, if_exists: bool, ops: &[OperRef], cascade: bool) -> Result<()> {
        let env = self.env()?;
        let mut targets = Vec::new();
        for op in ops {
            match lookup_oper_with_args(&env, op, if_exists)? {
                Some(oid) => {
                    let info = env.operator_info(oid).ok_or_else(|| errors::cache_lookup_failed("operator", oid))?;
                    let addr = ObjectAddress::new(ObjClass::Operator, oid);
                    if !addr.is_pinned() {
                        self.require_operator_owner(oid, &info.name)?;
                    }
                    if !targets.contains(&addr) {
                        targets.push(addr);
                    }
                }
                None => {
                    if let Some(s) = &op.name.schema {
                        if env.lookup_namespace(s).is_none() {
                            self.notice(PgError::notice(format!("schema \"{s}\" does not exist, skipping")));
                            continue;
                        }
                    }
                    self.notice(PgError::notice(format!("operator {} does not exist, skipping", op.name.display())));
                }
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        let behavior = if cascade { DropBehavior::Cascade } else { DropBehavior::Restrict };
        self.drop_objects(&targets, behavior)
    }
}

/// Operator display for `OperInfo` users outside this file.
pub fn operator_name_of(info: &OperInfo) -> &str {
    &info.name
}
