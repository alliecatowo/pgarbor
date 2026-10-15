//! Bound expressions: the typed, name-resolved expression tree produced by the binder and consumed
//! (after the `pgarbor-plan` set-refs pass) by the executor. Plain data, fully implemented, frozen (v1).
//!
//! ## Rules every producer and consumer relies on
//! 1. **Every node carries its result type** (`BoundExpr::ty`, a full `TypeId` with typmod). No node
//!    ever has an unresolved `unknown` type except a bare untyped NULL / string literal whose context
//!    gave no type (the binder resolves those to `text` per PG `resolveTargetListUnknowns` before it
//!    returns, except inside `Param` inference which is finalized by `binder::params`).
//! 2. **Implicit coercions are explicit nodes.** `1 + 1.5` binds to `Op(numeric + numeric)` whose left
//!    arg is `Cast(int4 -> numeric, form = Implicit)`. The executor never coerces by itself.
//! 3. **Two phases, one type.** The binder emits `Column` (bind-time variable: query level + range-table
//!    index + attnum). `pgarbor_plan` rewrites every `Column` into `InputRef` (slot in the node's input
//!    row), every correlated outer `Column` into `OuterRef`, every `Aggref`/`WindowFunc` into an
//!    `InputRef` into the aggregate/window node output, and every `SubLink` into `SubPlan`. A plan
//!    handed to the executor contains NONE of `Column`, `SubLink`, `Aggref`, `WindowFunc`, `Grouping`
//!    (they appear inside `AggCall`/`WindowCall` descriptors instead), except `Grouping` with
//!    `mask_slot: Some(..)`. Seeing one at run time is an internal error (`XX000`).
//! 4. Structural equality (`PartialEq`) is used by the planner to match GROUP BY expressions and to
//!    de-duplicate aggregates. Constants compare by `Datum` structural equality.
//! 5. Positions (for error messages) are NOT stored in the tree, with ONE exception: `ColumnRef::pos`
//!    (ignored by equality) so the grouping check can point at the offending column.
//!
//! PG references: `src/include/nodes/primnodes.h` (Var, Const, Param, FuncExpr, OpExpr,
//! ScalarArrayOpExpr, BoolExpr, SubLink, CaseExpr, ArrayExpr, RowExpr, RowCompareExpr, CoalesceExpr,
//! MinMaxExpr, SQLValueFunction, NullTest, BooleanTest, CoerceToDomain, Aggref, WindowFunc,
//! GroupingFunc, SetToDefault).
use pgarbor_common::Oid;
use pgarbor_types::catalog_data::{AggKind, Volatility};
use pgarbor_types::cast::CastPlan;
use pgarbor_types::{Datum, TypeId};

use super::query::BoundQuery;

/// A typed expression node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundExpr {
    pub kind: ExprKind,
    /// Result type (typmod included when known, else -1).
    pub ty: TypeId,
}

/// Reference to a column of a range-table entry (bind time). PG `Var`.
///
/// `PartialEq`/`Eq`/`Hash` IGNORE `pos` (two references to the same column are equal wherever they
/// were written); `pos` exists only so the grouping check can report `42803 column "t.b" must appear
/// in the GROUP BY clause ...` at the offending occurrence (PG `Var.location`).
// CONTRACT
#[derive(Clone, Copy, Debug)]
pub struct ColumnRef {
    /// 0 = the query being bound; n = n-th enclosing query (correlated reference, PG `varlevelsup`).
    pub levels_up: u32,
    /// 1-based index into that query's `range_table`.
    pub rt_index: u32,
    /// 1-based column number inside the range entry; `0` = whole-row reference (`SELECT t FROM t`,
    /// type = the relation's rowtype / `record`); negative = system column (`-1` ctid, `-2` xmin,
    /// `-3` cmin, `-4` xmax, `-5` cmax, `-6` tableoid).
    pub attnum: i16,
    /// 1-based character position of the reference in the query string; `0` = unknown (planner
    /// generated, stored-expression rebinding). Not part of equality.
    pub pos: u32,
}

