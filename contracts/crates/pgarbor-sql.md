# pgarbor-sql — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-sql.json` (SQL-01..SQL-46,
seed tasks SQL-01..SQL-14). Amendments: `pgarbor-sql-01-sqlparser-gaps`, `pgarbor-sql-02-opclass-lookup`
(plus catalog-04 which this crate implements). Depends on: pgarbor-common, pgarbor-types, pgarbor-catalog,
`sqlparser = =0.63.0` (never on storage, core, plan or exec).

## Purpose

The SQL front end of the engine:

1. **Parse** a submitted query string into statements with PG-style syntax errors and positions (`parser`,
   `parser_extra`, `syntax_error`, `classify`).
2. **Bind** each statement against the catalog snapshot of the moment (name resolution through `search_path`,
   type resolution and coercion, parameter inference, grouping/aggregate validity, DDL spec construction)
   into a typed IR (`ir::BoundStatement`).
3. **Deparse** bound expressions and queries to PostgreSQL's canonical text (`pg_get_expr`, `pg_get_viewdef`)
   for the catalog, and re-deparse after renames (`rewrite::SqlExprRewriter`).

It performs no I/O, takes no locks, writes no catalog rows. Binding is pure: the same statement, snapshot and
settings give the same result, so READ COMMITTED DDL can re-bind and retry (catalog protocol step 2).

## Public API overview

| Item | Use |
|---|---|
| `parse(&str) -> Result<Vec<RawStatement>>` | whole simple-Query string; syntax errors fail the call before anything executes; empty/comment-only strings give `[]` (caller answers EmptyQueryResponse) |
| `parse_one`, `parse_expr_text` | re-parsing stored text (views, defaults, CHECKs) |
| `RawStatement { parsed: Parsed::{Ast, Extra}, text, byte_start..end, pos, index, source: Arc<SourceMap> }` | one statement with exact source text (`pg_stat_activity`, `pg_prepared_statements`) |
| `bind(&RawStatement, &BindContext) -> Result<BindOutput { statement, param_types, notices }>` | the binder entry point |
| `BindContext { env: &CatalogEnv, session: BindSession{database}, io_config: IoConfig, statement_timestamp, declared_params }` | everything binding needs |
| `ir::BoundStatement` (+ `BoundQuery`, `BoundExpr`, DML/DDL/utility structs) | the IR consumed by pgarbor-plan (queries, DML) and pgarbor-exec (DDL, utility); `BoundStatement::result_columns()` for Describe, `tag()` for CommandComplete |
| `ir::expr_walk` helpers on `BoundExpr` | traversal/rewrite (`walk`, `transform`, `collect_columns`, `volatility`, ...) |
| `binder::utility::coerce_execute_args` | session coerces EXECUTE arguments to prepared parameter types |
| `deparse::expr::deparse_expr`, `deparse::query::{deparse_viewdef, deparse_query_inline}`, `deparse::{ColumnNamer, DeparseCtx}` | canonical text; reused by pgarbor-plan EXPLAIN |
| `rewrite::SqlExprRewriter` | pass to `CatalogTxn::rename_*` / `set_*_schema` (catalog amendment 04) |
| `diag::*` | PG-exact analysis error constructors (SQLSTATE, message, HINT, position) |

## Pipeline and module map

```text
&str --parse--> [RawStatement{Ast|Extra}] --classify::check_supported--> family
   --bind(ctx)--> dispatch --> select/insert/update_delete/ddl_*/utility*/copy --> BoundStatement
Module groups (binder/):
  state     context (Binder, BindContext, ExprCtx, BoundArg)  scope (QueryScope, NsItem, column lookup)  params
  types     types  literal  coerce
  exprs     expr (dispatcher)  expr_ops  expr_func  expr_agg  expr_misc  expr_sublink
  queries   select  from  join  target  group  order  window  setop  values  cte
  dml       target_table  insert  on_conflict  update_delete
  ddl       stored  ddl_table  ddl_constraint  ddl_index  ddl_alter  ddl_alter_other  ddl_objects  ddl_view  ddl_function  ddl_drop  ddl_grant
  utility   utility (prepare/explain/...)  utility_session (txn/SET/SHOW)  copy
deparse/    expr  query        rewrite.rs
```

## Key invariants

