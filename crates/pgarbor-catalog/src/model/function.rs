//! User-defined functions and procedures (`pg_proc` rows with OID >= 16384).
//!
//! Builtin procs are NOT stored: they come from `pgarbor_types::catalog_data::PROCS`.
//! Execution of SQL / PL/pgSQL bodies is later work (pgarbor-exec); the catalog stores the
//! definition so DDL, `\df`, `pg_get_functiondef` and information_schema.routines work.
//!
//! Plain data, frozen (v1).
use super::common::{Acl, StoredExpr};
use pgarbor_common::Oid;
use pgarbor_types::catalog_data::{ProcKind, Volatility};
use serde::{Deserialize, Serialize};

/// `prokind` (serde mirror of `pgarbor_types::catalog_data::ProcKind`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FuncKind {
    Function,
    Aggregate,
    Window,
    Procedure,
}

impl FuncKind {
    pub fn to_types(self) -> ProcKind {
        match self {
            FuncKind::Function => ProcKind::Function,
            FuncKind::Aggregate => ProcKind::Aggregate,
            FuncKind::Window => ProcKind::Window,
            FuncKind::Procedure => ProcKind::Procedure,
        }
    }
}

/// `provolatile` (serde mirror of `pgarbor_types::catalog_data::Volatility`).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FuncVolatility {
    Immutable,
    Stable,
    #[default]
    Volatile,
}

impl FuncVolatility {
    pub fn to_types(self) -> Volatility {
        match self {
            FuncVolatility::Immutable => Volatility::Immutable,
            FuncVolatility::Stable => Volatility::Stable,
            FuncVolatility::Volatile => Volatility::Volatile,
        }
    }
}

/// `proargmodes` entry.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArgMode {
    In,
    Out,
    InOut,
    Variadic,
    Table,
}

impl ArgMode {
    /// b'i', b'o', b'b', b'v', b't'.
    pub fn as_char(self) -> u8 {
        match self {
            ArgMode::In => b'i',
            ArgMode::Out => b'o',
            ArgMode::InOut => b'b',
            ArgMode::Variadic => b'v',
            ArgMode::Table => b't',
        }
    }
}

/// One declared parameter.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuncArg {
    /// Empty when unnamed.
    pub name: String,
    pub type_oid: Oid,
    pub mode: ArgMode,
    /// Default expression (`DEFAULT expr` / `= expr`), IN/INOUT/VARIADIC only.
    #[serde(default)]
    pub default: Option<StoredExpr>,
}

/// A user function / procedure.
///
/// Identity is (namespace, name, IN-argument types): `CREATE FUNCTION` of an existing identity →
/// 42723 `function f(integer) already exists with same argument types`.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Function {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub kind: FuncKind,
    /// `prolang`: 14 sql, 13647 plpgsql (builtin language OIDs, see `builtin::LANGUAGES`).
    pub language: Oid,
    /// All declared parameters in order (IN, OUT, INOUT, VARIADIC, TABLE).
    pub args: Vec<FuncArg>,
    /// `prorettype` (2249 record for multiple OUT params, 2278 void for procedures).
    pub ret: Oid,
    pub retset: bool,
    pub volatility: FuncVolatility,
    pub strict: bool,
    #[serde(default)]
    pub security_definer: bool,
    #[serde(default)]
    pub leakproof: bool,
    /// `proparallel`: b's' safe, b'r' restricted, b'u' unsafe (default).
    #[serde(default = "default_parallel")]
    pub parallel: u8,
    /// `procost` (default 100; 1 for C/internal) and `prorows` (1000 for SRFs, else 0).
    pub cost: f32,
    pub rows: f32,
    /// `prosrc`: the body text (for `AS $$...$$`).
    pub body: String,
    /// `prosqlbody` (SQL-standard `BEGIN ATOMIC ... END` / `RETURN expr` bodies), deparsed text.
    #[serde(default)]
    pub sql_body: Option<StoredExpr>,
    /// `proconfig` (`SET x = y` clauses), `name=value`.
    #[serde(default)]
    pub config: Vec<String>,
    #[serde(default)]
    pub acl: Option<Acl>,
    /// Types / relations / functions the definition depends on (argument & return types get
    /// NORMAL deps automatically; this lists additional body deps for BEGIN ATOMIC bodies).
    #[serde(default)]
    pub depends_on: Vec<super::common::ObjectAddress>,
}

fn default_parallel() -> u8 {
    b'u'
}

impl Function {
    /// `proargtypes`: IN, INOUT and VARIADIC parameter types.
    pub fn in_arg_types(&self) -> Vec<Oid> {
        self.args
            .iter()
            .filter(|a| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|a| a.type_oid)
            .collect()
    }
    /// `pronargdefaults`.
    pub fn n_defaults(&self) -> u16 {
        self.args.iter().filter(|a| a.default.is_some()).count() as u16
    }
    /// True when every parameter is IN (then `proallargtypes`/`proargmodes` are NULL in pg_proc).
    pub fn all_in(&self) -> bool {
        self.args.iter().all(|a| a.mode == ArgMode::In)
    }
}
