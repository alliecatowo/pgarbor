//! AST of the extensibility statements parsed by `parser_ext` (amendment pgarbor-sql-05):
//! CREATE/ALTER/DROP OPERATOR [FAMILY|CLASS], AGGREGATE, CAST, CONVERSION, COLLATION, EXTENSION and
//! COMMENT ON for those object kinds. Names are kept as written (`Ident` with quote style) so the
//! binder applies PG's case folding and truncation; positions are 1-based character positions.
use sqlparser::ast::{DataType, Ident, ObjectName, OperateFunctionArg};

/// A possibly schema-qualified operator name (`schema.===`).
#[derive(Clone, Debug, PartialEq)]
pub struct OperName {
    pub schema: Option<Ident>,
    pub name: String,
    pub pos: Option<u32>,
}

/// `PG def_arg`.
#[derive(Clone, Debug, PartialEq)]
pub enum DefArg {
    /// `func_type`: a type name, which may also be read as a (qualified) function or object name.
    Type(DataType),
    /// `SETOF type`.
    Setof(DataType),
    /// `qual_all_Op`: an operator symbol or `OPERATOR(schema.op)`.
    Operator(OperName),
    /// `NumericOnly` (sign kept).
    Number(String),
    /// `Sconst`.
    String(String),
    /// A reserved keyword (lower-cased) or `NONE` (`"none"`).
    Keyword(String),
}

/// `PG def_elem`: `name [= arg]`; `name` is lower-cased unless quoted.
#[derive(Clone, Debug, PartialEq)]
pub struct DefElem {
    pub name: String,
    pub arg: Option<DefArg>,
    pub pos: Option<u32>,
}

/// One operand type of `oper_argtypes`; `None` for `NONE`.
pub type OperArg = Option<DataType>;

/// `operator_with_argtypes`: `name (left, right)`.
#[derive(Clone, Debug, PartialEq)]
pub struct OperSig {
    pub name: OperName,
    pub left: OperArg,
    pub right: OperArg,
}

/// `aggr_args`: `(*)`, `(args)`, `(ORDER BY args)`, `(args ORDER BY args)`.
#[derive(Clone, Debug, PartialEq)]
pub struct AggrArgs {
    /// Every argument declaration in order (direct then ordered), with the duplicated VARIADIC item of
    /// the `(... VARIADIC x ORDER BY VARIADIC x)` form already dropped.
    pub args: Vec<OperateFunctionArg>,
    /// -1 without ORDER BY, else the number of direct arguments before ORDER BY.
    pub order_by_at: i32,
    pub star: bool,
}

/// `aggregate_with_argtypes`.
#[derive(Clone, Debug, PartialEq)]
pub struct AggSig {
    pub name: ObjectName,
    pub args: AggrArgs,
    pub pos: Option<u32>,
}

/// Generic ALTER actions.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterAct {
    Rename(Ident),
    Owner(Ident),
    SetSchema(Ident),
}

/// `opclass_item` / `opclass_drop` of CREATE OPERATOR CLASS / ALTER OPERATOR FAMILY.
#[derive(Clone, Debug, PartialEq)]
pub enum OpClassItem {
    /// `OPERATOR n op [(left, right)] [FOR SEARCH | FOR ORDER BY family]`.
    Operator { number: i32, op: OperName, args: Option<(OperArg, OperArg)>, order_family: Option<ObjectName>, pos: Option<u32> },
    /// `FUNCTION n [(left [, right])] func [(args)]`.
    Function {
        number: i32,
        types: Option<Vec<OperArg>>,
        func: ObjectName,
        func_args: Option<Vec<OperateFunctionArg>>,
        pos: Option<u32>,
    },
    /// `STORAGE type`.
    Storage(DataType),
}

/// `opclass_drop`: `OPERATOR n (types)` / `FUNCTION n (types)`.
#[derive(Clone, Debug, PartialEq)]
pub struct OpFamilyDropItem {
    pub is_operator: bool,
    pub number: i32,
    pub types: Vec<OperArg>,
    pub pos: Option<u32>,
}

