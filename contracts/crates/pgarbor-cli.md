# pgarbor-cli — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-cli.json` (CLI-01..CLI-28; seed tasks CLI-01..CLI-12).
Amendments: `pgarbor-cli-01-libc-for-signals`, `pgarbor-wire-01-copy-channel-and-streaming`, `harness-02-cli-serve-contract`.
Depends on: pgarbor-common, pgarbor-storage, pgarbor-branches, pgarbor, pgarbor-wire, clap 4.6 (derive; `env` feature OFF), serde, serde_json.

## Purpose

The `pgarbor` executable and the `pgarbor_cli` library behind it (so integration tests run commands in-process with
`pgarbor_cli::run_with(args, |cli| Ctx::with_sinks(..))`). All SQL goes through the embedded API; networking goes through
`pgarbor-wire`. RFC 001 sections 3 and 10.

## Command grammar (frozen in `cli.rs`)

See the table in `cli.rs` module docs. Highlights: `init|create`, `serve`, `shell`, `exec`, `status`, `inspect`, `verify`, `compact`,
`doctor`, `version`, `dump`, `import`, `branch {create,list,link,resolve,exec,serve,rename,delete,reset,compare,detach,export,snapshot,
restore,purge,set-default}`. Global flags `--json`, `-q`, `-v`. Database resolution: positional FILE > `--db` > `$PGARBOR_DB` > handle
(`--handle` or nearest `.pgabranch`). Branch: `--branch` > handle identity > `$PGARBOR_BRANCH` > the database's default branch.

## Exit codes (frozen in `exit.rs`)

