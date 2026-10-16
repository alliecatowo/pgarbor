//! Lookups of extensibility objects (amendment pgarbor-catalog-09): operators, aggregate
//! definitions, operator families / classes, casts, conversions, collations, extensions, access
//! methods and encodings. Builtin rows come from `pgarbor_types::catalog_data` and `crate::builtin`;
//! user rows from `CatalogData::ext` (`ExtObject`).
//!
//! PG references: `catalog/namespace.c` (`OpernameGetOprid`, `OpernameGetCandidates`,
//! `OperatorIsVisible`, `OpclassnameGetOpcid`, `OpfamilynameGetOpfid`, `CollationGetCollid`,
//! `ConversionGetConid`), `parser/parse_oper.c` (`LookupOperName`), `commands/opclasscmds.c`
//! (`get_opclass_oid`, `get_opfamily_oid`), `common/encnames.c`.
use crate::builtin;
use crate::env::CatalogEnv;
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{self, CastContext, CastMethod, OprKind};
use pgarbor_types::names::display_name;
use pgarbor_types::resolve::oper::OperCandidate;
use pgarbor_types::text::format::quote_identifier;

/// Everything about one operator, builtin or user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperInfo {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    /// b'b' infix, b'l' prefix.
    pub kind: u8,
    pub left: Oid,
    pub right: Oid,
    pub result: Oid,
    pub proc: Oid,
    pub commutator: Oid,
    pub negator: Oid,
    pub restrict: Oid,
    pub join: Oid,
    pub can_merge: bool,
    pub can_hash: bool,
    pub builtin: bool,
}

impl OperInfo {
    fn from_user(o: &Operator) -> OperInfo {
        OperInfo {
            oid: o.oid,
            name: o.name.clone(),
            namespace: o.namespace,
            owner: o.owner,
            kind: o.kind,
            left: o.left,
            right: o.right,
            result: o.result,
            proc: o.proc,
            commutator: o.commutator,
            negator: o.negator,
            restrict: o.restrict,
            join: o.join,
            can_merge: o.can_merge,
            can_hash: o.can_hash,
            builtin: false,
        }
    }
    fn from_builtin(o: &catalog_data::BuiltinOperator) -> OperInfo {
        OperInfo {
            oid: o.oid,
            name: o.name.to_string(),
            namespace: builtin::PG_CATALOG_NAMESPACE,
            owner: builtin::BOOTSTRAP_SUPERUSER,
            kind: if o.kind == OprKind::Prefix { b'l' } else { b'b' },
            left: o.left,
            right: o.right,
            result: o.result,
            proc: o.proc,
            commutator: o.commutator,
            negator: o.negator,
            restrict: 0,
            join: 0,
            can_merge: o.can_merge,
            can_hash: o.can_hash,
            builtin: true,
        }
    }
}

/// An operator family (builtin or user) without its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpFamilyInfo {
    pub oid: Oid,
    pub method: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
}

/// An operator class (builtin or user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpClassInfo {
    pub oid: Oid,
    pub method: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub family: Oid,
    pub input_type: Oid,
    pub is_default: bool,
    pub key_type: Oid,
}

/// A `pg_cast` row (builtin or user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastInfo {
    pub oid: Oid,
    pub source: Oid,
    pub target: Oid,
    pub func: Oid,
    pub context: u8,
    pub method: u8,
    pub builtin: bool,
}

/// A `pg_collation` row (builtin or user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollInfo {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub provider: u8,
    pub deterministic: bool,
    pub encoding: i32,
    pub collate: Option<String>,
    pub ctype: Option<String>,
    pub locale: Option<String>,
    pub icu_rules: Option<String>,
    pub version: Option<String>,
    pub builtin: bool,
}

/// A `pg_conversion` row (builtin or user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvInfo {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub for_encoding: i32,
    pub to_encoding: i32,
    pub proc: Oid,
    pub is_default: bool,
    pub builtin: bool,
}

fn cell_oid(c: Option<&str>) -> Oid {
    c.and_then(|s| s.parse::<Oid>().ok()).unwrap_or(0)
}

fn cell_str(c: Option<&str>) -> String {
    c.unwrap_or("").to_string()
}

/// Builtin operator families (`builtin::OPFAMILIES`).
pub fn builtin_opfamilies() -> Vec<OpFamilyInfo> {
    builtin::OPFAMILIES
        .rows
        .iter()
        .map(|r| OpFamilyInfo {
            oid: cell_oid(r.first().copied().flatten()),
            method: cell_oid(r.get(1).copied().flatten()),
            name: cell_str(r.get(2).copied().flatten()),
            namespace: cell_oid(r.get(3).copied().flatten()),
            owner: cell_oid(r.get(4).copied().flatten()),
        })
        .collect()
}

