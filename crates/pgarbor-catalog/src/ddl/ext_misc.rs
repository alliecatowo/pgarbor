//! CREATE/DROP CAST, CREATE/ALTER/DROP CONVERSION, CREATE/ALTER/DROP COLLATION and
//! CREATE/ALTER/DROP EXTENSION (amendment pgarbor-catalog-09). PG sources:
//! `commands/functioncmds.c` (`CreateCast`), `catalog/pg_cast.c` (`CastCreate`),
//! `commands/conversioncmds.c` (`CreateConversionCommand`), `catalog/pg_conversion.c`,
//! `commands/collationcmds.c` (`DefineCollation`, `AlterCollation`), `catalog/pg_collation.c`
//! (`CollationCreate`), `utils/adt/pg_locale.c` (`builtin_validate_locale`,
//! `get_collation_actual_version`), `commands/extension.c`, `commands/dropcmds.c`.
//!
//! Collations: comparisons under any collation are code-point order in PGArbor v1 (see
//! `docs/compatibility/KNOWN_GAPS.md`); the catalog rows, versions and validation follow PG 18.6 as
//! built in the reference container (glibc 2.41 with the C, POSIX, C.UTF-8 and en_US.UTF-8
//! locales; ICU 76 versions `153.128[.x]`).
use crate::builtin;
use crate::ddl::ext_operator::{def_bool, def_string, func_rettype, lookup_func_exact, resolve_tn};
use crate::ddl::ext_spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::ext::{encoding_name, encoding_number};
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names::display_name;
use pgarbor_types::oids;

fn typtype(env: &CatalogEnv, t: Oid) -> u8 {
    env.type_info(t).map(|i| i.typtype).unwrap_or(0)
}

/// `TypeNameToString`.
fn tn_string(t: &pgarbor_types::names::TypeName) -> String {
    let mut s = match &t.schema {
        Some(sc) => format!("{sc}.{}", t.name),
        None => t.name.clone(),
    };
    for _ in 0..t.array_dims {
        s.push_str("[]");
    }
    s
}

/// Contrib extensions shipped in the reference container that PGArbor cannot provide.
const CONTRIB_EXTENSIONS: &[&str] = &[
    "adminpack", "amcheck", "autoinc", "bloom", "btree_gin", "btree_gist", "citext", "cube", "dblink", "dict_int",
    "dict_xsyn", "earthdistance", "file_fdw", "fuzzystrmatch", "hstore", "insert_username", "intagg", "intarray",
    "isn", "lo", "ltree", "moddatetime", "old_snapshot", "pageinspect", "pg_buffercache", "pg_freespacemap",
    "pg_prewarm", "pg_stat_statements", "pg_surgery", "pg_trgm", "pg_visibility", "pg_walinspect", "pgcrypto",
    "pgrowlocks", "pgstattuple", "postgres_fdw", "refint", "seg", "sslinfo", "tablefunc", "tcn", "tsm_system_rows",
    "tsm_system_time", "unaccent", "uuid-ossp", "xml2",
];

/// Locales of the reference container (`locale -a`), normalized (`en_US.UTF-8` = `en_US.utf8`).
fn libc_locale_exists(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace("utf-8", "utf8");
    matches!(n.as_str(), "c" | "posix" | "c.utf8" | "en_us.utf8")
}

/// glibc collation version of a libc locale (`None` for the C locales).
fn libc_version(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    if n == "c" || n == "posix" || n.starts_with("c.") {
        None
    } else {
        Some("2.41".to_string())
    }
}

/// `icu_language_tag` for the simple forms (`de_DE` -> `de-DE`).
fn icu_language_tag(loc: &str) -> String {
    if loc.contains('@') || loc.contains("-u-") {
        return loc.to_string();
    }
    let parts: Vec<&str> = loc.split(['_', '-']).collect();
    let mut out: Vec<String> = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            out.push(p.to_ascii_lowercase());
        } else if p.len() == 2 {
            out.push(p.to_ascii_uppercase());
        } else if p.len() == 4 {
            let mut s = p.to_ascii_lowercase();
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            out.push(s);
        } else {
            out.push(p.to_string());
        }
    }
    let tag = out.join("-");
    if tag.is_empty() || tag == "root" {
        "und".to_string()
    } else {
        tag
    }
}