/// CREATE CAST method.
#[derive(Clone, Debug, PartialEq)]
pub enum CastMethodAst {
    Function { name: ObjectName, args: Option<Vec<OperateFunctionArg>> },
    WithoutFunction,
    Inout,
}

/// What ALTER EXTENSION ADD/DROP or COMMENT ON names.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectRef {
    /// TABLE / SEQUENCE / VIEW / MATERIALIZED VIEW / FOREIGN TABLE / INDEX (keyword upper-case).
    Relation { keyword: String, name: ObjectName },
    /// TYPE / DOMAIN.
    Type { keyword: String, ty: DataType },
    Schema(Ident),
    /// FUNCTION / PROCEDURE / ROUTINE with optional argument list.
    Routine { keyword: String, name: ObjectName, args: Option<Vec<OperateFunctionArg>> },
    Aggregate(AggSig),
    Operator(OperSig),
    OpClass { name: ObjectName, method: Ident },
    OpFamily { name: ObjectName, method: Ident },
    Cast { source: DataType, target: DataType },
    Conversion(ObjectName),
    Collation(ObjectName),
    Extension(Ident),
    /// Other keywords we recognize but do not support as members (`LANGUAGE x`, `SERVER x`, ...).
    Other { keyword: String },
}

/// One extensibility statement.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtStmt {
    CreateOperator { name: OperName, defs: Vec<DefElem> },
    AlterOperator { sig: OperSig, action: AlterOperatorAction },
    DropOperator { if_exists: bool, sigs: Vec<OperSig>, cascade: bool },

    CreateAggregate { or_replace: bool, name: ObjectName, args: Option<AggrArgs>, defs: Vec<DefElem>, pos: Option<u32> },
    AlterAggregate { sig: AggSig, action: AlterAct },
    DropAggregate { if_exists: bool, sigs: Vec<AggSig>, cascade: bool },

    CreateOpFamily { name: ObjectName, method: Ident },
    AlterOpFamilyAdd { name: ObjectName, method: Ident, items: Vec<OpClassItem> },
    AlterOpFamilyDrop { name: ObjectName, method: Ident, items: Vec<OpFamilyDropItem> },
    AlterOpFamily { name: ObjectName, method: Ident, action: AlterAct },
    DropOpFamily { if_exists: bool, names: Vec<ObjectName>, method: Ident, cascade: bool },
    CreateOpClass {
        name: ObjectName,
        is_default: bool,
        for_type: DataType,
        method: Ident,
        family: Option<ObjectName>,
        items: Vec<OpClassItem>,
    },
    AlterOpClass { name: ObjectName, method: Ident, action: AlterAct },
    DropOpClass { if_exists: bool, names: Vec<ObjectName>, method: Ident, cascade: bool },

    CreateCast { source: DataType, target: DataType, method: CastMethodAst, context: u8 },
    DropCast { if_exists: bool, source: DataType, target: DataType, cascade: bool },

    CreateConversion { is_default: bool, name: ObjectName, for_encoding: String, to_encoding: String, func: ObjectName },
    AlterConversion { name: ObjectName, action: AlterAct },
    DropConversion { if_exists: bool, names: Vec<ObjectName>, cascade: bool },

    CreateCollation { if_not_exists: bool, name: ObjectName, from: Option<ObjectName>, defs: Vec<DefElem> },
    AlterCollation { name: ObjectName, action: Option<AlterAct> },
    DropCollation { if_exists: bool, names: Vec<ObjectName>, cascade: bool },

    CreateExtension { if_not_exists: bool, name: Ident, schema: Option<Ident>, version: Option<String>, cascade: bool },
    AlterExtensionUpdate { name: Ident, version: Option<String> },
    AlterExtensionSchema { name: Ident, schema: Ident },
    AlterExtensionMember { name: Ident, add: bool, object: ObjectRef },
    DropExtension { if_exists: bool, names: Vec<Ident>, cascade: bool },

    Comment { object: ObjectRef, text: Option<String> },
}

/// ALTER OPERATOR actions.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterOperatorAction {
    Owner(Ident),
    SetSchema(Ident),
    /// `SET (option [= value|NONE], ...)`; `arg = None` for `NONE` or a bare option.
    Set(Vec<DefElem>),
}