impl PartialEq for ColumnRef {
    fn eq(&self, other: &ColumnRef) -> bool {
        self.levels_up == other.levels_up && self.rt_index == other.rt_index && self.attnum == other.attnum
    }
}
impl Eq for ColumnRef {}
impl std::hash::Hash for ColumnRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.levels_up.hash(state);
        self.rt_index.hash(state);
        self.attnum.hash(state);
    }
}

/// Plan-time reference to the enclosing query's current row (correlated subplan parameter).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OuterRef {
    /// 0 = the row of the node that evaluates the `SubPlan` (the parameter vector passed in
    /// `SubPlanRef::args`); n = skip n further subplan boundaries.
    pub levels_up: u32,
    /// Index into that parameter vector.
    pub index: u32,
}

/// `COERCE_*` display form of a cast (affects only deparse / EXPLAIN, never evaluation).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoercionForm {
    /// `x::type` / `CAST(x AS type)` written by the user.
    Explicit,
    /// `type(x)` function-call style.
    ExplicitCall,
    /// inserted by the binder; ruleutils prints only the operand (or a `::` for constants).
    Implicit,
}

/// Reference to a resolved operator.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct OpRef {
    /// `pg_operator` OID (builtin < 16384; user operators later).
    pub op: Oid,
    /// Implementing `pg_proc` OID.
    pub proc: Oid,
    /// Operator name as in `pg_operator.oprname` (`"="`, `"~~"`, `"+"`).
    pub name: String,
    pub volatility: Volatility,
    /// The implementing proc is STRICT (NULL in -> NULL out without calling).
    pub strict: bool,
}

/// Function call. PG `FuncExpr`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FuncCallExpr {
    /// `pg_proc` OID (builtin < 16384; user functions from the catalog).
    pub func: Oid,
    /// Bare function name (for deparse / EXPLAIN / error context).
    pub name: String,
    /// Arguments in DECLARED parameter order with defaults appended and named-notation resolved;
    /// each already coerced to the declared parameter type. A variadic tail is collected into ONE
    /// `Array` argument unless `variadic_any` (see below).
    pub args: Vec<BoundExpr>,
    /// The call used (or the binder synthesized) the VARIADIC array form: the last arg is an array.
    pub variadic: bool,
    /// Set-returning function (`prorettype` set). Only legal in FROM and the SELECT list.
    pub retset: bool,
    pub volatility: Volatility,
    pub strict: bool,
    /// Display form: `true` when the user wrote a cast-like call (`int4(x)`), printed `x::integer`.
    pub is_cast_call: bool,
}

/// Operator application (unary: 1 arg; binary: 2). PG `OpExpr`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct OpExpr {
    pub op: OpRef,
    pub args: Vec<BoundExpr>,
}

/// AND / OR / NOT. PG `BoolExpr`. `And`/`Or` have >= 2 args (LEFT-nested same-op chains are flattened by
/// the binder), `Not` exactly 1.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
    Not,
}

/// Boolean connective node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoolExpr {
    pub op: BoolOp,
    pub args: Vec<BoundExpr>,
}

/// `x IS [NOT] NULL`. PG `NullTest`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct NullTest {
    pub arg: BoundExpr,
    /// `true` = IS NULL, `false` = IS NOT NULL.
    pub is_null: bool,
    /// Row-wise semantics for composite arguments: `ROW(1,NULL) IS NULL` is false, `IS NOT NULL` is
    /// "every field non-null" (PG `argisrow`). `false` for scalars.
    pub row_semantics: bool,
}

/// Which `IS [NOT] TRUE|FALSE|UNKNOWN` test.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolTestKind {
    IsTrue,
    IsNotTrue,
    IsFalse,
    IsNotFalse,
    IsUnknown,
    IsNotUnknown,
}

/// `x IS [NOT] TRUE|FALSE|UNKNOWN`. PG `BooleanTest`. Never returns NULL.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BooleanTest {
    pub arg: BoundExpr,
    pub test: BoolTestKind,
}