1. **Every bound node carries its full `TypeId`.** Implicit coercions are explicit `Cast` nodes
   (`CoercionForm::Implicit`); `unknown` literals are converted at BIND time with `types::io::input` and the
   literal's position, so `select 1 + 'x'` fails with 22P02 pointing at `'x'`. A bare unresolved `unknown`
   output is coerced to `text`; an unresolved parameter is 42P18 (never `unknown` in `BindOutput`).
2. **Positions.** All errors carry `PgError.position` = 1-based CHARACTER offset into the WHOLE query
   string (`SourceMap`). `BoundExpr` stores no positions except `ColumnRef::pos`, used by the 42803 grouping
   check (ignored by equality).
3. **Parse before execute.** `parse` handles the entire string; semantic errors come from `bind`, per statement,
   at execution time. `CREATE TABLE t(..); INSERT INTO t ..` binds the INSERT only after the CREATE executed.
4. **No silent ignoring.** `classify::classify` is an exhaustive match over all 140 `sqlparser::Statement`
   variants (Handled / Unsupported 0A000 / NotPostgres 42601). Unsupported syntax inside handled statements
   raises 0A000 or 42601 naming the construct; the binder never drops a clause.
5. **Name folding.** Unquoted identifiers fold to lower case, >63 bytes truncate with a NOTICE
   (`names::ident_name`), collected in `BindOutput::notices`.
6. **Views are expanded at bind time** by re-parsing the stored `pg_get_viewdef` text; catalog expression text
   (defaults, CHECK, index expressions) is re-parsed and re-bound with `stored::bind_stored_expr`. Therefore the
   deparser output MUST be valid input for our own parser (round-trip tests).
7. **DDL IR is the catalog's own spec structs** (`CreateTableSpec`, `IndexSpec`, `ViewSpec`, ...) plus
   `AlterTableCmd`/`AlterAction` mapping 1:1 to `CatalogTxn` methods; DROP carries names, not OIDs.
8. **Grouping rules** (42803) are checked in `group::check_grouping` over the bound select block; aggregates
   stay inline as `Aggref`, GROUP BY/ORDER BY/DISTINCT ON refer to target entries by index (junk entries for
   expressions that are not in the select list).
9. **Dependency tracking** for DDL: `stored::collect_dependencies` lists referenced objects; the planner records
   its own `PlanDeps` from the range table.
10. **No panics on user input**, no unwrap on user-derived data, placeholders are
    `Err(PgError::unimplemented("SQL-NN", ..))` only (inventory by lease).

## Error conventions

PG 18.6 wording, verified against the oracle, lives in `diag.rs` (frozen) and in each module's docs. Typical:
`syntax error at or near "X"` 42601 (position), `column "x" does not exist` 42703, `column reference "a" is
ambiguous` 42702, `relation "t" does not exist` 42P01, `column "t.b" must appear in the GROUP BY clause or be used
in an aggregate function` 42803, `operator does not exist: integer + boolean` 42883, `invalid input syntax for type
integer: "x"` 22P02, `argument of WHERE must be type boolean, not type integer` 42804, `could not determine data
type of parameter $1` 42P18, `INSERT has more expressions than target columns` 42601, 0A000 for unsupported.

## How downstream crates use it

- **pgarbor-plan** consumes `BoundQuery`/DML structs; `Column`/`SubLink`/`Aggref`/`WindowFunc` are rewritten to
  executable forms there (`ir::expr` module docs rule 3).
- **pgarbor-exec** executes DDL/utility `BoundStatement`s with `CatalogTxn`; passes `SqlExprRewriter` to renames.
- **pgarbor (embedded API)** builds `BindContext` per statement: `env` from `CatalogCache::snapshot` + `SearchPath`,
  `io_config` from the session GUCs (DateStyle, TimeZone, IntervalStyle, extra_float_digits, bytea_output),
  `statement_timestamp` = transaction start, `declared_params` from the Parse message; calls `bind`, then `plan`
  for plannable statements (`BoundStatement::is_plannable`); forwards `notices`; uses `param_types` for
  ParameterDescription and `result_columns()` for RowDescription (Describe); re-binds when
  `invalidation::plan_is_stale` says so.
- **pgarbor-wire** never calls into this crate.

## Non-goals and known gaps (v1)

- PL/pgSQL and SQL-function bodies are stored, not bound (`ddl_function`); `CALL`/`DO` bind but execution is later.
- No inheritance, partitioning, temp tables, triggers, rules, policies, FDWs, extensions, cursors, MERGE
  (all 0A000 via classify/binders), `ON CONFLICT` inference with expressions/WHERE, subscript/field assignment in
  UPDATE (see amendment `pgarbor-sql-01`), non-btree index methods, collations other than code-point.
