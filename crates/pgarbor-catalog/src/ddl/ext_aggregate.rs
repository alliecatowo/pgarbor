//! CREATE [OR REPLACE] AGGREGATE and aggregate lookup (amendment pgarbor-catalog-09). PG sources:
//! `commands/aggregatecmds.c` (`DefineAggregate`), `catalog/pg_aggregate.c` (`AggregateCreate`,
//! `lookup_agg_function`), `catalog/pg_proc.c` (`ProcedureCreate` replace checks),
//! `parser/parse_func.c` (`LookupFuncWithArgs` for OBJECT_AGGREGATE).
//!
//! The aggregate's `pg_proc` row is a `Function` with `kind = Aggregate`, language `internal`,
//! `prosrc = aggregate_dummy`, immutable, not strict, cost 1; its `pg_aggregate` row is an
//! `ExtObject::Aggregate` keyed by the same OID.
use crate::ddl::ext_operator::{
    def_bool, def_int32, def_qname, def_string, def_typename, func_signature, resolve_tn, INVALID_FUNCTION_DEFINITION,
};
use crate::ddl::ext_spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::cast::lookup::is_binary_coercible;
use pgarbor_types::names::display_name;
use pgarbor_types::oids;
use pgarbor_types::resolve::func::{resolve_function, FuncCall};

const LANG_INTERNAL: Oid = 12;
const BYTEA: Oid = 17;

fn is_polymorphic(t: Oid) -> bool {
    matches!(t, 2283 | 2277 | 2776 | 3500 | 3831 | 4537 | 5077 | 5078 | 5079 | 5080 | 4538)
}

fn typtype(env: &CatalogEnv, t: Oid) -> u8 {
    use pgarbor_types::context::TypeEnv;
    env.type_info(t).map(|i| i.typtype).unwrap_or(0)
}

/// Input argument types of an `AggArgsSpec` (`extractAggrArgTypes`).
pub fn agg_arg_types(env: &CatalogEnv, a: &AggArgsSpec) -> Result<Vec<Oid>> {
    a.args.iter().map(|x| resolve_tn(env, &x.ty)).collect()
}

/// PG `LookupFuncWithArgs(OBJECT_AGGREGATE, ...)`.
pub fn lookup_aggregate(env: &CatalogEnv, agg: &AggRef, missing_ok: bool) -> Result<Option<Oid>> {
    let mut types = Vec::with_capacity(agg.args.args.len());
    for a in &agg.args.args {
        match resolve_tn(env, &a.ty) {
            Ok(t) => types.push(t),
            Err(e) if missing_ok && e.sqlstate == SqlState::UNDEFINED_OBJECT => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    let cands = match env.function_candidates(agg.name.schema.as_deref(), &agg.name.name) {
        Ok(c) => c,
        Err(e) if missing_ok && e.sqlstate == SqlState::INVALID_SCHEMA_NAME => return Ok(None),
        Err(e) => return Err(e),
    };
    let found = cands.iter().filter(|c| c.arg_types == types).min_by_key(|c| c.path_pos);
    match found {
        Some(c) => {
            if c.kind != pgarbor_types::catalog_data::ProcKind::Aggregate {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("function {} is not an aggregate", func_signature(env, &agg.name, &types)),
                ));
            }
            Ok(Some(c.oid))
        }
        None if missing_ok => Ok(None),
        None if types.is_empty() => Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("aggregate {}(*) does not exist", agg.name.display()),
        )),
        None => Err(PgError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("aggregate {} does not exist", func_signature(env, &agg.name, &types)),
        )),
    }
}

