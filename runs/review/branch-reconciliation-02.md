# Branch and setup reconciliation

Baseline audited: `de843843cc70c67849c91b048e145c304fc0d3f4`, after fetching origin.

All three published heads were checked. `feat/bindings-wasm` and
`fix/seq-role-parser` are ancestors of main (22 and 26 commits behind,
respectively); neither has commits missing from main. All three contain
`scripts/cloud-setup.sh`, originally added in `1428b170`.

The local inventory has 393 branches and 169 worktrees. There are 397 worker
commits absent from main by ancestry, across 379 branches. Of those commits,
376 appear directly in assembly records. Another 20 are earlier attempts for
files with selected implementations in the assembly records. The remaining
WIR-01 decoder (`987104dd`) was integrated manually in `057ea03e`: both contain
blob `f72a6ff4104ea85ca3692cf9ebf49dc52271b1d4` for frontend.rs.
This reconciles integration provenance; it is not a new semantic audit of every
historical worker implementation. The assembler copies leased files rather than
merging worker branches, so ancestry alone does not indicate missing work.

Three worktrees have uncommitted wire files. session_loop.rs exactly matches
main. connection.rs matches main before its later duplex_e2e tests; listener.rs
matches main before its later scripted-stream rejection arm. There is no new
implementation to import from those edits. Historical branches and worktrees
were retained, with no resets, deletions or pushes of stale worker branches.
The complete inventory and per-commit ledger references are in
branch-reconciliation-02.json.

The relative cloud setup command fails when its working directory does not
contain the script. cloud-bootstrap.sh now selects an existing checkout or
clones main when its selected path is absent. docs/CLOUD.md contains the exact
environment setup field text. Existing checkouts keep their branch and edits.
The environment settings themselves cannot be changed from this workspace.

Validation: bash syntax checks passed for both scripts; shellcheck passed for
the new bootstrap. Seven isolated stub checks passed: explicit override, project
directory, current Git root, fresh clone, existing fallback, wrong checkout and
override precedence. The pushed raw GitHub bootstrap returned HTTP 200 and was
byte-identical to the local script. No dependency installation or full cloud VM
startup was performed. Engine tests were not rerun for these shell/docs changes;
the prior review-02 engine results remain the baseline.
