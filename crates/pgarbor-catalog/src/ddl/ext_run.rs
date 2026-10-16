//! Dispatch of one [`ExtDdl`] statement (amendment pgarbor-catalog-09) and COMMENT ON /
//! ALTER EXTENSION ADD|DROP object resolution for the extensibility kinds.
use crate::ddl::ext_operator::{lookup_oper_with_args, resolve_tn};
use crate::ddl::ext_spec::*;
use crate::env::CatalogEnv;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};

impl<'a> CatalogTxn<'a> {
    /// Execute one extensibility statement.
    pub fn run_ext(&mut self, stmt: &ExtDdl) -> Result<()> {
        use ExtDdl::*;
        match stmt {
            CreateOperator { name, defs } => self.create_operator(name, defs).map(|_| ()),
            AlterOperatorSet { op, defs } => self.alter_operator_set(op, defs),
            AlterOperatorOwner { op, owner } => self.alter_operator_owner(op, *owner),
            AlterOperatorSchema { op, schema } => self.alter_operator_schema(op, schema),
            DropOperators { if_exists, ops, cascade } => self.drop_operators(*if_exists, ops, *cascade),
            CreateAggregate { or_replace, name, args, defs } => {
                self.create_aggregate(*or_replace, name, args.as_ref(), defs).map(|_| ())
            }
            Comment { object, text } => {
                let env = self.env()?;
                let addr = resolve_ext_object(&env, object, false)?
                    .ok_or_else(|| PgError::internal("COMMENT target not resolved"))?;
                self.comment_on_ext(&addr, text.as_deref())
            }
            CreateOpFamily { name, method } => self.create_opfamily(name, method).map(|_| ()),
            AlterOpFamilyAdd { name, method, items } => self.alter_opfamily_add(name, method, items),
            AlterOpFamilyDrop { name, method, items } => self.alter_opfamily_drop(name, method, items),
            AlterOpFamily { name, method, action } => self.alter_opclass_generic(true, name, method, action),
            DropOpFamilies { if_exists, names, method, cascade } => {
                self.drop_opclasses(true, *if_exists, names, method, *cascade)
            }
            CreateOpClass { name, is_default, for_type, method, family, items } => self
                .create_opclass(name, *is_default, for_type, method, family.as_ref(), items)
                .map(|_| ()),
            AlterOpClass { name, method, action } => self.alter_opclass_generic(false, name, method, action),
            DropOpClasses { if_exists, names, method, cascade } => {
                self.drop_opclasses(false, *if_exists, names, method, *cascade)
            }
            CreateCast { source, target, func, inout, context } => {
                self.create_cast(source, target, func.as_ref(), *inout, *context).map(|_| ())
            }
            DropCast { if_exists, source, target, cascade } => self.drop_cast(*if_exists, source, target, *cascade),
            CreateConversion { is_default, name, for_encoding, to_encoding, func } => {
                self.create_conversion(*is_default, name, for_encoding, to_encoding, func).map(|_| ())
            }
            AlterConversion { name, action } => {
                let addr = self.conversion_addr(name)?;
                self.alter_ext_generic(&addr, action)
            }
            DropConversions { if_exists, names, cascade } => {
                self.drop_named_ext(ObjClass::Conversion, "conversion", *if_exists, names, *cascade)
            }
            CreateCollation { if_not_exists, name, from, defs } => {
                self.create_collation(*if_not_exists, name, from.as_ref(), defs).map(|_| ())
            }
            AlterCollation { name, action } => self.alter_collation(name, action.as_ref()),
            DropCollations { if_exists, names, cascade } => {
                self.drop_named_ext(ObjClass::Collation, "collation", *if_exists, names, *cascade)
            }
            CreateExtension { if_not_exists, name, schema, .. } => {
                self.create_extension(*if_not_exists, name, schema.as_deref())
            }
            AlterExtensionUpdate { name, version } => self.alter_extension(name, version.as_deref(), None),
            AlterExtensionSchema { name, schema } => self.alter_extension(name, None, Some(schema)),
            AlterExtensionMember { .. } => {
                Err(PgError::unimplemented("CAT-EXT-08", "ALTER EXTENSION ADD/DROP of member objects"))
            }
            DropExtensions { if_exists, names, cascade } => self.drop_extensions(*if_exists, names, *cascade),
        }
    }

