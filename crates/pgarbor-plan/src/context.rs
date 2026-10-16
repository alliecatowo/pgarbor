//! Planner inputs and per-statement state. Fully implemented, frozen (v1).
use std::collections::HashMap;

use pgarbor_catalog::CatalogEnv;
use pgarbor_common::Oid;
use pgarbor_sql::ir::BoundCte;
use pgarbor_types::{Datum, TypeId};

use crate::plan::{CteDef, SubPlanDef};

/// Planner switches (PG `enable_*` GUCs and EXPLAIN needs).
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct PlanOptions {
    pub enable_seqscan: bool,
    pub enable_indexscan: bool,
    pub enable_hashjoin: bool,
    pub enable_mergejoin: bool,
    pub enable_nestloop: bool,
    pub enable_hashagg: bool,
    pub enable_sort: bool,
    pub enable_material: bool,
    /// Collect EXPLAIN properties (`explain::annotate_plan` runs before setrefs). Set when planning the
    /// inner statement of EXPLAIN.
    pub for_explain: bool,
    /// `EXPLAIN VERBOSE`: affects only property generation (qualified columns, `Output:` lines).
    pub verbose: bool,
    /// Default row estimate for a table without statistics.
    pub default_table_rows: f64,
}

impl Default for PlanOptions {
    fn default() -> PlanOptions {
        PlanOptions {
            enable_seqscan: true,
            enable_indexscan: true,
            enable_hashjoin: true,
            enable_mergejoin: true,
            enable_nestloop: true,
            enable_hashagg: true,
            enable_sort: true,
            enable_material: true,
            for_explain: false,
            verbose: false,
            default_table_rows: 1000.0,
        }
    }
}

/// Everything `plan()` needs.
pub struct PlanContext<'a> {
    pub env: &'a CatalogEnv,
    /// Final parameter types of the statement (`BindOutput::param_types`).
    pub param_types: Vec<TypeId>,
    /// Parameter VALUES when planning for a specific execution ("custom plan": `Param` nodes may be
    /// folded to constants for index selection). `None` = generic plan: keep `Param`.
    pub param_values: Option<Vec<Datum>>,
    pub options: PlanOptions,
    /// Optional row-count hints per table OID (approximate live row counts supplied by the engine,
    /// e.g. from storage tree metadata). Missing entries use `options.default_table_rows`.
    pub table_rows: HashMap<Oid, f64>,
}

/// One WITH list visible to a query level: the bound CTEs and the first `CteDef` id reserved for its
/// materialized members.
#[derive(Debug)]
pub struct CteScope {
    pub ctes: Vec<BoundCte>,
    pub base: u32,
}

/// Mutable state threaded through the planner modules for one statement.
pub struct PlannerState<'a> {
    pub ctx: &'a PlanContext<'a>,
    /// Sub-plans created so far (index = `plan_id`).
    pub subplans: Vec<SubPlanDef>,
    /// CTEs planned so far (index = `cte_id`).
    pub ctes: Vec<CteDef>,
    /// Bound CTE definitions of each enclosing query level (innermost last), for CteScan planning.
    pub cte_scopes: Vec<std::sync::Arc<CteScope>>,
    /// `cte_scopes` as it stood when each query level (index = level id) was entered. Sub-links and
    /// sub-selects are converted to physical plans after `build_query` has left their WITH scopes, so
    /// `planner::plan_logical` re-installs this snapshot while converting a level.
    pub level_cte_scopes: Vec<Vec<std::sync::Arc<CteScope>>>,
    /// Relations the plan reads (for `PlanDeps`).
    pub dep_relations: Vec<Oid>,
    pub dep_types: Vec<Oid>,
    pub dep_functions: Vec<Oid>,
    /// Range table names of EVERY query level seen so far, indexed by level id (never popped):
    /// (alias, column names by attnum-1). Registered by `build::build_query` via [`PlannerState::enter_level`].
    pub range_names: Vec<Vec<(String, Vec<String>)>>,
    /// Parent query level for resolving correlated references across derived-table boundaries.
    pub level_parents: Vec<Option<u32>>,
    /// Level ids of the query blocks currently being built (innermost last).
    pub level_stack: Vec<u32>,
    /// Level id stamped on nodes created by [`PlannerState::new_node`]; `planner::plan_logical` sets it to
    /// `LogicalPlan::level` while converting a logical node and restores it afterwards.
    pub cur_level: u32,
    /// Planner scratch by key (module-private bookkeeping).
    pub scratch: HashMap<&'static str, i64>,
}

impl<'a> PlannerState<'a> {
    pub fn new(ctx: &'a PlanContext<'a>) -> PlannerState<'a> {
        PlannerState {
            ctx,
            subplans: Vec::new(),
            ctes: Vec::new(),
            cte_scopes: Vec::new(),
            level_cte_scopes: Vec::new(),
            dep_relations: Vec::new(),
            dep_types: Vec::new(),
            dep_functions: Vec::new(),
            range_names: Vec::new(),
            level_parents: Vec::new(),
            level_stack: Vec::new(),
            cur_level: 0,
            scratch: HashMap::new(),
        }
    }
    /// Register a new query level with its range table names; returns the level id (also pushed on
    /// `level_stack`; pop with [`PlannerState::leave_level`]).
    pub fn enter_level(&mut self, names: Vec<(String, Vec<String>)>) -> u32 {
        let id = self.range_names.len() as u32;
        self.range_names.push(names);
        self.level_parents.push(self.level_stack.last().copied());
        self.level_cte_scopes.push(self.cte_scopes.clone());
        self.level_stack.push(id);
        id
    }
    /// Leave the innermost query level (the names stay registered).
    pub fn leave_level(&mut self) {
        self.level_stack.pop();
    }
    /// The innermost query level currently being built (0 when none).
    pub fn current_level(&self) -> u32 {
        self.level_stack.last().copied().unwrap_or(0)
    }
    /// New plan node stamped with `cur_level`.
    pub fn new_node(&self, kind: crate::plan::NodeKind, out: Vec<crate::plan::OutCol>) -> crate::plan::PlanNode {
        let mut n = crate::plan::PlanNode::new(kind, out);
        n.level = self.cur_level;
        n
    }
    /// Record a relation dependency (dedup).
    pub fn note_relation(&mut self, oid: Oid) {
        if !self.dep_relations.contains(&oid) {
            self.dep_relations.push(oid);
        }
    }
}