/// `a IS [NOT] DISTINCT FROM b` (PG `DistinctExpr`). Never returns NULL.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct DistinctFrom {
    pub left: BoundExpr,
    pub right: BoundExpr,
    /// The `=` operator for the (coerced, common) operand type.
    pub eq: OpRef,
    /// `true` = IS NOT DISTINCT FROM.
    pub negated: bool,
}

/// One `WHEN cond THEN result` arm.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CaseArm {
    /// Boolean condition. For the simple form `CASE x WHEN v` this is `CaseTestValue = v`.
    pub cond: BoundExpr,
    pub result: BoundExpr,
}

/// CASE expression. PG `CaseExpr`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CaseExpr {
    /// The simple-form operand, evaluated ONCE; `CaseTestValue` nodes inside `arms[*].cond` read it.
    pub arg: Option<Box<BoundExpr>>,
    pub arms: Vec<CaseArm>,
    /// `ELSE`; `None` yields NULL of the result type.
    pub default: Option<Box<BoundExpr>>,
}

/// `NULLIF(a, b)` (PG `NullIfExpr`): NULL when `a = b` else `a`; result type = type of `a`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct NullIfExpr {
    pub left: BoundExpr,
    pub right: BoundExpr,
    pub eq: OpRef,
}

/// GREATEST / LEAST. PG `MinMaxExpr`. NULL arguments are ignored; NULL only when all are NULL.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct MinMaxExpr {
    pub greatest: bool,
    pub args: Vec<BoundExpr>,
}

/// Type conversion. The `CastPlan` was computed at bind time by `pgarbor_types::cast::plan_cast`
/// (steps: relabel / function / via-I/O / array element coercion / typmod enforcement);
/// the executor applies it with `cast::apply_cast`. Result type = `plan.to`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CastExpr {
    pub arg: BoundExpr,
    pub plan: CastPlan,
    pub form: CoercionForm,
}

/// Domain constraint enforcement after the cast to the domain's base type (PG `CoerceToDomain`).
/// The executor looks up NOT NULL / CHECK constraints of `domain` in the catalog snapshot.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CoerceToDomain {
    pub arg: BoundExpr,
    pub domain: Oid,
    pub form: CoercionForm,
}

/// `expr COLLATE name`. v1 treats every collation as code-point order; the node is kept for
/// deparse and for the 42P21/42P22 collation-conflict rules.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct CollateExpr {
    pub arg: BoundExpr,
    pub collation: Oid,
}

/// `ARRAY[a, b, c]` (PG `ArrayExpr`). `ty` is the array type; `elem_type` the element type.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayExpr {
    /// Each element already coerced to `elem_type`. For `multidims`, each element is itself an
    /// `ArrayExpr` of the same dimensions.
    pub elements: Vec<BoundExpr>,
    pub elem_type: Oid,
    pub multidims: bool,
}

/// `ROW(a, b)` / `(a, b)` / composite construction. `ty` is `record` (oid 2249) or the composite
/// type the row was coerced to.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RowExpr {
    pub args: Vec<BoundExpr>,
    /// Field names (`f1`, `f2`, ... for anonymous records; the composite's attribute names after
    /// coercion to a named type).
    pub names: Vec<String>,
}

/// `(composite).field` (PG `FieldSelect`). `ty` is the field type.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSelect {
    pub arg: BoundExpr,
    /// 1-based field number inside the composite (for `record` values produced by `ROW`, the
    /// position).
    pub field: i16,
    pub field_name: String,
}

/// One subscript position `[lower:upper]` or `[idx]` (then `upper` is `None`, `lower` holds idx and
/// the parent `SubscriptExpr::is_slice` is false).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptItem {
    pub lower: Option<BoundExpr>,
    pub upper: Option<BoundExpr>,
}

