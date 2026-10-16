//! Extensibility catalog objects (amendment `pgarbor-catalog-09-extensibility-objects`):
//! user operators (`pg_operator`), aggregate definitions (`pg_aggregate`), operator families and
//! classes with their members (`pg_opfamily`, `pg_opclass`, `pg_amop`, `pg_amproc`), casts
//! (`pg_cast`), conversions (`pg_conversion`), collations (`pg_collation`) and extensions
//! (`pg_extension` + extension membership `pg_depend` rows with deptype `e`).
//!
//! All of them are persisted under ONE object kind byte `b'x'` (`ObjKind::Ext`) as an
//! externally-tagged serde enum [`ExtObject`]. OIDs come from the branch OID counter like every
//! other object, so they are unique inside the kind. The one exception to "own OID" is
//! [`AggregateDef`], whose key is the OID of its `pg_proc` row (`aggfnoid`, PG's `pg_aggregate`
//! primary key): the proc row is a normal `Function` with `kind = Aggregate`.
//!
//! Plain data. New fields must be `#[serde(default)]`.
use super::common::ObjectAddress;
use pgarbor_common::Oid;
use serde::{Deserialize, Serialize};

/// `pg_operator` row of a user operator. A SHELL operator (created by a forward `COMMUTATOR` /
/// `NEGATOR` reference) has `proc == 0` and `result == 0`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operator {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    /// `oprkind`: b'b' infix, b'l' prefix.
    pub kind: u8,
    pub can_merge: bool,
    pub can_hash: bool,
    /// 0 for prefix operators.
    pub left: Oid,
    pub right: Oid,
    pub result: Oid,
    pub commutator: Oid,
    pub negator: Oid,
    /// `oprcode` (0 = shell).
    pub proc: Oid,
    /// `oprrest` / `oprjoin` (0 = none).
    pub restrict: Oid,
    pub join: Oid,
}

impl Operator {
    pub fn is_shell(&self) -> bool {
        self.proc == 0
    }
}

/// `pg_aggregate` row; `fnoid` is the aggregate's `Function` OID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateDef {
    pub fnoid: Oid,
    /// `aggkind`: b'n' normal, b'o' ordered-set, b'h' hypothetical-set.
    pub kind: u8,
    pub num_direct_args: i16,
    pub transfn: Oid,
    pub finalfn: Oid,
    pub combinefn: Oid,
    pub serialfn: Oid,
    pub deserialfn: Oid,
    pub mtransfn: Oid,
    pub minvtransfn: Oid,
    pub mfinalfn: Oid,
    pub finalextra: bool,
    pub mfinalextra: bool,
    /// b'r' read-only, b's' shareable, b'w' read-write.
    pub finalmodify: u8,
    pub mfinalmodify: u8,
    pub sortop: Oid,
    pub transtype: Oid,
    pub transspace: i32,
    pub mtranstype: Oid,
    pub mtransspace: i32,
    pub initval: Option<String>,
    pub minitval: Option<String>,
}

/// One `pg_amop` row (operator family member).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmOpEntry {
    pub oid: Oid,
    pub lefttype: Oid,
    pub righttype: Oid,
    pub strategy: i16,
    /// b's' search, b'o' ordering.
    pub purpose: u8,
    pub operator: Oid,
    pub sortfamily: Oid,
    /// The operator class that created this member (`CREATE OPERATOR CLASS`): an internal
    /// dependency on the class (`pg_depend` deptype `i`); 0 for a loose family member (deptype `a`
    /// on the family).
    #[serde(default)]
    pub opclass: Oid,
}

/// One `pg_amproc` row (support function of an operator family).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmProcEntry {
    pub oid: Oid,
    pub lefttype: Oid,
    pub righttype: Oid,
    pub procnum: i16,
    pub proc: Oid,
    #[serde(default)]
    pub opclass: Oid,
}

/// `pg_opfamily` row plus its members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpFamily {
    pub oid: Oid,
    /// Access method OID (`pg_am`).
    pub method: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    #[serde(default)]
    pub operators: Vec<AmOpEntry>,
    #[serde(default)]
    pub procs: Vec<AmProcEntry>,
}

/// `pg_opclass` row of a user operator class. `family` may be a builtin family OID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpClass {
    pub oid: Oid,
    pub method: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub family: Oid,
    pub input_type: Oid,
    pub is_default: bool,
    /// `opckeytype` (0 = same as input).
    pub key_type: Oid,
}

/// `pg_cast` row of a user cast.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastDef {
    pub oid: Oid,
    pub source: Oid,
    pub target: Oid,
    /// 0 when binary-coercible or I/O.
    pub func: Oid,
    /// b'e' explicit, b'a' assignment, b'i' implicit.
    pub context: u8,
    /// b'f' function, b'b' binary, b'i' inout.
    pub method: u8,
}

/// `pg_conversion` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversion {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    /// PG encoding numbers (`pg_enc`).
    pub for_encoding: i32,
    pub to_encoding: i32,
    pub proc: Oid,
    pub is_default: bool,
}

/// `pg_collation` row of a user collation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collation {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    /// b'c' libc, b'i' icu, b'b' builtin, b'd' default.
    pub provider: u8,
    pub deterministic: bool,
    /// -1 = any encoding.
    pub encoding: i32,
    pub collate: Option<String>,
    pub ctype: Option<String>,
    pub locale: Option<String>,
    pub icu_rules: Option<String>,
    pub version: Option<String>,
}

/// `pg_extension` row plus its member objects (`pg_depend` deptype `e` rows).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub oid: Oid,
    pub name: String,
    pub owner: Oid,
    pub namespace: Oid,
    pub relocatable: bool,
    pub version: String,
    /// Members (objects created by the extension script or added with ALTER EXTENSION ADD).
    #[serde(default)]
    pub members: Vec<ObjectAddress>,
    /// Objects marked `DEPENDS ON EXTENSION` (`pg_depend` deptype `x`).
    #[serde(default)]
    pub auto_dependents: Vec<ObjectAddress>,
    /// Extensions this one requires (`pg_depend` deptype `n` on the other extension).
    #[serde(default)]
    pub requires: Vec<Oid>,
}

/// One persisted extensibility object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExtObject {
    Operator(Operator),
    Aggregate(AggregateDef),
    OpFamily(OpFamily),
    OpClass(OpClass),
    Cast(CastDef),
    Conversion(Conversion),
    Collation(Collation),
    Extension(Extension),
}

impl ExtObject {
    /// The object key OID (`fnoid` for aggregates).
    pub fn oid(&self) -> Oid {
        match self {
            ExtObject::Operator(o) => o.oid,
            ExtObject::Aggregate(a) => a.fnoid,
            ExtObject::OpFamily(f) => f.oid,
            ExtObject::OpClass(c) => c.oid,
            ExtObject::Cast(c) => c.oid,
            ExtObject::Conversion(c) => c.oid,
            ExtObject::Collation(c) => c.oid,
            ExtObject::Extension(e) => e.oid,
        }
    }
    /// Every OID embedded in the value that came from the OID counter (for `max_oid`).
    pub fn allocated_oids(&self) -> Vec<Oid> {
        match self {
            ExtObject::OpFamily(f) => {
                let mut v = vec![f.oid];
                v.extend(f.operators.iter().map(|o| o.oid));
                v.extend(f.procs.iter().map(|p| p.oid));
                v
            }
            other => vec![other.oid()],
        }
    }
}
