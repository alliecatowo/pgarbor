//! Constraints (`pg_constraint`): table constraints (PRIMARY KEY, UNIQUE, CHECK, FOREIGN KEY,
//! NOT NULL, EXCLUDE) and domain constraints (CHECK, NOT NULL).
//!
//! Plain data, frozen (v1). Enforcement is pgarbor-exec's job; the catalog describes.
use super::common::StoredExpr;
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};

/// `contype`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConstraintKind {
    /// 'c'
    Check,
    /// 'f'
    ForeignKey,
    /// 'n' (PG 18 records NOT NULL as constraints, named `<table>_<column>_not_null`, or
    /// `<domain>_not_null` for domains).
    NotNull,
    /// 'p'
    PrimaryKey,
    /// 'u'
    Unique,
    /// 't' constraint trigger (later)
    Trigger,
    /// 'x' exclusion (later)
    Exclusion,
}

impl ConstraintKind {
    pub fn as_char(self) -> u8 {
        match self {
            ConstraintKind::Check => b'c',
            ConstraintKind::ForeignKey => b'f',
            ConstraintKind::NotNull => b'n',
            ConstraintKind::PrimaryKey => b'p',
            ConstraintKind::Unique => b'u',
            ConstraintKind::Trigger => b't',
            ConstraintKind::Exclusion => b'x',
        }
    }
    /// information_schema.table_constraints.constraint_type spelling: `CHECK`, `FOREIGN KEY`,
    /// `PRIMARY KEY`, `UNIQUE`. NOT NULL constraints are reported as `CHECK` (oracle-verified PG 18.6:
    /// `t_a_not_null | CHECK`, and check_constraints.check_clause `a IS NOT NULL`).
    /// Trigger/Exclusion are not shown there (None).
    pub fn info_schema_type(self) -> Option<&'static str> {
        match self {
            ConstraintKind::Check => Some("CHECK"),
            ConstraintKind::ForeignKey => Some("FOREIGN KEY"),
            ConstraintKind::PrimaryKey => Some("PRIMARY KEY"),
            ConstraintKind::Unique => Some("UNIQUE"),
            ConstraintKind::NotNull => Some("CHECK"),
            ConstraintKind::Trigger | ConstraintKind::Exclusion => None,
        }
    }
}

/// FK referential action (`confupdtype` / `confdeltype`).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FkAction {
    /// 'a'
    #[default]
    NoAction,
    /// 'r'
    Restrict,
    /// 'c'
    Cascade,
    /// 'n'
    SetNull,
    /// 'd'
    SetDefault,
}

impl FkAction {
    pub fn as_char(self) -> u8 {
        match self {
            FkAction::NoAction => b'a',
            FkAction::Restrict => b'r',
            FkAction::Cascade => b'c',
            FkAction::SetNull => b'n',
            FkAction::SetDefault => b'd',
        }
    }
    /// SQL spelling (`NO ACTION`, `RESTRICT`, `CASCADE`, `SET NULL`, `SET DEFAULT`) as used by
    /// `pg_get_constraintdef` and information_schema.referential_constraints.
    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }
}

/// FK match type (`confmatchtype`).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FkMatch {
    /// 's' MATCH SIMPLE (default)
    #[default]
    Simple,
    /// 'f' MATCH FULL
    Full,
    /// 'p' MATCH PARTIAL (PG: 0A000 `MATCH PARTIAL not yet implemented`)
    Partial,
}

impl FkMatch {
    pub fn as_char(self) -> u8 {
        match self {
            FkMatch::Simple => b's',
            FkMatch::Full => b'f',
            FkMatch::Partial => b'p',
        }
    }
    /// information_schema.referential_constraints.match_option: `NONE` (simple), `FULL`, `PARTIAL`.
    pub fn info_schema(self) -> &'static str {
        match self {
            FkMatch::Simple => "NONE",
            FkMatch::Full => "FULL",
            FkMatch::Partial => "PARTIAL",
        }
    }
}

/// Foreign key details.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    /// `confrelid`: referenced table.
    pub ref_table: Oid,
    /// `confkey`: referenced columns, parallel to `Constraint::columns`.
    pub ref_columns: Vec<i16>,
    /// The referenced table's unique/PK index used (`conindid` of the FK constraint).
    pub ref_index: Oid,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    pub match_type: FkMatch,
    /// `confdelsetcols` (`ON DELETE SET NULL (a)`), empty = all.
    #[serde(default)]
    pub delete_set_columns: Vec<i16>,
    /// `conpfeqop` / `conppeqop` / `conffeqop`: equality operator OIDs per column pair.
    #[serde(default)]
    pub pf_eq_ops: Vec<Oid>,
    #[serde(default)]
    pub pp_eq_ops: Vec<Oid>,
    #[serde(default)]
    pub ff_eq_ops: Vec<Oid>,
}

/// A constraint.
///
/// Invariants:
/// - Table constraints: `table != 0`, `domain == 0`; domain constraints the reverse.
/// - `PrimaryKey`/`Unique`/`Exclusion`: `index` = the owning index (which has an INTERNAL dep on
///   this constraint); `columns` = key attnums (`conkey`).
/// - `ForeignKey`: `fk` is Some, `index` = referenced index (`conindid`), `columns` = referencing attnums.
/// - `Check`: `check` is Some (`(e > 0)`, `pg_get_constraintdef` prints `CHECK ((e > 0))`);
///   `columns` = referenced attnums (`conkey`; e.g. `{6}`), empty for domain checks.
/// - `NotNull`: `columns` = exactly one attnum (table) or empty (domain).
/// - Name unique per table (and per domain): 42710 `constraint "c" for relation "t" already exists`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub oid: Oid,
    pub name: String,
    /// `connamespace` (the table's / domain's namespace).
    pub namespace: Oid,
    pub kind: ConstraintKind,
    /// `conrelid` (0 for domain constraints).
    pub table: Oid,
    /// `contypid` (0 for table constraints).
    #[serde(default)]
    pub domain: Oid,
    /// `conkey`.
    #[serde(default)]
    pub columns: Vec<i16>,
    /// `conindid` (0 if none).
    #[serde(default)]
    pub index: Oid,
    #[serde(default)]
    pub deferrable: bool,
    #[serde(default)]
    pub initially_deferred: bool,
    /// `convalidated` (false after `ADD ... NOT VALID` until `VALIDATE CONSTRAINT`).
    #[serde(default = "default_true")]
    pub validated: bool,
    /// `conenforced` (PG 18 `NOT ENFORCED` for CHECK/FK).
    #[serde(default = "default_true")]
    pub enforced: bool,
    #[serde(default = "default_true")]
    pub is_local: bool,
    #[serde(default)]
    pub inh_count: i16,
    /// `connoinherit` (CHECK ... NO INHERIT).
    #[serde(default)]
    pub no_inherit: bool,
    /// `conperiod` (PG 18 `WITHOUT OVERLAPS` / `PERIOD`).
    #[serde(default)]
    pub period: bool,
    #[serde(default)]
    pub fk: Option<ForeignKey>,
    /// CHECK expression (`conbin`), dependencies included.
    #[serde(default)]
    pub check: Option<StoredExpr>,
    /// `conparentid` (partition constraints; 0).
    #[serde(default)]
    pub parent: Oid,
}

fn default_true() -> bool {
    true
}

impl Constraint {
    pub fn is_domain_constraint(&self) -> bool {
        self.domain != 0
    }
}
