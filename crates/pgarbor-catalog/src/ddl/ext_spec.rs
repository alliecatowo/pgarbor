//! Inputs of the extensibility DDL (amendment pgarbor-catalog-09). The binder folds identifiers,
//! converts written types to [`TypeName`]s (resolved here, where PG's `typenameTypeId(NULL, ..)`
//! runs: no error position), resolves roles, and leaves every object lookup to the catalog so a
//! statement sees its own earlier effects. One [`ExtDdl`] is one statement; `CatalogTxn::run_ext`
//! executes it.
use pgarbor_common::Oid;
use pgarbor_types::names::TypeName;

/// A folded, possibly schema-qualified name.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QName {
    pub schema: Option<String>,
    pub name: String,
}

impl QName {
    pub fn new(schema: Option<String>, name: impl Into<String>) -> QName {
        QName { schema, name: name.into() }
    }
    /// `NameListToString`: `schema.name` or `name`.
    pub fn display(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// A `def_arg` value (PG `DefElem.arg`).
#[derive(Clone, Debug, PartialEq)]
pub enum DefVal {
    /// `func_type` (`T_TypeName`): usable as a type or, through its name, as a qualified name.
    Type { tn: TypeName, setof: bool, qname: Option<QName> },
    /// `qual_all_Op` (a name list ending in an operator).
    Oper(QName),
    /// `NumericOnly` (`T_Integer` when it has no `.`/`e`, else `T_Float`).
    Number(String),
    /// `Sconst`, reserved keyword or `NONE` (`T_String`).
    Str(String),
}

/// One option: `name [= value]`.
#[derive(Clone, Debug, PartialEq)]
pub struct DefItem {
    pub name: String,
    pub value: Option<DefVal>,
}

/// A routine reference with an optional argument type list.
#[derive(Clone, Debug, PartialEq)]
pub struct FuncRef {
    pub name: QName,
    /// `None` = no argument list written.
    pub args: Option<Vec<TypeName>>,
}

/// One operand of an operator signature (`None` = `NONE`).
pub type OperArgType = Option<TypeName>;

/// `name(left, right)`.
#[derive(Clone, Debug, PartialEq)]
pub struct OperRef {
    pub name: QName,
    pub left: OperArgType,
    pub right: OperArgType,
}

/// One aggregate argument declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct AggArgSpec {
    pub name: String,
    pub ty: TypeName,
    /// b'i' (also for no mode) or b'v'.
    pub mode: u8,
    /// IN was written explicitly.
    pub explicit_mode: bool,
}

/// `aggr_args`.
#[derive(Clone, Debug, PartialEq)]
pub struct AggArgsSpec {
    pub args: Vec<AggArgSpec>,
    /// -1 without ORDER BY, else the number of direct arguments.
    pub order_by_at: i32,
    pub star: bool,
}

/// `aggregate_with_argtypes`.
#[derive(Clone, Debug, PartialEq)]
pub struct AggRef {
    pub name: QName,
    pub args: AggArgsSpec,
}

/// Generic ALTER action.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterExt {
    Rename(String),
    Owner(Oid),
    SetSchema(String),
}

/// CREATE OPERATOR CLASS / ALTER OPERATOR FAMILY ADD items.
#[derive(Clone, Debug, PartialEq)]
pub enum OpClassItemSpec {
    Operator { number: i32, op: QName, types: Option<(OperArgType, OperArgType)>, order_family: Option<QName> },
    Function { number: i32, types: Option<Vec<OperArgType>>, func: FuncRef },
    Storage(TypeName),
}

/// ALTER OPERATOR FAMILY DROP items.
#[derive(Clone, Debug, PartialEq)]
pub struct OpFamilyDropSpec {
    pub is_operator: bool,
    pub number: i32,
    pub types: Vec<TypeName>,
}

/// Object designators of COMMENT ON / ALTER EXTENSION ADD|DROP (handled by the catalog for the
/// extensibility kinds; the binder resolves every other kind to an `ObjectAddress`).
#[derive(Clone, Debug, PartialEq)]
pub enum ExtObjRef {
    Operator(OperRef),
    Aggregate(AggRef),
    OpClass { name: QName, method: String },
    OpFamily { name: QName, method: String },
    Cast { source: TypeName, target: TypeName },
    Conversion(QName),
    Collation(QName),
    Extension(String),
    /// Already resolved by the binder.
    Address(crate::model::ObjectAddress),
}

