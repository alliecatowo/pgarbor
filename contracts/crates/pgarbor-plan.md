# pgarbor-plan — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-plan.json` (PLN-01..PLN-17, seed
tasks PLAN-01..PLAN-09). Amendments: `pgarbor-plan-01-row-count-hints`, `pgarbor-plan-02-btree-strategy-and-hash-support`.
Depends on: pgarbor-common, pgarbor-types, pgarbor-catalog, pgarbor-sql.

## Purpose

Turn `BoundStatement`s (SELECT, INSERT, UPDATE, DELETE, CREATE TABLE AS) into an executable **physical plan**
(`plan::Plan`) with output column metadata (name, type OID, table OID, attnum) for RowDescription, apply a simple
rule-based optimizer (qual simplification and pushdown, btree index selection, join strategy, aggregate/sort/
distinct strategy) and render EXPLAIN. No statistics exist in v1; decisions are heuristic and plans are always
semantically correct. No async, no I/O, no catalog writes.

## Public API

| Item | Use |
|---|---|
| `plan(&BoundStatement, &PlanContext) -> Result<Plan>` | the entry point (ARCHITECTURE §10) |
| `PlanContext { env, param_types, param_values: Option<Vec<Datum>>, options: PlanOptions, table_rows }` | generic plan (`param_values = None`, cacheable) vs custom plan |
| `plan::{Plan, PlanNode, NodeKind, ColumnDesc, ...}` | **the plan/exec contract** (fully documented in `plan.rs`: row layouts, parameter passing via `OuterRef`, DML row layouts) |
| `explain_plan(&Plan, &ExplainOptions) -> Result<Vec<String>>` | EXPLAIN text/JSON/YAML lines (`explain_fmt::render_analyze` adds actuals) |
| `logical::*`, `context::PlannerState` | internal pipeline types (public for tests) |

## Pipeline

```text
BoundStatement
  build / build_dml   : BoundQuery -> LogicalPlan (scans, joins, aggregate, window, project, distinct, sort, limit)
  simplify, pushdown, subplan : constant folding, qual distribution, outer-join reduction, sub-links -> SubPlans
  planner (+ index, join, agg, window, sort_limit, setop, cte, cost) : LogicalPlan -> PlanNode tree (bind-time exprs)
  explain::annotate_plan   : (EXPLAIN only) deparse quals/keys into node props while names are still known
  setrefs::finalize        : Column -> InputRef, Aggref/WindowFunc/Grouping -> slots, sub-plan args; clears scratch
```

## Key invariants

1. **A finalized plan contains no `Column`, `SubLink`, `Aggref`, `WindowFunc`**; `Grouping` only with
   `mask_slot: Some`. `setrefs::verify_executable` asserts it.
2. **Row layouts are positional and documented per node** (`plan.rs`): scans emit the whole table row
   (`ncols` slots, dropped = NULL) plus an optional trailing rowid; joins concatenate outer ++ inner; Aggregate
   emits keys ++ aggs [++ grouping mask]; WindowAgg emits input ++ window results; DML nodes consume the layouts of
   `InsertSpec`/`UpdateSpec`.
3. **Parameterized rescans use `OuterRef`**: index nested loops and correlated sub-plans pass values through
   `inner_params` / `SubPlanRef::args`; uncorrelated sub-plans (`is_initplan`) run once.
4. **No index-only scans**: memcomparable index keys are not decodable.
5. **Semantics before speed**: every optimization is optional; the unoptimized plan (seq scans, nested loops,
   sorts) must be correct. Volatile expressions are never moved or folded.
