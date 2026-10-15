//! SQL type names ↔ type OIDs: parsing type names written in SQL and PG's `format_type`
//! (TYPE-02). Lease TYP-01.
//!
//! PG18 reference: `src/backend/parser/gram.y` (SimpleTypename, GenericType, Numeric, Character,
//! ConstDatetime, ConstInterval, opt_float), `parser/parse_type.c` (`typenameTypeIdAndMod`,
//! `typenameTypeMod`), `utils/adt/format_type.c` (`format_type_extended`, `printTypmod`).
//!
//! `resolve_type_name(env, tn)`: maps the SQL spelling to a type OID, then computes the typmod with
//! `typmod::typmod_in`, then applies array bounds (`int[]`, `int[3][4]` → the array type; bounds are
//! ignored like PG). Accepted spellings (case-insensitive, already lower-cased by the binder unless quoted):
//! `smallint|int2`, `int|integer|int4`, `bigint|int8`, `real|float4`, `double precision|float8`,
//! `float` (no p → float8; `float(p)`: 1..=24 → float4, 25..=53 → float8, p < 1 → 22023
//! `precision for type float must be at least 1 bit`, p > 53 → 22023
//! `precision for type float must be less than 54 bits`), `numeric|decimal|dec[(p[,s])]`,
//! `boolean|bool`, `char|character[(n)]` → bpchar with DEFAULT LENGTH 1 when no n (typmod 5),
//! `char varying|character varying|varchar[(n)]`, `bpchar[(n)]` (no default length), `text`, `name`,
//! quoted `"char"` (`TypeName::quoted`) → the 1-byte "char" type (18), `bytea`, `date`,
//! `time[(p)] [without time zone]`, `time[(p)] with time zone|timetz`, `timestamp[(p)] [without time zone]`,
//! `timestamp[(p)] with time zone|timestamptz`, `interval [fields] [(p)]` (fields → `interval_fields`
//! range bits, e.g. YEAR → YEAR, DAY TO SECOND → DAY|HOUR|MINUTE|SECOND), `bit[(n)]` (default 1),
//! `bit varying|varbit`, `json`, `jsonb`, `uuid`, `oid`, `regclass`..., and any other catalog `typname`
//! via `env.type_by_name(schema, name)` (user enums/domains/composites, `_int4`, `int4range`...).
//! Unknown → 42704 `type "nosuchtype" does not exist`; typmod on a type without typmodin → 42601
//! `type modifier is not allowed for type "text"`. `serial`/`bigserial`/`smallserial` are NOT types:
//! the DDL binder expands them; here they resolve to 42704.
//!
//! `format_type(env, oid, typmod)` (PG `format_type_extended` with FORMAT_TYPE_TYPEMOD_GIVEN when
//! typmod >= 0), oracle-verified: varchar typmod 14 → `character varying(10)`; numeric (10,2) →
//! `numeric(10,2)`; timestamptz typmod 3 → `timestamp(3) with time zone`; `_int4` → `integer[]`;
//! bpchar typmod 5 → `character(1)`, bpchar no typmod → `bpchar`; interval YEAR → `interval year`;
//! float8 → `double precision`; time typmod 2 → `time(2) without time zone`; oid 0 → `-`; unknown oid →
//! `???`; `_varchar` typmod 14 → `character varying(10)[]`; bit typmod 5 → `bit(5)`; timetz →
//! `time with time zone`; `"char"` → `"char"`; int2 → `smallint`; int8 → `bigint`; bool → `boolean`;
//! float4 → `real`; timestamp → `timestamp without time zone`; varbit → `bit varying`. User types are
//! printed schema-qualified when not visible in the search path (`env.type_by_name(None, name) != Some(oid)`)
//! and quoted with `text::format::quote_identifier` when needed.
use crate::context::{RegKind, TypeEnv};
use crate::datetime::interval_fields as ivf;
use crate::datum::TypeId;
use crate::oids;
use crate::registry;
use crate::text::format::quote_identifier;
use crate::typmod;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// A type name as written in SQL (produced by pgarbor-sql from sqlparser's AST).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeName {
    /// Explicit schema qualification, if any.
    pub schema: Option<String>,
    /// Lower-cased (unless quoted) multi-word SQL spelling, single spaces: `"double precision"`,
    /// `"character varying"`, `"timestamp with time zone"`, `"int4"`, `"mytype"`.
    pub name: String,
    /// The name was a quoted identifier (`"char"`, `"MyType"`): no SQL keyword aliasing.
    pub quoted: bool,
    /// Integer type modifiers in order (`varchar(10)` → [10]; `numeric(10,2)` → [10, 2]).
    pub typmods: Vec<i32>,
    /// Number of `[]` suffixes (0 = not an array).
    pub array_dims: u32,
    /// Interval field restriction as `datetime::interval_fields` range bits (None = full range).
    pub interval_range: Option<i32>,
}

