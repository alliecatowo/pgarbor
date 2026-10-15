//! Query deparser: `BoundQuery` -> `pg_get_viewdef` text (SQL-44, sonnet).
//! Reference: `ruleutils.c` — `get_query_def`, `get_select_query_def`, `get_basic_select_query`,
//! `get_target_list`, `get_from_clause`, `get_from_clause_item`, `get_setop_query`,
//! `get_rule_orderby`, `get_rule_windowclause`, `get_with_clause`, `get_values_def`,
//! `get_name_for_var_field`, `get_variable`.
//!
//! The text is stored in the catalog as `ViewSpec.query` (`" SELECT ...;"` with the exact whitespace
//! of `pg_get_viewdef(oid, false)` and a trailing `;`) and returned verbatim by
//! `pg_get_viewdef`/`pg_views.definition`. **Re-binding** a view later parses this text again
//! (`from.rs` view expansion), so the output must also be valid SQL for our own parser.
//!
//! ## Layout (oracle, PG 18.6, non-pretty)
//! ```text
//!  SELECT t.a,
//!     (t.b || 'x'::text) AS bx,
//!     count(*) OVER () AS n,
//!     u.d
//!    FROM (scr_tmp.t
//!      JOIN scr_tmp.u ON ((t.a = u.a)))
//!   WHERE ((t.a > 1) AND (t.b ~~ 'z%'::text));
//! ```
//! * Line 1 is `" SELECT "` (leading space) followed by the first target; further targets on new lines
//!   indented 4 spaces, separated by `,` at the end of the previous line. `AS alias` is printed only when
//!   the output name differs from the column's own name (`t.a` has none, `(t.b || 'x'::text) AS bx`;
//!   a function result whose default name equals the alias still prints `AS "coalesce"` for quoted
//!   keywords).
//! * `FROM` on its own line indented 3 spaces: `"   FROM "`; further comma-separated items follow on new
//!   lines indented 4 spaces after a trailing comma (`FROM scr_tmp.t,\n    ( VALUES (1), (2)) s(x);`);
//!   explicit joins are wrapped: `(a\n     JOIN b ON (cond))`, `LEFT JOIN`, `RIGHT JOIN`, `FULL JOIN`,
//!   `CROSS JOIN`, `JOIN ... USING (id)`; sub-selects `( SELECT ... ) alias`; function scans `generate_series(1, 3) g(x)`.
//! * `WHERE`: `"  WHERE "` (2 spaces); `GROUP BY` `"  GROUP BY "`; `HAVING` `" HAVING "` (1 space);
//!   `ORDER BY` `"  ORDER BY "`; `OFFSET` `" OFFSET 1"` comes BEFORE `LIMIT` `" LIMIT 3"` (oracle prints `ORDER BY ..\n OFFSET 1\n LIMIT 3;`); `FETCH FIRST n ROWS WITH
//!   TIES`; window clause `WINDOW w AS (...)`; `DISTINCT` / `DISTINCT ON (..)` after SELECT; locking
//!   `FOR UPDATE`.
//! * Set operations: operands as `" SELECT ..."` blocks joined by `"UNION ALL"`/`"UNION"`/`"INTERSECT"`/
//!   `"EXCEPT"` lines; operand sub-queries that need parentheses (ORDER BY/LIMIT inside) are wrapped.
//! * CASE inside a view body spans lines:
//!   ```text
//!      SELECT t.a,
//!             CASE
//!                 WHEN (t.a > 1) THEN 'x'::text
//!                 ELSE 'y'::text
//!             END AS k,
//!   ```
//! * `VALUES (1, 'a'), (2, 'b')` bodies, `WITH x AS (...)` prefix (`WITH x AS (`...`)`), `WITH RECURSIVE`.
//! * Column qualification: variables print as `t.a` when `varprefix` is on — i.e. when the query has
//!   MORE than one range table entry OR it is nested inside another query (set-operation arm, sub-select);
//!   a single-table top-level query prints bare names (`SELECT b,\n    count(*) AS c ...`). Use
//!   `RangeColumns { qualify }` accordingly, maintaining the scope stack while recursing.
//! * Relation names are schema-qualified unless visible in the search_path of the deparsing session
//!   (`public.t` prints as `t` when `public` is in the path); aliases are printed when the alias differs from the relation name (`FROM t x`).
//! * Output aliases: names that are not valid unquoted identifiers are quoted (`AS "coalesce"`).
//!
//!
//! ## More oracle fixtures (PG 18.6, `pg_get_viewdef`, non-pretty)
//! ```text
//!  SELECT t.a,
//!     x.n
//!    FROM (scr_tmp.t
//!      LEFT JOIN ( SELECT u.a AS n
//!            FROM scr_tmp.u) x ON ((x.n = t.a))),
//!     scr_tmp.u uu
//!   WHERE (uu.d = 1)
//!   ORDER BY t.a DESC NULLS LAST
//!  OFFSET 1
//!  LIMIT 3;
//! ```
//! ```text
//!  SELECT t.a
//!    FROM (scr_tmp.t
//!      JOIN scr_tmp.u USING (a))
//!   WHERE ((t.a IN ( SELECT u_1.d
//!            FROM scr_tmp.u u_1)) AND (EXISTS ( SELECT 1
//!            FROM scr_tmp.u uu
//!           WHERE (uu.a = t.a))));
//! ```
//! (a sub-select that repeats a relation name gets PG's generated alias `u_1`; an explicit alias is kept)
//! ```text
//!  WITH q AS (
//!          SELECT 1 AS a
//!         )
//!  SELECT a
//!    FROM q;
//! ```
//! ```text
//!  SELECT s.x,
//!     ( SELECT count(*) AS count
//!            FROM scr_tmp.u) AS c,
//!     ARRAY( SELECT u.d
//!            FROM scr_tmp.u) AS ar,
//!     (ARRAY[1, 2])[1] AS e,
//!     (t.a IS DISTINCT FROM 1) AS d,
//!     (t.a = ANY (ARRAY[1, 2])) AS an,
//!         CASE t.a
//!             WHEN 1 THEN 'x'::text
//!             ELSE NULL::text
//!         END AS cs,
//!     COALESCE(t.b, 'q'::text) AS cb,
//!     NULLIF(t.a, 1) AS nl,
//!     GREATEST(t.a, 2) AS gr,
//!     (- t.a) AS neg,
//!     (t.b ~ similar_to_escape('x%'::text)) AS st,
//!     CURRENT_DATE AS cd,
//!     CURRENT_USER AS cu,
//!     count(*) OVER (PARTITION BY t.b ORDER BY t.a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS w,
//!     (t.c)::text AS ct,
//!     ((t.a > 1) IS TRUE) AS tr,
//!     ARRAY[t.a, 2] AS aa
//!    FROM scr_tmp.t,
//!     ( VALUES (1), (2)) s(x);
//! ```
//! Set operations (qualified columns inside arms):
//! ```text
//!  SELECT t.a,
//!         CASE
//!             WHEN (t.a > 1) THEN 'x'::text
//!             ELSE 'y'::text
//!         END AS k,
//!     COALESCE(t.b, 'q'::text) AS "coalesce"
//!    FROM scr_tmp.t
//!   WHERE ((t.a = ANY (ARRAY[1, 2, 3])) AND (t.c IS NOT NULL))
//! UNION ALL
//!  SELECT u.d AS a,
//!     'z'::text AS k,
//!     NULL::text AS "coalesce"
//!    FROM scr_tmp.u;
//! ```
//! `deparse_viewdef` returns the full text WITH the trailing `;`. `deparse_query_inline` is the form
//! used inside parentheses by sub-links and sub-selects (no leading space, no `;`, single line
//! separators as PG's `get_query_def` with `PRETTYFLAG` off and `startIndent` 0).
use pgarbor_common::{PgError, Result};

