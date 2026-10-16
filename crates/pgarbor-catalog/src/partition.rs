//! Declarative partitioning metadata: partition bounds (`pg_class.relpartbound`), their canonical
//! text, overlap checks (`partbounds.c` `check_new_partition_bound`) and tuple routing
//! (`execPartition.c` `get_partition_for_tuple`). Amendment pgarbor-catalog-09.
//!
//! A partition stores its bound as TEXT in `Relation::partition_of = Some((parent, text))`, in the
//! form `pg_get_expr(relpartbound, oid)` prints: `FOR VALUES IN ('a', NULL)`,
//! `FOR VALUES FROM (1) TO (MAXVALUE)`, `FOR VALUES WITH (modulus 4, remainder 0)` or `DEFAULT`.
//! Values are the key type's text output (single-quoted unless numeric/boolean), so they can be
//! read back with the type's input function ([`PartitionBound::resolve`]).
//!
//! The partition key (`Relation::partition_key`) lists key column attnums (0 for an expression,
//! consumed in order from `exprs`). Key types come from the columns or `PartitionKey::expr_types`.
use std::cmp::Ordering;

use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::compare::cmp_datums;
use pgarbor_types::context::{IoConfig, IoCtx};
use pgarbor_types::{io, Datum, TypeEnv, TypeId};

use crate::model::{PartitionKey, Relation};
use crate::snapshot::CatalogSnapshot;

/// PG `HASH_PARTITION_SEED`.
pub const HASH_PARTITION_SEED: u64 = 0x7A5B_2236_7996_DCFD;

/// One value of a range bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeValue {
    MinValue,
    MaxValue,
    /// Text output of the key column type.
    Value(String),
}

/// Error positions of a written bound (1-based character offsets; `None` = unknown).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundPositions {
    /// `FOR` (or `DEFAULT`) keyword.
    pub spec: Option<u32>,
    /// One per list value (after duplicate removal).
    pub list: Vec<Option<u32>>,
    pub lower: Vec<Option<u32>>,
    pub upper: Vec<Option<u32>>,
}

/// A partition bound as written/stored (text values).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartitionBound {
    Default,
    /// `FOR VALUES IN (...)`; `None` = NULL.
    List(Vec<Option<String>>),
    Range { lower: Vec<RangeValue>, upper: Vec<RangeValue> },
    Hash { modulus: u32, remainder: u32 },
}

/// A range value with its datum.
#[derive(Clone, Debug, PartialEq)]
pub enum RangeDatum {
    MinValue,
    MaxValue,
    Value(Datum),
}

/// A bound with values converted to datums of the key types.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedBound {
    Default,
    List(Vec<Datum>),
    Range { lower: Vec<RangeDatum>, upper: Vec<RangeDatum> },
    Hash { modulus: u32, remainder: u32 },
}

fn io_config() -> IoConfig {
    IoConfig::default()
}

/// True for types whose literals PG prints without quotes in a partition bound.
fn unquoted_type(oid: Oid) -> bool {
    matches!(oid, 20 | 21 | 23 | 700 | 701 | 1700 | 16 | 26)
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Text of one bound value.
fn value_sql(ty: TypeId, v: &str) -> String {
    let negative = v.starts_with('-');
    if unquoted_type(ty.oid) && !negative && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')) {
        v.to_string()
    } else {
        quote_literal(v)
    }
}