fn syntax_err(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, msg)
}

fn written_name(tn: &TypeName) -> String {
    match &tn.schema {
        Some(s) => format!("{}.{}", s, tn.name),
        None => tn.name.clone(),
    }
}

fn not_exist(tn: &TypeName, brackets: bool) -> PgError {
    let mut n = written_name(tn);
    if brackets {
        for _ in 0..tn.array_dims.max(1) {
            n.push_str("[]");
        }
    }
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("type \"{n}\" does not exist"))
}

/// Types that have a `typmodin` function in pg_type (their array types share it).
fn has_typmodin(oid: Oid) -> bool {
    matches!(
        oid,
        oids::BPCHAR
            | oids::VARCHAR
            | oids::NUMERIC
            | oids::TIME
            | oids::TIMETZ
            | oids::TIMESTAMP
            | oids::TIMESTAMPTZ
            | oids::INTERVAL
            | oids::BIT
            | oids::VARBIT
    )
}

/// SQL keyword spellings that map to a fixed catalog type (gram.y), independent of search_path.
fn simple_alias(name: &str) -> Option<Oid> {
    Some(match name {
        "smallint" => oids::INT2,
        "int" | "integer" => oids::INT4,
        "bigint" => oids::INT8,
        "real" => oids::FLOAT4,
        "double precision" => oids::FLOAT8,
        "decimal" | "dec" => oids::NUMERIC,
        "boolean" => oids::BOOL,
        "character varying" | "char varying" | "national character varying" | "national char varying"
        | "nchar varying" => oids::VARCHAR,
        "bit varying" => oids::VARBIT,
        "time without time zone" => oids::TIME,
        "time with time zone" => oids::TIMETZ,
        "timestamp without time zone" => oids::TIMESTAMP,
        "timestamp with time zone" => oids::TIMESTAMPTZ,
        _ => return None,
    })
}

/// PG `intervaltypmodin` semantics (a precision above 6 is only a WARNING there; clamped here).
fn interval_typmod(range: Option<i32>, mods: &[i32]) -> Result<i32> {
    if range.is_none() && mods.is_empty() {
        return Ok(-1);
    }
    if mods.len() > 1 {
        return Err(syntax_err("invalid INTERVAL type modifier"));
    }
    let precision = match mods.first() {
        None => ivf::FULL_PRECISION,
        Some(&p) if p < 0 => {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("INTERVAL({p}) precision must not be negative"),
            ))
        }
        Some(&p) if p > 6 => 6,
        Some(&p) => p,
    };
    let range = range.unwrap_or(ivf::FULL_RANGE);
    if range == ivf::FULL_RANGE && precision == ivf::FULL_PRECISION {
        return Ok(-1);
    }
    Ok((range << 16) | precision)
}

