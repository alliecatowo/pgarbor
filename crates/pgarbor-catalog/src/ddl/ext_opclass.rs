//! CREATE/ALTER/DROP OPERATOR FAMILY and OPERATOR CLASS (amendment pgarbor-catalog-09). PG sources:
//! `commands/opclasscmds.c` (`DefineOpClass`, `DefineOpFamily`, `AlterOpFamily`,
//! `AlterOpFamilyAdd`, `AlterOpFamilyDrop`, `assignOperTypes`, `assignProcTypes`,
//! `addFamilyMember`, `storeOperators`, `storeProcedures`, `dropOperators`, `dropProcedures`,
//! `processTypesSpec`), `access/nbtree/nbtvalidate.c` (`btadjustmembers`),
//! `access/hash/hashvalidate.c` (`hashadjustmembers`).
use crate::ddl::ext_operator::{lookup_func_exact, lookup_oper_with_args, op_signature, resolve_tn};
use crate::ddl::ext_spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::ext::{access_method_does_not_exist, opclass_does_not_exist, opfamily_does_not_exist};
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::names::display_name;
use pgarbor_types::oids;

const BTREE: Oid = 403;
const HASH: Oid = 405;
const INT8: Oid = 20;

/// Index AM properties used by the opclass commands (PG `IndexAmRoutine`).
struct AmProps {
    strategies: i32,
    support: i32,
    opts_proc: i32,
    can_order: bool,
    can_order_by_op: bool,
    can_hash: bool,
    storage: bool,
}

fn am_props(am: Oid) -> Option<AmProps> {
    Some(match am {
        403 => AmProps { strategies: 5, support: 6, opts_proc: 5, can_order: true, can_order_by_op: false, can_hash: false, storage: false },
        405 => AmProps { strategies: 1, support: 3, opts_proc: 3, can_order: false, can_order_by_op: false, can_hash: true, storage: false },
        783 => AmProps { strategies: 0, support: 12, opts_proc: 10, can_order: false, can_order_by_op: true, can_hash: false, storage: true },
        2742 => AmProps { strategies: 0, support: 7, opts_proc: 7, can_order: false, can_order_by_op: false, can_hash: false, storage: true },
        3580 => AmProps { strategies: 0, support: 15, opts_proc: 5, can_order: false, can_order_by_op: false, can_hash: false, storage: true },
        4000 => AmProps { strategies: 0, support: 7, opts_proc: 7, can_order: false, can_order_by_op: true, can_hash: false, storage: true },
        _ => return None,
    })
}

/// One pending pg_amop / pg_amproc member (PG `OpFamilyMember`).
#[derive(Clone, Debug)]
struct Member {
    is_func: bool,
    object: Oid,
    number: i32,
    lefttype: Oid,
    righttype: Oid,
    sortfamily: Oid,
    /// Hard (internal) dependency on this opclass; 0 = soft dependency on the family.
    opclass: Oid,
}

fn invalid_def(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::INVALID_OBJECT_DEFINITION, msg)
}

fn type_name(env: &CatalogEnv, t: Oid) -> String {
    display_name(env, t)
}

/// `addFamilyMember`.
fn add_member(env: &CatalogEnv, list: &mut Vec<Member>, m: Member) -> Result<()> {
    if list.iter().any(|o| o.number == m.number && o.lefttype == m.lefttype && o.righttype == m.righttype) {
        let what = if m.is_func { "function" } else { "operator" };
        return Err(invalid_def(format!(
            "{what} number {} for ({},{}) appears more than once",
            m.number,
            type_name(env, m.lefttype),
            type_name(env, m.righttype)
        )));
    }
    list.push(m);
    Ok(())
}

/// `processTypesSpec`.
fn types_spec(env: &CatalogEnv, args: &[OperArgType]) -> Result<(Oid, Oid)> {
    let first = match args.first() {
        Some(Some(t)) => resolve_tn(env, t)?,
        _ => return Err(PgError::new(SqlState::SYNTAX_ERROR, "one or two argument types must be specified")),
    };
    let second = match args.get(1) {
        Some(Some(t)) => resolve_tn(env, t)?,
        Some(None) => 0,
        None => first,
    };
    if args.len() > 2 {
        return Err(PgError::new(SqlState::SYNTAX_ERROR, "one or two argument types must be specified"));
    }
    Ok((first, second))
}