impl PartitionBound {
    /// Canonical text (`pg_get_expr(relpartbound)` form). `key_types` is the parent's key.
    pub fn to_sql(&self, key_types: &[TypeId]) -> String {
        let ty = |i: usize| key_types.get(i).copied().unwrap_or(TypeId::TEXT);
        match self {
            PartitionBound::Default => "DEFAULT".to_string(),
            PartitionBound::List(vals) => {
                let items: Vec<String> = vals
                    .iter()
                    .map(|v| match v {
                        None => "NULL".to_string(),
                        Some(s) => value_sql(ty(0), s),
                    })
                    .collect();
                format!("FOR VALUES IN ({})", items.join(", "))
            }
            PartitionBound::Range { lower, upper } => {
                let fmt = |vals: &[RangeValue]| -> String {
                    vals.iter()
                        .enumerate()
                        .map(|(i, v)| match v {
                            RangeValue::MinValue => "MINVALUE".to_string(),
                            RangeValue::MaxValue => "MAXVALUE".to_string(),
                            RangeValue::Value(s) => value_sql(ty(i), s),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("FOR VALUES FROM ({}) TO ({})", fmt(lower), fmt(upper))
            }
            PartitionBound::Hash { modulus, remainder } => {
                format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})")
            }
        }
    }

    /// Parse the canonical text written by [`PartitionBound::to_sql`].
    pub fn parse(text: &str) -> Result<PartitionBound> {
        let toks = tokenize(text)?;
        let mut p = Parser { toks, i: 0 };
        if p.word("DEFAULT") {
            return Ok(PartitionBound::Default);
        }
        p.expect_word("FOR")?;
        p.expect_word("VALUES")?;
        if p.word("IN") {
            let vals = p.list()?;
            let mut out = Vec::with_capacity(vals.len());
            for v in vals {
                out.push(match v {
                    Tok::Null => None,
                    Tok::Str(s) | Tok::Num(s) => Some(s),
                    Tok::Word(w) => Some(w.to_ascii_lowercase()),
                    _ => return Err(bad_bound(text)),
                });
            }
            return Ok(PartitionBound::List(out));
        }
        if p.word("FROM") {
            let lower = range_values(p.list()?, text)?;
            p.expect_word("TO")?;
            let upper = range_values(p.list()?, text)?;
            return Ok(PartitionBound::Range { lower, upper });
        }
        if p.word("WITH") {
            let vals = p.list()?;
            let mut modulus = None;
            let mut remainder = None;
            for v in vals {
                if let Tok::Pair(k, n) = v {
                    match k.to_ascii_lowercase().as_str() {
                        "modulus" => modulus = Some(n),
                        "remainder" => remainder = Some(n),
                        _ => return Err(bad_bound(text)),
                    }
                }
            }
            return match (modulus, remainder) {
                (Some(modulus), Some(remainder)) => Ok(PartitionBound::Hash { modulus, remainder }),
                _ => Err(bad_bound(text)),
            };
        }
        Err(bad_bound(text))
    }

    pub fn is_default(&self) -> bool {
        matches!(self, PartitionBound::Default)
    }

    /// Convert the text values to datums of `key_types`.
    pub fn resolve(&self, env: &dyn TypeEnv, key_types: &[TypeId]) -> Result<ResolvedBound> {
        let cfg = io_config();
        let ctx = IoCtx::new(env, &cfg);
        let input = |i: usize, s: &str| -> Result<Datum> {
            let ty = key_types.get(i).copied().ok_or_else(|| PgError::internal("partition bound wider than its key"))?;
            io::input(&ctx, ty, s)
        };
        Ok(match self {
            PartitionBound::Default => ResolvedBound::Default,
            PartitionBound::List(vals) => {
                let mut out = Vec::with_capacity(vals.len());
                for v in vals {
                    out.push(match v {
                        None => Datum::Null,
                        Some(s) => input(0, s)?,
                    });
                }
                ResolvedBound::List(out)
            }
            PartitionBound::Range { lower, upper } => {
                let conv = |vals: &[RangeValue]| -> Result<Vec<RangeDatum>> {
                    let mut out = Vec::with_capacity(vals.len());
                    for (i, v) in vals.iter().enumerate() {
                        out.push(match v {
                            RangeValue::MinValue => RangeDatum::MinValue,
                            RangeValue::MaxValue => RangeDatum::MaxValue,
                            RangeValue::Value(s) => RangeDatum::Value(input(i, s)?),
                        });
                    }
                    Ok(out)
                };
                ResolvedBound::Range { lower: conv(lower)?, upper: conv(upper)? }
            }
            PartitionBound::Hash { modulus, remainder } => {
                ResolvedBound::Hash { modulus: *modulus, remainder: *remainder }
            }
        })
    }
}

fn bad_bound(text: &str) -> PgError {
    PgError::corruption(format!("invalid stored partition bound: {text}"))
}

fn range_values(vals: Vec<Tok>, text: &str) -> Result<Vec<RangeValue>> {
    let mut out = Vec::with_capacity(vals.len());
    for v in vals {
        out.push(match v {
            Tok::Word(w) if w.eq_ignore_ascii_case("MINVALUE") => RangeValue::MinValue,
            Tok::Word(w) if w.eq_ignore_ascii_case("MAXVALUE") => RangeValue::MaxValue,
            Tok::Word(w) => RangeValue::Value(w.to_ascii_lowercase()),
            Tok::Str(s) | Tok::Num(s) => RangeValue::Value(s),
            _ => return Err(bad_bound(text)),
        });
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(String),
    Null,
    LParen,
    RParen,
    Comma,
    /// `modulus 4` inside WITH (...)
    Pair(String, u32),
}

fn tokenize(text: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
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
        } else if c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(bad_bound(text)),
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(*ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '+' | '-')) {
                i += 1;
            }
            out.push(Tok::Num(chars[start..i].iter().collect()));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let w: String = chars[start..i].iter().collect();
            if w.eq_ignore_ascii_case("NULL") {
                out.push(Tok::Null);
            } else {
                out.push(Tok::Word(w));
            }
        } else {
            return Err(bad_bound(text));
        }
    }
    // Fold `word number` pairs (hash bounds).
    let mut folded = Vec::with_capacity(out.len());
    let mut k = 0;
    while k < out.len() {
        if let (Tok::Word(w), Some(Tok::Num(n))) = (&out[k], out.get(k + 1)) {
            if let Ok(v) = n.parse::<u32>() {
                folded.push(Tok::Pair(w.clone(), v));
                k += 2;
                continue;
            }
        }
        folded.push(out[k].clone());
        k += 1;
    }
    Ok(folded)
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn word(&mut self, w: &str) -> bool {
        if let Some(Tok::Word(x)) = self.toks.get(self.i) {
            if x.eq_ignore_ascii_case(w) {
                self.i += 1;
                return true;
            }
        }
        false
    }
    fn expect_word(&mut self, w: &str) -> Result<()> {
        if self.word(w) {
            Ok(())
        } else {
            Err(PgError::corruption(format!("invalid stored partition bound: expected {w}")))
        }
    }
    /// `( item, item, ... )`
    fn list(&mut self) -> Result<Vec<Tok>> {
        if self.toks.get(self.i) != Some(&Tok::LParen) {
            return Err(PgError::corruption("invalid stored partition bound: expected ("));
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.toks.get(self.i).cloned() {
                Some(Tok::RParen) => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(Tok::Comma) => self.i += 1,
                Some(t) => {
                    out.push(t);
                    self.i += 1;
                }
                None => return Err(PgError::corruption("invalid stored partition bound: unterminated list")),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------

/// Strategy names as PG prints them in errors.
pub fn strategy_name(strategy: u8) -> &'static str {
    match strategy {
        b'h' => "hash",
        b'l' => "list",
        _ => "range",
    }
}

/// Types of the key columns/expressions of `rel`'s partition key.
pub fn key_types(rel: &Relation) -> Vec<TypeId> {
    let Some(key) = &rel.partition_key else { return Vec::new() };
    key_types_of(rel, key)
}

/// Types of the key items of `key` over the columns of `rel`.
pub fn key_types_of(rel: &Relation, key: &PartitionKey) -> Vec<TypeId> {
    let mut exprs = key.expr_types.iter();
    key.columns
        .iter()
        .map(|a| {
            if *a > 0 {
                rel.column(*a).map(|c| c.type_id).unwrap_or(TypeId::TEXT)
            } else {
                exprs.next().copied().unwrap_or(TypeId::TEXT)
            }
        })
        .collect()
}

/// Display names of the key items (column names; expression text for expressions).
pub fn key_names(rel: &Relation) -> Vec<String> {
    let Some(key) = &rel.partition_key else { return Vec::new() };
    let mut exprs = key.exprs.iter();
    key.columns
        .iter()
        .map(|a| {
            if *a > 0 {
                rel.column(*a).map(|c| c.name.clone()).unwrap_or_default()
            } else {
                exprs.next().map(|e| e.sql.clone()).unwrap_or_default()
            }
        })
        .collect()
}

/// Direct partitions of `parent` with their stored bounds, ordered by OID.
pub fn partitions_of(snap: &CatalogSnapshot, parent: Oid) -> Result<Vec<(Oid, PartitionBound)>> {
    let mut out = Vec::new();
    for r in snap.user_relations() {
        if let Some((p, text)) = &r.partition_of {
            if *p == parent {
                out.push((r.oid, PartitionBound::parse(text)?));
            }
        }
    }
    out.sort_by_key(|(o, _)| *o);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------------------------

/// Compare a range bound value list with a key tuple's prefix: `bound` vs `key` (PG
/// `partition_rbound_datum_cmp`, sign reversed: this returns the ordering of the BOUND).
fn cmp_bound_key(env: &dyn TypeEnv, types: &[TypeId], bound: &[RangeDatum], key: &[Datum]) -> Result<Ordering> {
    for (i, b) in bound.iter().enumerate() {
        let o = match b {
            RangeDatum::MinValue => Ordering::Less,
            RangeDatum::MaxValue => Ordering::Greater,
            RangeDatum::Value(v) => {
                let k = key.get(i).cloned().unwrap_or(Datum::Null);
                cmp_datums(env, types.get(i).copied().unwrap_or(TypeId::TEXT), v, &k)?
            }
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// Compare two range bound value lists (MINVALUE < any value < MAXVALUE).
fn cmp_bounds(env: &dyn TypeEnv, types: &[TypeId], a: &[RangeDatum], b: &[RangeDatum]) -> Result<Ordering> {
    for i in 0..a.len().max(b.len()) {
        let rank = |d: Option<&RangeDatum>| match d {
            Some(RangeDatum::MinValue) => 0,
            Some(RangeDatum::Value(_)) => 1,
            Some(RangeDatum::MaxValue) | None => 2,
        };
        let (x, y) = (a.get(i), b.get(i));
        let o = match (x, y) {
            (Some(RangeDatum::Value(p)), Some(RangeDatum::Value(q))) => {
                cmp_datums(env, types.get(i).copied().unwrap_or(TypeId::TEXT), p, q)?
            }
            _ => rank(x).cmp(&rank(y)),
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
        // After an infinite value the remaining values do not matter.
        if !matches!(x, Some(RangeDatum::Value(_))) {
            break;
        }
    }
    Ok(Ordering::Equal)
}

/// Does `key` (one datum per key item) satisfy `bound`? DEFAULT is never matched here (the caller
/// treats it as "no other partition matched"). `hash` computes the combined row hash.
pub fn bound_accepts(
    env: &dyn TypeEnv,
    types: &[TypeId],
    bound: &ResolvedBound,
    key: &[Datum],
    row_hash: &mut dyn FnMut() -> Result<u64>,
) -> Result<bool> {
    match bound {
        ResolvedBound::Default => Ok(false),
        ResolvedBound::List(vals) => {
            let k = key.first().cloned().unwrap_or(Datum::Null);
            let ty = types.first().copied().unwrap_or(TypeId::TEXT);
            for v in vals {
                if v.is_null() != k.is_null() {
                    continue;
                }
                if k.is_null() || cmp_datums(env, ty, v, &k)? == Ordering::Equal {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        ResolvedBound::Range { lower, upper } => {
            if key.iter().any(|d| d.is_null()) {
                return Ok(false);
            }
            Ok(cmp_bound_key(env, types, lower, key)? != Ordering::Greater
                && cmp_bound_key(env, types, upper, key)? == Ordering::Greater)
        }
        ResolvedBound::Hash { modulus, remainder } => {
            let h = row_hash()?;
            Ok(*modulus > 0 && h % (*modulus as u64) == *remainder as u64)
        }
    }
}

/// PG `hash_combine64`.
pub fn hash_combine64(a: u64, b: u64) -> u64 {
    a ^ (b
        .wrapping_add(0x49a0_f4dd_15e5_a8e3)
        .wrapping_add(a << 54)
        .wrapping_add(a >> 7))
}

/// The extended hash support function (`amprocnum` 2 of the default hash operator class) for
/// values of type `ty`, e.g. `hashint4extended` for int4.
pub fn hash_extended_proc(env: &dyn TypeEnv, ty: Oid) -> Option<Oid> {
    let lookup = |t: Oid| -> Option<Oid> {
        let key = t.to_string();
        let family: Oid = crate::builtin::OPCLASSES.rows.iter().find_map(|r| {
            let method = r.get(1).copied().flatten()?;
            let intype = r.get(6).copied().flatten()?;
            let default = r.get(7).copied().flatten()?;
            if method == "405" && intype == key && default == "t" {
                r.get(5).copied().flatten()?.parse().ok()
            } else {
                None
            }
        })?;
        crate::builtin::ext_rows::AMPROCS
            .iter()
            .find(|(_, f, l, rt, n, _)| *f == family && *l == t && *rt == t && *n == 2)
            .map(|(_, _, _, _, _, p)| *p)
    };
    if let Some(p) = lookup(ty) {
        return Some(p);
    }
    // Binary-coercible fallbacks (varchar/bpchar use the text family; enums use anyenum).
    let info = env.type_info(ty)?;
    if info.basetype != 0 && info.basetype != ty {
        return hash_extended_proc(env, info.basetype);
    }
    match info.category {
        b'S' => lookup(25),
        b'E' => lookup(3500),
        b'A' => lookup(2277),
        _ => None,
    }
}

/// Bound vs bound overlap checks for a new partition (PG `check_new_partition_bound`).
/// `existing` lists `(name, bound)` of the parent's current partitions; `pos` gives error cursors.
pub fn check_new_partition_bound(
    env: &dyn TypeEnv,
    types: &[TypeId],
    new_name: &str,
    new_bound: &ResolvedBound,
    existing: &[(String, ResolvedBound)],
    pos: &BoundPositions,
) -> Result<()> {
    let overlap = |other: &str, at: Option<u32>| {
        let e = PgError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!("partition \"{new_name}\" would overlap partition \"{other}\""),
        );
        match at {
            Some(p) => e.with_position(p),
            None => e,
        }
    };
    match new_bound {
        ResolvedBound::Default => {
            if let Some((n, _)) = existing.iter().find(|(_, b)| matches!(b, ResolvedBound::Default)) {
                let e = PgError::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!("partition \"{new_name}\" conflicts with existing default partition \"{n}\""),
                );
                return Err(match pos.spec {
                    Some(p) => e.with_position(p),
                    None => e,
                });
            }
        }
        ResolvedBound::List(vals) => {
            let ty = types.first().copied().unwrap_or(TypeId::TEXT);
            for (i, v) in vals.iter().enumerate() {
                for (name, b) in existing {
                    if let ResolvedBound::List(other) = b {
                        for o in other {
                            let same = match (v.is_null(), o.is_null()) {
                                (true, true) => true,
                                (false, false) => cmp_datums(env, ty, v, o)? == Ordering::Equal,
                                _ => false,
                            };
                            if same {
                                return Err(overlap(name, pos.list.get(i).copied().flatten()));
                            }
                        }
                    }
                }
            }
        }
        ResolvedBound::Range { lower, upper } => {
            if let Some(i) = first_difference_gt(env, types, lower, upper)? {
                let e = PgError::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!("empty range bound specified for partition \"{new_name}\""),
                )
                .with_detail(format!(
                    "Specified lower bound {} is greater than or equal to upper bound {}.",
                    range_text(env, types, lower)?,
                    range_text(env, types, upper)?
                ));
                return Err(match pos.lower.get(i).copied().flatten() {
                    Some(p) => e.with_position(p),
                    None => e,
                });
            }
            // The existing partition containing the new lower bound, else the next one above it.
            let mut containing: Option<&String> = None;
            let mut next: Option<(&String, &Vec<RangeDatum>)> = None;
            for (name, b) in existing {
                if let ResolvedBound::Range { lower: l2, upper: u2 } = b {
                    if cmp_bounds(env, types, l2, lower)? != Ordering::Greater && cmp_bounds(env, types, lower, u2)? == Ordering::Less {
                        containing = Some(name);
                        break;
                    }
                    if cmp_bounds(env, types, l2, lower)? == Ordering::Greater {
                        let better = match next {
                            None => true,
                            Some((_, nl)) => cmp_bounds(env, types, l2, nl)? == Ordering::Less,
                        };
                        if better {
                            next = Some((name, l2));
                        }
                    }
                }
            }
            if let Some(name) = containing {
                let at = match diff_index(env, types, lower, existing_lower(existing, name))? {
                    Some(i) => pos.lower.get(i).copied().flatten(),
                    None => pos.lower.first().copied().flatten(),
                };
                return Err(overlap(name, at));
            }
            if let Some((name, nl)) = next {
                if cmp_bounds(env, types, nl, upper)? == Ordering::Less {
                    let at = match diff_index(env, types, upper, nl)? {
                        Some(i) => pos.upper.get(i).copied().flatten(),
                        None => pos.upper.first().copied().flatten(),
                    };
                    return Err(overlap(name, at));
                }
            }
        }
        ResolvedBound::Hash { modulus, remainder } => {
            // Every modulus must be a factor of the next larger one.
            let mut others: Vec<(&String, u32, u32)> = existing
                .iter()
                .filter_map(|(n, b)| match b {
                    ResolvedBound::Hash { modulus, remainder } => Some((n, *modulus, *remainder)),
                    _ => None,
                })
                .collect();
            others.sort_by_key(|(_, m, r)| (*m, *r));
            if !others.is_empty() {
                let prev = others.iter().filter(|(_, m, r)| (*m, *r) <= (*modulus, *remainder)).last();
                match prev {
                    None => {
                        let (n, m, _) = others[0];
                        if m % modulus != 0 {
                            return Err(PgError::new(
                                SqlState::INVALID_OBJECT_DEFINITION,
                                "every hash partition modulus must be a factor of the next larger modulus",
                            )
                            .with_detail(format!(
                                "The new modulus {modulus} is not a factor of {m}, the modulus of existing partition \"{n}\"."
                            )));
                        }
                    }
                    Some((n, m, r)) => {
                        if modulus % m != 0 {
                            return Err(PgError::new(
                                SqlState::INVALID_OBJECT_DEFINITION,
                                "every hash partition modulus must be a factor of the next larger modulus",
                            )
                            .with_detail(format!(
                                "The new modulus {modulus} is not divisible by {m}, the modulus of existing partition \"{n}\"."
                            )));
                        }
                        if let Some((n2, m2, _)) = others.iter().find(|(_, m2, r2)| (*m2, *r2) > (*m, *r)) {
                            if m2 % modulus != 0 {
                                return Err(PgError::new(
                                    SqlState::INVALID_OBJECT_DEFINITION,
                                    "every hash partition modulus must be a factor of the next larger modulus",
                                )
                                .with_detail(format!(
                                    "The new modulus {modulus} is not a factor of {m2}, the modulus of existing partition \"{n2}\"."
                                )));
                            }
                        }
                        let _ = n;
                    }
                }
            }
            for (n, m, r) in &others {
                let small = (*m).min(*modulus);
                if remainder % small == r % small {
                    return Err(overlap(n, pos.spec));
                }
            }
        }
    }
    Ok(())
}

fn existing_lower<'a>(existing: &'a [(String, ResolvedBound)], name: &str) -> &'a [RangeDatum] {
    for (n, b) in existing {
        if n == name {
            if let ResolvedBound::Range { lower, .. } = b {
                return lower;
            }
        }
    }
    &[]
}

/// Index of the first column where `a` and `b` differ (None when equal).
fn diff_index(env: &dyn TypeEnv, types: &[TypeId], a: &[RangeDatum], b: &[RangeDatum]) -> Result<Option<usize>> {
    for i in 0..a.len().min(b.len()) {
        if cmp_bounds(env, types, &a[i..=i], &b[i..=i])? != Ordering::Equal {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// For an empty range check: the column index where `lower >= upper` is decided, `None` when
/// lower < upper.
fn first_difference_gt(env: &dyn TypeEnv, types: &[TypeId], lower: &[RangeDatum], upper: &[RangeDatum]) -> Result<Option<usize>> {
    if cmp_bounds(env, types, lower, upper)? == Ordering::Less {
        return Ok(None);
    }
    Ok(Some(diff_index(env, types, lower, upper)?.unwrap_or(0)))
}

/// `(1, MAXVALUE)` style text of a resolved range bound (error details).
pub fn range_text(env: &dyn TypeEnv, types: &[TypeId], vals: &[RangeDatum]) -> Result<String> {
    let cfg = io_config();
    let ctx = IoCtx::new(env, &cfg);
    let mut parts = Vec::with_capacity(vals.len());
    for (i, v) in vals.iter().enumerate() {
        parts.push(match v {
            RangeDatum::MinValue => "MINVALUE".to_string(),
            RangeDatum::MaxValue => "MAXVALUE".to_string(),
            RangeDatum::Value(d) => io::output(&ctx, types.get(i).copied().unwrap_or(TypeId::TEXT), d)?,
        });
    }
    Ok(format!("({})", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_text_round_trip() {
        let b = PartitionBound::List(vec![Some("west".into()), None, Some("o'x".into())]);
        let t = b.to_sql(&[TypeId::TEXT]);
        assert_eq!(t, "FOR VALUES IN ('west', NULL, 'o''x')");
        assert_eq!(PartitionBound::parse(&t).expect("parse"), b);
        let r = PartitionBound::Range {
            lower: vec![RangeValue::MinValue, RangeValue::Value("5".into())],
            upper: vec![RangeValue::Value("10".into()), RangeValue::MaxValue],
        };
        let t = r.to_sql(&[TypeId::INT4, TypeId::INT4]);
        assert_eq!(t, "FOR VALUES FROM (MINVALUE, 5) TO (10, MAXVALUE)");
        assert_eq!(PartitionBound::parse(&t).expect("parse"), r);
        let h = PartitionBound::Hash { modulus: 4, remainder: 3 };
        assert_eq!(h.to_sql(&[]), "FOR VALUES WITH (modulus 4, remainder 3)");
        assert_eq!(PartitionBound::parse(&h.to_sql(&[])).expect("parse"), h);
        assert_eq!(PartitionBound::parse("DEFAULT").expect("parse"), PartitionBound::Default);
    }

    #[test]
    fn hash_combine_matches_pg() {
        assert_eq!(hash_combine64(0, 0), 0x49a0_f4dd_15e5_a8e3);
    }
}

/// Partitions in PG partition-descriptor order: range by lower bound, list by smallest value
/// (NULL-only partitions after), hash by (modulus, remainder), DEFAULT last. Unparsable bounds
/// keep OID order.
pub fn order_partitions(snap: &CatalogSnapshot, parent: &Relation, children: Vec<Oid>) -> Vec<Oid> {
    let env = crate::env::CatalogEnv::with_default_path(snap.clone());
    let types = key_types(parent);
    let mut items: Vec<(Oid, ResolvedBound)> = Vec::with_capacity(children.len());
    for c in &children {
        let Some((_, text)) = snap.relation(*c).and_then(|r| r.partition_of.clone()) else { return children };
        let Ok(b) = PartitionBound::parse(&text) else { return children };
        let Ok(rb) = b.resolve(&env, &types) else { return children };
        items.push((*c, rb));
    }
    let rank = |b: &ResolvedBound| match b {
        ResolvedBound::Default => 3,
        ResolvedBound::List(v) if v.iter().all(|d| d.is_null()) => 2,
        _ => 1,
    };
    let first = |b: &ResolvedBound| -> Option<Datum> {
        match b {
            ResolvedBound::List(v) => {
                let mut best: Option<Datum> = None;
                for d in v.iter().filter(|d| !d.is_null()) {
                    let better = match &best {
                        None => true,
                        Some(cur) => {
                            cmp_datums(&env, types.first().copied().unwrap_or(TypeId::TEXT), d, cur).ok() == Some(Ordering::Less)
                        }
                    };
                    if better {
                        best = Some(d.clone());
                    }
                }
                best
            }
            _ => None,
        }
    };
    items.sort_by(|(ao, a), (bo, b)| {
        let r = rank(a).cmp(&rank(b));
        if r != Ordering::Equal {
            return r;
        }
        let o = match (a, b) {
            (ResolvedBound::Range { lower: la, .. }, ResolvedBound::Range { lower: lb, .. }) => {
                cmp_bounds(&env, &types, la, lb).unwrap_or(Ordering::Equal)
            }
            (ResolvedBound::List(_), ResolvedBound::List(_)) => match (first(a), first(b)) {
                (Some(x), Some(y)) => {
                    cmp_datums(&env, types.first().copied().unwrap_or(TypeId::TEXT), &x, &y).unwrap_or(Ordering::Equal)
                }
                _ => Ordering::Equal,
            },
            (ResolvedBound::Hash { modulus: ma, remainder: ra }, ResolvedBound::Hash { modulus: mb, remainder: rb }) => {
                (ma, ra).cmp(&(mb, rb))
            }
            _ => Ordering::Equal,
        };
        o.then(ao.cmp(bo))
    });
    items.into_iter().map(|(o, _)| o).collect()
}
