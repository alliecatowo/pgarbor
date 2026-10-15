You are taking over PGArbor (/home/allie/develop/pgarbor) as lead engineer. Claude usage is exhausted for now,
so you own architecture, assembly, testing and review. Read AGENTS.md, STATUS.md, runs/review/codex-review-01.md
and runs/review/codex-test-audit-01.md (if present) first. Follow AGENTS.md rules strictly (PG18 is the truth;
never hand-edit tests/expected; no fake results; commits end with `Co-Authored-By:` lines only, never session links).

## 1. Assemble the three finished implementation waves
Each workflow journal has one `{"type":"result",...}` line per agent. For `impl:<LEASE>` (or `retry:<LEASE>`, which
takes precedence) results, read `sha`/`branch`. Build `[{lease_id, files, sha}]` (take `files` from the args file),
then apply with `python3 harness/factory/assemble.py <subs.json> <assembly.json>`.
- epoch-02a (sql+plan): journal ~/.claude/projects/-home-allie-develop-pgarbor/000fed8a-3882-41eb-8a7c-1d7e77c94b31/subagents/workflows/wf_6cb03fd2-840/journal.jsonl, args runs/epoch-01/wave-02a-args.json
- epoch-02b (exec+api): .../wf_130a587b-475/journal.jsonl, args runs/epoch-01/wave-02b-args.json
- epoch-02c (wire+cli): .../wf_ef74df02-bfc/journal.jsonl, args runs/epoch-01/wave-02c-args.json; WIR-01 is on branch luna-epoch-02c-WIR-01 (sha 987104ddc8).
Leases with no commit: implement them yourself (or with cheap helper agents if your runtime has them), per
contracts/leases/<crate>.json.

## 2. Finish architecture (steward role)
- Resolve contracts/amendments/*.md: apply the accepted ones to contracts and code, record decisions in each file.
- Make `cargo build --workspace --all-targets` clean. Fix integration mismatches centrally.
- Make `cd harness && uv run pytest -q` pass (including tests/test_fac_02.py lease validation).

## 3. Fix the review findings
Fix the P1/P2 items in runs/review/codex-review-01.md (export temp-file race, JSON nesting abort, domain-cycle abort,
superblock arithmetic, stale key-lock outcome, OID normalizer false PASS, recorder/recovery P2s), each with a
regression test.

## 4. Get to a running vertical slice, then test for real
- `cargo run -p pgarbor-cli -- serve` (or equivalent per contracts/crates/pgarbor-cli.md), connect with psql from the
  oracle image (`podman exec -it pgarbor-oracle psql -h host.containers.internal -p <port>`) or psycopg; `SELECT 1`.
- Wire the harness pgarbor target (harness/pgarbor_harness/runner/targets.py) and run the differential corpus:
  `uv run python -m pgarbor_harness diff ...` against pgarbor. Then the PG native regress/isolation runners.
- Run cargo test --workspace, the fault suite, and the tests/apps fixtures against pgarbor.
- Add the pgarbor-specific contract/e2e tests from the test audit (branching over SQL/CLI, .pga reopen,
  crash survival at SQL level, detach/export, compaction, wire e2e, CLI e2e).

## 5. Durable outputs (required)
- runs/review/codex-final-01.md: in-depth review + real results: build status, test counts per layer
  (PASS/FAIL/BLOCKED/UNRUN), differential pass rate by family, regress/isolation results, fixture app results,
  remaining P1/P2 issues, and a prioritized next-steps list. Report only what you actually ran.
- Update STATUS.md to match reality. Update Taskwarrior (`task project:pgarbor ...`, modify by UUID).
- Commit in small logical commits on main. Push to origin if the network allows.