use super::{DeparseCtx, DeparseOptions};
use crate::ir::BoundQuery;

/// `pg_get_viewdef(oid)` text of `q` (module docs). `view_columns`: the view's column names (after the
/// `CREATE VIEW v(a,b)` list) used for `AS` aliases.
#[allow(unused_variables)]
pub fn deparse_viewdef(env: &pgarbor_catalog::CatalogEnv, q: &BoundQuery, view_columns: &[String], opts: DeparseOptions) -> Result<String> {
    Err(PgError::unimplemented("SQL-44", "view definition deparse (deparse::query::deparse_viewdef)"))
}

/// Inline form for nested queries (sub-links, derived tables, CTE bodies).
#[allow(unused_variables)]
pub fn deparse_query_inline(ctx: &DeparseCtx, q: &BoundQuery) -> Result<String> {
    Err(PgError::unimplemented("SQL-44", "nested query deparse (deparse::query::deparse_query_inline)"))
}

/// Names of the relations / columns / functions / types a query depends on, as catalog dependency
/// addresses (for `ViewSpec.query.depends_on`). Walks the range table (`RangeKind::Relation` OIDs,
/// recursing through sub-queries, CTEs and sub-links) and expressions (`stored::collect_dependencies`
/// semantics); columns are collected per range entry from the `Column` nodes. Order: first
/// occurrence, de-duplicated.
#[allow(unused_variables)]
pub fn query_dependencies(q: &BoundQuery) -> Result<Vec<pgarbor_catalog::ObjectAddress>> {
    Err(PgError::unimplemented("SQL-44", "query dependency collection (deparse::query::query_dependencies)"))
}