/// Resolve a SQL type name (see module docs).
// CONTRACT
pub fn resolve_type_name(env: &dyn TypeEnv, tn: &TypeName) -> Result<TypeId> {
    let mut mods: &[i32] = &tn.typmods;
    let name = tn.name.as_str();
    let keyword = tn.schema.is_none() && !tn.quoted;

    let mut default_typmod: Option<i32> = None;
    let mut oid: Option<Oid> = None;
    if keyword {
        match name {
            "float" => {
                let o = match mods {
                    [] => oids::FLOAT8,
                    [p] => {
                        if *p < 1 {
                            return Err(PgError::new(
                                SqlState::INVALID_PARAMETER_VALUE,
                                "precision for type float must be at least 1 bit",
                            ));
                        } else if *p <= 24 {
                            oids::FLOAT4
                        } else if *p <= 53 {
                            oids::FLOAT8
                        } else {
                            return Err(PgError::new(
                                SqlState::INVALID_PARAMETER_VALUE,
                                "precision for type float must be less than 54 bits",
                            ));
                        }
                    }
                    _ => return Err(syntax_err("syntax error at or near \",\"")),
                };
                oid = Some(o);
                mods = &[];
            }
            "char" | "character" | "nchar" | "national char" | "national character" => {
                oid = Some(oids::BPCHAR);
                default_typmod = Some(1 + 4);
            }
            "bit" => {
                oid = Some(oids::BIT);
                default_typmod = Some(1);
            }
            _ => oid = simple_alias(name),
        }
    }
    let oid = match oid {
        Some(o) => o,
        None => env.type_by_name(tn.schema.as_deref(), name).ok_or_else(|| not_exist(tn, false))?,
    };

    let typmod_v = if oid == oids::INTERVAL {
        interval_typmod(tn.interval_range, mods)?
    } else if mods.is_empty() {
        default_typmod.unwrap_or(-1)
    } else if !has_typmodin(oid) {
        return Err(syntax_err(format!("type modifier is not allowed for type \"{}\"", written_name(tn))));
    } else {
        typmod::typmod_in(oid, mods)?.0
    };

    if tn.array_dims > 0 {
        let arr = registry::array_type_of(env, oid).ok_or_else(|| not_exist(tn, true))?;
        return Ok(TypeId::new(arr, typmod_v));
    }
    Ok(TypeId::new(oid, typmod_v))
}

// ---------------------------------------------------------------------------------------------
// parse_type_string
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Int(i32),
    LParen,
    RParen,
    Comma,
    LBracket,
    RBracket,
    Dot,
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Word(w) => w.clone(),
        Tok::Quoted(q) => format!("\"{q}\""),
        Tok::Int(i) => i.to_string(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::Comma => ",".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::Dot => ".".into(),
    }
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if c == ',' {
            out.push(Tok::Comma);
            i += 1;
        } else if c == '[' {
            out.push(Tok::LBracket);
            i += 1;
        } else if c == ']' {
            out.push(Tok::RBracket);
            i += 1;
        } else if c == '.' {
            out.push(Tok::Dot);
            i += 1;
        } else if c == '"' {
            let mut j = i + 1;
            let mut buf = String::new();
            loop {
                if j >= b.len() {
                    return Err(syntax_err("unterminated quoted identifier"));
                }
                if b[j] == '"' {
                    if j + 1 < b.len() && b[j + 1] == '"' {
                        buf.push('"');
                        j += 2;
                        continue;
                    }
                    break;
                }
                buf.push(b[j]);
                j += 1;
            }
            if buf.is_empty() {
                return Err(syntax_err("zero-length delimited identifier"));
            }
            out.push(Tok::Quoted(buf));
            i = j + 1;
        } else if c.is_ascii_digit() || ((c == '-' || c == '+') && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let txt: String = b[i..j].iter().collect();
            let v: i64 = txt.parse().map_err(|_| syntax_err(format!("syntax error at or near \"{txt}\"")))?;
            if v > i32::MAX as i64 || v < i32::MIN as i64 {
                return Err(syntax_err(format!("syntax error at or near \"{txt}\"")));
            }
            out.push(Tok::Int(v as i32));
            i = j;
        } else if c.is_alphabetic() || c == '_' || !c.is_ascii() {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_' || b[j] == '$' || !b[j].is_ascii()) {
                j += 1;
            }
            let w: String = b[i..j].iter().collect();
            out.push(Tok::Word(w.to_lowercase()));
            i = j;
        } else {
            return Err(syntax_err(format!("syntax error at or near \"{c}\"")));
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn peek_word(&self) -> Option<&str> {
        match self.toks.get(self.pos) {
            Some(Tok::Word(w)) => Some(w.as_str()),
            _ => None,
        }
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.peek_word() == Some(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn err_here(&self) -> PgError {
        match self.peek() {
            Some(t) => syntax_err(format!("syntax error at or near \"{}\"", tok_text(t))),
            None => syntax_err("syntax error at end of input"),
        }
    }
    fn expect_word(&mut self, w: &str) -> Result<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.err_here())
        }
    }
    /// Optional `( int [, int]* )`.
    fn mods_opt(&mut self) -> Result<Vec<i32>> {
        let mut v = Vec::new();
        if self.peek() != Some(&Tok::LParen) {
            return Ok(v);
        }
        self.pos += 1;
        loop {
            match self.peek() {
                Some(Tok::Int(i)) => {
                    v.push(*i);
                    self.pos += 1;
                }
                _ => return Err(self.err_here()),
            }
            match self.peek() {
                Some(Tok::Comma) => self.pos += 1,
                Some(Tok::RParen) => {
                    self.pos += 1;
                    return Ok(v);
                }
                _ => return Err(self.err_here()),
            }
        }
    }
    /// Index (0 = year .. 5 = second) of the interval field word at the cursor.
    fn interval_field(&self) -> Option<usize> {
        match self.peek_word() {
            Some("year") => Some(0),
            Some("month") => Some(1),
            Some("day") => Some(2),
            Some("hour") => Some(3),
            Some("minute") => Some(4),
            Some("second") => Some(5),
            _ => None,
        }
    }
    /// `[ int ]` after an `[` has been seen at the cursor; consumes through `]`.
    fn bracket_suffix(&mut self) -> Result<()> {
        self.pos += 1;
        if let Some(Tok::Int(_)) = self.peek() {
            self.pos += 1;
        }
        if self.peek() != Some(&Tok::RBracket) {
            return Err(self.err_here());
        }
        self.pos += 1;
        Ok(())
    }
}

