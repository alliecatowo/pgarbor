# PGArbor

**PostgreSQL without the server.** PGArbor is a work-in-progress, single-file, branch-native
relational database written in Rust. It aims for behavioral compatibility with PostgreSQL 18, and
it checks that compatibility by running the same tests against a pinned, real PostgreSQL 18.6.

> **Status: pre-alpha, not usable yet.** The storage, transaction, branching, type and catalog
> layers are implemented and unit-tested. The SQL front end, planner, executor, wire protocol
> and CLI are still being designed or are queued for implementation. You cannot run `psql`
> against PGArbor yet. Nothing here is published to crates.io, npm or PyPI.

## What exists today

| Layer | Crate | State |
|---|---|---|
| Common types, errors, KV traits | `pgarbor-common` | implemented |
| Single `.pga` file storage: CoW B+tree, dual superblock, crash recovery | `pgarbor-storage` | implemented, unit + property tests |
| Deterministic fault injection (torn writes, lost fsync, ENOSPC) | `pgarbor-fault` | implemented |
| Branches: create/delete/rename/reset, handles, detach/export, reflink | `pgarbor-branches` | implemented |
| Transactions: snapshots, savepoints, row locks, deadlock detection, sequences | `pgarbor-core` | implemented (SERIALIZABLE runs as REPEATABLE READ; known gap) |
| Types: numeric, date/time, json/jsonb, arrays, regex, aggregates, … | `pgarbor-types` | implemented |
| Catalog: `pg_catalog`, `information_schema`, DDL, dependencies, ACLs | `pgarbor-catalog` | implemented |
| SQL parser/binder, planner, executor, embedded API | `pgarbor-sql`, `-plan`, `-exec`, `pgarbor` | in design |
| PG wire protocol server, CLI | `pgarbor-wire`, `pgarbor-cli` | in design |

## Language bindings and WebAssembly

Everything below was run on this branch (Linux x86_64); the engine is still pre-release, so only the SQL the
engine supports today works.

**C ABI** (`crates/pgarbor-ffi`, header `include/pgarbor.h`). Handles are bound to the creating thread.

```sh
cargo build -p pgarbor-ffi            # target/debug/libpgarbor_ffi.{so,dylib,a}
cargo test -p pgarbor-ffi             # compiles crates/pgarbor-ffi/tests/smoke.c with cc and runs it
```

```c
uint64_t db, s; PgaResult *r;
pga_open("dev.pga", PGA_OPEN_CREATE, &db);
pga_session_open(db, NULL, NULL, NULL, &s);
if (pga_exec(s, "SELECT 1", &r) == 0) { puts(pga_result_value(r, 0, 0, 0)); pga_result_free(r); }
else fprintf(stderr, "%s %s\n", pga_errcode(), pga_errmsg());   // e.g. 23505 duplicate key ...
```

**Python** (`bindings/python`, ctypes, no build step beyond the cdylib; set `PGARBOR_LIB` to override the library path).

```python
import pgarbor
conn = pgarbor.connect("dev.pga")
conn.execute("CREATE TABLE t(id int PRIMARY KEY)")
try: conn.execute("INSERT INTO t VALUES (1),(1)")
except pgarbor.Error as e: print(e.sqlstate)       # 23505
```
Test: `cd bindings/python && uv run --with pytest python -m pytest tests`.

**Node** (`bindings/node`, [koffi](https://koffi.dev) over the C ABI). Test: `cd bindings/node && npm install && npm test`.

```js
const pgarbor = require('./bindings/node');
const c = pgarbor.connect('dev.pga');
console.log(c.exec('SELECT 1 AS one').rows);        // [ [ '1' ] ]
```

**WebAssembly** (`crates/pgarbor-wasm`, in-memory database on `MemVfs`; see `docs/WASM.md` for limits).

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
demo/build.sh                                      # builds demo/web/pkg (about 13 MB wasm, unoptimised)
node demo/test-node.mjs                            # SELECT / CREATE / INSERT / duplicate key under Node
python3 -m http.server -d demo/web 8000            # then open http://localhost:8000
```

## How compatibility is verified

`harness/` is a Python verification harness (run with `uv`):

- `tests/cases/` holds about 5,800 differential test cases, authored from the PostgreSQL 18 docs
  and command reference.
- `tests/expected/` holds their reference outputs, recorded **only** from a pinned
  PostgreSQL 18.6 container (`postgres@sha256:74935e72…`). Expected outputs are never written
  by hand.
- `tests/apps/` holds unmodified client fixture apps (sqlx, node-postgres, Drizzle, Prisma)
  that pass against PostgreSQL 18 and will be pointed at PGArbor.
- Unsupported behavior returns SQLSTATE `0A000` with a `pgarbor-unimplemented:` detail. It
  never silently returns wrong results.

```sh
cd harness
uv run python -m pgarbor_harness oracle up      # start the pinned PG18 oracle (podman)
uv run python -m pgarbor_harness cases validate
uv run pytest -q
cargo test --workspace                          # from the repo root
```

## Design documents

- [`docs/rfcs/PGARBOR_RFC_001.md`](docs/rfcs/PGARBOR_RFC_001.md): vision and architecture
- [`docs/rfcs/PGARBOR_RFC_002_AUTONOMOUS_VERIFICATION.md`](docs/rfcs/PGARBOR_RFC_002_AUTONOMOUS_VERIFICATION.md): verification
- [`contracts/ARCHITECTURE.md`](contracts/ARCHITECTURE.md): frozen architecture contract
- `contracts/crates/*.md`: per-crate contracts

PostgreSQL is a trademark of the PostgreSQL Community Association. PGArbor is not affiliated
with or endorsed by the PostgreSQL project.

## License

Dual-licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
