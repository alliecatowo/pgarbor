# PGArbor project state (2026-10-15 ~10:45 PT)

Durable snapshot so any agent (Claude or Codex) can resume. Rules are in `AGENTS.md`; open tasks are
in `task project:pgarbor list`. Public repo: https://github.com/alliecatowo/pgarbor

## Verified (built and tested centrally; numbers are real)
| Area | Evidence |
|---|---|
| common, storage, fault, types, branches, core | ~1,540 Rust tests passing at epoch-01c; logs in `runs/epoch-01/build/` |
| catalog (49 leases) | `cargo test -p pgarbor-catalog`: 376 passed, 0 failed (commit f8345c9) |
| harness | `uv run pytest` passes except `tests/test_fac_02.py` (lease marker coverage; expected to pass once the wire/cli leases are committed) |
| oracle corpus | 5,837 cases; 5,706+ PG18.6 transcripts in `tests/expected/` |
| fixture apps | `tests/apps/{sqlx,node-pg,drizzle,prisma}` pass against real PG18 (not yet against pgarbor) |

## In flight when this was written (may finish or die)
| What | Where results land | How to resume if it died |
|---|---|---|
| epoch-02a wave: sql + plan, 63 leases (haiku) | worker branches `worktree-wf_6cb03fd2-840-*`; per-agent results in `~/.claude/projects/-home-allie-develop-pgarbor/000fed8a-3882-41eb-8a7c-1d7e77c94b31/subagents/workflows/wf_6cb03fd2-840/journal.jsonl` | Args: `runs/epoch-01/wave-02a-args.json`. Unfinished leases: implement each file per `contracts/leases/pgarbor-{sql,plan}.json` |
| epoch-02b wave: exec + pgarbor API, 58 leases | branches `worktree-wf_130a587b-475-*`; journal `.../wf_130a587b-475/journal.jsonl` | Args: `runs/epoch-01/wave-02b-args.json` |
| wire + cli architect (sonnet) | writes `crates/pgarbor-{wire,cli}`, `contracts/crates/pgarbor-{wire,cli}.md`, `contracts/leases/pgarbor-{wire,cli}.json` (uncommitted in main checkout) | If missing, design per ARCHITECTURE §1 (std::net, thread per connection, PG v3 protocol: simple + extended query, COPY, cancel, SSL refusal) |
| Codex review (gpt-6.1-sol) | `runs/review/codex-review-01.md` | `/codex:status task-mv9tbhun-9jbxn9` |

## Assembling a finished wave
Each journal `{"type":"result",...}` line for an `impl:<LEASE>` agent has `sha` and `branch`. Build a
submissions JSON (`[{lease_id, files, sha}]`; take `files` from the lease JSON), then:
```
python3 harness/factory/assemble.py runs/epoch-01/submissions-02a.json runs/epoch-01/assembly-02a.json
cargo build -p pgarbor-sql -p pgarbor-plan --tests && cargo test -p pgarbor-sql -p pgarbor-plan
```
Fix compile errors centrally (they are usually small API mismatches between leases), commit, push.
Then remove the worktrees: `git worktree list`, then `git worktree remove --force <path>`.

## Next milestones (in order)
1. Assemble 02a + 02b, then commit the wire/cli contract and run its wave.
2. Get `pgarbor-cli serve` running, then `psql`/psycopg connects and `SELECT 1` works.
3. Differential campaign (`uv run python -m pgarbor_harness diff`) vs tests/expected; cluster failures; cheap-agent repair loop.
4. Fixture apps against pgarbor; north-star demo (one .pga, branch, crash survival).
5. WASM demo (MemFile), bindings (C ABI in `crates/pgarbor/src/ffi.rs`, Python, Node).

## Known gaps / debts
- SERIALIZABLE runs as REPEATABLE READ (recorded known gap).
- Tuple depth guard MAX_DEPTH lowered 512 to 128 (needs decision).
- Pending amendments in `contracts/amendments/` (not yet folded into contracts v2).
- 7 nondeterministic and 7 quarantined oracle cases; `cmd-alter-function.support-function` crashes PG18.6 itself.

## Cost policy (from the owner)
Cheapest model that works: haiku/gpt-6-luna implement; no LLM review of cheap code (the compiler and
the PG18 diff are the reviewers); sonnet/gpt-6.1-sol only for architecture and review passes.