fn parse_type_name(s: &str) -> Result<TypeName> {
    let mut p = Parser { toks: tokenize(s)?, pos: 0 };
    let mut parts: Vec<(String, bool)> = Vec::new();
    loop {
        match p.peek().cloned() {
            Some(Tok::Word(w)) => parts.push((w, false)),
            Some(Tok::Quoted(q)) => parts.push((q, true)),
            _ => return Err(p.err_here()),
        }
        p.pos += 1;
        if p.peek() == Some(&Tok::Dot) {
            p.pos += 1;
        } else {
            break;
        }
    }
    if parts.len() > 2 {
        let full: Vec<&str> = parts.iter().map(|x| x.0.as_str()).collect();
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cross-database references are not implemented: {}", full.join(".")),
        ));
    }
    let mut tn = TypeName::default();
    let (last, last_q) = parts.pop().unwrap();
    tn.quoted = last_q;
    tn.name = last;
    if let Some((sch, _)) = parts.pop() {
        tn.schema = Some(sch);
    }
    let keyword = tn.schema.is_none() && !tn.quoted;
    let mut mods_done = false;
    if keyword {
        let first = tn.name.clone();
        match first.as_str() {
            "double" => {
                p.expect_word("precision")?;
                tn.name = "double precision".into();
            }
            "character" | "char" | "nchar" | "national" => {
                if first == "national" && !(p.eat_word("character") || p.eat_word("char")) {
                    return Err(p.err_here());
                }
                tn.name = "character".into();
                if p.eat_word("varying") {
                    tn.name = "character varying".into();
                }
            }
            "bit" => {
                if p.eat_word("varying") {
                    tn.name = "bit varying".into();
                }
            }
            "interval" => {
                if let Some(a) = p.interval_field() {
                    p.pos += 1;
                    let mut b = a;
                    if p.eat_word("to") {
                        b = match p.interval_field() {
                            Some(b) => b,
                            None => return Err(p.err_here()),
                        };
                        p.pos += 1;
                    }
                    let legal = match a {
                        0 => b == 0 || b == 1,
                        1 => b == 1,
                        _ => b >= a,
                    };
                    if !legal {
                        return Err(syntax_err("syntax error at or near \"to\""));
                    }
                    let bits = [ivf::YEAR, ivf::MONTH, ivf::DAY, ivf::HOUR, ivf::MINUTE, ivf::SECOND];
                    let mut range = 0;
                    if a == 0 && b == 1 {
                        range = ivf::YEAR | ivf::MONTH;
                    } else {
                        for bit in &bits[a..=b] {
                            range |= *bit;
                        }
                    }
                    tn.interval_range = Some(range);
                    let m = p.mods_opt()?;
                    if !m.is_empty() && b != 5 {
                        return Err(syntax_err("syntax error at or near \"(\""));
                    }
                    tn.typmods = m;
                    mods_done = true;
                }
            }
            _ => {}
        }
    }
    if !mods_done {
        tn.typmods = p.mods_opt()?;
    }
    if keyword && (tn.name == "time" || tn.name == "timestamp") {
        if p.eat_word("with") {
            p.expect_word("time")?;
            p.expect_word("zone")?;
            tn.name.push_str(" with time zone");
        } else if p.eat_word("without") {
            p.expect_word("time")?;
            p.expect_word("zone")?;
            tn.name.push_str(" without time zone");
        }
    }
    loop {
        if p.peek() == Some(&Tok::LBracket) {
            p.bracket_suffix()?;
            tn.array_dims += 1;
        } else if p.eat_word("array") {
            if p.peek() == Some(&Tok::LBracket) {
                p.bracket_suffix()?;
            }
            tn.array_dims += 1;
        } else {
            break;
        }
    }
    if p.pos != p.toks.len() {
        return Err(p.err_here());
    }
    Ok(tn)
}

