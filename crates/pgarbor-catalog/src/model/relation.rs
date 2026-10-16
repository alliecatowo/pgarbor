//! Relations (`pg_class` rows of kind table / partitioned table / view / materialized view /
//! composite type / foreign table) and their columns (`pg_attribute` + `pg_attrdef`).
//!
//! Indexes and sequences are also `pg_class` rows but have their own structs
//! (`model::index::Index`, `model::sequence::Sequence`); they share the relation NAME namespace
//! (relname + relnamespace unique across all of them, PG's pg_class_relname_nsp_index).
//!
//! Plain data, frozen (v1).
use super::common::{Acl, StoredExpr};
use pgarbor_common::Oid;
use pgarbor_types::TypeId;
use serde::{Deserialize, Serialize};

/// `relkind`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelKind {
    /// 'r' ordinary table.
    Table,
    /// 'i' index (only used for `Index` objects / system indexes).
    Index,
    /// 'S' sequence (only used for `Sequence` objects).
    Sequence,
    /// 'v' view.
    View,
    /// 'm' materialized view.
    MatView,
    /// 'c' composite type (`CREATE TYPE c AS (...)`).
    CompositeType,
    /// 'f' foreign table (not supported; reserved).
    ForeignTable,
    /// 'p' partitioned table.
    PartitionedTable,
    /// 'I' partitioned index.
    PartitionedIndex,
    /// 't' TOAST table (never created by PGArbor; reserved).
    Toast,
}

impl RelKind {
    /// The pg_class.relkind character.
    pub fn as_char(self) -> u8 {
        match self {
            RelKind::Table => b'r',
            RelKind::Index => b'i',
            RelKind::Sequence => b'S',
            RelKind::View => b'v',
            RelKind::MatView => b'm',
            RelKind::CompositeType => b'c',
            RelKind::ForeignTable => b'f',
            RelKind::PartitionedTable => b'p',
            RelKind::PartitionedIndex => b'I',
            RelKind::Toast => b't',
        }
    }
    pub fn from_char(c: u8) -> Option<RelKind> {
        Some(match c {
            b'r' => RelKind::Table,
            b'i' => RelKind::Index,
            b'S' => RelKind::Sequence,
            b'v' => RelKind::View,
            b'm' => RelKind::MatView,
            b'c' => RelKind::CompositeType,
            b'f' => RelKind::ForeignTable,
            b'p' => RelKind::PartitionedTable,
            b'I' => RelKind::PartitionedIndex,
            b't' => RelKind::Toast,
            _ => return None,
        })
    }
    /// The word PG uses in object descriptions and most messages: `table`, `index`, `sequence`,
    /// `view`, `materialized view`, `composite type`, `foreign table`, `table` (partitioned),
    /// `index` (partitioned), `toast table`.
    pub fn description_word(self) -> &'static str {
        match self {
            RelKind::Table | RelKind::PartitionedTable => "table",
            RelKind::Index | RelKind::PartitionedIndex => "index",
            RelKind::Sequence => "sequence",
            RelKind::View => "view",
            RelKind::MatView => "materialized view",
            RelKind::CompositeType => "composite type",
            RelKind::ForeignTable => "foreign table",
            RelKind::Toast => "toast table",
        }
    }
    /// Kinds that hold rows in relation data space (`[relid][rowid]`): table, matview.
    pub fn has_storage(self) -> bool {
        matches!(self, RelKind::Table | RelKind::MatView | RelKind::Sequence | RelKind::Index | RelKind::Toast)
    }
    /// Kinds that have a rowtype (pg_type composite) and an array type: table, view, matview,
    /// composite type, foreign table, partitioned table.
    pub fn has_rowtype(self) -> bool {
        matches!(
            self,
            RelKind::Table
                | RelKind::View
                | RelKind::MatView
                | RelKind::CompositeType
                | RelKind::ForeignTable
                | RelKind::PartitionedTable
        )
    }
    /// Kinds that get the six system columns (-1 ctid .. -6 tableoid) in pg_attribute:
    /// table, matview, sequence, toast, foreign table, partitioned table (not views, indexes,
    /// composite types). Verified against PG 18.6 for tables.
    pub fn has_system_columns(self) -> bool {
        matches!(
            self,
            RelKind::Table
                | RelKind::MatView
                | RelKind::Sequence
                | RelKind::Toast
                | RelKind::ForeignTable
                | RelKind::PartitionedTable
        )
    }
}