| code | meaning |
|---|---|
| 0 | success |
| 1 | operation failed (SQL error in exec/import, I/O) |
| 2 | address in use (stderr contains `address in use`) |
| 3 | corrupt / not a PGArbor database (stderr contains `corrupt`) |
| 4 | database locked by another process |
| 5 | not found (file, branch, snapshot, handle) |
| 6 | already exists |
| 7 | refused (destructive without `--yes`, `DATABASE_URL` set without `--force-env`, non-loopback bind, read-only) |
| 8 | not implemented yet |
| 64 | usage error (clap errors map here, NOT to clap's default 2) |
| 65 | malformed input file (SQL script splitter, handle file) |
| 70 | internal error |
| 126/127 | `branch exec`: command not executable / not found |
| 128+N | `branch exec`: child killed by signal N (otherwise the child's code verbatim) |

## Output conventions

* Human output on stdout for results, diagnostics on stderr. `--quiet` suppresses informational stderr lines.
* `--json`: success -> ONE pretty JSON document on stdout (schemas in `reports.rs`, frozen); errors -> ONE JSON object on stderr
  (`{"error":{"kind","exit_code","sqlstate","message","detail","hint"}}`). `serve` emits single-line events; `branch exec` writes its
  summary event to stderr because stdout belongs to the child.
* `serve` prints exactly one stdout line `pgarbor: listening on <host>:<port>` (real port; harness-02 parses it); the URL is printed to
  stderr.
* Destructive commands (`branch delete/reset/purge`) require `--yes` unless a human confirms on a terminal.

## Serving and `branch exec`

* `serve` writes `<db>.serve.json` (`ServeInfo`, atomically, removed on clean exit) and, on unix, a control socket
  `<db>.serve.sock`. Because the `.pga` is exclusively locked by the serving process (ADR-004), `branch exec` against a served database
  asks the control socket for a listener on the requested branch (`listen` lease; the connection staying open IS the lease) and never
  stops a pre-existing listener. Without a serving process `branch exec` opens the database itself, starts an owned loopback listener on
  port 0 (no reserve/rebind race), injects `DATABASE_URL=postgres://user@127.0.0.1:PORT/db?sslmode=disable` (refusing to overwrite an
  existing value without `--force-env`), runs the child, stops the listener, and returns the child's exit status.
* Signals: handlers only set atomics (`signals.rs`); all loops poll (SA_RESTART). `serve` shuts down cleanly on SIGINT/SIGTERM/SIGHUP
  within 10 s and exits 0; `branch exec` forwards the signal to the child once.
* A listener is bound to one branch; the control protocol is documented in `control.rs`.

## Files and formats owned here

* `<db>.serve.json`: `ServeInfo` JSON, `format = "pgarbor-serve/1"`.
* Control protocol: newline-delimited JSON over a Unix socket (`ping`, `status`, `listen`).
* Dump format: plain SQL in `pg_dump` order (`cmd/dump.rs`), INSERT data by default, `COPY ... FROM stdin` blocks with `--copy`.
* `.pgabranch` handles are owned by `pgarbor-branches`; the CLI only creates/validates them.

## Error conventions

`CliError { exit, message, detail, hint, sqlstate }`; `From<PgError>` classifies by SQLSTATE (`exit::classify`). Engine errors keep PG
wording. Lock errors are enriched with the owner (`serve_registry`) and a remedy hint.

## Known gaps / non-goals (v1)

* No line editing or history in `shell` (std stdin only); no query cancel with Ctrl-C in the embedded shell.
* COPY FROM stdin blocks (import, exec, shell, `dump --copy`) are blocked on amendment `pgarbor-wire-01` (`pgarbor::MemChannel`).
* `branch create --copy-snapshots` uses the engine directly; `branch delete --allow-default` and reset with `ResetOptions` are not
  reachable through `pgarbor::Database` (documented fallbacks in the lease specs).
* The control socket is unix-only; on other targets `branch exec` against a served database exits 4.
* `import`/`dump` are not `pg_dump`/`pg_restore` binary-compatible; plain SQL only. `dump` holds one table's rows in memory.
* No progress bars; long operations print single status lines on stderr.
* Windows is untested.

## Lease index
| lease | file | tier | scope |
|---|---|---|---|
| CLI-01 | table.rs | haiku | psql-style tables |
| CLI-02 | paths.rs | haiku | db/branch/handle resolution |
| CLI-03 | open.rs | haiku | opening with lock/corruption diagnostics |
| CLI-04 | sql_script.rs | sonnet | SQL script splitter |
| CLI-05 | humanize.rs | haiku | sizes, timestamps |
| CLI-06 | cmd/init.rs (+tests/cli_06.rs) | haiku | init/create |
| CLI-07 | cmd/serve.rs (+tests/cli_07.rs) | sonnet | serve |
| CLI-08 | cmd/shell.rs | sonnet | shell loop |
| CLI-09 | cmd/shell_meta.rs | haiku | backslash commands |
| CLI-10 | cmd/exec.rs (+tests/cli_10.rs) | haiku | exec |
| CLI-11 | render.rs | haiku | result rendering |
| CLI-12 | cmd/status.rs | haiku | status |
| CLI-13 | cmd/inspect.rs | haiku | inspect |
| CLI-14 | cmd/verify.rs | haiku | verify |
| CLI-15 | cmd/compact.rs | haiku | compact |
| CLI-16 | cmd/doctor.rs | haiku | doctor |
| CLI-17 | cmd/branch/basic.rs | haiku | branch create/list/rename/delete/restore/purge/set-default |
| CLI-18 | cmd/branch/snapshot.rs | haiku | snapshots |
| CLI-19 | cmd/branch/link.rs | haiku | link/resolve |
| CLI-20 | cmd/branch/reset_compare.rs | haiku | reset/compare |
| CLI-21 | cmd/branch/detach_export.rs | haiku | detach/export |
| CLI-22 | cmd/branch/exec.rs (+tests/cli_22.rs) | sonnet | branch exec |
| CLI-23 | serve_registry.rs | haiku | discovery file |
| CLI-24 | control.rs | sonnet | control socket server |
| CLI-25 | control_client.rs | haiku | control socket client |
| CLI-26 | cmd/dump.rs | sonnet | dump (schema) |
| CLI-27 | cmd/dump_data.rs | haiku | dump (data) |
| CLI-28 | cmd/import.rs (+tests/cli_28.rs) | haiku | import |

Frozen and implemented (not leased): `lib.rs`, `main.rs`, `cli.rs`, `exit.rs`, `ctx.rs`, `reports.rs`, `signals.rs`, `confirm.rs`,
`dispatch.rs`, `transcript_file.rs`, `cmd/mod.rs`, `cmd/version.rs`, `cmd/branch/mod.rs`.