/// PG `lookup_agg_function`: returns (function OID, refined return type).
fn lookup_agg_function(env: &CatalogEnv, name: &QName, input: &[Oid], variadic: Oid) -> Result<(Oid, Oid)> {
    let sig = || func_signature(env, name, input);
    let cands = env.function_candidates(name.schema.as_deref(), &name.name)?;
    let call = FuncCall {
        name: name.name.clone(),
        arg_types: input.to_vec(),
        arg_names: vec![None; input.len()],
        variadic_keyword: false,
    };
    let resolved = match resolve_function(env, &call, &cands) {
        Ok(r) => r,
        Err(e) if e.sqlstate == SqlState::UNDEFINED_FUNCTION || e.sqlstate == SqlState::AMBIGUOUS_FUNCTION => {
            return Err(PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function {} does not exist", sig())));
        }
        Err(e) => return Err(e),
    };
    let Some(cand) = cands.iter().find(|c| c.oid == resolved.oid) else {
        return Err(PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function {} does not exist", sig())));
    };
    if cand.kind != pgarbor_types::catalog_data::ProcKind::Function || resolved.nvariadic > 0 || resolved.ndefaults_used > 0 {
        return Err(PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function {} does not exist", sig())));
    }
    if cand.retset {
        return Err(PgError::new(SqlState::DATATYPE_MISMATCH, format!("function {} returns a set", sig())));
    }
    if variadic == oids::ANY && cand.variadic != oids::ANY {
        return Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("function {} must accept VARIADIC ANY to be used in this aggregate", sig()),
        ));
    }
    let declared = &cand.arg_types;
    // enforce_generic_type_consistency(..., allow_poly = true).
    let ret = if is_polymorphic(cand.ret) {
        if input.iter().any(|t| is_polymorphic(*t)) {
            // Polymorphic inputs keep a polymorphic result (resolved per call).
            let pos = declared.iter().position(|d| *d == cand.ret);
            match pos {
                Some(p) => input[p],
                None => cand.ret,
            }
        } else {
            pgarbor_types::resolve::polymorphic::enforce_generic_type_consistency(env, input, declared, cand.ret)?.1
        }
    } else {
        cand.ret
    };
    for (i, t) in input.iter().enumerate() {
        let d = declared.get(i).copied().unwrap_or(0);
        if !is_binary_coercible(env, *t, d)? {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("function {} requires run-time type coercion", func_signature(env, name, declared)),
            ));
        }
    }
    Ok((cand.oid, ret))
}

/// `check_valid_polymorphic_signature` (simplified to the PG 18 rules for aggregates).
fn polymorphic_detail(ret: Oid, args: &[Oid]) -> Option<String> {
    if !is_polymorphic(ret) {
        return None;
    }
    let anycompatible = matches!(ret, 5077 | 5078 | 5079 | 5080 | 4538);
    let ok = if anycompatible {
        args.iter().any(|a| matches!(*a, 5077 | 5078 | 5079 | 5080 | 4538))
    } else {
        args.iter().any(|a| matches!(*a, 2283 | 2277 | 2776 | 3500 | 3831 | 4537))
    };
    if ok {
        None
    } else {
        let name = match ret {
            2283 => "anyelement",
            2277 => "anyarray",
            2776 => "anynonarray",
            3500 => "anyenum",
            3831 => "anyrange",
            4537 => "anymultirange",
            5077 => "anycompatible",
            5078 => "anycompatiblearray",
            5079 => "anycompatiblenonarray",
            5080 => "anycompatiblerange",
            _ => "anycompatiblemultirange",
        };
        Some(if anycompatible {
            format!("A result of type {name} requires at least one input of type anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange.")
        } else {
            format!("A result of type {name} requires at least one input of type anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange.")
        })
    }
}

impl<'a> CatalogTxn<'a> {
    /// CREATE [OR REPLACE] AGGREGATE (`DefineAggregate` + `AggregateCreate`).
    pub fn create_aggregate(
        &mut self,
        or_replace: bool,
        name: &QName,
        args: Option<&AggArgsSpec>,
        defs: &[DefItem],
    ) -> Result<Oid> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;