/// ICU collator version of a locale tag: the version PG's own ICU collations carry for that locale
/// (or its language), else the root version.
fn icu_version(tag: &str) -> String {
    let base = tag.split("-u-").next().unwrap_or(tag);
    let find = |loc: &str| builtin::COLLATIONS.iter().find(|c| c.provider == b'i' && c.locale == Some(loc)).and_then(|c| c.version);
    if let Some(v) = find(base) {
        return v.to_string();
    }
    let lang = base.split('-').next().unwrap_or(base);
    find(lang).or_else(|| find("und")).unwrap_or("153.128").to_string()
}

impl<'a> CatalogTxn<'a> {
    // ---------------------------------------------------------------- casts

    /// CREATE CAST (`CreateCast` + `CastCreate`).
    pub fn create_cast(
        &mut self,
        source: &pgarbor_types::names::TypeName,
        target: &pgarbor_types::names::TypeName,
        func: Option<&FuncRef>,
        inout: bool,
        context: u8,
    ) -> Result<Oid> {
        let env = self.env()?;
        let s = resolve_tn(&env, source)?;
        let t = resolve_tn(&env, target)?;
        let (st, tt) = (typtype(&env, s), typtype(&env, t));
        if st == b'p' {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("source data type {} is a pseudo-type", tn_string(source))));
        }
        if tt == b'p' {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("target data type {} is a pseudo-type", tn_string(target))));
        }
        self.require_cast_owner(s, t)?;
        if st == b'd' {
            self.notice(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, "cast will be ignored because the source data type is a domain")
                    .with_severity(Severity::Warning),
            );
        } else if tt == b'd' {
            self.notice(
                PgError::new(SqlState::WRONG_OBJECT_TYPE, "cast will be ignored because the target data type is a domain")
                    .with_severity(Severity::Warning),
            );
        }
        let invalid = |m: &str| Err(PgError::new(SqlState::INVALID_OBJECT_DEFINITION, m.to_string()));
        let (method, funcid, nargs) = if let Some(f) = func {
            let fid = match &f.args {
                Some(args) => {
                    let mut types = Vec::new();
                    for a in args {
                        types.push(resolve_tn(&env, a)?);
                    }
                    lookup_func_exact(&env, &f.name, &types, false)?.unwrap_or(0)
                }
                None => {
                    let cands = env.function_candidates(f.name.schema.as_deref(), &f.name.name)?;
                    match cands.as_slice() {
                        [] => return Err(errors::no_function_named("function", &f.name.display())),
                        [one] => one.oid,
                        _ => return Err(errors::function_name_not_unique(&f.name.display())),
                    }
                }
            };
            let (args, ret, kind, retset) = match pgarbor_types::catalog_data::builtin_proc(fid) {
                Some(p) => (p.arg_types.to_vec(), p.ret, p.kind, p.retset),
                None => match env.snapshot().function(fid) {
                    Some(u) => (u.in_arg_types(), u.ret, u.kind.to_types(), u.retset),
                    None => return Err(errors::cache_lookup_failed("function", fid)),
                },
            };
            let n = args.len();
            if !(1..=3).contains(&n) {
                return invalid("cast function must take one to three arguments");
            }
            if !binary_coercible_with_cast(&env, s, args[0])? {
                return invalid("argument of cast function must match or be binary-coercible from source data type");
            }
            if n > 1 && args[1] != oids::INT4 {
                return invalid("second argument of cast function must be type integer");
            }
            if n > 2 && args[2] != oids::BOOL {
                return invalid("third argument of cast function must be type boolean");
            }
            if !binary_coercible_with_cast(&env, ret, t)? {
                return invalid("return data type of cast function must match or be binary-coercible to target data type");
            }
            if kind != pgarbor_types::catalog_data::ProcKind::Function {
                return invalid("cast function must be a normal function");
            }
            if retset {
                return invalid("cast function must not return a set");
            }
            (b'f', fid, n)
        } else if inout {
            (b'i', 0, 0)
        } else {
            (b'b', 0, 0)
        };
        if method == b'b' {
            if !self.current_is_superuser_ext()? {
                return Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, "must be superuser to create a cast WITHOUT FUNCTION"));
            }
            let (si, ti) = (env.type_info(s), env.type_info(t));
            let phys = |i: &Option<pgarbor_types::registry::TypeInfo>| i.as_ref().map(|i| (i.len, i.byval, i.align));
            if phys(&si) != phys(&ti) {
                return invalid("source and target data types are not physically compatible");
            }
            if st == b'c' || tt == b'c' {
                return invalid("composite data types are not binary-compatible");
            }
            if pgarbor_types::registry::element_type_of(&env, s).is_some()
                || pgarbor_types::registry::element_type_of(&env, t).is_some()
            {
                return invalid("array data types are not binary-compatible");
            }
            if st == b'r' || tt == b'r' || st == b'm' || tt == b'm' {
                return invalid("range data types are not binary-compatible");
            }
            if st == b'e' || tt == b'e' {
                return invalid("enum data types are not binary-compatible");
            }
            if st == b'd' || tt == b'd' {
                return invalid("domain data types must not be marked binary-compatible");
            }
        }
        if s == t && nargs < 2 {
            return invalid("source data type and target data type are the same");
        }
        if env.find_cast(s, t).is_some() {
            return Err(PgError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("cast from type {} to type {} already exists", display_name(&env, s), display_name(&env, t)),
            ));
        }
        let oid = self.new_oid()?;
        self.put_ext(ExtObject::Cast(CastDef { oid, source: s, target: t, func: funcid, context, method }))?;
        self.record_created(ObjectAddress::new(ObjClass::Cast, oid));
        Ok(oid)
    }

    /// DROP CAST.
    pub fn drop_cast(
        &mut self,
        if_exists: bool,
        source: &pgarbor_types::names::TypeName,
        target: &pgarbor_types::names::TypeName,
        cascade: bool,
    ) -> Result<()> {
        let env = self.env()?;
        let resolve = |tn: &pgarbor_types::names::TypeName| -> Result<Option<Oid>> {
            match resolve_tn(&env, tn) {
                Ok(o) => Ok(Some(o)),
                Err(e) if if_exists && e.sqlstate == SqlState::UNDEFINED_OBJECT => Ok(None),
                Err(e) => Err(e),
            }
        };
        let Some(s) = resolve(source)? else {
            self.notice(PgError::notice(format!("type \"{}\" does not exist, skipping", tn_string(source))));
            return Ok(());
        };
        let Some(t) = resolve(target)? else {
            self.notice(PgError::notice(format!("type \"{}\" does not exist, skipping", tn_string(target))));
            return Ok(());
        };
        let Some(cast) = env.find_cast(s, t) else {
            let (sn, tn) = (display_name(&env, s), display_name(&env, t));
            if if_exists {
                self.notice(PgError::notice(format!("cast from type {sn} to type {tn} does not exist, skipping")));
                return Ok(());
            }
            return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("cast from type {sn} to type {tn} does not exist")));
        };
        let addr = ObjectAddress::new(ObjClass::Cast, cast.oid);
        if !addr.is_pinned() {
            self.require_cast_owner(s, t)?;
        }
        self.drop_objects(&[addr], if cascade { DropBehavior::Cascade } else { DropBehavior::Restrict })
    }

    // ---------------------------------------------------------------- conversions

    /// CREATE [DEFAULT] CONVERSION.
    pub fn create_conversion(
        &mut self,
        is_default: bool,
        name: &QName,
        for_enc: &str,
        to_enc: &str,
        func: &QName,
    ) -> Result<Oid> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;
        let from = encoding_number(for_enc);
        if from < 0 {
            return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("source encoding \"{for_enc}\" does not exist")));
        }
        let to = encoding_number(to_enc);
        if to < 0 {
            return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("destination encoding \"{to_enc}\" does not exist")));
        }
        if from == 0 || to == 0 {
            return Err(PgError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "encoding conversion to or from \"SQL_ASCII\" is not supported",
            ));
        }
        let args = [oids::INT4, oids::INT4, oids::CSTRING, oids::INTERNAL, oids::INT4, oids::BOOL];
        let f = lookup_func_exact(&env, func, &args, false)?.unwrap_or(0);
        if func_rettype(&env, f) != oids::INT4 {
            return Err(PgError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("encoding conversion function {} must return type integer", func.display()),
            ));
        }
        // The function is called with an empty string; the builtin conversion procs check their
        // encoding pair first (`check_encoding_conversion_args`).
        if pgarbor_types::catalog_data::builtin_proc(f).is_some() {
            let pairs: Vec<(i32, i32)> =
                builtin::ext_rows::CONVERSIONS.iter().filter(|c| c.4 == f).map(|c| (c.2, c.3)).collect();
            if !pairs.is_empty() && !pairs.contains(&(from, to)) {
                let srcs: Vec<i32> = pairs.iter().map(|p| p.0).collect();
                let dsts: Vec<i32> = pairs.iter().map(|p| p.1).collect();
                if !srcs.contains(&from) && srcs.iter().all(|s| *s == srcs[0]) {
                    return Err(PgError::internal(format!(
                        "expected source encoding \"{}\", but got \"{}\"",
                        encoding_name(srcs[0]),
                        encoding_name(from)
                    )));
                }
                if !dsts.contains(&to) && dsts.iter().all(|d| *d == dsts[0]) {
                    return Err(PgError::internal(format!(
                        "expected destination encoding \"{}\", but got \"{}\"",
                        encoding_name(dsts[0]),
                        encoding_name(to)
                    )));
                }
            }
        } else {
            return Err(PgError::unimplemented("CAT-EXT-04", "calling a user-defined encoding conversion function"));
        }
        // ConversionCreate.
        if env.all_conversions().iter().any(|c| c.namespace == ns && c.name == name.name) {
            return Err(PgError::new(SqlState::DUPLICATE_OBJECT, format!("conversion \"{}\" already exists", name.name)));
        }
        if is_default
            && env.all_conversions().iter().any(|c| c.namespace == ns && c.for_encoding == from && c.to_encoding == to && c.is_default)
        {
            return Err(PgError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("default conversion for {} to {} already exists", encoding_name(from), encoding_name(to)),
            ));
        }
        let oid = self.new_oid()?;
        self.put_ext(ExtObject::Conversion(Conversion {
            oid,
            name: name.name.clone(),
            namespace: ns,
            owner: cur,
            for_encoding: from,
            to_encoding: to,
            proc: f,
            is_default,
        }))?;
        self.record_created(ObjectAddress::new(ObjClass::Conversion, oid));
        Ok(oid)
    }

    pub(crate) fn conversion_addr(&mut self, name: &QName) -> Result<ObjectAddress> {
        let env = self.env()?;
        match env.lookup_conversion(name.schema.as_deref(), &name.name)? {
            Some(o) => Ok(ObjectAddress::new(ObjClass::Conversion, o)),
            None => Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("conversion \"{}\" does not exist", name.display()))),
        }
    }

    /// DROP CONVERSION / DROP COLLATION (simple named objects of one schema).
    pub(crate) fn drop_named_ext(&mut self, class: ObjClass, word: &str, if_exists: bool, names: &[QName], cascade: bool) -> Result<()> {
        let env = self.env()?;
        let mut targets = Vec::new();
        for n in names {
            if let Some(s) = &n.schema {
                if env.lookup_namespace(s).is_none() {
                    if if_exists {
                        self.notice(PgError::notice(format!("schema \"{s}\" does not exist, skipping")));
                        continue;
                    }
                    return Err(errors::schema_does_not_exist(s));
                }
            }
            let found = match class {
                ObjClass::Conversion => env.lookup_conversion(n.schema.as_deref(), &n.name)?,
                _ => env.collation_oid(n.schema.as_deref(), &n.name).ok(),
            };
            match found {
                Some(o) => {
                    let addr = ObjectAddress::new(class, o);
                    if !addr.is_pinned() {
                        self.require_owner(&addr)?;
                    }
                    targets.push(addr);
                }
                None => {
                    if if_exists {
                        self.notice(PgError::notice(format!("{word} \"{}\" does not exist, skipping", n.display())));
                    } else if class == ObjClass::Collation {
                        return Err(errors::collation_does_not_exist(&n.display()));
                    } else {
                        return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("{word} \"{}\" does not exist", n.display())));
                    }
                }
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        self.drop_objects(&targets, if cascade { DropBehavior::Cascade } else { DropBehavior::Restrict })
    }

    // ---------------------------------------------------------------- collations

    /// CREATE COLLATION (`DefineCollation` + `CollationCreate`).
    pub fn create_collation(&mut self, if_not_exists: bool, name: &QName, from: Option<&QName>, defs: &[DefItem]) -> Result<Option<Oid>> {
        let env = self.env()?;
        let ns = env.creation_namespace_for(name.schema.as_deref(), &name.name)?;
        let cur = self.current_user();
        self.require_schema_create_for(cur, ns)?;
        let mut locale_el: Option<&DefItem> = None;
        let mut lccollate_el: Option<&DefItem> = None;
        let mut lcctype_el: Option<&DefItem> = None;
        let mut provider_el: Option<&DefItem> = None;
        let mut deterministic_el: Option<&DefItem> = None;
        let mut rules_el: Option<&DefItem> = None;
        let mut version_el: Option<&DefItem> = None;
        for d in defs {
            let slot = match d.name.as_str() {
                "locale" => &mut locale_el,
                "lc_collate" => &mut lccollate_el,
                "lc_ctype" => &mut lcctype_el,
                "provider" => &mut provider_el,
                "deterministic" => &mut deterministic_el,
                "rules" => &mut rules_el,
                "version" => &mut version_el,
                "from" => {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
                        .with_detail("FROM cannot be specified together with any other options."))
                }
                other => {
                    return Err(PgError::new(SqlState::SYNTAX_ERROR, format!("collation attribute \"{other}\" not recognized")))
                }
            };
            if slot.is_some() {
                return Err(PgError::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options"));
            }
            *slot = Some(d);
        }
        if locale_el.is_some() && (lccollate_el.is_some() || lcctype_el.is_some()) {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
                .with_detail("LOCALE cannot be specified together with LC_COLLATE or LC_CTYPE."));
        }
        let invalid = |m: String| PgError::new(SqlState::INVALID_OBJECT_DEFINITION, m);
        let (provider, deterministic, encoding, collate, ctype, locale, rules, mut version);
        if let Some(f) = from {
            let oid = env.collation_oid(f.schema.as_deref(), &f.name)?;
            let c = env.collation_info(oid).ok_or_else(|| errors::cache_lookup_failed("collation", oid))?;
            if c.provider == b'd' {
                return Err(invalid("collation \"default\" cannot be copied".into()));
            }
            provider = c.provider;
            deterministic = c.deterministic;
            encoding = c.encoding;
            collate = c.collate.clone();
            ctype = c.ctype.clone();
            locale = c.locale.clone();
            rules = c.icu_rules.clone();
            version = None;
        } else {
            let pstr = match provider_el {
                Some(d) => Some(def_string(d)?),
                None => None,
            };
            deterministic = match deterministic_el {
                Some(d) => def_bool(d)?,
                None => true,
            };
            let r = match rules_el {
                Some(d) => Some(def_string(d)?),
                None => None,
            };
            version = match version_el {
                Some(d) => Some(def_string(d)?),
                None => None,
            };
            provider = match pstr.as_deref().map(|s| s.to_ascii_lowercase()) {
                None => b'c',
                Some(p) if p == "builtin" => b'b',
                Some(p) if p == "icu" => b'i',
                Some(p) if p == "libc" => b'c',
                Some(_) => {
                    return Err(invalid(format!("unrecognized collation provider: {}", pstr.unwrap_or_default())));
                }
            };
            let mut cc: Option<String> = None;
            let mut ct: Option<String> = None;
            let mut lc: Option<String> = None;
            if let Some(d) = locale_el {
                let v = def_string(d)?;
                if provider == b'c' {
                    cc = Some(v.clone());
                    ct = Some(v);
                } else {
                    lc = Some(v);
                }
            }
            if let Some(d) = lccollate_el {
                cc = Some(def_string(d)?);
            }
            if let Some(d) = lcctype_el {
                ct = Some(def_string(d)?);
            }
            match provider {
                b'b' => {
                    let Some(l) = lc.clone() else {
                        return Err(invalid("parameter \"locale\" must be specified".into()));
                    };
                    let canon = match l.as_str() {
                        "C" => "C",
                        "C.UTF-8" | "C.utf8" => "C.UTF-8",
                        "PG_UNICODE_FAST" => "PG_UNICODE_FAST",
                        _ => {
                            return Err(PgError::new(
                                SqlState::INVALID_PARAMETER_VALUE,
                                format!("invalid locale name \"{l}\" for builtin provider"),
                            ))
                        }
                    };
                    lc = Some(canon.to_string());
                }
                b'c' => {
                    if cc.is_none() {
                        return Err(invalid("parameter \"lc_collate\" must be specified".into()));
                    }
                    if ct.is_none() {
                        return Err(invalid("parameter \"lc_ctype\" must be specified".into()));
                    }
                }
                _ => {
                    let Some(l) = lc.clone() else {
                        return Err(invalid("parameter \"locale\" must be specified".into()));
                    };
                    let tag = icu_language_tag(&l);
                    if tag != l {
                        self.notice(PgError::notice(format!("using standard form \"{tag}\" for ICU locale \"{l}\"")));
                    }
                    lc = Some(tag);
                }
            }
            if !deterministic && provider != b'i' {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "nondeterministic collations not supported with this provider",
                ));
            }
            if r.is_some() && provider != b'i' {
                return Err(PgError::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "ICU rules cannot be specified unless locale provider is ICU",
                ));
            }
            if r.is_some() {
                return Err(PgError::unimplemented("CAT-EXT-04", "ICU collation rules (no ICU rule parser)"));
            }
            encoding = match provider {
                b'b' => {
                    if lc.as_deref() == Some("C") {
                        -1
                    } else {
                        builtin::UTF8_ENCODING
                    }
                }
                b'i' => -1,
                _ => builtin::UTF8_ENCODING,
            };
            collate = cc;
            ctype = ct;
            locale = lc;
            rules = r;
        }
        if version.is_none() {
            version = match provider {
                b'c' => collate.as_deref().and_then(libc_version),
                b'b' => match locale.as_deref() {
                    Some("C") | None => None,
                    Some(_) => Some("1".to_string()),
                },
                _ => locale.as_deref().map(icu_version),
            };
        }
        // CollationCreate: name clash (same encoding, then the any-encoding / database-encoding shadow).
        let env = self.env()?;
        let clash = |enc: i32| -> bool {
            (ns == builtin::PG_CATALOG_NAMESPACE && builtin::COLLATIONS.iter().any(|c| c.name == name.name && c.encoding == enc))
                || env.snapshot().user_collations().any(|c| c.namespace == ns && c.name == name.name && c.encoding == enc)
        };
        let enc_name = encoding_name(encoding);
        if clash(encoding) {
            let msg = if encoding == -1 {
                format!("collation \"{}\" already exists", name.name)
            } else {
                format!("collation \"{}\" for encoding \"{enc_name}\" already exists", name.name)
            };
            if if_not_exists {
                self.notice(PgError::notice(format!("{msg}, skipping")));
                return Ok(None);
            }
            return Err(PgError::new(SqlState::DUPLICATE_OBJECT, msg));
        }
        let other = if encoding == -1 { builtin::UTF8_ENCODING } else { -1 };
        if clash(other) {
            if if_not_exists {
                self.notice(PgError::notice(format!("collation \"{}\" already exists, skipping", name.name)));
                return Ok(None);
            }
            return Err(PgError::new(SqlState::DUPLICATE_OBJECT, format!("collation \"{}\" already exists", name.name)));
        }
        // pg_newlocale_from_collation: the libc locale must exist.
        if provider == b'c' {
            for l in [collate.as_deref(), ctype.as_deref()].into_iter().flatten() {
                if !libc_locale_exists(l) {
                    return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("could not create locale \"{l}\": No such file or directory"))
                        .with_detail(format!("The operating system could not find any locale data for the locale name \"{l}\".")));
                }
            }
        }
        let oid = self.new_oid()?;
        self.put_ext(ExtObject::Collation(Collation {
            oid,
            name: name.name.clone(),
            namespace: ns,
            owner: cur,
            provider,
            deterministic,
            encoding,
            collate,
            ctype,
            locale,
            icu_rules: rules,
            version,
        }))?;
        self.record_created(ObjectAddress::new(ObjClass::Collation, oid));
        Ok(Some(oid))
    }

    /// ALTER COLLATION ... REFRESH VERSION / RENAME / OWNER / SET SCHEMA.
    pub fn alter_collation(&mut self, name: &QName, action: Option<&AlterExt>) -> Result<()> {
        let env = self.env()?;
        let oid = env.collation_oid(name.schema.as_deref(), &name.name)?;
        let addr = ObjectAddress::new(ObjClass::Collation, oid);
        match action {
            Some(a) => self.alter_ext_generic(&addr, a),
            None => {
                if oid == 100 {
                    return Err(PgError::new(SqlState::INTERNAL_ERROR, "cannot refresh version of default collation")
                        .with_hint("Use ALTER DATABASE ... REFRESH COLLATION VERSION instead."));
                }
                if self.require_owner(&addr).is_err() {
                    return Err(errors::must_be_owner("collation", &name.display()));
                }
                self.notice(PgError::notice("version has not changed"));
                Ok(())
            }
        }
    }

    // ---------------------------------------------------------------- extensions

    /// CREATE EXTENSION: `plpgsql` is installed; other names are unavailable (or contrib modules
    /// PGArbor cannot load).
    pub fn create_extension(&mut self, if_not_exists: bool, name: &str, schema: Option<&str>) -> Result<()> {
        let env = self.env()?;
        if env.extension_oid(name).is_some() {
            if if_not_exists {
                self.notice(PgError::notice(format!("extension \"{name}\" already exists, skipping")));
                return Ok(());
            }
            return Err(PgError::new(SqlState::DUPLICATE_OBJECT, format!("extension \"{name}\" already exists")));
        }
        if let Some(s) = schema {
            env.namespace_oid(s)?;
        }
        if CONTRIB_EXTENSIONS.contains(&name) {
            return Err(PgError::unimplemented("CAT-EXT-05", format!("CREATE EXTENSION {name}")));
        }
        Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("extension \"{name}\" is not available"))
            .with_hint("The extension must first be installed on the system where PostgreSQL is running."))
    }

    /// DROP EXTENSION.
    pub fn drop_extensions(&mut self, if_exists: bool, names: &[String], cascade: bool) -> Result<()> {
        let env = self.env()?;
        let mut targets = Vec::new();
        for n in names {
            match env.extension_oid(n) {
                Some(o) => {
                    let addr = ObjectAddress::new(ObjClass::Extension, o);
                    if !addr.is_pinned() {
                        self.require_owner(&addr)?;
                    }
                    targets.push(addr);
                }
                None => {
                    if if_exists {
                        self.notice(PgError::notice(format!("extension \"{n}\" does not exist, skipping")));
                    } else {
                        return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("extension \"{n}\" does not exist")));
                    }
                }
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        if targets.iter().any(|t| t.is_pinned()) {
            return Err(PgError::unimplemented("CAT-EXT-06", "DROP EXTENSION of the built-in plpgsql extension"));
        }
        self.drop_objects(&targets, if cascade { DropBehavior::Cascade } else { DropBehavior::Restrict })
    }

    /// ALTER EXTENSION name UPDATE [TO version] / SET SCHEMA.
    pub fn alter_extension(&mut self, name: &str, version: Option<&str>, schema: Option<&str>) -> Result<()> {
        let env = self.env()?;
        let Some(oid) = env.extension_oid(name) else {
            return Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("extension \"{name}\" does not exist")));
        };
        let addr = ObjectAddress::new(ObjClass::Extension, oid);
        if self.require_owner(&addr).is_err() {
            return Err(errors::must_be_owner("extension", name));
        }
        let (cur_version, ns, relocatable) = match env.snapshot().user_extension(oid) {
            Some(e) => (e.version.clone(), e.namespace, e.relocatable),
            None => match builtin::EXTENSIONS.iter().find(|e| e.oid == oid) {
                Some(e) => (e.version.to_string(), e.namespace, e.relocatable),
                None => return Err(errors::cache_lookup_failed("extension", oid)),
            },
        };
        if let Some(s) = schema {
            let target = env.namespace_oid(s)?;
            if target == ns {
                return Ok(());
            }
            if !relocatable {
                return Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("extension \"{name}\" does not support SET SCHEMA"),
                ));
            }
            return Err(PgError::unimplemented("CAT-EXT-07", "ALTER EXTENSION SET SCHEMA"));
        }
        let target = version.map(str::to_string).unwrap_or_else(|| cur_version.clone());
        if target == cur_version {
            self.notice(PgError::notice(format!("version \"{cur_version}\" of extension \"{name}\" is already installed")));
            return Ok(());
        }
        Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("extension \"{name}\" has no update path from version \"{cur_version}\" to version \"{target}\""),
        ))
    }
}

/// `IsBinaryCoercibleWithCast`: binary-coercible, ignoring the cast OID.
fn binary_coercible_with_cast(env: &CatalogEnv, s: Oid, t: Oid) -> Result<bool> {
    pgarbor_types::cast::lookup::is_binary_coercible(env, s, t)
}