/// `relpersistence`.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Persistence {
    /// 'p'
    #[default]
    Permanent,
    /// 'u' (UNLOGGED: accepted and stored; PGArbor treats it like permanent).
    Unlogged,
    /// 't' (TEMP: session-local schema `pg_temp_N`; later work).
    Temp,
}

impl Persistence {
    pub fn as_char(self) -> u8 {
        match self {
            Persistence::Permanent => b'p',
            Persistence::Unlogged => b'u',
            Persistence::Temp => b't',
        }
    }
}

/// `relreplident`.
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplicaIdentity {
    /// 'd' (the default for tables; views/indexes/sequences show 'n').
    #[default]
    Default,
    /// 'n'
    Nothing,
    /// 'f'
    Full,
    /// 'i' (index named by `Index::is_replident`).
    Index,
}

impl ReplicaIdentity {
    pub fn as_char(self) -> u8 {
        match self {
            ReplicaIdentity::Default => b'd',
            ReplicaIdentity::Nothing => b'n',
            ReplicaIdentity::Full => b'f',
            ReplicaIdentity::Index => b'i',
        }
    }
}

/// `attidentity`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IdentityKind {
    /// 'a' GENERATED ALWAYS AS IDENTITY
    Always,
    /// 'd' GENERATED BY DEFAULT AS IDENTITY
    ByDefault,
}

impl IdentityKind {
    pub fn as_char(self) -> u8 {
        match self {
            IdentityKind::Always => b'a',
            IdentityKind::ByDefault => b'd',
        }
    }
}

/// An identity column's sequence link (PG: internal pg_depend from the sequence to the column).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub kind: IdentityKind,
    /// The identity sequence (named `<table>_<column>_seq` like serial; OWNED BY the column).
    pub sequence: Oid,
}

/// `attgenerated`. PG 18: `GENERATED ALWAYS AS (expr)` without a keyword is VIRTUAL
/// (attgenerated 'v'); `... STORED` is 's'. The expression is stored in the column's
/// `default` slot (pg_attrdef row, `atthasdef = true`), exactly like PG.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GeneratedKind {
    Stored,
    Virtual,
}

impl GeneratedKind {
    pub fn as_char(self) -> u8 {
        match self {
            GeneratedKind::Stored => b's',
            GeneratedKind::Virtual => b'v',
        }
    }
}

/// A column default (or generation expression): one `pg_attrdef` row.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDefault {
    /// pg_attrdef OID (allocated from the branch OID counter).
    pub oid: Oid,
    /// `pg_get_expr(adbin, adrelid)` text, e.g. `nextval('t_id_seq'::regclass)`, `'x'::text`, `(a * 2)`.
    pub expr: StoredExpr,
}

/// One column (`pg_attribute` row with attnum > 0).
///
/// Invariants:
/// - `Relation.columns[i].attnum == i + 1` always; attnums are never reused or renumbered.
/// - A dropped column keeps its slot with `dropped = true`, `name = "........pg.dropped.N........"`
///   (N = attnum, exactly 8 dots each side, as PG), `type_id.oid = 0`, `not_null = false`,
///   `default = None`; it is invisible to name lookup and `SELECT *`, but its slot stays so stored
///   row tuples (positional) keep decoding.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub attnum: i16,
    pub name: String,
    /// Declared type + typmod (`atttypid`, `atttypmod`).
    pub type_id: TypeId,
    /// `attnotnull`. PG 18 also records every NOT NULL as a `pg_constraint` row of kind 'n'
    /// (`<table>_<col>_not_null`); this flag mirrors "some valid NOT NULL constraint exists".
    pub not_null: bool,
    /// Default value or generation expression (`atthasdef` = `default.is_some()`).
    #[serde(default)]
    pub default: Option<ColumnDefault>,
    #[serde(default)]
    pub identity: Option<Identity>,
    #[serde(default)]
    pub generated: Option<GeneratedKind>,
    /// `attcollation`: 100 (default) for collatable types unless COLLATE given, 950 for `name`
    /// columns, 0 for non-collatable types. Equal to the type's `typcollation` by default.
    pub collation: Oid,
    /// `attndims` (declared array dimensions: `int[]` → 1, `int[][]` → 2; informational only).
    #[serde(default)]
    pub ndims: i32,
    #[serde(default)]
    pub dropped: bool,
    /// `atthasmissing`/`attmissingval`: value for rows stored before `ADD COLUMN ... DEFAULT <const>`,
    /// as the type's text output (`io::output`) of the evaluated default, None = NULL. Readers of
    /// a stored tuple with fewer columns than `attnum` use this value (else NULL).
    #[serde(default)]
    pub missing_value: Option<String>,
    /// `attislocal` (false only for inherited columns; later).
    #[serde(default = "default_true")]
    pub is_local: bool,
    /// `attinhcount`.
    #[serde(default)]
    pub inh_count: i16,
    /// Column privileges (`attacl`).
    #[serde(default)]
    pub acl: Option<Acl>,
    /// `attoptions` (`ALTER COLUMN SET (n_distinct = ...)`), `name=value` strings.
    #[serde(default)]
    pub options: Vec<String>,
    /// `attstattarget` (None = default -1 / NULL in PG 18).
    #[serde(default)]
    pub stat_target: Option<i16>,
    /// `attstorage` override (`ALTER COLUMN SET STORAGE`), None = the type's `typstorage`.
    #[serde(default)]
    pub storage: Option<u8>,
    /// `attcompression` (b'p' pglz / b'l' lz4), None = '\0'.
    #[serde(default)]
    pub compression: Option<u8>,
}