- Auto-updatable views: `INSERT/UPDATE/DELETE` on a view raise 0A000.
- Positions of errors inside stored expression text (views, defaults) are meaningless and omitted.
- EXPLAIN option parsing is here; plan formatting is pgarbor-plan.

## Lease index

| lease | file | tier | scope |
|---|---|---|---|
| SQL-01 | parser.rs | haiku | [north-star] Statement splitting and parsing with sqlparser 0.63 (PostgreSqlDialect), source spans, PG syntax errors. |
| SQL-02 | parser_extra.rs | haiku | Hand parser for PostgreSQL statements missing from sqlparser (REFRESH, REINDEX, CLUSTER, CHECKPOINT, SET CONSTRAINTS, DO, ALTER SEQUENCE/DOMAIN/DATABASE, DROP/REASSIGN OWNED, recognized-unsupported commands). |
| SQL-03 | syntax_error.rs | haiku | [north-star] sqlparser/tokenizer error -> PostgreSQL syntax error (message, position, SQLSTATE 42601). |
| SQL-04 | binder/dispatch.rs | haiku | [north-star] Statement dispatch: bind_statement, bind_inner_statement, extras routing, parameter finalization. |
| SQL-05 | binder/scope.rs | sonnet | [north-star] Query scopes, FROM namespace, column resolution (qualified/unqualified/outer/whole-row/system), star expansion. |
| SQL-06 | binder/params.rs | haiku | [north-star] Parameter ($n) typing: declared types, inference, finalization (42P18, 42P08). |
| SQL-07 | binder/types.rs | haiku | [north-star] sqlparser DataType/ObjectName -> pgarbor_types TypeName/TypeId, array dims, typmods. |
| SQL-08 | binder/literal.rs | haiku | [north-star] Literal binding: integers/numerics/strings/NULL/booleans/typed strings/escape strings. |
| SQL-09 | binder/coerce.rs | sonnet | [north-star] Bind-time coercion: unknown literals/params, implicit/assignment/explicit casts, common types, boolean/specific coercion. |
| SQL-10 | binder/expr.rs | haiku | [north-star] Expression dispatcher, column references, casts. |
| SQL-11 | binder/expr_ops.rs | haiku | [north-star] Operators: binary/unary, AND/OR/NOT, comparison, BETWEEN, IN lists, LIKE/ILIKE/SIMILAR, ANY/ALL arrays, IS tests, row comparison, AT TIME ZONE. |
| SQL-12 | binder/expr_func.rs | sonnet | [north-star] Function calls: resolution, named args, variadic, defaults, special syntax, SQL value functions, SRF placement. |
| SQL-13 | binder/expr_agg.rs | sonnet | Aggregates, window function calls, GROUPING(): levels, placement errors, FILTER/DISTINCT/ORDER BY/WITHIN GROUP. |
| SQL-14 | binder/expr_misc.rs | haiku | [north-star] CASE, COALESCE/NULLIF/GREATEST/LEAST, ARRAY[], ROW, subscripts, field selection, COLLATE, INTERVAL literals. |
| SQL-15 | binder/expr_sublink.rs | haiku | Sub-select expressions: EXISTS, scalar, IN/ANY/ALL (subquery), ARRAY(subquery). |
| SQL-16 | binder/select.rs | haiku | [north-star] Query expressions: WITH/body/ORDER BY/LIMIT/locking orchestration and the SELECT block algorithm. |
| SQL-17 | binder/from.rs | haiku | [north-star] FROM items: relations, views (expanded), CTE refs, sub-selects, table functions, VALUES, aliases. |
| SQL-18 | binder/join.rs | sonnet | JOIN binding: ON/USING/NATURAL, merged columns, namespace updates, LATERAL rules. |
| SQL-19 | binder/target.rs | haiku | [north-star] SELECT/RETURNING target lists: star expansion, output names (FigureColname), column origin. |
| SQL-20 | binder/group.rs | sonnet | GROUP BY / HAVING: grouping expressions, GROUPING SETS/ROLLUP/CUBE expansion, ungrouped-column check (42803). |
| SQL-21 | binder/order.rs | haiku | [north-star] ORDER BY, DISTINCT/DISTINCT ON, LIMIT/OFFSET/FETCH binding. |
| SQL-22 | binder/window.rs | haiku | WINDOW clause and OVER specifications with frame validation. |
| SQL-23 | binder/setop.rs | haiku | UNION/INTERSECT/EXCEPT binding with common type resolution; recursive union typing. |
| SQL-24 | binder/values.rs | haiku | [north-star] VALUES lists (standalone and INSERT source) with common types and DEFAULT. |
| SQL-25 | binder/target_table.rs | haiku | [north-star] DML target resolution and TargetTable construction (defaults, generated, identity, CHECKs). |
| SQL-26 | binder/insert.rs | haiku | [north-star] INSERT binding: column list, VALUES/SELECT/DEFAULT VALUES, OVERRIDING, identity/generated rules, RETURNING. |
| SQL-27 | binder/on_conflict.rs | haiku | [north-star] INSERT ... ON CONFLICT: arbiter inference, excluded pseudo relation, DO UPDATE. |
| SQL-28 | binder/update_delete.rs | haiku | [north-star] UPDATE / DELETE binding, SET assignments, RETURNING. |
| SQL-29 | binder/stored.rs | sonnet | [north-star] Stored expressions: rebinding catalog text, StoredExpr construction with canonical text and dependencies. |
| SQL-30 | binder/ddl_table.rs | haiku | [north-star] CREATE TABLE (columns, serial, identity, generated, defaults), CREATE TABLE AS, SELECT INTO. |
| SQL-31 | binder/ddl_constraint.rs | haiku | [north-star] Table and column constraints -> ConstraintSpec (PK, UNIQUE, CHECK, FOREIGN KEY). |
| SQL-32 | binder/ddl_index.rs | haiku | CREATE INDEX binding (columns, expressions, predicates, opclasses, INCLUDE, WITH). |
| SQL-33 | binder/ddl_alter.rs | sonnet | ALTER TABLE / VIEW / INDEX sub-commands -> AlterTableCmd with sequential evolving-schema binding. |
| SQL-34 | binder/ddl_alter_other.rs | haiku | ALTER SCHEMA/FUNCTION/TYPE/ROLE/SEQUENCE/DOMAIN/DATABASE/MATERIALIZED VIEW and rename/owner/schema moves. |
| SQL-35 | binder/ddl_objects.rs | haiku | CREATE SCHEMA/SEQUENCE/TYPE/DOMAIN/ROLE/DATABASE binding and sequence option validation. |
| SQL-36 | binder/ddl_view.rs | sonnet | CREATE [MATERIALIZED] VIEW and REFRESH MATERIALIZED VIEW. |
| SQL-37 | binder/ddl_function.rs | haiku | CREATE FUNCTION/PROCEDURE, CALL, DO binding (definitions stored, not executed). |
| SQL-38 | binder/ddl_drop.rs | haiku | DROP (all kinds), TRUNCATE, COMMENT, DROP/REASSIGN OWNED, DROP DATABASE binding. |
| SQL-39 | binder/ddl_grant.rs | haiku | GRANT / REVOKE of privileges and roles. |
| SQL-40 | binder/utility_session.rs | haiku | [north-star] Transaction control, SET/RESET/SHOW, DISCARD, SET CONSTRAINTS binding. |
| SQL-41 | binder/utility.rs | haiku | [north-star] PREPARE/EXECUTE/DEALLOCATE, EXPLAIN, LISTEN/NOTIFY, LOCK, maintenance statements, EXECUTE argument coercion. |
| SQL-42 | binder/copy.rs | haiku | COPY binding: table/query source, endpoints, options and defaults. |
| SQL-43 | deparse/expr.rs | sonnet | [north-star] Expression deparser (ruleutils get_rule_expr) producing canonical pg_get_expr text. |
| SQL-44 | deparse/query.rs | sonnet | Query deparser (ruleutils get_query_def): pg_get_viewdef text, nested queries, dependency extraction. |
| SQL-45 | rewrite.rs | sonnet | ExprRewriter implementation: re-deparse stored expressions and view queries after renames. |
| SQL-46 | binder/cte.rs | haiku | WITH clauses: visibility, RECURSIVE well-formedness, data-modifying CTEs. |

Frozen and implemented (not leased): `source.rs`, `names.rs`, `diag.rs`, `classify.rs`, `extra_stmt.rs`,
`ir/{expr,expr_walk,query,stmt}.rs`, `binder/context.rs`, the data types in `binder/{scope,cte,params,select,
values,ddl_constraint,expr_func,target_table}.rs` (marked `// CONTRACT`), `deparse/mod.rs`.