/// `arr[i]`, `arr[i][j]`, `arr[lo:hi]` (PG `SubscriptingRef`). All index expressions are int4.
/// `ty` = element type for plain subscripts, the array type for slices. Out-of-range -> NULL.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptExpr {
    pub array: BoundExpr,
    pub subscripts: Vec<SubscriptItem>,
    pub is_slice: bool,
}

/// `left op ANY|ALL (array)` (PG `ScalarArrayOpExpr`). `x IN (1,2,3)` binds to
/// `ScalarArrayOp{ use_or: true, op: "=", right: ArrayExpr[1,2,3] }`; `x NOT IN (...)` to
/// `ScalarArrayOp{ use_or: false, op: "<>" }`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarArrayOp {
    pub op: OpRef,
    /// `true` = ANY/SOME (OR semantics), `false` = ALL (AND semantics).
    pub use_or: bool,
    pub left: BoundExpr,
    /// The array operand (type is an array whose element type equals the operator's right type).
    pub right: BoundExpr,
}

/// Comparison kind of a row comparison.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCompareKind {
    Lt,
    Le,
    Gt,
    Ge,
}

/// `(a1, a2) < (b1, b2)` lexicographic comparison (PG `RowCompareExpr`). `=`/`<>` on rows are
/// expanded into AND/OR chains by the binder and never produce this node.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct RowCompare {
    pub kind: RowCompareKind,
    pub left: Vec<BoundExpr>,
    pub right: Vec<BoundExpr>,
    /// Per-column comparison operators: `=` for the leading columns, `kind`'s operator for the last
    /// decisive pair; one `OpRef` per column pair, the `<`/`<=`/`>`/`>=` operator of that column's type.
    pub ops: Vec<OpRef>,
    /// Per-column `=` operators.
    pub eq_ops: Vec<OpRef>,
}

/// Kind of a subquery expression. PG `SubLinkType`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubLinkKind {
    /// `EXISTS (select)`; result boolean.
    Exists,
    /// `test op ANY (select)` and `test IN (select)`; boolean (three-valued).
    Any,
    /// `test op ALL (select)`.
    All,
    /// `(select single_column)` scalar subquery: NULL if no rows, 21000 `more than one row
    /// returned by a subquery used as an expression` if more than one.
    Scalar,
    /// `ARRAY(select single_column)`.
    Array,
    /// `(select a, b)` used as a row (`(x,y) = (select a,b)`); PG `ROWCOMPARE_SUBLINK`/`MULTIEXPR`.
    Row,
}

/// Subquery expression (bind time). Replaced by `SubPlan` in the physical plan.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SubLink {
    pub kind: SubLinkKind,
    /// Left operand of ANY/ALL (a `Row` for multi-column tests).
    pub test: Option<Box<BoundExpr>>,
    /// One comparison operator per tested column (`=` for IN). Empty for other kinds.
    pub ops: Vec<OpRef>,
    pub subquery: Box<BoundQuery>,
}

/// Plan-time form of a subquery expression (module docs rule 3).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SubPlanRef {
    /// Index into `PhysicalPlan::subplans`.
    pub plan_id: u32,
    pub kind: SubLinkKind,
    /// ANY/ALL test expression, in terms of the evaluating node's input row.
    pub test: Option<Box<BoundExpr>>,
    pub ops: Vec<OpRef>,
    /// Values for the subplan's `OuterRef{levels_up: 0, index: i}` parameters, evaluated in the
    /// evaluating node's input row for every probe (correlation). Empty for uncorrelated subplans
    /// (executed once and cached).
    pub args: Vec<BoundExpr>,
    /// Hashed ANY subplan (uncorrelated `x IN (select ...)`): build a hash table once.
    pub use_hash: bool,
}

/// One aggregate-ORDER-BY / WITHIN GROUP / window ORDER BY key.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct SortExpr {
    pub expr: BoundExpr,
    pub descending: bool,
    pub nulls_first: bool,
    /// `ORDER BY x USING <op>`: the ordering operator, else the type's default `<` / `>`.
    pub using_op: Option<OpRef>,
}

