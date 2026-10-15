# Amendment request: order-free CREATE SEQUENCE and CREATE/ALTER ROLE option lists

Status: APPLIED (2026-10-15). Task: Taskwarrior 727.

## Problem

PG 18 accepts the options of `CREATE SEQUENCE` (`SeqOptList`) and of `CREATE|ALTER ROLE|USER`
(`OptRoleList` / `AlterOptRoleList`) in any order, with signed numbers (`CONNECTION LIMIT -2`,
`INCREMENT +3`). sqlparser 0.63 fixes the CREATE SEQUENCE option order and rejects signed numbers in
role options, so four `ddl_objects` tests failed. sqlparser stays pinned.

## Change

- `ExtraStatement` (contract enum) gains `CreateSequence`, `CreateRole` and `AlterRole`, parsed by
  `parser_extra` (hand parser, same route as `ALTER SEQUENCE`). `CREATE ROLE|USER|GROUP` and the option
  form of `ALTER ROLE|USER r [WITH] options` no longer go through sqlparser; `ALTER ROLE ... RENAME TO`,
  `SET`, `RESET` and `IN DATABASE` still do. `CREATE USER MAPPING` keeps its unsupported route.
- New contract types in `extra_stmt`: `RoleStmtKind`, `RoleOptionAst`, `RoleOptionItem` (option plus the
  character position used for 42601 conflicts). `SequenceOptionAst` gains `Logged(bool)` and
  `SequenceName`, produced only for CREATE SEQUENCE (ALTER SEQUENCE still reports them as 0A000 at
  parse time).
- Binder: `ddl_objects::bind_create_sequence(b, temporary, unlogged, if_not_exists, name, options)` and
  `bind_create_role(b, kind, name, options)` take the extra-statement fields; `ddl_alter_other` gains
  `bind_alter_role_options`. `bind_sequence_options` is unchanged; `signed_number` now also accepts a
  sign kept inside the number text (this also fixes `ALTER SEQUENCE ... INCREMENT -n`, which builds
  its option list from text).

## PG behavior matched (oracle)

- Any option order; a repeat is 42601 `conflicting or redundant options` (sequence: statement
  position; role: position of the repeated option). `ROLE`/`USER` lists and `IN ROLE`/`IN GROUP`
  lists conflict with each other as in PG.
- `CONNECTION LIMIT` takes a `SignedIconst`: a value that does not fit `int` is 42601 syntax error at
  the number; `< -1` is 22023 `invalid connection limit: N`.
- Unknown plain identifier: 42601 `unrecognized role option "x"`; `UNENCRYPTED PASSWORD`: 0A000 with
  PG's hint; `SYSID n`: accepted, NOTICE `SYSID can no longer be specified`; CREATE-only options in
  ALTER ROLE are syntax errors.
- CREATE SEQUENCE: `RESTART [n]` accepted and ignored; `SEQUENCE NAME` 42601 `invalid sequence
  option SEQUENCE NAME`; `LOGGED`/`UNLOGGED` XX000 `option "logged" not recognized`.
- `CREATE USER` implies LOGIN (an explicit `NOLOGIN` wins); `CREATE GROUP` is `CREATE ROLE`.

## Not implemented

- `TEMP` and `UNLOGGED` sequences remain 0A000. `ALTER ROLE r USER x` (member list) is 0A000.
- Conflict detection for sequences is reported at the statement position, not at the repeated option,
  and `AS type` validity is checked before the numeric option errors (as before this change).
- `ALTER GROUP ... ADD|DROP USER` is not covered (a different grammar, no option list).