        let mut kind = b'n';
        let mut num_direct: i32 = 0;
        if let Some(a) = args {
            if a.order_by_at >= 0 {
                kind = b'o';
                num_direct = a.order_by_at;
            }
        }
        let mut transfn: Option<QName> = None;
        let mut finalfn: Option<QName> = None;
        let mut combinefn: Option<QName> = None;
        let mut serialfn: Option<QName> = None;
        let mut deserialfn: Option<QName> = None;
        let mut mtransfn: Option<QName> = None;
        let mut minvtransfn: Option<QName> = None;
        let mut mfinalfn: Option<QName> = None;
        let mut finalextra = false;
        let mut mfinalextra = false;
        let mut finalmodify: u8 = 0;
        let mut mfinalmodify: u8 = 0;
        let mut sortop: Option<QName> = None;
        let mut basetype: Option<pgarbor_types::names::TypeName> = None;
        let mut transtype: Option<pgarbor_types::names::TypeName> = None;
        let mut mtranstype: Option<pgarbor_types::names::TypeName> = None;
        let mut transspace = 0i32;
        let mut mtransspace = 0i32;
        let mut initval: Option<String> = None;
        let mut minitval: Option<String> = None;
        let mut parallel: Option<String> = None;
        let modify = |d: &DefItem| -> Result<u8> {
            let v = def_string(d)?;
            match v.as_str() {
                "read_only" => Ok(b'r'),
                "shareable" => Ok(b's'),
                "read_write" => Ok(b'w'),
                _ => Err(PgError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("parameter \"{}\" must be READ_ONLY, SHAREABLE, or READ_WRITE", d.name),
                )),
            }
        };
        for d in defs {
            match d.name.as_str() {
                "sfunc" | "sfunc1" => transfn = Some(def_qname(d)?),
                "finalfunc" => finalfn = Some(def_qname(d)?),
                "combinefunc" => combinefn = Some(def_qname(d)?),
                "serialfunc" => serialfn = Some(def_qname(d)?),
                "deserialfunc" => deserialfn = Some(def_qname(d)?),
                "msfunc" => mtransfn = Some(def_qname(d)?),
                "minvfunc" => minvtransfn = Some(def_qname(d)?),
                "mfinalfunc" => mfinalfn = Some(def_qname(d)?),
                "finalfunc_extra" => finalextra = def_bool(d)?,
                "mfinalfunc_extra" => mfinalextra = def_bool(d)?,
                "finalfunc_modify" => finalmodify = modify(d)?,
                "mfinalfunc_modify" => mfinalmodify = modify(d)?,
                "sortop" => sortop = Some(def_qname(d)?),
                "basetype" => basetype = Some(def_typename(d)?.0),
                "hypothetical" => {
                    if def_bool(d)? {
                        if kind == b'n' {
                            return Err(PgError::new(
                                INVALID_FUNCTION_DEFINITION,
                                "only ordered-set aggregates can be hypothetical",
                            ));
                        }
                        kind = b'h';
                    }
                }
                "stype" | "stype1" => transtype = Some(def_typename(d)?.0),
                "sspace" => transspace = def_int32(d)?,
                "mstype" => mtranstype = Some(def_typename(d)?.0),
                "msspace" => mtransspace = def_int32(d)?,
                "initcond" | "initcond1" => initval = Some(def_string(d)?),
                "minitcond" => minitval = Some(def_string(d)?),
                "parallel" => parallel = Some(def_string(d)?),
                other => self.notice(
                    PgError::new(SqlState::SYNTAX_ERROR, format!("aggregate attribute \"{other}\" not recognized"))
                        .with_severity(Severity::Warning),
                ),
            }
        }
        let e = |m: &str| Err(PgError::new(INVALID_FUNCTION_DEFINITION, m.to_string()));
        let Some(transtype) = transtype else { return e("aggregate stype must be specified") };
        let Some(transfn) = transfn else { return e("aggregate sfunc must be specified") };
        if mtranstype.is_some() {
            if mtransfn.is_none() {
                return e("aggregate msfunc must be specified when mstype is specified");
            }
            if minvtransfn.is_none() {
                return e("aggregate minvfunc must be specified when mstype is specified");
            }
        } else {
            if mtransfn.is_some() {
                return e("aggregate msfunc must not be specified without mstype");
            }
            if minvtransfn.is_some() {
                return e("aggregate minvfunc must not be specified without mstype");
            }
            if mfinalfn.is_some() {
                return e("aggregate mfinalfunc must not be specified without mstype");
            }
            if mtransspace != 0 {
                return e("aggregate msspace must not be specified without mstype");
            }
            if minitval.is_some() {
                return e("aggregate minitcond must not be specified without mstype");
            }
        }
        if finalmodify == 0 {
            finalmodify = if kind == b'n' { b'r' } else { b'w' };
        }
        if mfinalmodify == 0 {
            mfinalmodify = if kind == b'n' { b'r' } else { b'w' };
        }

        // Input types.
        let mut fargs: Vec<FuncArg> = Vec::new();
        let mut variadic: Oid = 0;
        let arg_types: Vec<Oid> = match args {
            None => {
                let Some(bt) = &basetype else { return e("aggregate input type must be specified") };
                if bt.schema.is_none() && bt.name.eq_ignore_ascii_case("any") && bt.array_dims == 0 {
                    Vec::new()
                } else {
                    let t = resolve_tn(&env, bt)?;
                    fargs.push(FuncArg { name: String::new(), type_oid: t, mode: ArgMode::In, default: None });
                    vec![t]
                }
            }
            Some(a) => {
                if basetype.is_some() {
                    return e("basetype is redundant with aggregate input type specification");
                }
                let mut v = Vec::new();
                for (i, x) in a.args.iter().enumerate() {
                    let t = resolve_tn(&env, &x.ty)?;
                    let mode = if x.mode == b'v' { ArgMode::Variadic } else { ArgMode::In };
                    if mode == ArgMode::Variadic {
                        if i + 1 != a.args.len() {
                            return e("VARIADIC parameter must be the last input parameter");
                        }
                        variadic = if t == oids::ANY {
                            oids::ANY
                        } else {
                            match pgarbor_types::registry::element_type_of(&env, t) {
                                Some(el) => el,
                                None => return e("VARIADIC parameter must be an array"),
                            }
                        };
                    }
                    if t == oids::VOID || typtype(&env, t) == b'p' && t == 2249 {
                        // PG: "SQL function cannot accept shell type" etc. are not reachable here.
                    }
                    fargs.push(FuncArg { name: x.name.clone(), type_oid: t, mode, default: None });
                    v.push(t);
                }
                v
            }
        };
        let num_args = arg_types.len() as i32;

        let trans = resolve_tn(&env, &transtype)?;
        let is_superuser = self.current_is_superuser_ext()?;
        if typtype(&env, trans) == b'p' && !is_polymorphic(trans) && !(trans == oids::INTERNAL && is_superuser) {
            return Err(PgError::new(
                INVALID_FUNCTION_DEFINITION,
                format!("aggregate transition data type cannot be {}", display_name(&env, trans)),
            ));
        }
        match (&serialfn, &deserialfn) {
            (Some(_), Some(_)) => {
                if trans != oids::INTERNAL {
                    return e("serialization functions may be specified only when the aggregate transition data type is internal");
                }
            }
            (None, None) => {}
            _ => return e("must specify both or neither of serialization and deserialization functions"),
        }
        let mtrans = match &mtranstype {
            Some(t) => {
                let m = resolve_tn(&env, t)?;
                if typtype(&env, m) == b'p' && !is_polymorphic(m) && !(m == oids::INTERNAL && is_superuser) {
                    return Err(PgError::new(
                        INVALID_FUNCTION_DEFINITION,
                        format!("aggregate transition data type cannot be {}", display_name(&env, m)),
                    ));
                }
                m
            }
            None => 0,
        };
        // Initial values must be valid input of the transition type.
        let io = pgarbor_types::IoConfig::default();
        let check_init = |v: &Option<String>, t: Oid| -> Result<()> {
            if let Some(s) = v {
                if typtype(&env, t) != b'p' {
                    let ctx = pgarbor_types::context::IoCtx::new(&env, &io);
                    pgarbor_types::io::input(&ctx, pgarbor_types::TypeId::new(t, -1), s)?;
                }
            }
            Ok(())
        };
        check_init(&initval, trans)?;
        if mtrans != 0 {
            check_init(&minitval, mtrans)?;
        }
        let proparallel = match parallel.as_deref() {
            None | Some("unsafe") => b'u',
            Some("safe") => b's',
            Some("restricted") => b'r',
            Some(_) => {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE"))
            }
        };

        // AggregateCreate.
        if num_args > 100 - 1 {
            return Err(PgError::new(SqlState(*b"54023"), "aggregates cannot have more than 99 arguments"));
        }
        if let Some(d) = polymorphic_detail(trans, &arg_types) {
            return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "cannot determine transition data type").with_detail(d));
        }
        if mtrans != 0 {
            if let Some(d) = polymorphic_detail(mtrans, &arg_types) {
                return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "cannot determine transition data type").with_detail(d));
            }
        }
        if kind != b'n' && variadic != 0 && variadic != oids::ANY {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "a variadic ordered-set aggregate must use VARIADIC type ANY",
            ));
        }
        if kind == b'h' && num_direct < num_args {
            let n_agg = num_args - num_direct;
            let nd = num_direct as usize;
            let na = n_agg as usize;
            if variadic != 0 || num_direct < n_agg || arg_types[nd - na..nd] != arg_types[nd..] {
                return e("a hypothetical-set aggregate must have direct arguments matching its aggregated arguments");
            }
        }
        // transfn
        let mut fn_args: Vec<Oid> = vec![trans];
        if kind != b'n' {
            if num_direct < num_args {
                fn_args.extend_from_slice(&arg_types[num_direct as usize..]);
            } else {
                fn_args.push(*arg_types.last().unwrap_or(&oids::ANY));
            }
        } else {
            fn_args.extend_from_slice(&arg_types);
        }
        let (transfn_oid, rettype) = lookup_agg_function(&env, &transfn, &fn_args, variadic)?;
        if rettype != trans {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("return type of transition function {} is not {}", transfn.display(), display_name(&env, trans)),
            ));
        }
        if func_is_strict(&env, transfn_oid) && initval.is_none() {
            let first_ok = match arg_types.first() {
                Some(a) => is_binary_coercible(&env, *a, trans)?,
                None => false,
            };
            if !first_ok {
                return e("must not omit initial value when transition function is strict and transition type is not compatible with input type");
            }
        }
        let mut mtransfn_oid = 0;
        let mut mtrans_strict = false;
        if let Some(m) = &mtransfn {
            let mut a = fn_args.clone();
            a[0] = mtrans;
            let (o, r) = lookup_agg_function(&env, m, &a, variadic)?;
            if r != mtrans {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("return type of transition function {} is not {}", m.display(), display_name(&env, mtrans)),
                ));
            }
            mtrans_strict = func_is_strict(&env, o);
            if mtrans_strict && minitval.is_none() {
                let first_ok = match arg_types.first() {
                    Some(x) => is_binary_coercible(&env, *x, mtrans)?,
                    None => false,
                };
                if !first_ok {
                    return e("must not omit initial value when transition function is strict and transition type is not compatible with input type");
                }
            }
            mtransfn_oid = o;
        }
        let mut minvtransfn_oid = 0;
        if let Some(m) = &minvtransfn {
            let mut a = fn_args.clone();
            a[0] = mtrans;
            let (o, r) = lookup_agg_function(&env, m, &a, variadic)?;
            if r != mtrans {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "return type of inverse transition function {} is not {}",
                        m.display(),
                        display_name(&env, mtrans)
                    ),
                ));
            }
            if func_is_strict(&env, o) != mtrans_strict {
                return e("strictness of aggregate's forward and inverse transition functions must match");
            }
            minvtransfn_oid = o;
        }
        let final_args = |state: Oid, extra: bool| -> (Vec<Oid>, Oid) {
            let mut a = vec![state];
            a.extend_from_slice(&arg_types);
            let n = if extra { num_args + 1 } else { num_direct + 1 };
            a.truncate(n as usize);
            let v = if !extra && num_direct < num_args { 0 } else { variadic };
            (a, v)
        };
        let mut finalfn_oid = 0;
        let finaltype = match &finalfn {
            Some(f) => {
                let (a, v) = final_args(trans, finalextra);
                let (o, r) = lookup_agg_function(&env, f, &a, v)?;
                if finalextra && func_is_strict(&env, o) {
                    return e("final function with extra arguments must not be declared STRICT");
                }
                finalfn_oid = o;
                r
            }
            None => trans,
        };
        let mut combinefn_oid = 0;
        if let Some(c) = &combinefn {
            let (o, r) = lookup_agg_function(&env, c, &[trans, trans], 0)?;
            if r != trans {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("return type of combine function {} is not {}", c.display(), display_name(&env, trans)),
                ));
            }
            if trans == oids::INTERNAL && func_is_strict(&env, o) {
                return e("combine function with transition type internal must not be declared STRICT");
            }
            combinefn_oid = o;
        }
        let mut serialfn_oid = 0;
        if let Some(s) = &serialfn {
            let (o, r) = lookup_agg_function(&env, s, &[oids::INTERNAL], 0)?;
            if r != BYTEA {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("return type of serialization function {} is not bytea", s.display()),
                ));
            }
            serialfn_oid = o;
        }
        let mut deserialfn_oid = 0;
        if let Some(s) = &deserialfn {
            let (o, r) = lookup_agg_function(&env, s, &[BYTEA, oids::INTERNAL], 0)?;
            if r != oids::INTERNAL {
                return Err(PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("return type of deserialization function {} is not internal", s.display()),
                ));
            }
            deserialfn_oid = o;
        }
        if let Some(d) = polymorphic_detail(finaltype, &arg_types) {
            return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot determine result data type").with_detail(d));
        }
        if finaltype == oids::INTERNAL && !arg_types.contains(&oids::INTERNAL) {
            return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "unsafe use of pseudo-type \"internal\"")
                .with_detail("A result of type internal requires at least one input of type internal."));
        }
        let mut mfinalfn_oid = 0;
        if mtrans != 0 {
            let r = match &mfinalfn {
                Some(f) => {
                    let (a, v) = final_args(mtrans, mfinalextra);
                    let (o, r) = lookup_agg_function(&env, f, &a, v)?;
                    if mfinalextra && func_is_strict(&env, o) {
                        return e("final function with extra arguments must not be declared STRICT");
                    }
                    mfinalfn_oid = o;
                    r
                }
                None => mtrans,
            };
            if r != finaltype {
                return Err(PgError::new(
                    INVALID_FUNCTION_DEFINITION,
                    format!(
                        "moving-aggregate implementation returns type {}, but plain implementation returns type {}",
                        display_name(&env, r),
                        display_name(&env, finaltype)
                    ),
                ));
            }
        }
        let mut sortop_oid = 0;
        if let Some(s) = &sortop {
            if num_args != 1 {
                return e("sort operator can only be specified for single-argument aggregates");
            }
            let t = arg_types[0];
            sortop_oid = match env.lookup_operator(s.schema.as_deref(), &s.name, t, t)? {
                Some(o) => o,
                None => {
                    return Err(PgError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("operator does not exist: {}", crate::ddl::ext_operator::op_signature(&env, s, t, t)),
                    ))
                }
            };
        }

        // ProcedureCreate.
        let existing: Option<Function> = self
            .snapshot()?
            .functions_in_namespace(ns, &name.name)
            .into_iter()
            .find(|g| g.in_arg_types() == arg_types)
            .cloned();
        let old_agg = existing.as_ref().and_then(|f| self.data().ext.get(&f.oid).cloned());
        let oid = match &existing {
            Some(old) => {
                if !or_replace {
                    return Err(errors::function_already_exists(&name.name));
                }
                self.require_owner(&ObjectAddress::proc(old.oid))?;
                if old.kind != FuncKind::Aggregate {
                    let detail = match old.kind {
                        FuncKind::Function => "is a function.",
                        FuncKind::Procedure => "is a procedure.",
                        FuncKind::Window => "is a window function.",
                        FuncKind::Aggregate => "is an aggregate function.",
                    };
                    return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, "cannot change routine kind")
                        .with_detail(format!("\"{}\" {detail}", name.name)));
                }
                if old.ret != finaltype || old.retset {
                    let sig = crate::ddl::function::format_signature(&env, old.namespace, &old.name, &old.in_arg_types(), false)?;
                    return Err(PgError::new(INVALID_FUNCTION_DEFINITION, "cannot change return type of existing function")
                        .with_hint(format!("Use DROP AGGREGATE {sig} first.")));
                }
                if let Some(ExtObject::Aggregate(oa)) = &old_agg {
                    if oa.kind != kind {
                        let detail = match oa.kind {
                            b'n' => format!("\"{}\" is an ordinary aggregate function.", name.name),
                            b'o' => format!("\"{}\" is an ordered-set aggregate.", name.name),
                            _ => format!("\"{}\" is a hypothetical-set aggregate.", name.name),
                        };
                        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, "cannot change routine kind").with_detail(detail));
                    }
                    if oa.num_direct_args as i32 != num_direct {
                        return e("cannot change number of direct arguments of an aggregate function");
                    }
                }
                old.oid
            }
            None => self.new_oid()?,
        };
        let func = Function {
            oid,
            name: name.name.clone(),
            namespace: ns,
            owner: existing.as_ref().map(|f| f.owner).unwrap_or(cur),
            kind: FuncKind::Aggregate,
            language: LANG_INTERNAL,
            args: fargs,
            ret: finaltype,
            retset: false,
            volatility: FuncVolatility::Immutable,
            strict: false,
            security_definer: false,
            leakproof: false,
            parallel: proparallel,
            cost: 1.0,
            rows: 0.0,
            body: "aggregate_dummy".to_string(),
            sql_body: None,
            config: Vec::new(),
            acl: existing.as_ref().and_then(|f| f.acl.clone()),
            depends_on: Vec::new(),
        };
        self.put(CatalogObject::Function(func))?;
        let def = AggregateDef {
            fnoid: oid,
            kind,
            num_direct_args: num_direct as i16,
            transfn: transfn_oid,
            finalfn: finalfn_oid,
            combinefn: combinefn_oid,
            serialfn: serialfn_oid,
            deserialfn: deserialfn_oid,
            mtransfn: mtransfn_oid,
            minvtransfn: minvtransfn_oid,
            mfinalfn: mfinalfn_oid,
            finalextra,
            mfinalextra,
            finalmodify,
            mfinalmodify,
            sortop: sortop_oid,
            transtype: trans,
            transspace,
            mtranstype: mtrans,
            mtransspace,
            initval,
            minitval,
        };
        self.put_ext(ExtObject::Aggregate(def))?;
        if existing.is_none() {
            self.record_created(ObjectAddress::proc(oid));
        }
        Ok(oid)
    }
}

/// `func_strict`.
pub fn func_is_strict(env: &CatalogEnv, f: Oid) -> bool {
    if let Some(p) = pgarbor_types::catalog_data::builtin_proc(f) {
        return p.strict;
    }
    env.snapshot().function(f).is_some_and(|f| f.strict)
}

/// The `AggregateDef` of a user aggregate, or `None` for builtin / non-aggregate OIDs.
pub fn user_aggregate(env: &CatalogEnv, fnoid: Oid) -> Option<AggregateDef> {
    env.snapshot().aggregate_def(fnoid).cloned()
}