/// One extensibility statement.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtDdl {
    CreateOperator { name: QName, defs: Vec<DefItem> },
    AlterOperatorSet { op: OperRef, defs: Vec<DefItem> },
    AlterOperatorOwner { op: OperRef, owner: Oid },
    AlterOperatorSchema { op: OperRef, schema: String },
    DropOperators { if_exists: bool, ops: Vec<OperRef>, cascade: bool },

    CreateAggregate { or_replace: bool, name: QName, args: Option<AggArgsSpec>, defs: Vec<DefItem> },

    CreateOpFamily { name: QName, method: String },
    AlterOpFamilyAdd { name: QName, method: String, items: Vec<OpClassItemSpec> },
    AlterOpFamilyDrop { name: QName, method: String, items: Vec<OpFamilyDropSpec> },
    AlterOpFamily { name: QName, method: String, action: AlterExt },
    DropOpFamilies { if_exists: bool, names: Vec<QName>, method: String, cascade: bool },
    CreateOpClass {
        name: QName,
        is_default: bool,
        for_type: TypeName,
        method: String,
        family: Option<QName>,
        items: Vec<OpClassItemSpec>,
    },
    AlterOpClass { name: QName, method: String, action: AlterExt },
    DropOpClasses { if_exists: bool, names: Vec<QName>, method: String, cascade: bool },

    CreateCast { source: TypeName, target: TypeName, func: Option<FuncRef>, inout: bool, context: u8 },
    DropCast { if_exists: bool, source: TypeName, target: TypeName, cascade: bool },

    CreateConversion { is_default: bool, name: QName, for_encoding: String, to_encoding: String, func: QName },
    AlterConversion { name: QName, action: AlterExt },
    DropConversions { if_exists: bool, names: Vec<QName>, cascade: bool },

    CreateCollation { if_not_exists: bool, name: QName, from: Option<QName>, defs: Vec<DefItem> },
    AlterCollation { name: QName, action: Option<AlterExt> },
    DropCollations { if_exists: bool, names: Vec<QName>, cascade: bool },

    CreateExtension { if_not_exists: bool, name: String, schema: Option<String>, version: Option<String>, cascade: bool },
    AlterExtensionUpdate { name: String, version: Option<String> },
    AlterExtensionSchema { name: String, schema: String },
    AlterExtensionMember { name: String, add: bool, object: ExtObjRef, kind_word: String },
    DropExtensions { if_exists: bool, names: Vec<String>, cascade: bool },

    Comment { object: ExtObjRef, text: Option<String> },
}

impl ExtDdl {
    /// PG command tag.
    pub fn tag(&self) -> &'static str {
        use ExtDdl::*;
        match self {
            CreateOperator { .. } => "CREATE OPERATOR",
            AlterOperatorSet { .. } | AlterOperatorOwner { .. } | AlterOperatorSchema { .. } => "ALTER OPERATOR",
            DropOperators { .. } => "DROP OPERATOR",
            CreateAggregate { .. } => "CREATE AGGREGATE",
            CreateOpFamily { .. } => "CREATE OPERATOR FAMILY",
            AlterOpFamilyAdd { .. } | AlterOpFamilyDrop { .. } | AlterOpFamily { .. } => "ALTER OPERATOR FAMILY",
            DropOpFamilies { .. } => "DROP OPERATOR FAMILY",
            CreateOpClass { .. } => "CREATE OPERATOR CLASS",
            AlterOpClass { .. } => "ALTER OPERATOR CLASS",
            DropOpClasses { .. } => "DROP OPERATOR CLASS",
            CreateCast { .. } => "CREATE CAST",
            DropCast { .. } => "DROP CAST",
            CreateConversion { .. } => "CREATE CONVERSION",
            AlterConversion { .. } => "ALTER CONVERSION",
            DropConversions { .. } => "DROP CONVERSION",
            CreateCollation { .. } => "CREATE COLLATION",
            AlterCollation { .. } => "ALTER COLLATION",
            DropCollations { .. } => "DROP COLLATION",
            CreateExtension { .. } => "CREATE EXTENSION",
            AlterExtensionUpdate { .. } | AlterExtensionSchema { .. } | AlterExtensionMember { .. } => "ALTER EXTENSION",
            DropExtensions { .. } => "DROP EXTENSION",
            Comment { .. } => "COMMENT",
        }
    }
}