/// Parse a type name from text (regtype input, `to_regtype`, `format('%s'::regtype)`):
/// e.g. `"character varying(10)[]"`, `"timestamp(3) with time zone"`, `"public.mood"`, `"\"char\""`.
// CONTRACT
pub fn parse_type_string(env: &dyn TypeEnv, s: &str) -> Result<TypeId> {
    let tn = parse_type_name(s)?;
    resolve_type_name(env, &tn)
}

// ---------------------------------------------------------------------------------------------
// format_type
// ---------------------------------------------------------------------------------------------

fn quote_ident(s: &str) -> String {
    match quote_identifier(s) {
        Ok(q) => q,
        Err(_) => {
            let mut chars = s.chars();
            let ok = match chars.next() {
                Some(c) => {
                    (c.is_ascii_lowercase() || c == '_')
                        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
                }
                None => false,
            };
            if ok {
                s.to_string()
            } else {
                format!("\"{}\"", s.replace('"', "\"\""))
            }
        }
    }
}

/// PG `printTypmod`: base name plus the type's typmodout text.
fn print_typmod(base: &str, oid: Oid, typmod_v: i32) -> Result<String> {
    let mut suffix = typmod::typmod_out(oid, typmod_v)?;
    let tz = match oid {
        oids::TIME | oids::TIMESTAMP => Some(" without time zone"),
        oids::TIMETZ | oids::TIMESTAMPTZ => Some(" with time zone"),
        _ => None,
    };
    if let Some(tz) = tz {
        if !suffix.contains("time zone") {
            suffix.push_str(tz);
        }
    }
    Ok(format!("{base}{suffix}"))
}

