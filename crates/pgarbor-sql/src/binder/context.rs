//! Shared binder state and helper types. Fully implemented, frozen (v1).
use std::collections::{HashMap, VecDeque};

use pgarbor_catalog::CatalogEnv;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::context::{IoConfig, IoCtx};
use pgarbor_types::names as type_names;
use pgarbor_types::{TypeEnv, TypeId};
use sqlparser::ast::{Ident, ObjectName, Spanned};
use sqlparser::tokenizer::Span;

use crate::diag::ExprClause;
use crate::ir::BoundStatement;
use crate::names::{ident_name, qual_name, QualName};
use crate::source::SourceMap;

use super::cte::CteFrame;
use super::params::ParamTable;
use super::scope::QueryScope;

/// Session facts the binder needs that are not in the catalog snapshot.
// CONTRACT
#[derive(Clone, Debug)]
pub struct BindSession {
    /// Name of the connected database (`current_database()`); a 3-part name must match it
    /// (otherwise 0A000 `cross-database references are not implemented: db.s.t`).
    pub database: String,
}

/// Everything `bind` needs besides the statement.
// CONTRACT
pub struct BindContext<'a> {
    /// Catalog snapshot + resolved `search_path` + current/session user (current at statement start).
    pub env: &'a CatalogEnv,
    pub session: BindSession,
    /// Session `DateStyle` etc.: literal conversion (`'01/02/2020'::date`) depends on it.
    pub io_config: IoConfig,
    /// Transaction start time (TimestampTz microseconds since 2000-01-01 UTC) for the special
    /// literals `'now'`, `'today'`, `'tomorrow'`, `'yesterday'` converted at bind time.
    pub statement_timestamp: i64,
    /// Parameter types declared by the client in Parse (`Parse` message `paramTypes`). Entry `i`
    /// is the type of `$i+1`; OID 0 (or `unknown`) = infer. May be shorter than the number of
    /// parameters the statement uses. `PREPARE name (t1, t2)` fills this too.
    pub declared_params: Vec<TypeId>,
}

/// Result of binding one statement.
// CONTRACT
#[derive(Clone, Debug)]
pub struct BindOutput {
    pub statement: BoundStatement,
    /// Final type of `$1..$n` (n = highest parameter number referenced or declared). Describe uses
    /// them for ParameterDescription. Unresolved parameters are an error (42P18), never `unknown`.
    pub param_types: Vec<TypeId>,
    /// NOTICE-severity diagnostics raised during binding (identifier truncation, ...), in order.
    pub notices: Vec<PgError>,
}

/// An already-bound argument together with the source position of the AST node it came from, so
/// that unknown-literal conversion and coercion failures point at the right token
/// (`select 1 + 'x'` -> `invalid input syntax for type integer: "x"` at the literal, position 12).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct BoundArg {
    pub expr: BoundExpr,
    pub pos: Option<u32>,
}

use crate::ir::BoundExpr;

impl BoundArg {
    pub fn new(expr: BoundExpr, pos: Option<u32>) -> BoundArg {
        BoundArg { expr, pos }
    }
    /// The argument's type.
    pub fn ty(&self) -> TypeId {
        self.expr.ty
    }
    /// `unknown`-typed literal / NULL / parameter that still awaits its type.
    pub fn is_unknown(&self) -> bool {
        self.expr.ty.oid == pgarbor_types::oids::UNKNOWN
    }
}

/// Which expression positions allow which constructs (PG `ParseExprKind` + the pstate flags).
/// Constructors below name the usual contexts.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExprCtx {
    /// Used in `aggregate functions are not allowed in X` style errors.
    pub clause: ExprClause,
    pub aggs_ok: bool,
    pub windows_ok: bool,
    /// Set-returning functions in the target list (only SELECT list / ORDER BY? no: SELECT list).
    pub srf_ok: bool,
    pub sublinks_ok: bool,
    /// Column references allowed (false for `DEFAULT` expressions, `LIMIT`, `VALUES` in INSERT-less
    /// contexts: 42703 `cannot use column reference in DEFAULT expression` etc.).
    pub columns_ok: bool,
}

