# Amendment request pgarbor-plan-01: approximate table row counts for the planner

Status: PROPOSED (architect, pgarbor-plan). Requirement on pgarbor (embedded API) / pgarbor-core.

## Problem

The catalog keeps no statistics (`relpages`/`reltuples` are static: catalog contract "Documented divergences").
The planner's join order and hash build-side choices need at least a coarse row count per table. Defaults
(1000 rows for every table) make a 10-row lookup table and a 10-million-row fact table look identical.

## Proposed change

`PlanContext::table_rows: HashMap<Oid, f64>` (already in the frozen `context.rs`) is filled by the `pgarbor`
crate per statement for the tables the bound statement references (`PlanDeps` / range tables). Source: an
O(1) or O(log n) approximate count from the storage tree of the relation (for example subtree key counts kept by
the CoW B+tree, or a count cached in the branch-local metadata space maintained at commit). Missing entries fall
back to `PlanOptions::default_table_rows`. No other behavior depends on it: plans are always correct, only their
cost-based choices change.

## Affected crates

pgarbor (fills `table_rows`), pgarbor-core / pgarbor-storage (provide the counts), pgarbor-plan (consumes).