/// `typmod_given` mirrors FORMAT_TYPE_TYPEMOD_GIVEN (false for `format_type_be`).
fn format_base(env: &dyn TypeEnv, info: &registry::TypeInfo, typmod_v: i32, typmod_given: bool) -> Result<String> {
    let with_typemod = typmod_given && typmod_v >= 0;
    let oid = info.oid;
    let plain = |given_name: &str, plain_name: &str| -> Result<String> {
        if with_typemod {
            print_typmod(given_name, oid, typmod_v)
        } else {
            Ok(plain_name.to_string())
        }
    };
    Ok(match oid {
        oids::BIT => {
            if with_typemod {
                print_typmod("bit", oid, typmod_v)?
            } else if typmod_given {
                // bit with typmod -1 is not BIT (= BIT(1)); the quoted name avoids a bogus typmod.
                "\"bit\"".to_string()
            } else {
                "bit".to_string()
            }
        }
        oids::BOOL => "boolean".to_string(),
        oids::BPCHAR => {
            if with_typemod {
                print_typmod("character", oid, typmod_v)?
            } else if typmod_given {
                "bpchar".to_string()
            } else {
                "character".to_string()
            }
        }
        oids::FLOAT4 => "real".to_string(),
        oids::FLOAT8 => "double precision".to_string(),
        oids::INT2 => "smallint".to_string(),
        oids::INT4 => "integer".to_string(),
        oids::INT8 => "bigint".to_string(),
        oids::NUMERIC => plain("numeric", "numeric")?,
        oids::INTERVAL => plain("interval", "interval")?,
        oids::TIME => plain("time", "time without time zone")?,
        oids::TIMETZ => plain("time", "time with time zone")?,
        oids::TIMESTAMP => plain("timestamp", "timestamp without time zone")?,
        oids::TIMESTAMPTZ => plain("timestamp", "timestamp with time zone")?,
        oids::VARBIT => plain("bit varying", "bit varying")?,
        oids::VARCHAR => plain("character varying", "character varying")?,
        oids::CHAR => "\"char\"".to_string(),
        _ => {
            let name: &str = &info.name;
            if env.type_by_name(None, name) == Some(oid) {
                quote_ident(name)
            } else {
                let nsp = if info.namespace == oids::PG_CATALOG_NAMESPACE {
                    Some("pg_catalog".to_string())
                } else {
                    env.reg_name(RegKind::Namespace, info.namespace)
                };
                match nsp {
                    Some(n) => format!("{}.{}", n, quote_ident(name)),
                    None => quote_ident(name),
                }
            }
        }
    })
}

fn format_type_impl(env: &dyn TypeEnv, oid: Oid, typmod_v: i32, typmod_given: bool) -> Result<String> {
    if oid == 0 {
        return Ok("-".to_string());
    }
    let info = match env.type_info(oid) {
        Some(i) => i,
        None => return Ok("???".to_string()),
    };
    if info.is_array() && info.storage != b'p' {
        let elem = match env.type_info(info.elem) {
            Some(e) => e,
            None => return Ok("???".to_string()),
        };
        let mut s = format_base(env, &elem, typmod_v, typmod_given)?;
        s.push_str("[]");
        return Ok(s);
    }
    format_base(env, &info, typmod_v, typmod_given)
}

/// PG `format_type` (see module docs for exact outputs). typmod < 0 = none.
// CONTRACT
pub fn format_type(env: &dyn TypeEnv, oid: Oid, typmod: i32) -> Result<String> {
    format_type_impl(env, oid, typmod, true)
}