/// `assignOperTypes`.
fn assign_oper_types(env: &CatalogEnv, m: &mut Member, am: Oid, props: &AmProps) -> Result<()> {
    let info = env.operator_info(m.object).ok_or_else(|| errors::cache_lookup_failed("operator", m.object))?;
    if info.kind != b'b' {
        return Err(invalid_def("index operators must be binary"));
    }
    if m.sortfamily != 0 {
        if !props.can_order_by_op {
            return Err(invalid_def(format!(
                "access method \"{}\" does not support ordering operators",
                env.access_method_name(am).unwrap_or("?")
            )));
        }
    } else if info.result != oids::BOOL {
        return Err(invalid_def("index search operators must return boolean"));
    }
    if m.lefttype == 0 {
        m.lefttype = info.left;
    }
    if m.righttype == 0 {
        m.righttype = info.right;
    }
    Ok(())
}

fn proc_sig(env: &CatalogEnv, f: Oid) -> Option<(Vec<Oid>, Oid)> {
    if let Some(p) = pgarbor_types::catalog_data::builtin_proc(f) {
        return Some((p.arg_types.to_vec(), p.ret));
    }
    env.snapshot().function(f).map(|f| (f.in_arg_types(), f.ret))
}

/// `assignProcTypes`.
fn assign_proc_types(env: &CatalogEnv, m: &mut Member, typeoid: Oid, props: &AmProps) -> Result<()> {
    let (args, ret) = proc_sig(env, m.object).ok_or_else(|| errors::cache_lookup_failed("function", m.object))?;
    if m.number == props.opts_proc {
        if typeoid != 0 {
            if (m.lefttype != 0 && m.lefttype != typeoid) || (m.righttype != 0 && m.righttype != typeoid) {
                return Err(invalid_def(
                    "associated data types for operator class options parsing functions must match opclass input type",
                ));
            }
        } else if m.lefttype != m.righttype {
            return Err(invalid_def(
                "left and right associated data types for operator class options parsing functions must match",
            ));
        }
        if ret != oids::VOID || args.len() != 1 || args[0] != oids::INTERNAL {
            return Err(invalid_def("invalid operator class options parsing function")
                .with_hint("Valid signature of operator class options parsing function is (internal) RETURNS void."));
        }
    } else if props.can_order {
        match m.number {
            1 => {
                if args.len() != 2 {
                    return Err(invalid_def("ordering comparison functions must have two arguments"));
                }
                if ret != oids::INT4 {
                    return Err(invalid_def("ordering comparison functions must return integer"));
                }
                if m.lefttype == 0 {
                    m.lefttype = args[0];
                }
                if m.righttype == 0 {
                    m.righttype = args[1];
                }
            }
            2 => {
                if args.len() != 1 || args[0] != oids::INTERNAL {
                    return Err(invalid_def("ordering sort support functions must accept type \"internal\""));
                }
                if ret != oids::VOID {
                    return Err(invalid_def("ordering sort support functions must return void"));
                }
            }
            3 => {
                if args.len() != 5 {
                    return Err(invalid_def("ordering in_range functions must have five arguments"));
                }
                if ret != oids::BOOL {
                    return Err(invalid_def("ordering in_range functions must return boolean"));
                }
                if m.lefttype == 0 {
                    m.lefttype = args[0];
                }
                if m.righttype == 0 {
                    m.righttype = args[2];
                }
            }
            4 => {
                if args.len() != 1 {
                    return Err(invalid_def("ordering equal image functions must have one argument"));
                }
                if ret != oids::BOOL {
                    return Err(invalid_def("ordering equal image functions must return boolean"));
                }
                if m.lefttype != m.righttype {
                    return Err(invalid_def("ordering equal image functions must not be cross-type"));
                }
            }
            6 => {
                if args.len() != 1 || args[0] != oids::INTERNAL {
                    return Err(invalid_def("btree skip support functions must accept type \"internal\""));
                }
                if ret != oids::VOID {
                    return Err(invalid_def("btree skip support functions must return void"));
                }
                if m.lefttype != m.righttype {
                    return Err(invalid_def("btree skip support functions must not be cross-type"));
                }
            }
            _ => {}
        }
    } else if props.can_hash {
        if m.number == 1 {
            if args.len() != 1 {
                return Err(invalid_def("hash function 1 must have one argument"));
            }
            if ret != oids::INT4 {
                return Err(invalid_def("hash function 1 must return integer"));
            }
        } else if m.number == 2 {
            if args.len() != 2 {
                return Err(invalid_def("hash function 2 must have two arguments"));
            }
            if ret != INT8 {
                return Err(invalid_def("hash function 2 must return bigint"));
            }
        }
        if m.lefttype == 0 {
            m.lefttype = args.first().copied().unwrap_or(0);
        }
        if m.righttype == 0 {
            m.righttype = args.first().copied().unwrap_or(0);
        }
    }
    if m.lefttype == 0 {
        m.lefttype = typeoid;
    }
    if m.righttype == 0 {
        m.righttype = typeoid;
    }
    if m.lefttype == 0 || m.righttype == 0 {
        return Err(invalid_def("associated data types must be specified for index support function"));
    }
    Ok(())
}