/// Builtin operator classes (`builtin::OPCLASSES`).
pub fn builtin_opclasses() -> Vec<OpClassInfo> {
    builtin::OPCLASSES
        .rows
        .iter()
        .map(|r| OpClassInfo {
            oid: cell_oid(r.first().copied().flatten()),
            method: cell_oid(r.get(1).copied().flatten()),
            name: cell_str(r.get(2).copied().flatten()),
            namespace: cell_oid(r.get(3).copied().flatten()),
            owner: cell_oid(r.get(4).copied().flatten()),
            family: cell_oid(r.get(5).copied().flatten()),
            input_type: cell_oid(r.get(6).copied().flatten()),
            is_default: r.get(7).copied().flatten() == Some("t"),
            key_type: cell_oid(r.get(8).copied().flatten()),
        })
        .collect()
}

fn coll_from_builtin(c: &builtin::BuiltinCollation) -> CollInfo {
    CollInfo {
        oid: c.oid,
        name: c.name.to_string(),
        namespace: c.namespace,
        owner: c.owner,
        provider: c.provider,
        deterministic: c.deterministic,
        encoding: c.encoding,
        collate: c.collate.map(str::to_string),
        ctype: c.ctype.map(str::to_string),
        locale: c.locale.map(str::to_string),
        icu_rules: c.icu_rules.map(str::to_string),
        version: c.version.map(str::to_string),
        builtin: true,
    }
}

fn coll_from_user(c: &Collation) -> CollInfo {
    CollInfo {
        oid: c.oid,
        name: c.name.clone(),
        namespace: c.namespace,
        owner: c.owner,
        provider: c.provider,
        deterministic: c.deterministic,
        encoding: c.encoding,
        collate: c.collate.clone(),
        ctype: c.ctype.clone(),
        locale: c.locale.clone(),
        icu_rules: c.icu_rules.clone(),
        version: c.version.clone(),
        builtin: false,
    }
}

// ------------------------------------------------------------------ encodings (encnames.c)

/// `pg_enc2name_tbl`: (encoding number, canonical name). Server encodings are < 35 (PG_SQL_ASCII 0 ..
/// PG_KOI8U 34 with the client-only ones after).
pub const ENCODINGS: &[(i32, &str)] = &[
    (0, "SQL_ASCII"),
    (1, "EUC_JP"),
    (2, "EUC_CN"),
    (3, "EUC_KR"),
    (4, "EUC_TW"),
    (5, "EUC_JIS_2004"),
    (6, "UTF8"),
    (7, "MULE_INTERNAL"),
    (8, "LATIN1"),
    (9, "LATIN2"),
    (10, "LATIN3"),
    (11, "LATIN4"),
    (12, "LATIN5"),
    (13, "LATIN6"),
    (14, "LATIN7"),
    (15, "LATIN8"),
    (16, "LATIN9"),
    (17, "LATIN10"),
    (18, "WIN1256"),
    (19, "WIN1258"),
    (20, "WIN866"),
    (21, "WIN874"),
    (22, "KOI8R"),
    (23, "WIN1251"),
    (24, "WIN1252"),
    (25, "ISO_8859_5"),
    (26, "ISO_8859_6"),
    (27, "ISO_8859_7"),
    (28, "ISO_8859_8"),
    (29, "WIN1250"),
    (30, "WIN1253"),
    (31, "WIN1254"),
    (32, "WIN1255"),
    (33, "WIN1257"),
    (34, "KOI8U"),
    (35, "SJIS"),
    (36, "BIG5"),
    (37, "GBK"),
    (38, "UHC"),
    (39, "GB18030"),
    (40, "JOHAB"),
    (41, "SHIFT_JIS_2004"),
];

/// `pg_encname_tbl` aliases (names compared after `clean_encoding_name`: lower-case, only
/// alphanumerics).
const ENCODING_ALIASES: &[(&str, i32)] = &[
    ("abc", 23),
    ("alt", 20),
    ("big5", 36),
    ("cp1250", 29),
    ("cp1251", 23),
    ("cp1252", 24),
    ("cp1253", 30),
    ("cp1254", 31),
    ("cp1255", 32),
    ("cp1256", 18),
    ("cp1257", 33),
    ("cp1258", 19),
    ("cp866", 20),
    ("cp874", 21),
    ("cp932", 35),
    ("cp936", 37),
    ("cp949", 38),
    ("eucjis2004", 5),
    ("eucjp", 1),
    ("euccn", 2),
    ("euckr", 3),
    ("euctw", 4),
    ("gb18030", 39),
    ("gbk", 37),
    ("iso88591", 8),
    ("iso885910", 13),
    ("iso885913", 14),
    ("iso885914", 15),
    ("iso885915", 16),
    ("iso885916", 17),
    ("iso88592", 9),
    ("iso88593", 10),
    ("iso88594", 11),
    ("iso88595", 25),
    ("iso88596", 26),
    ("iso88597", 27),
    ("iso88598", 28),
    ("iso88599", 12),
    ("johab", 40),
    ("koi8", 22),
    ("koi8r", 22),
    ("koi8u", 34),
    ("latin1", 8),
    ("latin10", 17),
    ("latin2", 9),
    ("latin3", 10),
    ("latin4", 11),
    ("latin5", 12),
    ("latin6", 13),
    ("latin7", 14),
    ("latin8", 15),
    ("latin9", 16),
    ("mskanji", 35),
    ("muleinternal", 7),
    ("shiftjis", 35),
    ("shiftjis2004", 41),
    ("sjis", 35),
    ("sqlascii", 0),
    ("tcvn", 19),
    ("tcvn5712", 19),
    ("uhc", 38),
    ("unicode", 6),
    ("utf8", 6),
    ("vscii", 19),
    ("win", 23),
    ("win1250", 29),
    ("win1251", 23),
    ("win1252", 24),
    ("win1253", 30),
    ("win1254", 31),
    ("win1255", 32),
    ("win1256", 18),
    ("win1257", 33),
    ("win1258", 19),
    ("win866", 20),
    ("win874", 21),
    ("win932", 35),
    ("win936", 37),
    ("win949", 38),
    ("win950", 36),
    ("windows1250", 29),
    ("windows1251", 23),
    ("windows1252", 24),
    ("windows1253", 30),
    ("windows1254", 31),
    ("windows1255", 32),
    ("windows1256", 18),
    ("windows1257", 33),
    ("windows1258", 19),
    ("windows866", 20),
    ("windows874", 21),
    ("windows932", 35),
    ("windows936", 37),
    ("windows949", 38),
    ("windows950", 36),
];