impl ExprCtx {
    const fn base(clause: ExprClause) -> ExprCtx {
        ExprCtx { clause, aggs_ok: false, windows_ok: false, srf_ok: false, sublinks_ok: true, columns_ok: true }
    }
    /// SELECT list (aggregates, windows, SRFs allowed).
    pub const fn select_list() -> ExprCtx {
        ExprCtx { aggs_ok: true, windows_ok: true, srf_ok: true, ..ExprCtx::base(ExprClause::Other("SELECT list")) }
    }
    /// `WHERE`.
    pub const fn where_() -> ExprCtx {
        ExprCtx::base(ExprClause::Where)
    }
    /// `HAVING` (aggregates allowed, windows not).
    pub const fn having() -> ExprCtx {
        ExprCtx { aggs_ok: true, ..ExprCtx::base(ExprClause::Having) }
    }
    /// `GROUP BY`.
    pub const fn group_by() -> ExprCtx {
        ExprCtx::base(ExprClause::GroupBy)
    }
    /// `ORDER BY` / `DISTINCT ON` of a query (aggregates and windows allowed like the target list).
    pub const fn order_by() -> ExprCtx {
        ExprCtx { aggs_ok: true, windows_ok: true, srf_ok: true, ..ExprCtx::base(ExprClause::Other("ORDER BY")) }
    }
    /// `JOIN ... ON`.
    pub const fn join_on() -> ExprCtx {
        ExprCtx::base(ExprClause::JoinOn)
    }
    /// `RETURNING` (aggregates/windows not allowed in RETURNING; SRFs not allowed either).
    pub const fn returning() -> ExprCtx {
        ExprCtx::base(ExprClause::Other("RETURNING"))
    }
    /// `LIMIT` / `OFFSET` / `FETCH`: no columns, no sublinks of the same level.
    pub const fn limit() -> ExprCtx {
        ExprCtx { columns_ok: false, ..ExprCtx::base(ExprClause::Limit) }
    }
    /// VALUES list cells.
    pub const fn values() -> ExprCtx {
        ExprCtx { srf_ok: false, ..ExprCtx::base(ExprClause::Values) }
    }
    /// Column `DEFAULT` expression (no columns, no sublinks, no aggregates).
    pub const fn column_default() -> ExprCtx {
        ExprCtx { columns_ok: false, sublinks_ok: false, ..ExprCtx::base(ExprClause::ColumnDefault) }
    }
    /// `CHECK` constraint / domain check (columns of the table allowed, no sublinks).
    pub const fn check() -> ExprCtx {
        ExprCtx { sublinks_ok: false, ..ExprCtx::base(ExprClause::CheckConstraint) }
    }
    /// Index expression / predicate.
    pub const fn index_expr() -> ExprCtx {
        ExprCtx { sublinks_ok: false, ..ExprCtx::base(ExprClause::IndexExpression) }
    }
    /// `GENERATED ALWAYS AS (expr) STORED`.
    pub const fn generated() -> ExprCtx {
        ExprCtx { sublinks_ok: false, ..ExprCtx::base(ExprClause::GeneratedColumn) }
    }
    /// Arguments of `EXECUTE name(args)` / `CALL` (no columns).
    pub const fn execute_args() -> ExprCtx {
        ExprCtx { columns_ok: false, ..ExprCtx::base(ExprClause::ExecuteParameter) }
    }
    /// A function in the FROM clause: arguments may reference earlier LATERAL items only.
    pub const fn function_in_from() -> ExprCtx {
        ExprCtx::base(ExprClause::FunctionInFrom)
    }
    /// `UPDATE .. SET col = expr`.
    pub const fn update_set() -> ExprCtx {
        ExprCtx::base(ExprClause::ReturningInUpdate)
    }
    /// Window `PARTITION BY` / `ORDER BY` / frame offsets.
    pub const fn window_def(clause: ExprClause) -> ExprCtx {
        ExprCtx { aggs_ok: true, ..ExprCtx::base(clause) }
    }
    /// The same context with aggregates/windows/SRFs/sublinks switched off (used for the argument of
    /// an aggregate: no nested aggregates, no windows).
    pub const fn inside_aggregate(self) -> ExprCtx {
        ExprCtx { aggs_ok: false, windows_ok: false, ..self }
    }
}