fn default_true() -> bool {
    true
}

impl Column {
    /// A plain nullable column with the type's default collation (`collation` must be supplied by
    /// the caller from `TypeInfo::collation`).
    pub fn new(attnum: i16, name: impl Into<String>, type_id: TypeId, collation: Oid) -> Column {
        Column {
            attnum,
            name: name.into(),
            type_id,
            not_null: false,
            default: None,
            identity: None,
            generated: None,
            collation,
            ndims: 0,
            dropped: false,
            missing_value: None,
            is_local: true,
            inh_count: 0,
            acl: None,
            options: Vec::new(),
            stat_target: None,
            storage: None,
            compression: None,
        }
    }
    /// PG's name for a dropped column: `........pg.dropped.<attnum>........`.
    pub fn dropped_name(attnum: i16) -> String {
        format!("........pg.dropped.{attnum}........")
    }
    /// Visible to name lookup / `*` expansion.
    pub fn is_visible(&self) -> bool {
        !self.dropped
    }
    pub fn has_default(&self) -> bool {
        self.default.is_some()
    }
}

/// View metadata (`relkind` 'v' / 'm').
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewDef {
    /// The defining query in `pg_get_viewdef(oid)` format (leading space, 4-space indented clauses,
    /// trailing `;`), with `depends_on` listing every referenced relation/column/function/type.
    /// For views, column-level dependencies are recorded on the `_RETURN` rule in PG; PGArbor records
    /// them on the view (`describe` still prints `view v depends on column b of table t`).
    pub query: StoredExpr,
    /// `WITH [CASCADED|LOCAL] CHECK OPTION`: None, Some("local"), Some("cascaded").
    #[serde(default)]
    pub check_option: Option<String>,
    /// Materialized view populated flag (`relispopulated`; `WITH NO DATA` → false). Views: true.
    #[serde(default = "default_true")]
    pub populated: bool,
}

/// Partition key (`pg_partitioned_table`).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionKey {
    /// b'h' hash, b'l' list, b'r' range.
    pub strategy: u8,
    /// Key column attnums (0 for an expression, consumed in order from `exprs`).
    pub columns: Vec<i16>,
    pub exprs: Vec<StoredExpr>,
    /// Result types of `exprs`, parallel to it (amendment pgarbor-catalog-policies-rules).
    #[serde(default)]
    pub expr_types: Vec<TypeId>,
}

