# plan-04: retain query parentage for LATERAL references

Decision: ACCEPTED by central integration (2026-10-15).

PlannerState records each query level's parent alongside range_names. This is
scratch metadata only; physical row layouts, bound IR, and dependency direction
are unchanged. LATERAL parameterization resolves a Column's levels_up through
this ancestry instead of confusing equally numbered local and outer range
entries. This also covers nested derived tables inside a LATERAL query.

Validation: API regressions check local/outer name separation and left-join NULL
extension; the embedded incremental-sort.rescan-lateral case covers nesting.