/// PG `pg_char_to_encoding`: -1 for unknown names.
pub fn encoding_number(name: &str) -> i32 {
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    ENCODING_ALIASES.iter().find(|(n, _)| *n == clean).map(|(_, e)| *e).unwrap_or(-1)
}

/// PG `pg_encoding_to_char`: "" for unknown numbers.
pub fn encoding_name(enc: i32) -> &'static str {
    ENCODINGS.iter().find(|(e, _)| *e == enc).map(|(_, n)| *n).unwrap_or("")
}

// ------------------------------------------------------------------ snapshot accessors

impl CatalogSnapshot {
    /// Every persisted extensibility object, ordered by key OID.
    pub fn ext_objects(&self) -> impl Iterator<Item = &ExtObject> {
        self.data().ext.values()
    }
    pub fn user_operators(&self) -> impl Iterator<Item = &Operator> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Operator(o) => Some(o),
            _ => None,
        })
    }
    pub fn user_operator(&self, oid: Oid) -> Option<&Operator> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::Operator(o)) => Some(o),
            _ => None,
        }
    }
    pub fn aggregate_def(&self, fnoid: Oid) -> Option<&AggregateDef> {
        match self.data().ext.get(&fnoid) {
            Some(ExtObject::Aggregate(a)) => Some(a),
            _ => None,
        }
    }
    pub fn user_aggregates(&self) -> impl Iterator<Item = &AggregateDef> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Aggregate(a) => Some(a),
            _ => None,
        })
    }
    pub fn user_opfamilies(&self) -> impl Iterator<Item = &OpFamily> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::OpFamily(f) => Some(f),
            _ => None,
        })
    }
    pub fn user_opfamily(&self, oid: Oid) -> Option<&OpFamily> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::OpFamily(f)) => Some(f),
            _ => None,
        }
    }
    pub fn user_opclasses(&self) -> impl Iterator<Item = &OpClass> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::OpClass(c) => Some(c),
            _ => None,
        })
    }
    pub fn user_opclass(&self, oid: Oid) -> Option<&OpClass> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::OpClass(c)) => Some(c),
            _ => None,
        }
    }
    pub fn user_casts(&self) -> impl Iterator<Item = &CastDef> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Cast(c) => Some(c),
            _ => None,
        })
    }
    pub fn user_cast(&self, oid: Oid) -> Option<&CastDef> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::Cast(c)) => Some(c),
            _ => None,
        }
    }
    pub fn user_conversions(&self) -> impl Iterator<Item = &Conversion> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Conversion(c) => Some(c),
            _ => None,
        })
    }
    pub fn user_conversion(&self, oid: Oid) -> Option<&Conversion> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::Conversion(c)) => Some(c),
            _ => None,
        }
    }
    pub fn user_collations(&self) -> impl Iterator<Item = &Collation> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Collation(c) => Some(c),
            _ => None,
        })
    }
    pub fn user_collation(&self, oid: Oid) -> Option<&Collation> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::Collation(c)) => Some(c),
            _ => None,
        }
    }
    pub fn user_extensions(&self) -> impl Iterator<Item = &Extension> {
        self.ext_objects().filter_map(|e| match e {
            ExtObject::Extension(x) => Some(x),
            _ => None,
        })
    }
    pub fn user_extension(&self, oid: Oid) -> Option<&Extension> {
        match self.data().ext.get(&oid) {
            Some(ExtObject::Extension(x)) => Some(x),
            _ => None,
        }
    }
    /// The extension an object is a member of (`pg_depend` deptype `e`).
    pub fn extension_member_of(&self, addr: &ObjectAddress) -> Option<Oid> {
        let whole = addr.whole();
        self.user_extensions().find(|e| e.members.contains(&whole)).map(|e| e.oid)
    }
    /// The operator family that holds `pg_amop` / `pg_amproc` row `oid`, if any.
    pub fn amop_owner(&self, oid: Oid) -> Option<(&OpFamily, bool)> {
        for f in self.user_opfamilies() {
            if f.operators.iter().any(|o| o.oid == oid) {
                return Some((f, true));
            }
            if f.procs.iter().any(|p| p.oid == oid) {
                return Some((f, false));
            }
        }
        None
    }
}

