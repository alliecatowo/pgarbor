# exec-09: incremental recursive CTE state

Decision: ACCEPTED by central integration (2026-10-15).

Runtime CteRt gains RecStream state and CteState gains Streaming. Recursive
CteScan consumers pull one admitted row at a time and may stop at LIMIT; other
consumers share the same buffered prefix. The working-table change increments
the executor epoch before rescanning the recursive term, invalidating cached
hash/materialize inputs. UNION keeps a shared seen-key set across iterations.
These are executor internals; physical plan row layouts and dependency direction
are unchanged. Errors restore the temporarily taken body and propagate normally.

Validation: API regressions cover finite recursion, infinite recursion under
LIMIT, manual graph cycle detection (16 rows), and sequence side effects proving
unused seed rows are not evaluated.