/// Per-statement mutable binder state, threaded as `&mut Binder` through every bind function.
pub struct Binder<'a> {
    pub ctx: &'a BindContext<'a>,
    pub env: &'a CatalogEnv,
    pub src: &'a SourceMap,
    /// NOTICEs collected for [`BindOutput::notices`].
    pub notices: Vec<PgError>,
    /// `$n` types (declared + inferred).
    pub params: ParamTable,
    /// Query scopes being bound, outermost first; `scopes.last()` is the innermost (`levels_up == 0`).
    pub scopes: Vec<QueryScope>,
    /// WITH-clause name frames, outermost first (lookup goes innermost -> outermost).
    pub cte_frames: Vec<CteFrame>,
    /// Current nesting depth of `bind_expr` / `bind_query` recursion (guard: 54001).
    pub depth: u32,
    /// > 0 while binding the arguments of an aggregate (nested aggregate detection).
    pub agg_depth: u32,
    /// Free-form per-statement cache: stored-expression rebinding results, view expansion guard
    /// (`views being expanded`, detects 42P17 infinite recursion).
    pub expanding_views: Vec<Oid>,
    /// Dependencies observed while binding DDL expressions (`ObjectAddress` list is built by
    /// `stored`); keyed by object kind + oid, insertion ordered.
    pub deps: Vec<pgarbor_catalog::ObjectAddress>,
    /// Scratch map for modules that need to remember a per-statement fact by name.
    pub scratch: HashMap<&'static str, String>,
    /// Identity option lists of the statement (`identity_opts`), consumed front to back by each
    /// identity column the binder processes.
    pub identity_groups: VecDeque<crate::identity_opts::IdentityGroup>,
    /// `ON DELETE SET NULL|SET DEFAULT` groups of the statement (`fk_set_opts`), consumed by each foreign key
    /// the binder processes with that action.
    pub fk_set_groups: Vec<crate::fk_set_opts::FkSetGroup>,
    /// Trailing `WITH [NO] DATA` / `WITH [CASCADED|LOCAL] CHECK OPTION` of a CREATE [MATERIALIZED] VIEW
    /// (`view_trailer_opts`), read by `ddl_view::bind_create_view`.
    pub view_trailer: Option<crate::view_trailer_opts::ViewTrailer>,
    /// `OVERRIDING {SYSTEM|USER} VALUE` of the top-level INSERT (`parser::extract_insert_overriding`), taken by
    /// `insert::bind_insert_in_scope`.
    pub overriding: Option<crate::ir::stmt::Overriding>,
}

/// Maximum recursion depth of expression/query binding before 54001 `stack depth limit exceeded`.
pub const MAX_BIND_DEPTH: u32 = 256;

impl<'a> Binder<'a> {
    /// New state for one statement.
    pub fn new(ctx: &'a BindContext<'a>, src: &'a SourceMap) -> Binder<'a> {
        Binder {
            ctx,
            env: ctx.env,
            src,
            notices: Vec::new(),
            params: ParamTable::new(&ctx.declared_params),
            scopes: Vec::new(),
            cte_frames: Vec::new(),
            depth: 0,
            agg_depth: 0,
            expanding_views: Vec::new(),
            deps: Vec::new(),
            scratch: HashMap::new(),
            identity_groups: VecDeque::new(),
            fk_set_groups: Vec::new(),
            view_trailer: None,
            overriding: None,
        }
    }

    /// Position (1-based char offset) of the start of an AST node (`None` for empty spans).
    pub fn pos<S: Spanned>(&self, node: &S) -> Option<u32> {
        self.src.pos_of_span(node.span())
    }
    /// Position of a raw span.
    pub fn pos_span(&self, span: Span) -> Option<u32> {
        self.src.pos_of_span(span)
    }
    /// Record a NOTICE.
    pub fn notice(&mut self, n: PgError) {
        self.notices.push(n);
    }
    /// Folded + truncated identifier (pushes the truncation NOTICE).
    pub fn ident(&mut self, id: &Ident) -> String {
        ident_name(id, &mut self.notices)
    }
    /// Qualified name conversion (`names::qual_name`), notices collected.
    pub fn qual(&mut self, name: &ObjectName) -> Result<QualName> {
        qual_name(name, self.src, &mut self.notices)
    }
    /// Type input/output context with the statement timestamp.
    pub fn io_ctx(&self) -> IoCtx<'a> {
        IoCtx::new(self.env as &dyn TypeEnv, &self.ctx.io_config).with_now(self.ctx.statement_timestamp)
    }
    /// `format_type_be`-style name of a type for error messages.
    pub fn type_name(&self, ty: TypeId) -> String {
        type_names::display_name(self.env, ty.oid)
    }
    /// `format_type` with typmod (`character varying(10)`).
    pub fn type_name_typmod(&self, ty: TypeId) -> String {
        type_names::format_type(self.env, ty.oid, ty.typmod).unwrap_or_else(|_| self.type_name(ty))
    }
    /// The innermost query scope. Binder bug (XX000) when no scope is open.
    pub fn scope(&self) -> Result<&QueryScope> {
        self.scopes.last().ok_or_else(|| PgError::internal("binder: no open query scope"))
    }
    /// Mutable innermost scope.
    pub fn scope_mut(&mut self) -> Result<&mut QueryScope> {
        self.scopes.last_mut().ok_or_else(|| PgError::internal("binder: no open query scope"))
    }
    /// Enter a nested bind call; 54001 when too deep.
    pub fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_BIND_DEPTH {
            return Err(PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
                "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate.",
            ));
        }
        Ok(())
    }
    /// Leave a nested bind call.
    pub fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}