    /// COMMENT ON for any address (extensibility kinds included): ownership as PG
    /// `check_object_ownership` (casts: owner of the source or target type).
    pub fn comment_on_ext(&mut self, addr: &ObjectAddress, text: Option<&str>) -> Result<()> {
        match addr.class {
            ObjClass::Operator
            | ObjClass::OpClass
            | ObjClass::OpFamily
            | ObjClass::Conversion
            | ObjClass::Collation
            | ObjClass::Extension => {
                if !addr.is_pinned() {
                    self.require_owner(addr)?;
                } else if !self.current_is_superuser_ext()? {
                    let env = self.env()?;
                    let (_, word, name) = env.ext_owner_word_name(addr).unwrap_or((0, "object", addr.oid.to_string()));
                    return Err(crate::errors::must_be_owner(word, &name));
                }
                let key = DescriptionKey { objoid: addr.oid, classoid: addr.class.classid(), objsubid: 0 };
                self.set_description(key, text)
            }
            ObjClass::Cast => {
                let env = self.env()?;
                let cast = env.cast_info(addr.oid).ok_or_else(|| crate::errors::cache_lookup_failed("cast", addr.oid))?;
                self.require_cast_owner(cast.source, cast.target)?;
                let key = DescriptionKey { objoid: addr.oid, classoid: addr.class.classid(), objsubid: 0 };
                self.set_description(key, text)
            }
            _ => self.comment_on(addr, text),
        }
    }

    /// PG `check_object_ownership` for casts: owner of the source or the target type, else 42501
    /// `must be owner of type x or type y`.
    pub fn require_cast_owner(&mut self, source: Oid, target: Oid) -> Result<()> {
        if self.current_is_superuser_ext()? {
            return Ok(());
        }
        let s = self.require_owner(&ObjectAddress::type_(source));
        let t = self.require_owner(&ObjectAddress::type_(target));
        if s.is_ok() || t.is_ok() {
            return Ok(());
        }
        let env = self.env()?;
        Err(PgError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!(
                "must be owner of type {} or type {}",
                pgarbor_types::names::display_name(&env, source),
                pgarbor_types::names::display_name(&env, target)
            ),
        ))
    }
}

/// Resolve an [`ExtObjRef`] to an address (PG `get_object_address`). `missing_ok` returns `Ok(None)`
/// for a missing object.
pub fn resolve_ext_object(env: &CatalogEnv, r: &ExtObjRef, missing_ok: bool) -> Result<Option<ObjectAddress>> {
    match r {
        ExtObjRef::Address(a) => Ok(Some(*a)),
        ExtObjRef::Operator(op) => {
            Ok(lookup_oper_with_args(env, op, missing_ok)?.map(|o| ObjectAddress::new(ObjClass::Operator, o)))
        }
        ExtObjRef::Aggregate(agg) => Ok(crate::ddl::ext_aggregate::lookup_aggregate(env, agg, missing_ok)?.map(ObjectAddress::proc)),
        ExtObjRef::OpClass { name, method } => {
            let am = env.access_method_oid(method).ok_or_else(|| crate::ext::access_method_does_not_exist(method))?;
            match env.lookup_opclass(am, name.schema.as_deref(), &name.name)? {
                Some(o) => Ok(Some(ObjectAddress::new(ObjClass::OpClass, o))),
                None if missing_ok => Ok(None),
                None => Err(crate::ext::opclass_does_not_exist(&name.display(), method)),
            }
        }
        ExtObjRef::OpFamily { name, method } => {
            let am = env.access_method_oid(method).ok_or_else(|| crate::ext::access_method_does_not_exist(method))?;
            match env.lookup_opfamily(am, name.schema.as_deref(), &name.name)? {
                Some(o) => Ok(Some(ObjectAddress::new(ObjClass::OpFamily, o))),
                None if missing_ok => Ok(None),
                None => Err(crate::ext::opfamily_does_not_exist(&name.display(), method)),
            }
        }
        ExtObjRef::Cast { source, target } => {
            let s = resolve_tn(env, source)?;
            let t = resolve_tn(env, target)?;
            match env.find_cast(s, t) {
                Some(c) => Ok(Some(ObjectAddress::new(ObjClass::Cast, c.oid))),
                None if missing_ok => Ok(None),
                None => Err(PgError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "cast from type {} to type {} does not exist",
                        pgarbor_types::names::display_name(env, s),
                        pgarbor_types::names::display_name(env, t)
                    ),
                )),
            }
        }
        ExtObjRef::Conversion(name) => match env.lookup_conversion(name.schema.as_deref(), &name.name)? {
            Some(o) => Ok(Some(ObjectAddress::new(ObjClass::Conversion, o))),
            None if missing_ok => Ok(None),
            None => Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("conversion \"{}\" does not exist", name.display()),
            )),
        },
        ExtObjRef::Collation(name) => match env.collation_oid(name.schema.as_deref(), &name.name) {
            Ok(o) => Ok(Some(ObjectAddress::new(ObjClass::Collation, o))),
            Err(e) if missing_ok && e.sqlstate == SqlState::UNDEFINED_OBJECT => Ok(None),
            Err(e) => Err(e),
        },
        ExtObjRef::Extension(name) => match env.extension_oid(name) {
            Some(o) => Ok(Some(ObjectAddress::new(ObjClass::Extension, o))),
            None if missing_ok => Ok(None),
            None => Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("extension \"{name}\" does not exist"))),
        },
    }
}
