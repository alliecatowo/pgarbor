# pgarbor-catalog-07: statistics scans require real measurements

Status: ACCEPTED (2026-10-15, lead engineering review).

Decision: reject the earlier CAT-39 contract exception permitting synthetic zero
statistics. All 18 CAT-39 pg_stat/pg_statio virtual relation scans return leased
0A000 until actual counter collection is connected. Schemas and names remain
available for binding and inspection. Scans do not report an empty result or
pretend that counters were measured as zero.

This applies architecture §0's no-fake-results rule to catalog statistics and
supersedes contrary CAT-39 module/lease descriptions. A regression checks every
registered statistics view both before and after creating a table with an index.
The previous tests of fabricated fresh-table counters are replaced with this
required rejection behavior. Real activity/lock diagnostics are unaffected.
