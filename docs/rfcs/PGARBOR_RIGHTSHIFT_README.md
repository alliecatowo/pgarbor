# PGArbor RightShift Planning Bundle

Generated 2026-10-14. **Specification artifacts, not a claimed implementation.**

## Read order

1. `PGARBOR_RFC_001.md` — product and engine semantics.
2. `PGARBOR_RFC_002_AUTONOMOUS_VERIFICATION.md` — independent correctness oracles.
3. `PGARBOR_RFC_003_RIGHTSHIFT.md` — epoch-level speculative coding; supersedes RFC 002 per-worker gates.
4. `PGARBOR_RIGHTSHIFT_TASK_GRAPH.json` — seed dependency DAG for harness import.
5. `PGARBOR_RIGHTSHIFT_OVERVIEW.mmd` — graph visualization (Mermaid).
6. `PGARBOR_ULTRACODE_RIGHTSHIFT_BOOTSTRAP.md` — optional one-shot execution instruction.

## Graph stats

- **209 tasks**, **16 implementation domains**, **344 explicit integration edges**.
- Epoch hints: {0: 40, 1: 85, 2: 51, 3: 26, 4: 7}. These are hints only; work can begin speculatively before dependencies are implemented if contracts are frozen.
- Every ID and dependency validated, directed graph confirmed acyclic.
- Task ownership globs are broad domains; scheduler must refine to nonoverlapping file-level author leases before fanout.
- The project name still requires registry/ownership clearance *before* any actual scaffold or package names are committed.

## Key decision

**No per-worker full builds or test suites by default.** Freeze contracts; implement giant batches; consolidate; central build; central PostgreSQL/storage verification; generate repair DAG. Verification is expensive and definitive, not eliminated.

### Domain task counts

- storage: 19
- compatibility: 18
- execution: 17
- swarm: 15
- types: 15
- sql: 14
- oracle: 13
- transactions: 13
- wire: 13
- architecture: 12
- branching: 12
- catalogs: 12
- tooling: 12
- applications: 9
- planning: 9
- integration: 6
