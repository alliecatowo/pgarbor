//! Canonical SQL text for bound expressions and queries, byte-exact with PostgreSQL's `ruleutils.c`
//! (`pg_get_expr`, `pg_get_constraintdef`, `pg_get_viewdef`, EXPLAIN expression display).
//!
//! The catalog stores expressions as CANONICAL TEXT (catalog contract invariant 5): column defaults,
//! CHECK bodies, index expressions/predicates, generation expressions, view queries. This module
//! produces that text from the bound IR (SQL-43/44) and is also used by `pgarbor-plan`'s EXPLAIN for
//! quals, targets and sort keys (SQL-43 `ColumnNamer` indirection).
//!
//! ## Naming columns: [`ColumnNamer`]
//! `BoundExpr` columns are `Column{levels_up, rt_index, attnum}` (bind time) or `InputRef(n)` /
//! `OuterRef{..}` (plan time). The deparser never looks at a range table itself; the caller supplies a
//! [`ColumnNamer`] that renders each reference as text (`a`, `t.a`, `"Weird Col"`). Provided namers:
//! [`TableColumns`] (single table, unqualified names: DDL expressions) and [`RangeColumns`]
//! (`pgarbor_sql` range tables: view bodies, bind-time EXPLAIN). The planner provides its own.
//!
//! ## Options
//! `DeparseOptions` mirror the ruleutils flags the tests exercise.
use pgarbor_common::Result;
use pgarbor_types::TypeEnv;

use crate::ir::{ColumnRef, OuterRef};

pub mod expr;
pub mod query;

/// Renders variable references. Return `None` for a reference the namer cannot resolve (the
/// deparser then fails with XX000 `could not deparse variable`).
// CONTRACT
pub trait ColumnNamer {
    /// Text for a bind-time column reference (quoted per `quote_identifier` rules, qualified or not as
    /// the namer decides).
    fn column(&self, col: &ColumnRef) -> Option<String>;
    /// Text for a plan-time input slot (`a`, `t.a`, or `INNER_VAR.col`-style for join outputs).
    fn input_ref(&self, index: u32) -> Option<String>;
    /// Text for a correlated parameter, e.g. `t.a` of the outer plan (EXPLAIN prints the outer column).
    fn outer_ref(&self, outer: &OuterRef) -> Option<String>;
    /// Text for a plan-time sub-plan reference, e.g. `(SubPlan 1)` / `(InitPlan 2).col1`.
    fn subplan(&self, plan_id: u32) -> Option<String> {
        let _ = plan_id;
        None
    }
}

/// Deparse flags.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeparseOptions {
    /// `PRETTY_PAREN`: omit redundant parentheses (`a + b` instead of `(a + b)`). pg_get_expr(.., false)
    /// default is NOT pretty: full parenthesization.
    pub pretty_paren: bool,
    /// Print `schema.` on functions/relations/types that are not visible in the search_path (always
    /// done by ruleutils; tests may disable for stability).
    pub qualify_non_visible: bool,
    /// EXPLAIN display mode: plan-time nodes allowed (`InputRef`), parameters as `$1`, no trailing
    /// `;`, `VERBOSE` column qualification decided by the namer.
    pub explain: bool,
}

impl Default for DeparseOptions {
    fn default() -> DeparseOptions {
        DeparseOptions { pretty_paren: false, qualify_non_visible: true, explain: false }
    }
}

/// Everything the deparser needs.
pub struct DeparseCtx<'a> {
    pub env: &'a dyn TypeEnv,
    /// Catalog access for object names (relations, functions, types) and search_path visibility.
    pub catalog: &'a pgarbor_catalog::CatalogEnv,
    pub namer: &'a dyn ColumnNamer,
    pub opts: DeparseOptions,
}

/// Namer for expressions over ONE table whose columns are known by position: `Column{attnum}` ->
/// `names[attnum-1]` quoted when needed, unqualified. `levels_up > 0` or `rt_index != 1` -> `None`.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct TableColumns {
    pub names: Vec<String>,
}

impl ColumnNamer for TableColumns {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        if col.levels_up != 0 || col.rt_index != 1 || col.attnum < 1 {
            return None;
        }
        self.names.get(col.attnum as usize - 1).and_then(|n| pgarbor_types::text::format::quote_identifier(n).ok())
    }
    fn input_ref(&self, _index: u32) -> Option<String> {
        None
    }
    fn outer_ref(&self, _outer: &OuterRef) -> Option<String> {
        None
    }
}

/// Namer over bind-time range tables (innermost scope last in `scopes`): renders `alias.col` when
/// `qualify` is true else `col`. Used by `query::deparse_viewdef` and bind-time diagnostics.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct RangeColumns {
    /// `scopes[len-1-levels_up]` is the range table of that level: (alias, column names by attnum).
    pub scopes: Vec<Vec<(String, Vec<String>)>>,
    pub qualify: bool,
}

impl ColumnNamer for RangeColumns {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        let level = self.scopes.len().checked_sub(1 + col.levels_up as usize)?;
        let rt = self.scopes.get(level)?;
        let (alias, cols) = rt.get(col.rt_index as usize - 1)?;
        if col.attnum < 1 {
            return None;
        }
        let name = pgarbor_types::text::format::quote_identifier(cols.get(col.attnum as usize - 1)?).ok()?;
        if self.qualify {
            Some(format!("{}.{}", pgarbor_types::text::format::quote_identifier(alias).ok()?, name))
        } else {
            Some(name)
        }
    }
    fn input_ref(&self, _index: u32) -> Option<String> {
        None
    }
    fn outer_ref(&self, _outer: &OuterRef) -> Option<String> {
        None
    }
}

#[allow(dead_code)]
fn _result_marker() -> Result<()> {
    Ok(())
}
