Status: PROPOSED
Crate: harness
Affects: contracts/dag/task_graph.v1.json
Leases: HAR-01..HAR-31, FAC-01..FAC-19

# harness-01: update owned_path_glob of ORACLE-* and SWARM-* seed tasks

## Problem
The seed DAG gives ORACLE-01..13 `owned_path_glob: harness/oracle/**` and SWARM-01..15
`owned_path_glob: harness/swarm/**`. The frozen harness contract puts this code elsewhere:
- ORACLE code is in the `pgarbor_harness` Python package (`harness/pgarbor_harness/**`).
- SWARM code is in `harness/pgarbor_factory/**`.

`harness/oracle/**` is also a PROTECTED path: ARCHITECTURE §0.4, `config.PROTECTED_GLOBS`, and
`dag.validate_leases` all treat it as protected. No lease can ever own files there, so the seed
globs cannot be leased as written. Only the reference-dump scripts (`dump_reference.sh`) live
there, and they change only through the oracle review path.

## Proposed change
In `contracts/dag/task_graph.v1.json`:
- ORACLE-01..13: `owned_path_glob` -> `harness/pgarbor_harness/**`
- SWARM-01..15: `owned_path_glob` -> `harness/pgarbor_factory/**`

`ownership_domain` stays the same. The validator does not check globs against lease files, so
nothing breaks before this is applied. Until then, the mismatch is cosmetic.

## Affected crates
None (Rust). Affects the harness lease file `contracts/leases/harness.json` (already written
against the new paths).
