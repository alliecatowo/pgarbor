//! FROM clause items: relations, views, sub-selects, table functions, VALUES (SQL-17).
//! PG references: `parse_clause.c` (`transformFromClause`, `transformFromClauseItem`,
//! `transformTableEntry`, `transformRangeSubselect`, `transformRangeFunction`,
//! `transformRangeTableFunc`), `parse_relation.c` (`addRangeTableEntry*`, `buildNSItemFromLists`,
//! `chooseScalarFunctionAlias`, `addRangeTableEntryForFunction`), `rewriteHandler.c`
//! (`ApplyRetrieveRule`: views are expanded by re-parsing their stored definition).
//!
//! `bind_from_clause(b, from)` processes `Vec<TableWithJoins>` (comma-separated items, each a
//! `relation` plus `joins`). For each item: `bind_table_factor` for the relation, then
//! `join::bind_join` for every join in order. Items are cross-joined implicitly (they become
//! separate entries of `BoundSelect::from`). The namespace is reset so that items of the SAME FROM
//! list do not see each other except through LATERAL (`NsItem::lateral_ok`).
//!
//! ## Table factors (sqlparser `TableFactor`)
//! * `Table { name, alias, args: None, with_hints, version, partitions, with_ordinality, sample, .. }`
//!   - resolution order: unqualified name -> CTE (`cte::find_cte`) first, then catalog relation via
//!     `env.resolve_relation(schema, name)` (42P01 `relation "x" does not exist`, position = name);
//!   - the catalog `Relation`: table/matview/sequence/virtual -> `RangeKind::Relation` (columns from
//!     `Relation::columns`, dropped ones flagged); `RelKind::View` -> expand: `parser::parse_one(view
//!     .query.sql)` + `select::bind_subselect` with the view's column names as aliases, produce
//!     `RangeKind::Subquery` (guard recursion with `Binder::expanding_views`, 42P17 `infinite recursion detected in rules for relation "v"`);
//!     composite type/index/foreign table as FROM item -> 42809 `"x" is not a table or view`?
//!     (PG: `cannot select from index "i"`? -> use 42809 `"i" is an index`);
//!   - `ONLY t` sets `only`; `t*` fine; `TABLESAMPLE` -> 0A000; `WITH (hint)`, `VERSION`, `PARTITION (..)`
//!     (MySQL), `INDEXED BY`: 42601;
//!   - alias: `AS a` / `a` plus optional column alias list `a(c1, c2)` (fewer aliases than columns is
//!     fine, more -> 42P10 `table "t" has 2 columns available but 3 columns specified`,
//!     `diag::too_many_column_aliases`); alias folding via `Binder::ident`. No alias -> relation
//!     name is the qualifier (unqualified; `schema.t` can still be referenced as `schema.t.col`).
//!   - `AccessNeeded.select = true` for the entry (columns added as referenced).
//! * `Table { args: Some(..) }` / `Function { lateral, name, args, alias, with_ordinality }` = table
//!   function: each function expression is bound with `ExprCtx::function_in_from()`; allowed
//!   LATERAL references to earlier FROM items (implicitly lateral). Result columns: scalar SRF ->
//!   one column named after the alias, else the function name (`generate_series`), type = return
//!   type; composite/OUT-parameter functions (`BuiltinProc::out_columns`) -> their columns;
//!   `AS t(a int, b text)` column definition lists (sqlparser: alias columns carry `data_type`)
//!   for `record`-returning functions; `WITH ORDINALITY` adds a trailing `ordinality bigint` column;
//!   `ROWS FROM (f(), g())` = `TableFactor::TableFunction`? (check 0.63: multi-function); a non-set
//!   function in FROM is allowed (one row). Builtin examples: `generate_series(1,3)`,
//!   `generate_series(ts, ts, interval)`, `unnest(array[1,2])`, `unnest(a, b)` (multi-array form),
//!   `jsonb_array_elements`, `jsonb_each` (key,value), `regexp_matches`, `string_to_table`.
//!   Aggregate or window function in FROM: 42809? (`aggregate functions are not allowed in FROM clause
//!   of their own query level` 42803).
//! * `UNNEST { alias, array_exprs, with_offset, with_ordinality }` -> same as `unnest(..)` function.
//! * `Derived { lateral, subquery, alias }` -> `select::bind_subselect`; `RangeKind::Subquery`;
//!   alias REQUIRED in PG < 16, optional in PG 16+: no alias gets the generated name `unnamed_subquery`
//!   (which cannot be referenced); duplicate output names allowed. `LATERAL` lets the sub-select reference
//!   earlier items (`lateral_ok`); non-lateral subqueries see only OUTER scopes.
//! * `Values` as `Derived` with `SetExpr::Values` -> `values::bind_values_body`, `RangeKind::Values`,
//!   columns `column1, column2, ...` unless aliased; alias `*VALUES*` when absent.
//! * `NestedJoin { table_with_joins, alias }` -> bind the inner joins (parenthesized join); alias on a
//!   nested join names the joined row (rare; support without alias, 0A000 with alias).
//! * `JsonTable`, `OpenJsonTable`, `Pivot`, `Unpivot`, `TableFunction` (non-PG), `XmlTable`,
//!   `SemanticView`, `MatchRecognize`, `Unnest` with offset: 0A000/42601 naming the construct.
//!
//! ## Helpers
//! [`relation_range_entry`] builds the `RangeEntry` for a catalog relation (also used by DML
//! targets and `INSERT ... ON CONFLICT excluded`). [`apply_column_aliases`] applies alias lists.
use pgarbor_catalog::Relation;
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{TableAlias, TableFactor, TableWithJoins};

use super::context::Binder;
use crate::ir::{FromItem, RangeEntry};

/// Bind the whole FROM clause, returning the top-level (implicitly cross-joined) items.
#[allow(unused_variables)]
pub fn bind_from_clause(b: &mut Binder, from: &[TableWithJoins]) -> Result<Vec<FromItem>> {
    Err(PgError::unimplemented("SQL-17", "FROM clause binding (from::bind_from_clause)"))
}

/// Bind one table factor (without joins); registers range entry + namespace item in the innermost
/// scope and returns `FromItem::Range(rt_index)` (or the inner join tree for `NestedJoin`).
#[allow(unused_variables)]
pub fn bind_table_factor(b: &mut Binder, tf: &TableFactor, lateral_ok: bool) -> Result<FromItem> {
    Err(PgError::unimplemented("SQL-17", "table factor binding (from::bind_table_factor)"))
}

/// `RangeEntry` for a catalog relation (not a view): columns = all `Relation::columns` (dropped
/// flagged), `alias` = alias or relation name, empty `AccessNeeded` with `select = true`.
#[allow(unused_variables)]
pub fn relation_range_entry(b: &Binder, rel: &Relation, alias: Option<&str>, only: bool) -> Result<RangeEntry> {
    Err(PgError::unimplemented("SQL-17", "relation range entry (from::relation_range_entry)"))
}

/// Apply a `TableAlias` (name + column list) to `entry`: renames the first N visible columns;
/// too many aliases -> `diag::too_many_column_aliases`.
#[allow(unused_variables)]
pub fn apply_column_aliases(b: &mut Binder, entry: &mut RangeEntry, alias: &TableAlias) -> Result<()> {
    Err(PgError::unimplemented("SQL-17", "column alias application (from::apply_column_aliases)"))
}