// ------------------------------------------------------------------ env lookups

impl CatalogEnv {
    /// `(namespace, path_pos)` pairs searched for a possibly-qualified name; qualified with a
    /// missing schema → 3F000.
    fn searched_namespaces(&self, schema: Option<&str>) -> Result<Vec<(Oid, u32)>> {
        Ok(match schema {
            Some(s) => {
                let ns = self.explicit_namespace(s).ok_or_else(|| crate::errors::schema_does_not_exist(s))?;
                vec![(ns, 0)]
            }
            None => self.path().effective.iter().enumerate().map(|(i, ns)| (*ns, i as u32)).collect(),
        })
    }

    /// Operator by OID (builtin or user).
    pub fn operator_info(&self, oid: Oid) -> Option<OperInfo> {
        if let Some(o) = catalog_data::builtin_operator(oid) {
            return Some(OperInfo::from_builtin(o));
        }
        self.snapshot().user_operator(oid).map(OperInfo::from_user)
    }

    /// Every operator (builtin and user) named `name` in namespace `ns`.
    fn operators_in_ns(&self, ns: Oid, name: &str) -> Vec<OperInfo> {
        let mut out = Vec::new();
        if ns == builtin::PG_CATALOG_NAMESPACE {
            out.extend(catalog_data::operators_named(name).map(OperInfo::from_builtin));
        }
        out.extend(self.snapshot().user_operators().filter(|o| o.namespace == ns && o.name == name).map(OperInfo::from_user));
        out
    }

    /// Operator resolution candidates for `[schema.]name` (PG `OpernameGetCandidates`): every
    /// operator with that name in the searched namespaces, `path_pos` = namespace position; an
    /// operator hidden by an identical signature earlier in the path is dropped by
    /// `resolve_operator`. Shell operators are included (resolution reports them).
    pub fn operator_candidates(&self, schema: Option<&str>, name: &str) -> Result<Vec<OperCandidate>> {
        let searched = self.searched_namespaces(schema)?;
        let mut out = Vec::new();
        for (ns, pos) in searched {
            for o in self.operators_in_ns(ns, name) {
                out.push(OperCandidate {
                    oid: o.oid,
                    name: o.name.clone(),
                    kind: if o.kind == b'l' { OprKind::Prefix } else { OprKind::Infix },
                    left: o.left,
                    right: o.right,
                    result: o.result,
                    proc: o.proc,
                    commutator: o.commutator,
                    negator: o.negator,
                    path_pos: pos,
                });
            }
        }
        Ok(out)
    }

    /// Exact operator lookup (PG `OpernameGetOprid`): `left = 0` for prefix operators.
    pub fn lookup_operator(&self, schema: Option<&str>, name: &str, left: Oid, right: Oid) -> Result<Option<Oid>> {
        for (ns, _) in self.searched_namespaces(schema)? {
            if let Some(o) = self.operators_in_ns(ns, name).into_iter().find(|o| o.left == left && o.right == right) {
                return Ok(Some(o.oid));
            }
        }
        Ok(None)
    }

    /// Exact lookup that does not fail on a missing schema (returns `None`).
    pub fn lookup_operator_opt(&self, schema: Option<&str>, name: &str, left: Oid, right: Oid) -> Option<Oid> {
        self.lookup_operator(schema, name, left, right).ok().flatten()
    }

    /// PG `OperatorIsVisible`.
    pub fn operator_is_visible(&self, oid: Oid) -> bool {
        let Some(info) = self.operator_info(oid) else { return false };
        if !self.path().effective.contains(&info.namespace) {
            return false;
        }
        match self.lookup_operator(None, &info.name, info.left, info.right) {
            Ok(Some(found)) => found == oid,
            _ => false,
        }
    }

    /// `regoperatorout` text: `name(left,right)` (`NONE` for a missing side), schema-qualified when
    /// the operator is not visible.
    pub fn format_operator(&self, oid: Oid) -> Option<String> {
        let info = self.operator_info(oid)?;
        let mut out = String::new();
        if !self.operator_is_visible(oid) {
            let ns = self.snapshot().namespace(info.namespace).map(|n| n.name.clone()).unwrap_or_default();
            out.push_str(&quote_identifier(&ns).unwrap_or(ns));
            out.push('.');
        }
        out.push_str(&info.name);
        out.push('(');
        if info.left == 0 {
            out.push_str("NONE");
        } else {
            out.push_str(&display_name(self, info.left));
        }
        out.push(',');
        if info.right == 0 {
            out.push_str("NONE");
        } else {
            out.push_str(&display_name(self, info.right));
        }
        out.push(')');
        Some(out)
    }

    /// `regoperout` text: the bare name when it is the only visible operator of that name, else
    /// schema-qualified.
    pub fn format_oper_name(&self, oid: Oid) -> Option<String> {
        let info = self.operator_info(oid)?;
        let cands = self.operator_candidates(None, &info.name).unwrap_or_default();
        if cands.len() == 1 && cands[0].oid == oid {
            return Some(info.name);
        }
        let ns = self.snapshot().namespace(info.namespace).map(|n| n.name.clone()).unwrap_or_default();
        Some(format!("{}.{}", quote_identifier(&ns).unwrap_or(ns), info.name))
    }

