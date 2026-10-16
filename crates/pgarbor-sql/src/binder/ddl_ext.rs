//! Binding of the extensibility statements (amendment pgarbor-sql-05): identifiers are folded,
//! written types become `TypeName`s and roles OIDs; object lookups happen in the catalog
//! (`CatalogTxn::run_ext`) except where an existing bound statement is reused (ALTER AGGREGATE ->
//! `AlterAction` on the function, DROP AGGREGATE -> `BoundDrop` of kind `Aggregate`).
use pgarbor_catalog::ddl::drop_stmt::{DropKind, DropTarget};
use pgarbor_catalog::ddl::ext_aggregate::lookup_aggregate;
use pgarbor_catalog::ddl::ext_spec::*;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::names::TypeName;
use sqlparser::ast::{ArgMode, DataType, Ident, ObjectName, ObjectNamePart, OperateFunctionArg};

use super::context::Binder;
use super::types::data_type_to_type_name;
use crate::ext_stmt::*;
use crate::ir::{AlterAction, BoundDrop, BoundStatement};

fn ext(d: ExtDdl) -> BoundStatement {
    BoundStatement::Ext(Box::new(d))
}

/// Folded `[schema.]name`.
fn qname(b: &mut Binder, name: &ObjectName) -> Result<QName> {
    let q = b.qual(name)?;
    Ok(QName { schema: q.schema, name: q.name })
}

fn oper_qname(b: &mut Binder, n: &OperName) -> QName {
    let schema = n.schema.as_ref().map(|s| b.ident(s));
    QName { schema, name: n.name.clone() }
}

fn tn(b: &mut Binder, dt: &DataType) -> Result<TypeName> {
    data_type_to_type_name(b, dt, None)
}

