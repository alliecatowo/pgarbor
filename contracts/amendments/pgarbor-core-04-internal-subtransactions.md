# pgarbor-core amendment: internal subtransactions

- `SavepointStack::{begin_subxact, release_subxact, rollback_subxact}` open unnamed internal levels for
  PL/pgSQL `EXCEPTION` blocks. They are legal in implicit transactions and are invisible to SQL
  `SAVEPOINT` / `ROLLBACK TO` (token = stack index, validated on release / rollback).
- Rolling back an internal level discards its writes and locks; the enclosing transaction stays usable.