    // -------------------------------------------------------------- access methods

    /// Access method OID by name.
    pub fn access_method_oid(&self, name: &str) -> Option<Oid> {
        builtin::ACCESS_METHODS.iter().find(|a| a.name == name).map(|a| a.oid)
    }
    /// Access method name by OID.
    pub fn access_method_name(&self, oid: Oid) -> Option<&'static str> {
        builtin::ACCESS_METHODS.iter().find(|a| a.oid == oid).map(|a| a.name)
    }

    // -------------------------------------------------------------- operator families / classes

    pub fn opfamily_info(&self, oid: Oid) -> Option<OpFamilyInfo> {
        if let Some(f) = self.snapshot().user_opfamily(oid) {
            return Some(OpFamilyInfo {
                oid: f.oid,
                method: f.method,
                name: f.name.clone(),
                namespace: f.namespace,
                owner: f.owner,
            });
        }
        if oid < pgarbor_common::FIRST_NORMAL_OID {
            return builtin_opfamilies().into_iter().find(|f| f.oid == oid);
        }
        None
    }

    pub fn opclass_info(&self, oid: Oid) -> Option<OpClassInfo> {
        if let Some(c) = self.snapshot().user_opclass(oid) {
            return Some(OpClassInfo {
                oid: c.oid,
                method: c.method,
                name: c.name.clone(),
                namespace: c.namespace,
                owner: c.owner,
                family: c.family,
                input_type: c.input_type,
                is_default: c.is_default,
                key_type: c.key_type,
            });
        }
        if oid < pgarbor_common::FIRST_NORMAL_OID {
            return builtin_opclasses().into_iter().find(|c| c.oid == oid);
        }
        None
    }

    /// All operator families (builtin then user).
    pub fn all_opfamilies(&self) -> Vec<OpFamilyInfo> {
        let mut v = builtin_opfamilies();
        v.extend(self.snapshot().user_opfamilies().map(|f| OpFamilyInfo {
            oid: f.oid,
            method: f.method,
            name: f.name.clone(),
            namespace: f.namespace,
            owner: f.owner,
        }));
        v
    }

    /// All operator classes (builtin then user).
    pub fn all_opclasses(&self) -> Vec<OpClassInfo> {
        let mut v = builtin_opclasses();
        v.extend(self.snapshot().user_opclasses().filter_map(|c| self.opclass_info(c.oid)));
        v
    }

    /// PG `get_opfamily_oid(amoid, name, missing_ok=true)`.
    pub fn lookup_opfamily(&self, method: Oid, schema: Option<&str>, name: &str) -> Result<Option<Oid>> {
        let all = self.all_opfamilies();
        for (ns, _) in self.searched_namespaces(schema)? {
            if let Some(f) = all.iter().find(|f| f.namespace == ns && f.method == method && f.name == name) {
                return Ok(Some(f.oid));
            }
        }
        Ok(None)
    }

    /// PG `get_opclass_oid(amoid, name, missing_ok=true)`.
    pub fn lookup_opclass(&self, method: Oid, schema: Option<&str>, name: &str) -> Result<Option<Oid>> {
        let all = self.all_opclasses();
        for (ns, _) in self.searched_namespaces(schema)? {
            if let Some(c) = all.iter().find(|c| c.namespace == ns && c.method == method && c.name == name) {
                return Ok(Some(c.oid));
            }
        }
        Ok(None)
    }

    /// `pg_amop` rows of a family (builtin family: compiled-in rows; user family: its members).
    pub fn family_operators(&self, family: Oid) -> Vec<AmOpEntry> {
        if let Some(f) = self.snapshot().user_opfamily(family) {
            return f.operators.clone();
        }
        builtin::ext_rows::AMOPS
            .iter()
            .filter(|r| r.1 == family)
            .map(|r| AmOpEntry {
                oid: r.0,
                lefttype: r.2,
                righttype: r.3,
                strategy: r.4,
                purpose: r.5,
                operator: r.6,
                sortfamily: r.8,
                opclass: 0,
            })
            .collect()
    }

    /// `pg_amproc` rows of a family.
    pub fn family_procs(&self, family: Oid) -> Vec<AmProcEntry> {
        if let Some(f) = self.snapshot().user_opfamily(family) {
            return f.procs.clone();
        }
        builtin::ext_rows::AMPROCS
            .iter()
            .filter(|r| r.1 == family)
            .map(|r| AmProcEntry { oid: r.0, lefttype: r.2, righttype: r.3, procnum: r.4, proc: r.5, opclass: 0 })
            .collect()
    }

    /// Qualified-if-invisible display name for an object in `namespace` (opclass, opfamily,
    /// collation, conversion): visible when the namespace is in the path and no same-named object
    /// earlier in the path (`same_name_ns` lists the namespaces holding that name).
    pub fn ext_display_name(&self, namespace: Oid, name: &str, same_name_ns: &[Oid]) -> String {
        let q = quote_identifier(name).unwrap_or_else(|_| name.to_string());
        let first = self.path().effective.iter().find(|ns| same_name_ns.contains(ns));
        if first == Some(&namespace) {
            return q;
        }
        let ns = self.snapshot().namespace(namespace).map(|n| n.name.clone()).unwrap_or_default();
        format!("{}.{}", quote_identifier(&ns).unwrap_or(ns), q)
    }

    // -------------------------------------------------------------- collations

    /// Collation by OID (builtin or user).
    pub fn collation_info(&self, oid: Oid) -> Option<CollInfo> {
        if let Some(c) = builtin::builtin_collation(oid) {
            return Some(coll_from_builtin(c));
        }
        self.snapshot().user_collation(oid).map(coll_from_user)
    }

    /// Collation usable with the database encoding (UTF8) named `name` in `ns`.
    pub fn collation_in_ns(&self, ns: Oid, name: &str) -> Option<Oid> {
        let usable = |enc: i32| enc == -1 || enc == builtin::UTF8_ENCODING;
        if ns == builtin::PG_CATALOG_NAMESPACE {
            if let Some(c) = builtin::COLLATIONS.iter().find(|c| c.name == name && usable(c.encoding)) {
                return Some(c.oid);
            }
        }
        self.snapshot().user_collations().find(|c| c.namespace == ns && c.name == name && usable(c.encoding)).map(|c| c.oid)
    }

    /// PG `CollationIsVisible`.
    pub fn collation_is_visible(&self, oid: Oid) -> bool {
        let Some(c) = self.collation_info(oid) else { return false };
        for ns in &self.path().effective {
            if let Some(found) = self.collation_in_ns(*ns, &c.name) {
                return found == oid;
            }
        }
        false
    }

    /// `quote_identifier(name)`, schema-qualified when the collation is not visible.
    pub fn format_collation(&self, oid: Oid) -> Option<String> {
        let c = self.collation_info(oid)?;
        let q = quote_identifier(&c.name).unwrap_or_else(|_| c.name.clone());
        if self.collation_is_visible(oid) {
            return Some(q);
        }
        let ns = self.snapshot().namespace(c.namespace).map(|n| n.name.clone()).unwrap_or_default();
        Some(format!("{}.{}", quote_identifier(&ns).unwrap_or(ns), q))
    }

    // -------------------------------------------------------------- conversions

    /// Conversion by OID (builtin or user).
    pub fn conversion_info(&self, oid: Oid) -> Option<ConvInfo> {
        if let Some(c) = self.snapshot().user_conversion(oid) {
            return Some(ConvInfo {
                oid: c.oid,
                name: c.name.clone(),
                namespace: c.namespace,
                owner: c.owner,
                for_encoding: c.for_encoding,
                to_encoding: c.to_encoding,
                proc: c.proc,
                is_default: c.is_default,
                builtin: false,
            });
        }
        builtin::ext_rows::CONVERSIONS.iter().find(|c| c.0 == oid).map(|c| ConvInfo {
            oid: c.0,
            name: c.1.to_string(),
            namespace: builtin::PG_CATALOG_NAMESPACE,
            owner: builtin::BOOTSTRAP_SUPERUSER,
            for_encoding: c.2,
            to_encoding: c.3,
            proc: c.4,
            is_default: c.5,
            builtin: true,
        })
    }

    fn conversion_in_ns(&self, ns: Oid, name: &str) -> Option<Oid> {
        if ns == builtin::PG_CATALOG_NAMESPACE {
            if let Some(c) = builtin::ext_rows::CONVERSIONS.iter().find(|c| c.1 == name) {
                return Some(c.0);
            }
        }
        self.snapshot().user_conversions().find(|c| c.namespace == ns && c.name == name).map(|c| c.oid)
    }

    /// PG `get_conversion_oid(name, missing_ok=true)`.
    pub fn lookup_conversion(&self, schema: Option<&str>, name: &str) -> Result<Option<Oid>> {
        for (ns, _) in self.searched_namespaces(schema)? {
            if let Some(o) = self.conversion_in_ns(ns, name) {
                return Ok(Some(o));
            }
        }
        Ok(None)
    }

    /// Every conversion (builtin then user).
    pub fn all_conversions(&self) -> Vec<ConvInfo> {
        let mut v: Vec<ConvInfo> =
            builtin::ext_rows::CONVERSIONS.iter().filter_map(|c| self.conversion_info(c.0)).collect();
        v.extend(self.snapshot().user_conversions().filter_map(|c| self.conversion_info(c.oid)));
        v
    }

    // -------------------------------------------------------------- casts

    /// The `pg_cast` row for (source, target): a user cast wins over nothing; builtin casts and user
    /// casts never coexist for one pair (CREATE CAST refuses an existing pair).
    pub fn find_cast(&self, source: Oid, target: Oid) -> Option<CastInfo> {
        if let Some(c) = self.snapshot().user_casts().find(|c| c.source == source && c.target == target) {
            return Some(CastInfo {
                oid: c.oid,
                source: c.source,
                target: c.target,
                func: c.func,
                context: c.context,
                method: c.method,
                builtin: false,
            });
        }
        catalog_data::CASTS.iter().find(|c| c.source == source && c.target == target).map(|c| CastInfo {
            oid: c.oid,
            source: c.source,
            target: c.target,
            func: c.func,
            context: match c.context {
                CastContext::Implicit => b'i',
                CastContext::Assignment => b'a',
                CastContext::Explicit => b'e',
            },
            method: match c.method {
                CastMethod::Function => b'f',
                CastMethod::Binary => b'b',
                CastMethod::InOut => b'i',
            },
            builtin: true,
        })
    }

    /// Cast by OID (builtin or user).
    pub fn cast_info(&self, oid: Oid) -> Option<CastInfo> {
        if let Some(c) = self.snapshot().user_cast(oid) {
            return self.find_cast(c.source, c.target);
        }
        catalog_data::CASTS.iter().find(|c| c.oid == oid).and_then(|c| self.find_cast(c.source, c.target))
    }

    // -------------------------------------------------------------- extensions

    /// Installed extension OID by name (builtin plpgsql or user).
    pub fn extension_oid(&self, name: &str) -> Option<Oid> {
        if let Some(e) = self.snapshot().user_extensions().find(|e| e.name == name) {
            return Some(e.oid);
        }
        builtin::EXTENSIONS.iter().find(|e| e.name == name).map(|e| e.oid)
    }
}

