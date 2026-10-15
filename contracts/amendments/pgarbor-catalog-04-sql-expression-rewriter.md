# Amendment request pgarbor-catalog-04: pgarbor-sql must provide the `ExprRewriter` and canonical deparse

Status: ACCEPTED WITH LIMITED SCOPE (2026-10-15, lead engineering review).

## Problem

pgarbor-catalog stores expressions (column defaults, CHECK bodies, index expressions and predicates,
view queries, generation expressions) as CANONICAL DEPARSED TEXT plus a dependency list
(`StoredExpr { sql, bound, depends_on }`), because the catalog cannot depend on pgarbor-sql. PostgreSQL
stores parsed trees and re-deparses on demand, so renaming a column, table, sequence, schema or type
is free there. With stored text, a rename must re-deparse every expression that mentions the renamed
object (oracle: after `ALTER TABLE t RENAME b TO bb`, `pg_get_constraintdef` prints `CHECK ((bb <> ''::text))`
and `pg_get_viewdef` prints `bb AS b`).

## Proposed change

pgarbor-sql (or pgarbor-exec) implements `pgarbor_catalog::ddl::spec::ExprRewriter` and passes it to
`rename_column`, `rename_relation`, `set_relation_schema`, `rename_type`, `set_type_schema`,
`rename_function`, `set_function_schema`. The catalog calls it once per affected expression (found
through `StoredExpr::depends_on`) with a `NameChange`; the rewriter returns the expression with the same
meaning and canonical text under the new names, typically by re-binding `bound`/`sql` against the
catalog snapshot AFTER the change (`env`) and deparsing with PG's `ruleutils` rules.
The deparser must also produce `pg_get_expr`/`pg_get_viewdef`-exact text when binding DDL in the first
place (e.g. `nextval('t_id_seq'::regclass)`, `'x'::character varying`, ` SELECT a,\n    bb AS b\n   FROM t;`).

## Affected crates

pgarbor-sql (deparser + rewriter), pgarbor-exec (passes the rewriter), pgarbor-catalog (already calls it).


## Steward decision (2026-10-15)

Accept the dependency inversion through the existing `ExprRewriter` trait. Assembled SQL supplies `SqlExprRewriter`; executor rename paths pass it into the catalog. Exact ruleutils equivalence for arbitrary stored expressions is not established and remains a differential-testing requirement, rather than an approved completeness guarantee.