/// Aggregate call. PG `Aggref`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct Aggref {
    /// `pg_aggregate.aggfnoid` (= `pg_proc` OID).
    pub agg: Oid,
    pub name: String,
    /// Aggregated arguments (declared types; `count(*)` has none and `star` set).
    pub args: Vec<BoundExpr>,
    /// Direct arguments of ordered-set / hypothetical-set aggregates (`percentile_cont(0.5)`).
    pub direct_args: Vec<BoundExpr>,
    pub kind: AggKind,
    pub distinct: bool,
    pub filter: Option<Box<BoundExpr>>,
    /// `agg(x ORDER BY ...)` for normal aggregates; `WITHIN GROUP (ORDER BY ...)` for ordered-set.
    pub order_by: Vec<SortExpr>,
    pub star: bool,
    pub variadic: bool,
    /// Query level that evaluates the aggregate (PG `agglevelsup`): `0` = the query where it is
    /// written; `1` = it only references outer-query variables, so the enclosing query computes it.
    pub levels_up: u32,
}

/// Window function call (aggregate used as window function included). PG `WindowFunc`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct WindowFunc {
    pub func: Oid,
    pub name: String,
    pub args: Vec<BoundExpr>,
    /// The function is an aggregate (`sum(x) OVER ...`) rather than a pure window function.
    pub is_agg: bool,
    pub star: bool,
    pub distinct: bool,
    pub filter: Option<Box<BoundExpr>>,
    /// `IGNORE NULLS` (accepted by the parser; PG 18 rejects it for most functions with 0A000).
    pub ignore_nulls: bool,
    /// Index into `BoundSelect::windows`.
    pub window: usize,
}

/// `GROUPING(a, b)`: bitmask of which arguments are NOT grouped in the current grouping set
/// (bit for the LAST argument is the least significant: `GROUPING(a,b)` = 2*[a not grouped] +
/// [b not grouped]). Result `int4`.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct GroupingExpr {
    /// Indexes into `GroupBy::exprs` of the (validated) arguments (bind time) / into the
    /// `Aggregate` node's `group_keys` (plan time: setrefs translates them).
    pub args: Vec<usize>,
    /// Plan time only: input slot holding the `Aggregate` node's grouping mask column (`int8`; bit
    /// `k` set = group key `k` is NOT part of the current grouping set). `None` at bind time. The
    /// executor evaluates `GROUPING` from this slot; it is the only bind-time node kind that may
    /// remain in an executable plan.
    pub mask_slot: Option<u32>,
}

/// Parameterless SQL-standard value functions. PG `SQLValueFunction`.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlValueOp {
    CurrentDate,
    CurrentTime,
    CurrentTimestamp,
    LocalTime,
    LocalTimestamp,
    CurrentRole,
    CurrentUser,
    SessionUser,
    User,
    CurrentCatalog,
    CurrentSchema,
}

/// The expression variants (module docs).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    /// Literal or folded constant (typed NULL = `Const(Datum::Null)` with the NULL's type).
    Const(Datum),
    /// `$n` (1-based). Type in `BoundExpr::ty` (finalized by parameter inference).
    Param(u32),
    /// Bind-time column variable.
    Column(ColumnRef),
    /// Plan-time: slot `n` of the evaluating node's input row (joins: left row ++ right row).
    InputRef(u32),
    /// Plan-time: correlated parameter of a subplan.
    OuterRef(OuterRef),
    FuncCall(Box<FuncCallExpr>),
    Op(Box<OpExpr>),
    Bool(Box<BoolExpr>),
    NullTest(Box<NullTest>),
    BooleanTest(Box<BooleanTest>),
    DistinctFrom(Box<DistinctFrom>),
    Case(Box<CaseExpr>),
    /// Placeholder for the simple-CASE operand inside `CaseArm::cond`.
    CaseTestValue,
    Coalesce(Vec<BoundExpr>),
    NullIf(Box<NullIfExpr>),
    MinMax(Box<MinMaxExpr>),
    Cast(Box<CastExpr>),
    CoerceToDomain(Box<CoerceToDomain>),
    Collate(Box<CollateExpr>),
    Array(Box<ArrayExpr>),
    Row(Box<RowExpr>),
    FieldSelect(Box<FieldSelect>),
    Subscript(Box<SubscriptExpr>),
    ScalarArrayOp(Box<ScalarArrayOp>),
    RowCompare(Box<RowCompare>),
    SubLink(Box<SubLink>),
    SubPlan(Box<SubPlanRef>),
    Aggref(Box<Aggref>),
    WindowFunc(Box<WindowFunc>),
    Grouping(Box<GroupingExpr>),
    SqlValue(SqlValueOp),
    /// The `DEFAULT` keyword in INSERT VALUES / UPDATE SET: replaced by the column default during
    /// planning (`pgarbor_plan::dml`). `attnum` is filled by the binder.
    SetToDefault { attnum: i16 },
}