/// 42704 `operator class "x" does not exist for access method "btree"`.
pub fn opclass_does_not_exist(name: &str, am: &str) -> PgError {
    PgError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("operator class \"{name}\" does not exist for access method \"{am}\""),
    )
}

/// 42704 `operator family "x" does not exist for access method "btree"`.
pub fn opfamily_does_not_exist(name: &str, am: &str) -> PgError {
    PgError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("operator family \"{name}\" does not exist for access method \"{am}\""),
    )
}

/// 42704 `access method "x" does not exist`.
pub fn access_method_does_not_exist(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("access method \"{name}\" does not exist"))
}

// ------------------------------------------------------------------ descriptions / ownership

impl CatalogEnv {
    /// Display name of an operator class (qualified when not visible: PG `OpclassIsVisible` looks for
    /// the first same-named class of the same access method along the path).
    pub fn format_opclass(&self, oid: Oid) -> Option<String> {
        let c = self.opclass_info(oid)?;
        let holders: Vec<Oid> =
            self.all_opclasses().iter().filter(|o| o.method == c.method && o.name == c.name).map(|o| o.namespace).collect();
        Some(self.ext_display_name(c.namespace, &c.name, &holders))
    }

    /// Display name of an operator family (PG `OpfamilyIsVisible`).
    pub fn format_opfamily(&self, oid: Oid) -> Option<String> {
        let f = self.opfamily_info(oid)?;
        let holders: Vec<Oid> =
            self.all_opfamilies().iter().filter(|o| o.method == f.method && o.name == f.name).map(|o| o.namespace).collect();
        Some(self.ext_display_name(f.namespace, &f.name, &holders))
    }