/// Short display name used inside error messages for builtin types (PG uses `format_type_be`, i.e.
/// `format_type` without typmod): `integer`, `text`, `double precision`, `timestamp with time zone`.
pub fn display_name(env: &dyn TypeEnv, oid: Oid) -> String {
    format_type_impl(env, oid, -1, false).unwrap_or_else(|_| {
        env.type_info(oid).map(|t| t.name.into_owned()).unwrap_or_else(|| oid.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::builtin_env;

    fn tn(name: &str, mods: &[i32]) -> TypeName {
        TypeName { name: name.to_string(), typmods: mods.to_vec(), ..Default::default() }
    }

    #[test]
    fn format_acceptance() {
        let e = builtin_env();
        assert_eq!(format_type(e, 1043, 14).unwrap(), "character varying(10)");
        assert_eq!(format_type(e, 1700, ((10 << 16) | 2) + 4).unwrap(), "numeric(10,2)");
        assert_eq!(format_type(e, 1184, 3).unwrap(), "timestamp(3) with time zone");
        assert_eq!(format_type(e, 1083, 2).unwrap(), "time(2) without time zone");
        assert_eq!(format_type(e, 1007, -1).unwrap(), "integer[]");
        assert_eq!(format_type(e, 1015, 14).unwrap(), "character varying(10)[]");
        assert_eq!(format_type(e, 1042, 5).unwrap(), "character(1)");
        assert_eq!(format_type(e, 1042, -1).unwrap(), "bpchar");
        assert_eq!(format_type(e, 0, -1).unwrap(), "-");
        assert_eq!(format_type(e, 999_999_999, -1).unwrap(), "???");
        assert_eq!(format_type(e, 18, -1).unwrap(), "\"char\"");
        assert_eq!(format_type(e, 701, -1).unwrap(), "double precision");
        assert_eq!(format_type(e, 1560, 5).unwrap(), "bit(5)");
        assert_eq!(format_type(e, 1562, -1).unwrap(), "bit varying");
        assert_eq!(display_name(e, 1042), "character");
        assert_eq!(display_name(e, 25), "text");
    }

    #[test]
    fn resolve_acceptance() {
        let e = builtin_env();
        assert_eq!(resolve_type_name(e, &tn("character varying", &[10])).unwrap(), TypeId::new(1043, 14));
        assert_eq!(resolve_type_name(e, &tn("char", &[])).unwrap(), TypeId::new(1042, 5));
        let mut q = tn("char", &[]);
        q.quoted = true;
        assert_eq!(resolve_type_name(e, &q).unwrap(), TypeId::new(18, -1));
        assert_eq!(resolve_type_name(e, &tn("float", &[25])).unwrap().oid, 701);
        assert_eq!(resolve_type_name(e, &tn("float", &[24])).unwrap().oid, 700);
        assert_eq!(resolve_type_name(e, &tn("float", &[0])).unwrap_err().sqlstate.code(), "22023");
        assert_eq!(resolve_type_name(e, &tn("float", &[54])).unwrap_err().sqlstate.code(), "22023");
        let err = resolve_type_name(e, &tn("nosuchtype", &[])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "type \"nosuchtype\" does not exist");
        let err = resolve_type_name(e, &tn("text", &[5])).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42601");
        assert_eq!(err.message, "type modifier is not allowed for type \"text\"");
        assert_eq!(resolve_type_name(e, &tn("serial", &[])).unwrap_err().sqlstate.code(), "42704");
        let mut a = tn("integer", &[]);
        a.array_dims = 2;
        assert_eq!(resolve_type_name(e, &a).unwrap().oid, 1007);
    }

    #[test]
    fn parse_strings() {
        let e = builtin_env();
        assert_eq!(parse_type_string(e, "character varying(10)[]").unwrap(), TypeId::new(1015, 14));
        assert_eq!(parse_type_string(e, "timestamp(3) with time zone").unwrap(), TypeId::new(1184, 3));
        assert_eq!(parse_type_string(e, "  Double   Precision ").unwrap().oid, 701);
        assert_eq!(parse_type_string(e, "\"char\"").unwrap().oid, 18);
        assert_eq!(parse_type_string(e, "pg_catalog.int4").unwrap().oid, 23);
        assert_eq!(parse_type_string(e, "int4 array").unwrap().oid, 1007);
        assert_eq!(parse_type_string(e, "bit varying(5)").unwrap(), TypeId::new(1562, 5));
        assert_eq!(parse_type_string(e, "time with time zone").unwrap().oid, 1266);
        assert_eq!(
            parse_type_string(e, "interval year to month").unwrap(),
            TypeId::new(1186, ((ivf::YEAR | ivf::MONTH) << 16) | ivf::FULL_PRECISION)
        );
        assert_eq!(
            parse_type_string(e, "interval day to second(3)").unwrap(),
            TypeId::new(1186, ((ivf::DAY | ivf::HOUR | ivf::MINUTE | ivf::SECOND) << 16) | 3)
        );
        assert_eq!(parse_type_string(e, "interval").unwrap(), TypeId::new(1186, -1));
        assert_eq!(parse_type_string(e, "nosuchtype").unwrap_err().sqlstate.code(), "42704");
        assert_eq!(parse_type_string(e, "int4 int4").unwrap_err().sqlstate.code(), "42601");
        assert_eq!(parse_type_string(e, "").unwrap_err().sqlstate.code(), "42601");
    }
}
