# Amendment request pgarbor-catalog-03: more `pg_proc` / `pg_type` / `pg_operator` / `pg_aggregate` columns in `pgarbor_types::catalog_data`

Status: DEFERRED (2026-10-15, lead engineering review).

## Problem

`pgarbor_types::catalog_data` carries only the columns the type system itself needs. The virtual
catalogs served by pgarbor-catalog must expose full PG rows. Missing today (the catalog emits 0/NULL/
documented defaults and lists the gap in contracts/crates/pgarbor-catalog.md):
- `BuiltinType`: `typsubscript`, `typmodin`, `typmodout`, `typanalyze` (regproc names).
- `BuiltinProc`: `procost`, `prorows` (real values), `proleakproof`, `proparallel`, `prosupport`, `prosecdef`.
- `BuiltinOperator`: `oprrest`, `oprjoin`.
- `BuiltinAggregate`: `aggcombinefn`, `aggserialfn`, `aggdeserialfn`, `aggmtransfn`, `aggminvtransfn`,
  `aggmfinalfn`, `aggfinalextra`, `aggmfinalextra`, `aggfinalmodify`, `aggmfinalmodify`, `aggsortop`,
  `aggtransspace`, `aggmtranstype`, `aggmtransspace`, `aggminitval`, `aggnumdirectargs`.

## Proposed change

Extend the structs and the generator (`gen` script that reads `contracts/reference/pg18/*.csv`; the CSVs
already contain these columns) in a v2 contract epoch; every `BuiltinX { .. }` literal in generated files
changes, hand-written code only uses field access. Until then pgarbor-catalog derives what it can
(`typsubscript` from the array/jsonb/vector rules, `typanalyze = array_typanalyze` for arrays).

## Affected crates

pgarbor-types (structs + generated tables), pgarbor-catalog (vrel::pg_type, vrel::pg_proc).


## Steward decision (2026-10-15)

Defer generated metadata expansion to a coordinated type/catalog epoch. Current default/NULL substitutions are incomplete catalog fidelity and remain a P2 gap; this decision does not approve them as PostgreSQL measurements or exact metadata. Do not count affected catalog queries as compatibility passes without oracle comparison.