    /// Display name of a conversion (PG `ConversionIsVisible`).
    pub fn format_conversion(&self, oid: Oid) -> Option<String> {
        let c = self.conversion_info(oid)?;
        let holders: Vec<Oid> = self.all_conversions().iter().filter(|o| o.name == c.name).map(|o| o.namespace).collect();
        Some(self.ext_display_name(c.namespace, &c.name, &holders))
    }

    /// PG `getOpFamilyDescription`: `operator family x for access method btree`.
    pub fn opfamily_description(&self, oid: Oid) -> Option<String> {
        let f = self.opfamily_info(oid)?;
        let am = self.access_method_name(f.method).unwrap_or("?");
        Some(format!("operator family {} for access method {am}", self.format_opfamily(oid)?))
    }

    /// `pg_amop` / `pg_amproc` row by OID with its family (builtin rows included).
    pub fn amop_row(&self, oid: Oid) -> Option<(Oid, AmOpEntry)> {
        if let Some((f, true)) = self.snapshot().amop_owner(oid) {
            return f.operators.iter().find(|o| o.oid == oid).map(|o| (f.oid, o.clone()));
        }
        builtin::ext_rows::AMOPS.iter().find(|r| r.0 == oid).map(|r| {
            (
                r.1,
                AmOpEntry {
                    oid: r.0,
                    lefttype: r.2,
                    righttype: r.3,
                    strategy: r.4,
                    purpose: r.5,
                    operator: r.6,
                    sortfamily: r.8,
                    opclass: 0,
                },
            )
        })
    }

    pub fn amproc_row(&self, oid: Oid) -> Option<(Oid, AmProcEntry)> {
        if let Some((f, false)) = self.snapshot().amop_owner(oid) {
            return f.procs.iter().find(|p| p.oid == oid).map(|p| (f.oid, p.clone()));
        }
        builtin::ext_rows::AMPROCS.iter().find(|r| r.0 == oid).map(|r| {
            (r.1, AmProcEntry { oid: r.0, lefttype: r.2, righttype: r.3, procnum: r.4, proc: r.5, opclass: 0 })
        })
    }
}