impl BoundExpr {
    pub fn new(kind: ExprKind, ty: TypeId) -> BoundExpr {
        BoundExpr { kind, ty }
    }
    /// Typed constant.
    pub fn constant(value: Datum, ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(value), ty }
    }
    /// Typed NULL constant.
    pub fn null(ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Null), ty }
    }
    pub fn bool_const(v: bool) -> BoundExpr {
        BoundExpr::constant(Datum::Bool(v), TypeId::BOOL)
    }
    pub fn param(index: u32, ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Param(index), ty }
    }
    pub fn column(levels_up: u32, rt_index: u32, attnum: i16, ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::Column(ColumnRef { levels_up, rt_index, attnum, pos: 0 }), ty }
    }
    /// Bind-time column variable with its source position.
    pub fn column_at(levels_up: u32, rt_index: u32, attnum: i16, ty: TypeId, pos: Option<u32>) -> BoundExpr {
        BoundExpr {
            kind: ExprKind::Column(ColumnRef { levels_up, rt_index, attnum, pos: pos.unwrap_or(0) }),
            ty,
        }
    }
    pub fn input_ref(index: u32, ty: TypeId) -> BoundExpr {
        BoundExpr { kind: ExprKind::InputRef(index), ty }
    }
    /// `a AND b AND ...`; one arg returns it unchanged; empty returns constant TRUE.
    pub fn and_all(mut args: Vec<BoundExpr>) -> BoundExpr {
        match args.len() {
            0 => BoundExpr::bool_const(true),
            1 => args.remove(0),
            _ => BoundExpr {
                kind: ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::And, args })),
                ty: TypeId::BOOL,
            },
        }
    }
    /// `a OR b OR ...`; one arg returns it unchanged; empty returns constant FALSE.
    pub fn or_all(mut args: Vec<BoundExpr>) -> BoundExpr {
        match args.len() {
            0 => BoundExpr::bool_const(false),
            1 => args.remove(0),
            _ => BoundExpr {
                kind: ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::Or, args })),
                ty: TypeId::BOOL,
            },
        }
    }
    /// `NOT arg`.
    pub fn not(arg: BoundExpr) -> BoundExpr {
        BoundExpr {
            kind: ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::Not, args: vec![arg] })),
            ty: TypeId::BOOL,
        }
    }
    pub fn as_const(&self) -> Option<&Datum> {
        match &self.kind {
            ExprKind::Const(d) => Some(d),
            _ => None,
        }
    }
    pub fn is_const(&self) -> bool {
        matches!(self.kind, ExprKind::Const(_))
    }
    pub fn is_null_const(&self) -> bool {
        matches!(self.kind, ExprKind::Const(Datum::Null))
    }
    pub fn is_true_const(&self) -> bool {
        matches!(self.kind, ExprKind::Const(Datum::Bool(true)))
    }
    pub fn is_false_const(&self) -> bool {
        matches!(self.kind, ExprKind::Const(Datum::Bool(false)))
    }
    /// The type of this node.
    pub fn type_id(&self) -> TypeId {
        self.ty
    }
}
