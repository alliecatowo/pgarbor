# Amendment request pgarbor-types-01: regular-expression engine for PostgreSQL ARE semantics

Status: PROPOSED (architect, pgarbor-types). The v1 skeleton designs around it (lease TYP-17 writes a
self-contained engine in `crates/pgarbor-types/src/text/regex.rs`).

## Problem

`~`, `~*`, `SIMILAR TO`, `substring(... from ...)`, `regexp_replace/match/matches/split_*/count/instr/like/substr`
are used heavily by ORMs and introspection queries (psql `\d` uses `~` with `^(...)$` patterns, Prisma/Drizzle
use `regexp_replace`/`~` in migrations). PostgreSQL implements Henry Spencer's ARE: back references (`\1`),
lookahead and lookbehind, `\m \M \y \Y`, embedded options `(?i)`, and leftmost-longest matching for greedy REs.
The frozen dependency set has no regex crate. The `regex` crate lacks back references and lookaround, and its
leftmost-first semantics differ from PG's. `fancy-regex` adds backrefs and lookaround but is still leftmost-first.

## Proposed change

Pick one of:
1. (current plan) Keep deps frozen. TYP-17 (opus) writes a backtracking ARE engine with a leftmost-longest
   pass for greedy REs. Highest fidelity, the most code (about 1,500 lines), and a risk of exponential
   backtracking. Mitigate it with a step budget that raises 54001 `regular expression is too complex`.
2. Add `regex = "1"` to `[workspace.dependencies]` and pgarbor-types as a fast path for patterns without
   backrefs or lookaround, plus the TYP-17 engine as a fallback.

## Affected crates

pgarbor-types only (`text::regex` API is engine-agnostic: `compile`, `find_at`, `is_match`).