/// `LookupFuncWithArgs(OBJECT_FUNCTION, ...)` for opclass FUNCTION items.
fn lookup_support_func(env: &CatalogEnv, f: &FuncRef) -> Result<Oid> {
    match &f.args {
        Some(args) => {
            let mut types = Vec::with_capacity(args.len());
            for a in args {
                types.push(resolve_tn(env, a)?);
            }
            Ok(lookup_func_exact(env, &f.name, &types, false)?.unwrap_or(0))
        }
        None => {
            let cands = env.function_candidates(f.name.schema.as_deref(), &f.name.name)?;
            match cands.as_slice() {
                [] => Err(errors::no_function_named("function", &f.name.display())),
                [one] => Ok(one.oid),
                _ => Err(errors::function_name_not_unique(&f.name.display())),
            }
        }
    }
}

impl<'a> CatalogTxn<'a> {
    fn require_superuser_for(&mut self, what: &str) -> Result<()> {
        if self.current_is_superuser_ext()? {
            Ok(())
        } else {
            Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be superuser to {what}")))
        }
    }

    fn index_am(&mut self, method: &str) -> Result<Oid> {
        let env = self.env()?;
        let am = env.access_method_oid(method).ok_or_else(|| access_method_does_not_exist(method))?;
        if am == 2 {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("access method \"{method}\" is not of type INDEX")));
        }
        Ok(am)
    }

    fn opfamily_oid(&mut self, am: Oid, method: &str, name: &QName) -> Result<Oid> {
        let env = self.env()?;
        env.lookup_opfamily(am, name.schema.as_deref(), &name.name)?
            .ok_or_else(|| opfamily_does_not_exist(&name.display(), method))
    }

    /// `CreateOpFamily` (no checks besides the name clash).
    fn insert_opfamily(&mut self, name: &str, ns: Oid, am: Oid, method: &str) -> Result<Oid> {
        let env = self.env()?;
        if env.all_opfamilies().iter().any(|f| f.namespace == ns && f.method == am && f.name == name) {
            return Err(PgError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("operator family \"{name}\" for access method \"{method}\" already exists"),
            ));
        }
        let oid = self.new_oid()?;
        let owner = self.current_user();
        self.put_ext(ExtObject::OpFamily(OpFamily {
            oid,
            method: am,
            name: name.to_string(),
            namespace: ns,
            owner,
            operators: Vec::new(),
            procs: Vec::new(),
        }))?;
        self.record_created(ObjectAddress::new(ObjClass::OpFamily, oid));
        Ok(oid)
    }

    /// CREATE OPERATOR FAMILY (`DefineOpFamily`).
    pub fn create_opfamily(&mut self, name: &QName, method: &str) -> Result<Oid> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;
        let am = self.index_am(method)?;
        self.require_superuser_for("create an operator family")?;
        self.insert_opfamily(&name.name, ns, am, method)
    }

    /// Operator / function items -> members (shared by CREATE OPERATOR CLASS and ALTER ... ADD).
    fn build_members(
        &mut self,
        items: &[OpClassItemSpec],
        am: Oid,
        props: &AmProps,
        typeoid: Oid,
        for_class: bool,
        storage: &mut Oid,
    ) -> Result<(Vec<Member>, Vec<Member>)> {
        let env = self.env()?;
        let max_op = if props.strategies <= 0 { i16::MAX as i32 } else { props.strategies };
        let mut ops: Vec<Member> = Vec::new();
        let mut procs: Vec<Member> = Vec::new();
        for it in items {
            match it {
                OpClassItemSpec::Operator { number, op, types, order_family } => {
                    if *number <= 0 || *number > max_op {
                        return Err(invalid_def(format!("invalid operator number {number}, must be between 1 and {max_op}")));
                    }
                    let oper = match types {
                        Some((l, r)) => {
                            let r = OperRef { name: op.clone(), left: l.clone(), right: r.clone() };
                            lookup_oper_with_args(&env, &r, false)?.unwrap_or(0)
                        }
                        None => {
                            if !for_class {
                                return Err(PgError::new(
                                    SqlState::SYNTAX_ERROR,
                                    "operator argument types must be specified in ALTER OPERATOR FAMILY",
                                ));
                            }
                            match env.lookup_operator(op.schema.as_deref(), &op.name, typeoid, typeoid)? {
                                Some(o) => o,
                                None => {
                                    return Err(PgError::new(
                                        SqlState::UNDEFINED_FUNCTION,
                                        format!("operator does not exist: {}", op_signature(&env, op, typeoid, typeoid)),
                                    ))
                                }
                            }
                        }
                    };
                    let sortfamily = match order_family {
                        Some(f) => self.opfamily_oid(BTREE, "btree", f)?,
                        None => 0,
                    };
                    let mut m = Member { is_func: false, object: oper, number: *number, lefttype: 0, righttype: 0, sortfamily, opclass: 0 };
                    assign_oper_types(&env, &mut m, am, props)?;
                    add_member(&env, &mut ops, m)?;
                }
                OpClassItemSpec::Function { number, types, func } => {
                    if *number <= 0 || *number > props.support {
                        return Err(invalid_def(format!(
                            "invalid function number {number}, must be between 1 and {}",
                            props.support
                        )));
                    }
                    let f = lookup_support_func(&env, func)?;
                    let mut m = Member { is_func: true, object: f, number: *number, lefttype: 0, righttype: 0, sortfamily: 0, opclass: 0 };
                    if let Some(ts) = types {
                        let (l, r) = types_spec(&env, ts)?;
                        m.lefttype = l;
                        m.righttype = r;
                    }
                    assign_proc_types(&env, &mut m, typeoid, props)?;
                    add_member(&env, &mut procs, m)?;
                }
                OpClassItemSpec::Storage(t) => {
                    if !for_class {
                        return Err(PgError::new(SqlState::SYNTAX_ERROR, "STORAGE cannot be specified in ALTER OPERATOR FAMILY"));
                    }
                    if *storage != 0 {
                        return Err(invalid_def("storage type specified more than once"));
                    }
                    *storage = resolve_tn(&env, t)?;
                }
            }
        }
        Ok((ops, procs))
    }

    /// `btadjustmembers` / `hashadjustmembers`: which members are tied to an opclass.
    fn adjust_members(&mut self, am: Oid, family: Oid, opclass: Oid, members: &mut [Member]) -> Result<()> {
        let env = self.env()?;
        for m in members.iter_mut() {
            m.opclass = opclass;
            if am != BTREE && am != HASH {
                continue;
            }
            let required_proc = if am == BTREE { 1 } else { 1 };
            if (m.is_func && m.number != required_proc) || m.lefttype != m.righttype {
                m.opclass = 0;
                continue;
            }
            let class = env
                .all_opclasses()
                .into_iter()
                .find(|c| c.method == am && c.family == family && c.input_type == m.lefttype)
                .map(|c| c.oid);
            m.opclass = match (opclass, class) {
                (o, _) if o != 0 && env.opclass_info(o).is_some_and(|c| c.input_type == m.lefttype) => o,
                (_, Some(c)) => c,
                _ => 0,
            };
        }
        Ok(())
    }

    /// `storeOperators` + `storeProcedures`.
    fn store_members(&mut self, family: Oid, family_name: &str, ops: &[Member], procs: &[Member], is_add: bool) -> Result<()> {
        let env = self.env()?;
        let Some(ExtObject::OpFamily(mut f)) = self.ext_object(family) else {
            return Err(PgError::unimplemented("CAT-EXT-02", "adding members to a built-in operator family"));
        };
        for m in ops {
            if is_add && f.operators.iter().any(|o| o.lefttype == m.lefttype && o.righttype == m.righttype && o.strategy as i32 == m.number) {
                return Err(PgError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "operator {}({},{}) already exists in operator family \"{family_name}\"",
                        m.number,
                        type_name(&env, m.lefttype),
                        type_name(&env, m.righttype)
                    ),
                ));
            }
            let oid = self.new_oid_avoiding(&f)?;
            f.operators.push(AmOpEntry {
                oid,
                lefttype: m.lefttype,
                righttype: m.righttype,
                strategy: m.number as i16,
                purpose: if m.sortfamily != 0 { b'o' } else { b's' },
                operator: m.object,
                sortfamily: m.sortfamily,
                opclass: m.opclass,
            });
        }
        for m in procs {
            if is_add && f.procs.iter().any(|o| o.lefttype == m.lefttype && o.righttype == m.righttype && o.procnum as i32 == m.number) {
                return Err(PgError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "function {}({},{}) already exists in operator family \"{family_name}\"",
                        m.number,
                        type_name(&env, m.lefttype),
                        type_name(&env, m.righttype)
                    ),
                ));
            }
            let oid = self.new_oid_avoiding(&f)?;
            f.procs.push(AmProcEntry {
                oid,
                lefttype: m.lefttype,
                righttype: m.righttype,
                procnum: m.number as i16,
                proc: m.object,
                opclass: m.opclass,
            });
        }
        self.put_ext(ExtObject::OpFamily(f))
    }

    /// A fresh OID that is also not used by the not-yet-written members of `f`.
    fn new_oid_avoiding(&mut self, f: &OpFamily) -> Result<Oid> {
        loop {
            let o = self.new_oid()?;
            if !f.operators.iter().any(|m| m.oid == o) && !f.procs.iter().any(|m| m.oid == o) {
                return Ok(o);
            }
        }
    }

    /// CREATE OPERATOR CLASS (`DefineOpClass`).
    pub fn create_opclass(
        &mut self,
        name: &QName,
        is_default: bool,
        for_type: &pgarbor_types::names::TypeName,
        method: &str,
        family: Option<&QName>,
        items: &[OpClassItemSpec],
    ) -> Result<Oid> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;
        let am = self.index_am(method)?;
        let props = am_props(am).ok_or_else(|| access_method_does_not_exist(method))?;
        self.require_superuser_for("create an operator class")?;
        let typeoid = resolve_tn(&env, for_type)?;
        let family_oid = match family {
            Some(f) => self.opfamily_oid(am, method, f)?,
            None => {
                let env = self.env()?;
                match env.all_opfamilies().into_iter().find(|f| f.method == am && f.name == name.name && f.namespace == ns) {
                    Some(f) => f.oid,
                    None => self.insert_opfamily(&name.name, ns, am, method)?,
                }
            }
        };
        let mut storage: Oid = 0;
        let (mut ops, mut procs) = self.build_members(items, am, &props, typeoid, true, &mut storage)?;
        if storage != 0 {
            if storage == typeoid {
                storage = 0;
            } else if !props.storage {
                return Err(invalid_def(format!("storage type cannot be different from data type for access method \"{method}\"")));
            }
        }
        let env = self.env()?;
        if env.all_opclasses().iter().any(|c| c.method == am && c.name == name.name && c.namespace == ns) {
            return Err(PgError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("operator class \"{}\" for access method \"{method}\" already exists", name.name),
            ));
        }
        if is_default {
            if let Some(c) = env.all_opclasses().iter().find(|c| c.method == am && c.input_type == typeoid && c.is_default) {
                return Err(PgError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "could not make operator class \"{}\" be default for type {}",
                        name.name,
                        type_name_to_string(for_type)
                    ),
                )
                .with_detail(format!("Operator class \"{}\" already is the default.", c.name)));
            }
        }
        let oid = self.new_oid()?;
        self.put_ext(ExtObject::OpClass(OpClass {
            oid,
            method: am,
            name: name.name.clone(),
            namespace: ns,
            owner: cur,
            family: family_oid,
            input_type: typeoid,
            is_default,
            key_type: storage,
        }))?;
        self.record_created(ObjectAddress::new(ObjClass::OpClass, oid));
        self.adjust_members(am, family_oid, oid, &mut ops)?;
        self.adjust_members(am, family_oid, oid, &mut procs)?;
        let fam_name = family.map(|f| f.display()).unwrap_or_else(|| name.display());
        self.store_members(family_oid, &fam_name, &ops, &procs, false)?;
        Ok(oid)
    }

    /// ALTER OPERATOR FAMILY ... ADD (`AlterOpFamily` + `AlterOpFamilyAdd`).
    pub fn alter_opfamily_add(&mut self, name: &QName, method: &str, items: &[OpClassItemSpec]) -> Result<()> {
        let am = self.index_am(method)?;
        let props = am_props(am).ok_or_else(|| access_method_does_not_exist(method))?;
        let family = self.opfamily_oid(am, method, name)?;
        self.require_superuser_for("alter an operator family")?;
        let mut storage = 0;
        let (mut ops, mut procs) = self.build_members(items, am, &props, 0, false, &mut storage)?;
        self.adjust_members(am, family, 0, &mut ops)?;
        self.adjust_members(am, family, 0, &mut procs)?;
        self.store_members(family, &name.display(), &ops, &procs, true)
    }

    /// ALTER OPERATOR FAMILY ... DROP (`AlterOpFamilyDrop`).
    pub fn alter_opfamily_drop(&mut self, name: &QName, method: &str, items: &[OpFamilyDropSpec]) -> Result<()> {
        let am = self.index_am(method)?;
        let props = am_props(am).ok_or_else(|| access_method_does_not_exist(method))?;
        let family = self.opfamily_oid(am, method, name)?;
        self.require_superuser_for("alter an operator family")?;
        let env = self.env()?;
        let max_op = if props.strategies <= 0 { i16::MAX as i32 } else { props.strategies };
        let mut ops: Vec<Member> = Vec::new();
        let mut procs: Vec<Member> = Vec::new();
        for it in items {
            let max = if it.is_operator { max_op } else { props.support };
            if it.number <= 0 || it.number > max {
                let what = if it.is_operator { "operator" } else { "function" };
                return Err(invalid_def(format!("invalid {what} number {}, must be between 1 and {max}", it.number)));
            }
            let args: Vec<OperArgType> = it.types.iter().cloned().map(Some).collect();
            let (l, r) = types_spec(&env, &args)?;
            let m = Member { is_func: !it.is_operator, object: 0, number: it.number, lefttype: l, righttype: r, sortfamily: 0, opclass: 0 };
            if it.is_operator {
                add_member(&env, &mut ops, m)?;
            } else {
                add_member(&env, &mut procs, m)?;
            }
        }
        let rows_ops = env.family_operators(family);
        let rows_procs = env.family_procs(family);
        for m in ops.iter().chain(procs.iter()) {
            let found = if m.is_func {
                rows_procs
                    .iter()
                    .find(|p| p.lefttype == m.lefttype && p.righttype == m.righttype && p.procnum as i32 == m.number)
                    .map(|p| ObjectAddress::new(ObjClass::AmProc, p.oid))
            } else {
                rows_ops
                    .iter()
                    .find(|p| p.lefttype == m.lefttype && p.righttype == m.righttype && p.strategy as i32 == m.number)
                    .map(|p| ObjectAddress::new(ObjClass::AmOp, p.oid))
            };
            let Some(addr) = found else {
                let what = if m.is_func { "function" } else { "operator" };
                return Err(PgError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "{what} {}({},{}) does not exist in operator family \"{}\"",
                        m.number,
                        type_name(&env, m.lefttype),
                        type_name(&env, m.righttype),
                        name.display()
                    ),
                ));
            };
            self.drop_objects(&[addr], DropBehavior::Restrict)?;
        }
        Ok(())
    }

    /// ALTER OPERATOR FAMILY / CLASS ... RENAME / OWNER / SET SCHEMA.
    pub fn alter_opclass_generic(&mut self, is_family: bool, name: &QName, method: &str, action: &AlterExt) -> Result<()> {
        let env = self.env()?;
        let am = env.access_method_oid(method).ok_or_else(|| access_method_does_not_exist(method))?;
        let addr = if is_family {
            ObjectAddress::new(
                ObjClass::OpFamily,
                env.lookup_opfamily(am, name.schema.as_deref(), &name.name)?
                    .ok_or_else(|| opfamily_does_not_exist(&name.display(), method))?,
            )
        } else {
            ObjectAddress::new(
                ObjClass::OpClass,
                env.lookup_opclass(am, name.schema.as_deref(), &name.name)?
                    .ok_or_else(|| opclass_does_not_exist(&name.display(), method))?,
            )
        };
        self.alter_ext_generic(&addr, action)
    }

    /// Dispatch of a generic ALTER action on an extensibility address.
    pub fn alter_ext_generic(&mut self, addr: &ObjectAddress, action: &AlterExt) -> Result<()> {
        if addr.is_pinned() || !self.data().ext.contains_key(&addr.oid) {
            // Builtin objects: only superusers pass the ownership check, and they cannot be changed.
            let env = self.env()?;
            if !self.current_is_superuser_ext()? {
                let (_, word, n) = env.ext_owner_word_name(addr).unwrap_or((0, "object", addr.oid.to_string()));
                return Err(errors::must_be_owner(word, &n));
            }
            return Err(PgError::unimplemented("CAT-EXT-03", "altering a built-in catalog object"));
        }
        match action {
            AlterExt::Rename(n) => self.rename_ext(addr, n),
            AlterExt::Owner(o) => self.alter_ext_owner(addr, *o),
            AlterExt::SetSchema(s) => {
                let env = self.env()?;
                let ns = env.namespace_oid(s)?;
                self.set_ext_schema(addr, ns)
            }
        }
    }

    /// DROP OPERATOR FAMILY / CLASS.
    pub fn drop_opclasses(&mut self, is_family: bool, if_exists: bool, names: &[QName], method: &str, cascade: bool) -> Result<()> {
        let env = self.env()?;
        let am = match env.access_method_oid(method) {
            Some(a) => a,
            None => {
                if if_exists {
                    self.notice(PgError::notice(format!("access method \"{method}\" does not exist, skipping")));
                    return Ok(());
                }
                return Err(access_method_does_not_exist(method));
            }
        };
        let word = if is_family { "operator family" } else { "operator class" };
        let mut targets = Vec::new();
        for n in names {
            let found = if is_family {
                env.lookup_opfamily(am, n.schema.as_deref(), &n.name)
            } else {
                env.lookup_opclass(am, n.schema.as_deref(), &n.name)
            };
            let found = match found {
                Ok(f) => f,
                Err(e) if if_exists && e.sqlstate == SqlState::INVALID_SCHEMA_NAME => {
                    self.notice(PgError::notice(format!(
                        "schema \"{}\" does not exist, skipping",
                        n.schema.clone().unwrap_or_default()
                    )));
                    continue;
                }
                Err(e) => return Err(e),
            };
            match found {
                Some(oid) => {
                    let addr = ObjectAddress::new(if is_family { ObjClass::OpFamily } else { ObjClass::OpClass }, oid);
                    if !addr.is_pinned() {
                        self.require_owner(&addr)?;
                    }
                    targets.push(addr);
                }
                None => {
                    if if_exists {
                        self.notice(PgError::notice(format!(
                            "{word} \"{}\" does not exist for access method \"{method}\", skipping",
                            n.display()
                        )));
                    } else if is_family {
                        return Err(opfamily_does_not_exist(&n.display(), method));
                    } else {
                        return Err(opclass_does_not_exist(&n.display(), method));
                    }
                }
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        self.drop_objects(&targets, if cascade { DropBehavior::Cascade } else { DropBehavior::Restrict })
    }
}

/// PG `TypeNameToString`.
fn type_name_to_string(t: &pgarbor_types::names::TypeName) -> String {
    let mut s = match &t.schema {
        Some(sc) => format!("{sc}.{}", t.name),
        None => t.name.clone(),
    };
    for _ in 0..t.array_dims {
        s.push_str("[]");
    }
    s
}
