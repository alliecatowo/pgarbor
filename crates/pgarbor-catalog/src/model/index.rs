//! Indexes (`pg_class` relkind 'i' + `pg_index`).
//!
//! Plain data, frozen (v1). Index DATA lives under the index OID as relation space
//! (`[index relid][memcomparable key][rowid if non-unique]`, ARCHITECTURE §4); maintaining it is
//! pgarbor-exec's job. The catalog only describes the index.
use super::common::StoredExpr;
use super::relation::Persistence;
use pgarbor_common::Oid;
use pgarbor_types::TypeId;
use serde::{Deserialize, Serialize};

/// Index access method (`pg_am` OIDs). v1 implements btree only; others are accepted by the catalog
/// only if the DDL layer allows them (it raises 0A000 for non-btree today).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndexMethod {
    #[default]
    Btree,
    Hash,
    Gist,
    Gin,
    Brin,
    SpGist,
}

impl IndexMethod {
    /// pg_am OID: btree 403, hash 405, gist 783, gin 2742, brin 3580, spgist 4000.
    pub fn am_oid(self) -> Oid {
        match self {
            IndexMethod::Btree => 403,
            IndexMethod::Hash => 405,
            IndexMethod::Gist => 783,
            IndexMethod::Gin => 2742,
            IndexMethod::Brin => 3580,
            IndexMethod::SpGist => 4000,
        }
    }
    /// `amname` (also the `USING <name>` spelling in `pg_get_indexdef`).
    pub fn name(self) -> &'static str {
        match self {
            IndexMethod::Btree => "btree",
            IndexMethod::Hash => "hash",
            IndexMethod::Gist => "gist",
            IndexMethod::Gin => "gin",
            IndexMethod::Brin => "brin",
            IndexMethod::SpGist => "spgist",
        }
    }
    /// Parse a `USING` name (case-insensitive already folded). Unknown → None (caller raises 42704
    /// `access method "foo" does not exist`).
    pub fn from_name(name: &str) -> Option<IndexMethod> {
        Some(match name {
            "btree" => IndexMethod::Btree,
            "hash" => IndexMethod::Hash,
            "gist" => IndexMethod::Gist,
            "gin" => IndexMethod::Gin,
            "brin" => IndexMethod::Brin,
            "spgist" => IndexMethod::SpGist,
            _ => return None,
        })
    }
}

/// One index column (key column or INCLUDE column).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexKey {
    /// Table column attnum, or None for an expression key (`indkey` entry 0).
    pub column: Option<i16>,
    /// Expression for expression keys (`pg_get_indexdef` column text, e.g. `lower(b)`,
    /// `(a + 1)`), with its dependencies. None for plain column keys.
    #[serde(default)]
    pub expr: Option<StoredExpr>,
    /// Name of this index attribute in pg_attribute: the column name for column keys, else
    /// PG's `ChooseIndexColumnNames` result: the function name for a function call (`lower`),
    /// `expr`, `expr1`, `expr2`, ... otherwise (see `crate::naming`).
    pub name: String,
    /// Key type (the column's / expression's type; typmod -1 for expressions).
    pub type_id: TypeId,
    /// `indcollation` entry (0 for non-collatable).
    pub collation: Oid,
    /// `indclass` entry: operator class OID (default btree opclass of the type, e.g. int4_ops 1978).
    pub opclass: Oid,
    /// DESC.
    #[serde(default)]
    pub descending: bool,
    /// NULLS FIRST (default: true iff DESC, as PG).
    #[serde(default)]
    pub nulls_first: bool,
}

impl IndexKey {
    /// `indoption` bits for this key: 1 = DESC, 2 = NULLS FIRST (PG `INDOPTION_*`).
    pub fn indoption(&self) -> i16 {
        (self.descending as i16) | ((self.nulls_first as i16) << 1)
    }
}

/// An index.
///
/// Invariants: `keys.len() >= n_key_columns >= 1` (INCLUDE columns follow the key columns);
/// `primary ⇒ unique`; a `primary`/`unique`-constraint index has `constraint = Some(oid)` and an
/// INTERNAL dependency on that constraint (dropping the index alone is refused, 2BP01
/// `cannot drop index t_pkey because constraint t_pkey on table t requires it` + HINT
/// `You can drop constraint t_pkey on table t instead.`).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub oid: Oid,
    pub name: String,
    /// Always the table's namespace.
    pub namespace: Oid,
    /// `indrelid`.
    pub table: Oid,
    /// Owner (always equals the table owner).
    pub owner: Oid,
    pub method: IndexMethod,
    pub keys: Vec<IndexKey>,
    /// `indnkeyatts`.
    pub n_key_columns: u16,
    pub unique: bool,
    /// `indnullsnotdistinct` (UNIQUE NULLS NOT DISTINCT).
    #[serde(default)]
    pub nulls_not_distinct: bool,
    pub primary: bool,
    /// `indisexclusion`.
    #[serde(default)]
    pub exclusion: bool,
    /// `indimmediate`: uniqueness checked immediately (false for DEFERRABLE constraints).
    #[serde(default = "default_true")]
    pub immediate: bool,
    #[serde(default)]
    pub clustered: bool,
    /// `indisvalid` / `indisready` / `indislive` (always true in v1: no concurrent builds).
    #[serde(default = "default_true")]
    pub valid: bool,
    #[serde(default)]
    pub replident: bool,
    /// Partial index predicate (`WHERE ...` text as `pg_get_expr(indpred)` prints it, e.g. `(a > 0)`).
    #[serde(default)]
    pub predicate: Option<StoredExpr>,
    /// The PRIMARY KEY / UNIQUE / EXCLUDE constraint that owns this index.
    #[serde(default)]
    pub constraint: Option<Oid>,
    /// `reloptions` of the index (`WITH (fillfactor=..)`).
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub persistence: Persistence,
}

fn default_true() -> bool {
    true
}

impl Index {
    /// Key columns (excluding INCLUDE columns).
    pub fn key_columns(&self) -> &[IndexKey] {
        &self.keys[..(self.n_key_columns as usize).min(self.keys.len())]
    }
    /// INCLUDE columns.
    pub fn include_columns(&self) -> &[IndexKey] {
        &self.keys[(self.n_key_columns as usize).min(self.keys.len())..]
    }
    /// `indkey` int2vector: attnums, 0 for expressions.
    pub fn indkey(&self) -> Vec<i16> {
        self.keys.iter().map(|k| k.column.unwrap_or(0)).collect()
    }
    /// Table columns referenced as plain keys (expressions' column deps are in their StoredExpr).
    pub fn referenced_columns(&self) -> Vec<i16> {
        self.keys.iter().filter_map(|k| k.column).collect()
    }
}