/// `getObjectDescription` for the extensibility classes (and collations/casts/extensions, builtin or
/// user). `None` when `addr.class` is not one of them; `Some(None)` when the object is missing.
pub fn describe_ext(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<Option<String>>> {
    use pgarbor_types::names::format_type;
    let oid = addr.oid;
    let s = match addr.class {
        ObjClass::Operator => env.format_operator(oid).map(|o| format!("operator {o}")),
        ObjClass::OpClass => match env.opclass_info(oid) {
            Some(c) => env
                .format_opclass(oid)
                .map(|n| format!("operator class {n} for access method {}", env.access_method_name(c.method).unwrap_or("?"))),
            None => None,
        },
        ObjClass::OpFamily => env.opfamily_description(oid),
        ObjClass::AmOp => match env.amop_row(oid) {
            Some((fam, row)) => Some(format!(
                "operator {} ({}, {}) of {}: {}",
                row.strategy,
                format_type(env, row.lefttype, -1)?,
                format_type(env, row.righttype, -1)?,
                env.opfamily_description(fam).unwrap_or_default(),
                env.format_operator(row.operator).unwrap_or_default()
            )),
            None => None,
        },
        ObjClass::AmProc => match env.amproc_row(oid) {
            Some((fam, row)) => {
                let proc = crate::describe::describe(env, &ObjectAddress::proc(row.proc))?
                    .map(|d| d.trim_start_matches("function ").to_string())
                    .unwrap_or_else(|| row.proc.to_string());
                Some(format!(
                    "function {} ({}, {}) of {}: {}",
                    row.procnum,
                    format_type(env, row.lefttype, -1)?,
                    format_type(env, row.righttype, -1)?,
                    env.opfamily_description(fam).unwrap_or_default(),
                    proc
                ))
            }
            None => None,
        },
        ObjClass::Conversion => env.format_conversion(oid).map(|n| format!("conversion {n}")),
        ObjClass::Collation => env.format_collation(oid).map(|n| format!("collation {n}")),
        ObjClass::Cast => match env.cast_info(oid) {
            Some(c) => Some(format!("cast from {} to {}", format_type(env, c.source, -1)?, format_type(env, c.target, -1)?)),
            None => None,
        },
        ObjClass::Extension => match env.snapshot().user_extension(oid) {
            Some(e) => Some(format!("extension {}", e.name)),
            None => builtin::EXTENSIONS.iter().find(|e| e.oid == oid).map(|e| format!("extension {}", e.name)),
        },
        _ => return Ok(None),
    };
    Ok(Some(s))
}

/// `pg_identify_object(...).type` words for the classes of [`describe_ext`].
pub fn type_description_ext(env: &CatalogEnv, addr: &ObjectAddress) -> Option<Option<&'static str>> {
    let oid = addr.oid;
    Some(match addr.class {
        ObjClass::Operator => env.operator_info(oid).map(|_| "operator"),
        ObjClass::OpClass => env.opclass_info(oid).map(|_| "operator class"),
        ObjClass::OpFamily => env.opfamily_info(oid).map(|_| "operator family"),
        ObjClass::AmOp => env.amop_row(oid).map(|_| "operator of access method"),
        ObjClass::AmProc => env.amproc_row(oid).map(|_| "function of access method"),
        ObjClass::Conversion => env.conversion_info(oid).map(|_| "conversion"),
        ObjClass::Collation => env.collation_info(oid).map(|_| "collation"),
        ObjClass::Cast => env.cast_info(oid).map(|_| "cast"),
        ObjClass::Extension => env.extension_name(oid).map(|_| "extension"),
        _ => return None,
    })
}

impl CatalogEnv {
    /// Extension name by OID (builtin or user).
    pub fn extension_name(&self, oid: Oid) -> Option<String> {
        if let Some(e) = self.snapshot().user_extension(oid) {
            return Some(e.name.clone());
        }
        builtin::EXTENSIONS.iter().find(|e| e.oid == oid).map(|e| e.name.to_string())
    }

    /// `(owner, must-be-owner word, bare name)` of an extensibility object; `None` for other classes
    /// or missing objects. Builtin objects belong to the bootstrap superuser.
    pub fn ext_owner_word_name(&self, addr: &ObjectAddress) -> Option<(Oid, &'static str, String)> {
        let oid = addr.oid;
        match addr.class {
            ObjClass::Operator => self.operator_info(oid).map(|o| (o.owner, "operator", o.name)),
            ObjClass::OpClass => self.opclass_info(oid).map(|c| (c.owner, "operator class", c.name)),
            ObjClass::OpFamily => self.opfamily_info(oid).map(|f| (f.owner, "operator family", f.name)),
            ObjClass::Conversion => self.conversion_info(oid).map(|c| (c.owner, "conversion", c.name)),
            ObjClass::Collation => self.collation_info(oid).map(|c| (c.owner, "collation", c.name)),
            ObjClass::Extension => match self.snapshot().user_extension(oid) {
                Some(e) => Some((e.owner, "extension", e.name.clone())),
                None => builtin::EXTENSIONS.iter().find(|e| e.oid == oid).map(|e| (e.owner, "extension", e.name.to_string())),
            },
            _ => None,
        }
    }
}