/// The simple-name reading of a `func_type` def_arg (PG `TypeName.names`).
fn type_as_qname(b: &mut Binder, dt: &DataType) -> Option<QName> {
    match dt {
        DataType::Custom(obj, mods) if mods.is_empty() => {
            let parts: Vec<&Ident> = obj
                .0
                .iter()
                .filter_map(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i),
                    _ => None,
                })
                .collect();
            match parts.as_slice() {
                [n] => Some(QName { schema: None, name: b.ident(n) }),
                [s, n] => Some(QName { schema: Some(b.ident(s)), name: b.ident(n) }),
                [_, s, n] => Some(QName { schema: Some(b.ident(s)), name: b.ident(n) }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn def_items(b: &mut Binder, defs: &[DefElem]) -> Result<Vec<DefItem>> {
    let mut out = Vec::with_capacity(defs.len());
    for d in defs {
        let value = match &d.arg {
            None => None,
            Some(DefArg::Type(dt)) => {
                let qn = type_as_qname(b, dt);
                Some(DefVal::Type { tn: tn(b, dt)?, setof: false, qname: qn })
            }
            Some(DefArg::Setof(dt)) => {
                let qn = type_as_qname(b, dt);
                Some(DefVal::Type { tn: tn(b, dt)?, setof: true, qname: qn })
            }
            Some(DefArg::Operator(o)) => Some(DefVal::Oper(oper_qname(b, o))),
            Some(DefArg::Number(n)) => Some(DefVal::Number(n.clone())),
            Some(DefArg::String(s)) => Some(DefVal::Str(s.clone())),
            Some(DefArg::Keyword(k)) => Some(DefVal::Str(k.clone())),
        };
        out.push(DefItem { name: d.name.clone(), value });
    }
    Ok(out)
}

fn oper_arg(b: &mut Binder, a: &OperArg) -> Result<OperArgType> {
    match a {
        Some(dt) => Ok(Some(tn(b, dt)?)),
        None => Ok(None),
    }
}

fn oper_ref(b: &mut Binder, sig: &OperSig) -> Result<OperRef> {
    let name = oper_qname(b, &sig.name);
    Ok(OperRef { name, left: oper_arg(b, &sig.left)?, right: oper_arg(b, &sig.right)? })
}

fn agg_args(b: &mut Binder, a: &AggrArgs) -> Result<AggArgsSpec> {
    let mut args = Vec::with_capacity(a.args.len());
    for x in &a.args {
        let name = x.name.as_ref().map(|n| b.ident(n)).unwrap_or_default();
        args.push(AggArgSpec {
            name,
            ty: tn(b, &x.data_type)?,
            mode: if matches!(x.mode, Some(ArgMode::Variadic)) { b'v' } else { b'i' },
            explicit_mode: x.mode.is_some(),
        });
    }
    Ok(AggArgsSpec { args, order_by_at: a.order_by_at, star: a.star })
}

fn agg_ref(b: &mut Binder, s: &AggSig) -> Result<AggRef> {
    Ok(AggRef { name: qname(b, &s.name)?, args: agg_args(b, &s.args)? })
}

fn role(b: &mut Binder, id: &Ident) -> Result<Oid> {
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

fn alter_ext(b: &mut Binder, a: &AlterAct) -> Result<AlterExt> {
    Ok(match a {
        AlterAct::Rename(n) => AlterExt::Rename(b.ident(n)),
        AlterAct::Owner(r) => AlterExt::Owner(role(b, r)?),
        AlterAct::SetSchema(s) => AlterExt::SetSchema(b.ident(s)),
    })
}

fn func_ref(b: &mut Binder, name: &ObjectName, args: &Option<Vec<OperateFunctionArg>>) -> Result<FuncRef> {
    let n = qname(b, name)?;
    let args = match args {
        None => None,
        Some(list) => {
            let mut v = Vec::new();
            for a in list {
                if matches!(a.mode, Some(ArgMode::Out)) {
                    continue;
                }
                v.push(tn(b, &a.data_type)?);
            }
            Some(v)
        }
    };
    Ok(FuncRef { name: n, args })
}

fn opclass_items(b: &mut Binder, items: &[OpClassItem]) -> Result<Vec<OpClassItemSpec>> {
    let mut out = Vec::new();
    for it in items {
        out.push(match it {
            OpClassItem::Operator { number, op, args, order_family, .. } => {
                let types = match args {
                    Some((l, r)) => Some((oper_arg(b, l)?, oper_arg(b, r)?)),
                    None => None,
                };
                let order_family = match order_family {
                    Some(f) => Some(qname(b, f)?),
                    None => None,
                };
                OpClassItemSpec::Operator { number: *number, op: oper_qname(b, op), types, order_family }
            }
            OpClassItem::Function { number, types, func, func_args, .. } => {
                let types = match types {
                    Some(list) => {
                        let mut v = Vec::new();
                        for t in list {
                            v.push(oper_arg(b, t)?);
                        }
                        Some(v)
                    }
                    None => None,
                };
                OpClassItemSpec::Function { number: *number, types, func: func_ref(b, func, func_args)? }
            }
            OpClassItem::Storage(dt) => OpClassItemSpec::Storage(tn(b, dt)?),
        });
    }
    Ok(out)
}

fn ext_obj_ref(b: &mut Binder, o: &ObjectRef) -> Result<Option<ExtObjRef>> {
    Ok(Some(match o {
        ObjectRef::Operator(sig) => ExtObjRef::Operator(oper_ref(b, sig)?),
        ObjectRef::Aggregate(sig) => ExtObjRef::Aggregate(agg_ref(b, sig)?),
        ObjectRef::OpClass { name, method } => ExtObjRef::OpClass { name: qname(b, name)?, method: b.ident(method) },
        ObjectRef::OpFamily { name, method } => ExtObjRef::OpFamily { name: qname(b, name)?, method: b.ident(method) },
        ObjectRef::Cast { source, target } => ExtObjRef::Cast { source: tn(b, source)?, target: tn(b, target)? },
        ObjectRef::Conversion(n) => ExtObjRef::Conversion(qname(b, n)?),
        ObjectRef::Collation(n) => ExtObjRef::Collation(qname(b, n)?),
        ObjectRef::Extension(n) => ExtObjRef::Extension(b.ident(n)),
        _ => return Ok(None),
    }))
}

/// `ExtraStatement::Ext`.
pub fn bind_ext(b: &mut Binder, stmt: &ExtStmt) -> Result<BoundStatement> {
    match stmt {
        ExtStmt::CreateOperator { name, defs } => {
            let name = oper_qname(b, name);
            let defs = def_items(b, defs)?;
            Ok(ext(ExtDdl::CreateOperator { name, defs }))
        }
        ExtStmt::AlterOperator { sig, action } => {
            let op = oper_ref(b, sig)?;
            Ok(ext(match action {
                AlterOperatorAction::Owner(r) => ExtDdl::AlterOperatorOwner { op, owner: role(b, r)? },
                AlterOperatorAction::SetSchema(s) => ExtDdl::AlterOperatorSchema { op, schema: b.ident(s) },
                AlterOperatorAction::Set(defs) => ExtDdl::AlterOperatorSet { op, defs: def_items(b, defs)? },
            }))
        }
        ExtStmt::DropOperator { if_exists, sigs, cascade } => {
            let mut ops = Vec::new();
            for s in sigs {
                ops.push(oper_ref(b, s)?);
            }
            Ok(ext(ExtDdl::DropOperators { if_exists: *if_exists, ops, cascade: *cascade }))
        }
        ExtStmt::CreateAggregate { or_replace, name, args, defs, .. } => {
            let name = qname(b, name)?;
            let args = match args {
                Some(a) => Some(agg_args(b, a)?),
                None => None,
            };
            let defs = def_items(b, defs)?;
            Ok(ext(ExtDdl::CreateAggregate { or_replace: *or_replace, name, args, defs }))
        }
        ExtStmt::AlterAggregate { sig, action } => {
            let r = agg_ref(b, sig)?;
            let func = lookup_aggregate(b.env, &r, false)?.unwrap_or(0);
            let inner = match action {
                AlterAct::Rename(n) => AlterAction::RenameFunction { func, new_name: b.ident(n) },
                AlterAct::Owner(r) => AlterAction::FunctionOwner { func, new_owner: role(b, r)? },
                AlterAct::SetSchema(s) => {
                    let s = b.ident(s);
                    let ns = b.env.namespace_oid(&s)?;
                    AlterAction::SetFunctionSchema { func, new_schema: ns }
                }
            };
            Ok(BoundStatement::Alter(Box::new(AlterAction::Routine { tag: "ALTER AGGREGATE", inner: Box::new(inner) })))
        }
        ExtStmt::DropAggregate { if_exists, sigs, cascade } => {
            let mut targets = Vec::new();
            let mut notices = Vec::new();
            for s in sigs {
                let r = agg_ref(b, s)?;
                let mut types = Vec::new();
                let mut skip = false;
                for a in &r.args.args {
                    match pgarbor_types::names::resolve_type_name(b.env, &a.ty) {
                        Ok(t) => types.push(t.oid),
                        Err(e) if *if_exists && e.sqlstate == SqlState::UNDEFINED_OBJECT => {
                            let mut n = PgError::notice(format!("type \"{}\" does not exist, skipping", type_text(&a.ty)));
                            n.sqlstate = SqlState::SUCCESSFUL_COMPLETION;
                            notices.push(n);
                            skip = true;
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
                if skip {
                    continue;
                }
                targets.push(DropTarget { schema: r.name.schema, name: r.name.name, args: Some(types), all_args_lookup: false });
            }
            for n in notices {
                b.notice(n);
            }
            if targets.is_empty() {
                return Ok(BoundStatement::Noop { tag: "DROP AGGREGATE".to_string(), notices: Vec::new() });
            }
            Ok(BoundStatement::Drop(Box::new(BoundDrop {
                kind: DropKind::Aggregate,
                targets,
                if_exists: *if_exists,
                cascade: *cascade,
            })))
        }
        ExtStmt::CreateOpFamily { name, method } => {
            Ok(ext(ExtDdl::CreateOpFamily { name: qname(b, name)?, method: b.ident(method) }))
        }
        ExtStmt::AlterOpFamilyAdd { name, method, items } => {
            let name = qname(b, name)?;
            let method = b.ident(method);
            let items = opclass_items(b, items)?;
            Ok(ext(ExtDdl::AlterOpFamilyAdd { name, method, items }))
        }
        ExtStmt::AlterOpFamilyDrop { name, method, items } => {
            let name = qname(b, name)?;
            let method = b.ident(method);
            let mut out = Vec::new();
            for it in items {
                let mut types = Vec::new();
                for t in &it.types {
                    match t {
                        Some(dt) => types.push(tn(b, dt)?),
                        None => return Err(PgError::new(SqlState::SYNTAX_ERROR, "syntax error at or near \"NONE\"")),
                    }
                }
                out.push(OpFamilyDropSpec { is_operator: it.is_operator, number: it.number, types });
            }
            Ok(ext(ExtDdl::AlterOpFamilyDrop { name, method, items: out }))
        }
        ExtStmt::AlterOpFamily { name, method, action } => {
            let name = qname(b, name)?;
            let method = b.ident(method);
            Ok(ext(ExtDdl::AlterOpFamily { name, method, action: alter_ext(b, action)? }))
        }
        ExtStmt::DropOpFamily { if_exists, names, method, cascade } => {
            let mut v = Vec::new();
            for n in names {
                v.push(qname(b, n)?);
            }
            Ok(ext(ExtDdl::DropOpFamilies { if_exists: *if_exists, names: v, method: b.ident(method), cascade: *cascade }))
        }
        ExtStmt::CreateOpClass { name, is_default, for_type, method, family, items } => {
            let name = qname(b, name)?;
            let for_type = tn(b, for_type)?;
            let method = b.ident(method);
            let family = match family {
                Some(f) => Some(qname(b, f)?),
                None => None,
            };
            let items = opclass_items(b, items)?;
            Ok(ext(ExtDdl::CreateOpClass { name, is_default: *is_default, for_type, method, family, items }))
        }
        ExtStmt::AlterOpClass { name, method, action } => {
            let name = qname(b, name)?;
            let method = b.ident(method);
            Ok(ext(ExtDdl::AlterOpClass { name, method, action: alter_ext(b, action)? }))
        }
        ExtStmt::DropOpClass { if_exists, names, method, cascade } => {
            let mut v = Vec::new();
            for n in names {
                v.push(qname(b, n)?);
            }
            Ok(ext(ExtDdl::DropOpClasses { if_exists: *if_exists, names: v, method: b.ident(method), cascade: *cascade }))
        }
        ExtStmt::CreateCast { source, target, method, context } => {
            let source = tn(b, source)?;
            let target = tn(b, target)?;
            let (func, inout) = match method {
                CastMethodAst::Function { name, args } => (Some(func_ref(b, name, args)?), false),
                CastMethodAst::WithoutFunction => (None, false),
                CastMethodAst::Inout => (None, true),
            };
            Ok(ext(ExtDdl::CreateCast { source, target, func, inout, context: *context }))
        }
        ExtStmt::DropCast { if_exists, source, target, cascade } => {
            let source = tn(b, source)?;
            let target = tn(b, target)?;
            Ok(ext(ExtDdl::DropCast { if_exists: *if_exists, source, target, cascade: *cascade }))
        }
        ExtStmt::CreateConversion { is_default, name, for_encoding, to_encoding, func } => {
            let name = qname(b, name)?;
            let func = qname(b, func)?;
            Ok(ext(ExtDdl::CreateConversion {
                is_default: *is_default,
                name,
                for_encoding: for_encoding.clone(),
                to_encoding: to_encoding.clone(),
                func,
            }))
        }
        ExtStmt::AlterConversion { name, action } => {
            let name = qname(b, name)?;
            Ok(ext(ExtDdl::AlterConversion { name, action: alter_ext(b, action)? }))
        }
        ExtStmt::DropConversion { if_exists, names, cascade } => {
            let mut v = Vec::new();
            for n in names {
                v.push(qname(b, n)?);
            }
            Ok(ext(ExtDdl::DropConversions { if_exists: *if_exists, names: v, cascade: *cascade }))
        }
        ExtStmt::CreateCollation { if_not_exists, name, from, defs } => {
            let name = qname(b, name)?;
            let from = match from {
                Some(f) => Some(qname(b, f)?),
                None => None,
            };
            let defs = def_items(b, defs)?;
            Ok(ext(ExtDdl::CreateCollation { if_not_exists: *if_not_exists, name, from, defs }))
        }
        ExtStmt::AlterCollation { name, action } => {
            let name = qname(b, name)?;
            let action = match action {
                Some(a) => Some(alter_ext(b, a)?),
                None => None,
            };
            Ok(ext(ExtDdl::AlterCollation { name, action }))
        }
        ExtStmt::DropCollation { if_exists, names, cascade } => {
            let mut v = Vec::new();
            for n in names {
                v.push(qname(b, n)?);
            }
            Ok(ext(ExtDdl::DropCollations { if_exists: *if_exists, names: v, cascade: *cascade }))
        }
        ExtStmt::CreateExtension { if_not_exists, name, schema, version, cascade } => Ok(ext(ExtDdl::CreateExtension {
            if_not_exists: *if_not_exists,
            name: b.ident(name),
            schema: schema.as_ref().map(|s| b.ident(s)),
            version: version.clone(),
            cascade: *cascade,
        })),
        ExtStmt::AlterExtensionUpdate { name, version } => {
            Ok(ext(ExtDdl::AlterExtensionUpdate { name: b.ident(name), version: version.clone() }))
        }
        ExtStmt::AlterExtensionSchema { name, schema } => {
            Ok(ext(ExtDdl::AlterExtensionSchema { name: b.ident(name), schema: b.ident(schema) }))
        }
        ExtStmt::AlterExtensionMember { .. } => {
            Err(PgError::unimplemented("SQL-EXT-01", "ALTER EXTENSION ADD/DROP of member objects"))
        }
        ExtStmt::DropExtension { if_exists, names, cascade } => {
            let names = names.iter().map(|n| b.ident(n)).collect();
            Ok(ext(ExtDdl::DropExtensions { if_exists: *if_exists, names, cascade: *cascade }))
        }
        ExtStmt::Comment { object, text } => {
            let text = match text {
                Some(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            match ext_obj_ref(b, object)? {
                Some(o) => Ok(ext(ExtDdl::Comment { object: o, text })),
                None => Err(PgError::unimplemented("SQL-EXT-02", "COMMENT ON this object kind")),
            }
        }
    }
}

/// The written type name for messages.
fn type_text(t: &TypeName) -> String {
    match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}