/// A relation: table, partitioned table, view, materialized view, composite type, foreign table.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub oid: Oid,
    /// Max 63 bytes.
    pub name: String,
    pub namespace: Oid,
    pub kind: RelKind,
    pub owner: Oid,
    #[serde(default)]
    pub persistence: Persistence,
    /// Columns in attnum order (see `Column` invariants). `relnatts` = `columns.len()` (dropped included).
    pub columns: Vec<Column>,
    /// `reltype`: the composite rowtype's pg_type OID (kinds with `has_rowtype()`), else 0.
    pub rowtype: Oid,
    /// The rowtype's array type OID (`_<name>`), else 0.
    pub array_type: Oid,
    /// The array type's typname when it is NOT the canonical `naming::array_type_name(name)`
    /// (PG `makeArrayTypeName` prepends extra `_` on collision). None = canonical. Renaming the
    /// relation renames the array type to the new canonical name (and resets this to None).
    #[serde(default)]
    pub array_type_name: Option<String>,
    /// `reloptions` (`WITH (fillfactor=70)`), `name=value` strings in given order.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub acl: Option<Acl>,
    #[serde(default)]
    pub replica_identity: ReplicaIdentity,
    /// `relrowsecurity` / `relforcerowsecurity` (later).
    #[serde(default)]
    pub row_security: bool,
    #[serde(default)]
    pub force_row_security: bool,
    /// Present for views and materialized views.
    #[serde(default)]
    pub view: Option<ViewDef>,
    /// Present for partitioned tables (later).
    #[serde(default)]
    pub partition_key: Option<PartitionKey>,
    /// `relispartition` + bound text (`FOR VALUES ...`) for partitions (later).
    #[serde(default)]
    pub partition_of: Option<(Oid, String)>,
    /// Parents in `pg_inherits` order (later).
    #[serde(default)]
    pub inherits: Vec<Oid>,
    /// Row-level security policies (`pg_policy` rows), in creation order (amendment pgarbor-catalog-policies-rules).
    #[serde(default)]
    pub policies: Vec<Policy>,
    /// User rewrite rules (`pg_rewrite` rows other than a view's `_RETURN`), in creation order
    /// (amendment pgarbor-catalog-policies-rules).
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// A row-level security policy (`pg_policy`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub oid: Oid,
    /// `polname` (unique per table).
    pub name: String,
    /// `polcmd`: b'*' ALL, b'r' SELECT, b'a' INSERT, b'w' UPDATE, b'd' DELETE.
    pub cmd: u8,
    /// `polpermissive`.
    pub permissive: bool,
    /// `polroles`; `0` = PUBLIC.
    pub roles: Vec<Oid>,
    /// `polqual` (USING), canonical text over the table's columns.
    #[serde(default)]
    pub using: Option<StoredExpr>,
    /// `polwithcheck` (WITH CHECK).
    #[serde(default)]
    pub with_check: Option<StoredExpr>,
}

/// A user rewrite rule (`pg_rewrite`, `CREATE RULE`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub oid: Oid,
    pub name: String,
    /// `ev_type`: b'1' SELECT, b'2' UPDATE, b'3' INSERT, b'4' DELETE.
    pub event: u8,
    /// `is_instead`.
    pub instead: bool,
    /// `ev_enabled`: b'O' (origin, default), b'D' disabled, b'R' replica, b'A' always.
    #[serde(default = "default_rule_enabled")]
    pub enabled: u8,
    /// `WHERE` condition text as written in the canonical rule definition (over `old`/`new`).
    #[serde(default)]
    pub qual: Option<String>,
    /// The actions as canonical statement texts (empty = `NOTHING`).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Objects the rule's condition and actions reference (relations, columns, functions, types).
    #[serde(default)]
    pub depends_on: Vec<super::common::ObjectAddress>,
}

fn default_rule_enabled() -> u8 {
    b'O'
}

impl Relation {
    /// Column by attnum (1-based), dropped columns included.
    pub fn column(&self, attnum: i16) -> Option<&Column> {
        if attnum < 1 {
            return None;
        }
        self.columns.get(attnum as usize - 1)
    }
    /// Visible column by exact (already case-folded) name.
    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| !c.dropped && c.name == name)
    }
    /// Visible columns in attnum order.
    pub fn visible_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| !c.dropped)
    }
    /// Next attnum for ADD COLUMN.
    pub fn next_attnum(&self) -> i16 {
        self.columns.len() as i16 + 1
    }
    /// Built-in (virtual) system relation (`oid < FIRST_NORMAL_OID`): no row storage; rows come
    /// from `crate::vrel`.
    pub fn is_virtual(&self) -> bool {
        self.oid < pgarbor_common::FIRST_NORMAL_OID
    }
    pub fn is_view(&self) -> bool {
        matches!(self.kind, RelKind::View)
    }
}
