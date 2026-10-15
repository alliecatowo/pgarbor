# CLI import and conservative table dump

Status: ACCEPTED (lead-engineer decision, 2026-10-15).

CLI-28 and CLI-10 install the API memory COPY channel for one complete split script block;
SQL is executed by the regular session and the channel is removed after success/error.
COPY binding now owns a TableExpr scope and pops it on every returned result.

CLI-26 supports a conservative ordinary-table schema/data dump from one REPEATABLE READ
READ ONLY transaction. It preflights the inventory and builds the complete output before
writing anything. Sequences, views, user routines/types/comments, identity/generated
columns, custom relation privileges/ownership/options/persistence/replica identity, row
security and deferred/unvalidated constraints are rejected with leased 0A000. This is
not full pg_dump compatibility. Default table columns/defaults, named NOT NULL, constraints,
indexes and INSERT data are emitted with identifier/literal quoting. Constraints follow
data so FK dependency cycles can reload; indexes backed by constraints are not duplicated.
Output files use an owned exclusive temporary file, sync then rename; a failed inventory
never publishes a partial SQL file. `--copy` dump remains unsupported.

Pure unit fixture corrections: MD5(secret + postgres) is independently computed as
53f48b7c4b76a86ce72276c5755f217d. AuthenticationSASL's length is 4+4+13+1+1=23,
not 19. Centered table fixture widths use max(header, cells), not an extra price column
cell. A single backslash in E-string input needs two output backslashes, not four.
These corrections change implementation-authored unit examples only, never oracle cases
or expected outputs. CSV NULL/empty, zero-row footer, regex escaping and byte units are
implementation fixes preserving the existing tests.