6. **Planner scratch fields** (`PlanNode::vars`, `ordering`, `level`) exist only between construction and `setrefs`.
   Sub-query boundaries are pass-through `Project`s (the child's own `vars` are never rewritten); `setrefs`
   translates each scan against its own `vars` and removes identity projections. Nothing is keyed by rt_index
   across nodes (rt indexes restart per query level; self-joins share OIDs). `needed` masks are all-true in v1.
7. **EXPLAIN**: shape may differ from PG (no statistics); the LINE FORMAT, labels and property names are PG's
   (fixtures in `explain.rs`/`explain_fmt.rs`); `COSTS OFF` is what differential tests compare.
8. Unimplemented behavior is `Err(PgError::unimplemented("PLN-NN", ..))`, never a wrong plan.

## Heuristics (v1, no statistics)

Unique-index point lookups always use the index; equality prefix / range on an index uses it; ORDER BY +
LIMIT over an ordering index avoids the sort; equi-joins use hash joins (smaller estimated input builds), merge
join when both sides already ordered, index nested loop for unique point lookups on the inner side; non-equi joins
use nested loops (inner materialized); FULL/RIGHT joins need hash or merge keys (else 0A000 as PG); DISTINCT and
GROUP BY hash unless an ordering is already available. Estimates: 1000 rows per table unless `table_rows` hints;
default selectivities from PG `selfuncs.c`.

## How downstream crates use it

- **pgarbor-exec** interprets `Plan` (volcano iterators); evaluates expressions with pgarbor-types; implements
  `NestedLoop` parameter passing, `SubPlan` caching, `CteDef` buffering, `ModifyTable` constraint/index maintenance.
- **pgarbor** calls `plan` after `bind` (generic plans for prepared statements, custom plans for known parameter
  values), caches plans keyed by statement text + catalog version (`Plan::deps` / `catalog_version` with
  `invalidation::plan_is_stale`), supplies `table_rows` hints (amendment plan-01), and for EXPLAIN plans the inner
  statement with `options.for_explain = true` then calls `explain_plan`.

## Non-goals and known gaps

Statistics/ANALYZE, parallel plans, bitmap scans, partition pruning, inheritance, cost-based join enumeration beyond
greedy ordering, expression-index matching, partial-index implication beyond exact match, merge append, JIT, EXPLAIN
FORMAT XML (0A000), `ANALYZE` actuals collection (the executor feeds `ActualStats`).

## Lease index

| lease | file | tier | scope |
|---|---|---|---|
| PLN-01 | build.rs | haiku | [north-star] BoundQuery -> LogicalPlan: range scans, FROM tree, WHERE, project; assembly of agg/window/sort/limit stages. |
| PLN-02 | simplify.rs | haiku | [north-star] Expression simplification: constant folding, boolean normalization, qual lists. |
| PLN-03 | pushdown.rs | sonnet | Predicate pushdown through joins, outer-join reduction, constant propagation. |
| PLN-04 | subplan.rs | sonnet | Sub-links to sub-plans: correlation parameters, initplans, hashed ANY, EXISTS/IN pull-up. |
| PLN-05 | index.rs | sonnet | [north-star] Access path selection: seq scan vs btree index scan with equality prefix + range + ordering. |
| PLN-06 | join.rs | sonnet | [north-star] Join ordering and strategy selection: hash, merge, nested loop, index nested loop, LATERAL. |
| PLN-07 | agg.rs | haiku | Aggregation: Aggref collection, LogicalAggregate, strategy choice, AggCall construction. |
| PLN-08 | window.rs | haiku | Window planning: LogicalWindow stacking, required sort, WindowAggNode. |
| PLN-09 | sort_limit.rs | haiku | [north-star] Sort, Limit, Distinct planning; ordering reasoning; top-N. |
| PLN-10 | setop.rs | haiku | UNION/INTERSECT/EXCEPT planning: Append, Distinct, SetOpNode. |
| PLN-11 | cte.rs | haiku | CTE planning: inline vs materialize, recursive union, data-modifying CTEs. |
| PLN-12 | cost.rs | haiku | Rule-based cardinality and cost estimates. |
| PLN-13 | planner.rs | haiku | [north-star] Planner orchestration: plan_statement, logical->physical conversion, projections, output columns. |
| PLN-14 | setrefs.rs | sonnet | [north-star] Final expression translation to executable form (Column -> InputRef, Aggref/WindowFunc/Grouping slots, sub-plan args). |
| PLN-15 | explain.rs | haiku | [north-star] EXPLAIN node labels and property annotation (deparsed quals, keys, join conditions). |
| PLN-16 | explain_fmt.rs | haiku | [north-star] EXPLAIN renderers: text (indentation, costs), JSON, YAML, XML, ANALYZE actuals. |
| PLN-17 | build_dml.rs | sonnet | [north-star] INSERT/UPDATE/DELETE/CTAS planning: row layouts, defaults, generated columns, ON CONFLICT, RETURNING. |

Frozen and implemented (not leased): `plan.rs`, `logical.rs`, `context.rs` and the small pure helpers inside leased
files that are marked as fully implemented (`simplify::{split_conjuncts, make_and}`, `pushdown::{expr_relids,
is_volatile}`, `planner::output_columns`, `explain::{PlanColumnNamer, is_shown}`).
