# pgarbor-types — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-types.json` (TYP-01..TYP-57,
seed tasks TYPE-01..TYPE-15). Amendments: `contracts/amendments/pgarbor-types-01-regex-engine.md`,
`-02-error-context.md`, `-03-datum-variants.md`, `-04-timezone-data.md`.

## Purpose

All PostgreSQL 18 value semantics that need no catalog storage and no transaction state live here:

- the runtime value (`Datum`)
- type identity (`TypeId`, OIDs) and the compiled-in PG18 catalog rows (`catalog_data`)
- text and binary I/O, byte-exact with PG 18.6 under the pinned GUCs
- typmods
- casts and coercion
- function, operator and polymorphic resolution
- builtin scalar functions, SRFs and aggregates
- three-valued logic, SQL comparison and hashing
- the two on-disk value encodings: row tuples (`tuple`) and index keys (`keycodec`)

The crate depends only on `pgarbor-common` plus `bigdecimal`, `num-bigint`, `serde` and `serde_json` (`proptest` for tests).
It knows nothing about the KV store, branches, catalogs-in-storage, sessions or the wire protocol.
Those reach it through two traits, `TypeEnv` and `SessionContext`.

## Module map (frozen; workers may not add modules or edit lib.rs / mod.rs / Cargo.toml)

| Module | Role | Lease |
|---|---|---|
| `datum` | `Datum`, `TypeId`, `Interval`, infinity constants, the type→variant mapping table | — (done) |
| `oids` | 201 generated OID constants (`INT4`, `INT4_ARRAY`, `PG_CATALOG_NAMESPACE`...) | — (generated) |
| `catalog_data::{types,procs,operators,casts,aggregates}` | all PG18.6 `pg_type` (622), `pg_proc` (3413), `pg_operator` (800), `pg_cast` (236), `pg_aggregate` (162) rows; lookups in `catalog_data` | — (generated) |
| `registry` | `TypeInfo`, `BuiltinTypeEnv`, `builtin_env()`, type predicates (array/element/category/preferred/base/polymorphic) | — (done) |
| `context` | `TypeEnv`, `SessionContext`, `IoConfig` (+ GUC enums), `IoCtx`, `FuncCtx`, `TestSession` | — (done) |
| `logic` | three-valued logic | — (done) |
| `io` | `input`/`output`/`recv`/`send` dispatch by `IoFamily` (typinput name), domains, `void` | — (done) |
| `names`, `typmod` | SQL type names ↔ OID + typmod; `format_type`; typmod in/out/enforce | TYP-01, TYP-02 |
| `scalar::{bool,int,int_ops,float,float_ops,oid_types}` | primitive families | TYP-03..08 |
| `numeric::{mod,io,arith,transcendental,funcs}` | `Numeric` value + I/O + arithmetic + functions | TYP-09..12 |
| `text::{mod,io,funcs,format,like,regex}` | character types, string functions, `format()`, LIKE, ARE regex | TYP-13..17 |
| `bytea`, `crypto`, `uuid` | binary strings, md5/sha2/crc, uuid | TYP-18..20 |
| `datetime::{mod,calendar,lexer,decode,encode,tz,date,time,timestamp,interval,funcs}` | full PG datetime stack | TYP-21..30 |
| `formatting` | `to_char`/`to_date`/`to_timestamp(text,text)`/`to_number` | TYP-31 |
| `json::{mod,parse,io,ops,build,srf}` | json/jsonb | TYP-32..36 |
| `array::{mod,io,funcs,subscript}` | arrays | TYP-37..39 |
| `record`, `enum_type`, `extra_types`, `system_funcs` | composites, enums, unsupported families, system info functions | TYP-40..43 |
| `func::{mod,series}` | proc OID → implementation dispatcher; generate_series | — / TYP-44 |
| `agg::{mod,basic,collect,stats,ordered_set}` | `AggState` trait + aggregate families | — / TYP-45..48 |
| `compare` | SQL ordering, equality, hashing; PG `hash_bytes` ports | TYP-49 |
| `keycodec` | memcomparable index keys (on-disk v1) | TYP-50 |
| `tuple` | self-describing row encoding (on-disk v1) | TYP-51 |
| `cast::{mod,lookup,apply}` | `find_cast`/`plan_cast`/`apply_cast`/`cast_datum` | TYP-52, TYP-53 |
| `resolve::{mod,coerce,func,oper,polymorphic}` (`ops` re-exports `resolve::oper`) | `can_coerce_type`, `select_common_type`, function/operator/polymorphic resolution | TYP-54..57 |

The doc comments in each file are the detailed spec. They include PG source references, exact error texts
and fixtures checked against the PG 18.6 oracle. This document gives the cross-cutting rules.

## Public API overview (what downstream crates use)

| Item | Use |
|---|---|
| `Datum`, `TypeId`, `Interval`, `Numeric`, `Jsonb`, `Array` | values. A `Datum` never carries its type; a `TypeId{oid, typmod}` always travels next to it |
| `oids::*`, `catalog_data::{builtin_type, builtin_proc, builtin_operator, builtin_cast, builtin_aggregate, procs_named, operators_named}` | compiled-in PG18 catalog rows. pgarbor-catalog serves `pg_type`/`pg_proc`/... views from them |
| `TypeEnv` (trait), `registry::builtin_env()` | type metadata: builtin + user (enums, domains, composites, user casts, `reg*` name lookup) |
| `SessionContext` (trait), `IoConfig`, `IoCtx`, `FuncCtx` | per-call environment: GUCs, clocks, randomness, notices, external procs |
| `io::{input, output, recv, send, output_opt}` | PG typinput/typoutput/typreceive/typsend |
| `names::{TypeName, resolve_type_name, parse_type_string, format_type, display_name}` | DDL/CAST type names and `format_type` |
| `typmod::{typmod_in, typmod_out, enforce_typmod}` | typmods |
| `cast::{find_cast, plan_cast, apply_cast, cast_datum, CastPlan, CastContext}` | casts (planner plans once, executor applies) |
| `resolve::{coerce::*, func::*, polymorphic::*}`, `ops::{resolve_operator, resolve_builtin_operator}` | binder resolution of calls, operators, UNION/CASE/COALESCE/ARRAY types |
| `func::{implementation, FuncHandle, FuncImpl, call, call_set, SetIter}` | executable builtins by proc OID |
| `agg::{make_aggregate, AggSpec, AggState, aggregate_info}` | aggregate execution |
| `compare::{cmp_datums, cmp_sort, sql_eq, not_distinct, hash_datum, is_orderable}` | sorting, joins, DISTINCT, GROUP BY, hashing |
| `logic::*` | AND/OR/NOT, WHERE/CHECK qualification |
| `keycodec::{KeyColumn, encode_key, encode_key_vec}` | index keys |
| `tuple::{encode_row, decode_row, decode_columns}` | table rows |

### Typical flows

```rust
// pgarbor-exec: literal input, function call, output
let io  = IoCtx::new(env, &session_io_config).with_now(txn_ts);
let v   = io::input(&io, TypeId::INT4, "42")?;                 // typmod applied by input
let h   = func::implementation(proc_oid)?;                     // cache in the plan
let ctx = FuncCtx { env, io: &cfg, session, arg_types: &tys, result_type, collation: 0 };
let r   = h.call(&[v, Datum::Int4(1)], &ctx)?;                 // strict NULL handling inside
let s   = io::output(&ctx.io_ctx(), result_type, &r)?;

// pgarbor-plan/sql binder: operator, cast, common type
let op   = ops::resolve_builtin_operator(env, "+", Some(oids::INT4), oids::UNKNOWN)?;   // int4 + int4
let plan = cast::plan_cast(env, from, to, CastContext::Explicit)?;  // executor: cast::apply_cast(&plan, v, &ctx)
let t    = resolve::coerce::select_common_type(env, &[oids::INT4, oids::NUMERIC], "UNION")?;

// pgarbor-core storage of rows / indexes
let row = tuple::encode_row(&values)?;                         // value under [relid][rowid BE]
let key = keycodec::encode_key_vec(env, &[KeyColumn::asc(TypeId::INT4)], &[v])?;
```

## Invariants

1. **Datum↔type mapping.** The `datum.rs` table is the single source of truth for which SQL types use
   which variant. A mismatched variant is an internal bug: `datum::type_mismatch` → XX000. It is not a user
   error.
   - `bpchar` is stored with its blank padding.
   - `timetz` zone is in seconds **west** of UTC, as in PG.
   - date/timestamp infinities are `i32::MAX/MIN` and `i64::MAX/MIN`.
   - Infinite intervals have all three fields at MAX or MIN.
2. **Numeric.** `Numeric::{NaN, PosInf, NegInf, Finite(BigDecimal)}`.
   - Finite values always have scale ≥ 0, and the scale is PG's display scale (dscale). `1.50` keeps scale 2.
   - Only `numeric::io` and `Numeric::from_decimal` construct finite values. Every constructor normalizes a
     negative scale.
   - `PartialEq` is by value. `pg_cmp` orders NaN above +Inf and treats NaN = NaN.
3. **Jsonb.** `Object` pairs are unique and sorted in jsonb storage order: shorter key first, then bytewise
   (`json::jsonb_key_cmp`). On duplicate keys the last one wins. Numbers are `Numeric`. `Json` is the
   verbatim validated input text. Parsed JSON/JSONB input is limited to 128 open containers
   (54001 on excess; accepted amendment `pgarbor-types-05-json-depth-safety`).
4. **Array.**
   - `dims` is a list of (len, lbound) pairs, and `items.len() == product(len)`, in row-major order.
   - An empty array has `dims == []`. `int2vector`/`oidvector` are 1-D arrays with lbound 0.
   - `elem` is the element type OID.
5. **Equality.** `#[derive(PartialEq)]` is structural and meant for tests. SQL equality, ordering and
   hashing are `compare::*` only. Within one type, `hash_datum` agrees with `not_distinct`.
6. **Strictness.** `FuncHandle::call` returns NULL for a strict proc with any NULL argument before
   calling the implementation. Implementations may assume non-NULL arguments for strict procs.
7. **Typmods** use PG's encodings exactly (`typmod.rs`, `datum.rs::TypeId`). `io::input`/`recv` apply
   `ty.typmod`; `output` ignores it.
8. **Collation.** v1 treats every collation as C / code-point order (ARCHITECTURE §2). `FuncCtx.collation`
   is carried but not interpreted.
9. **Pinned GUCs.** `IoConfig::default()` = DateStyle ISO, MDY; IntervalStyle postgres;
   extra_float_digits 1; bytea_output hex; TimeZone UTC. These are the oracle settings, and every fixture
   assumes them unless it says otherwise.
10. **Determinism.** Nothing in this crate reads the wall clock, environment or OS randomness. All of
    that goes through `SessionContext`. `IoCtx.now = None` makes `'now'`/`'today'` input fail with 0A000
    (lease TYP-23) rather than guess.

## Byte-exact encodings

Two on-disk encodings, both **frozen at v1**. The authoritative byte layouts are the module docs of
`crates/pgarbor-types/src/tuple.rs` and `crates/pgarbor-types/src/keycodec.rs`. Summary:

- **Row tuple (`tuple`, TYP-51).** `0x01` version byte, then `varint(ncols)`, then one self-describing
  value per column: a tag byte (0x00 Null … 0x17 Record) and a payload.
  - Fixed-width fields are little-endian. `varint` is unsigned LEB128 with minimal length.
  - Numeric is a kind byte, then `varint(scale)` and the BE two's-complement unscaled BigInt.
  - Jsonb is a recursive tree with object pairs in storage order.
  - Decoding needs no catalog, so ADD/DROP COLUMN and binary-compatible type changes keep old rows readable.
  - Encoding is canonical: `encode_row(decode_row(b)) == b`.
  - Malformed input gives XX001. It never panics.
- **Index key (`keycodec`, TYP-50).** Memcomparable; bytes are not decodable.
  - Per column: NULL is `0x00` (nulls first) or `0xFF` (nulls last). Non-NULL is `0x80` followed by BODY,
    with BODY bitwise inverted for DESC.
  - Fixed-width BODYs are big-endian with the sign bit flipped.
  - Floats are canonicalized (-0 becomes 0, every NaN becomes the canonical NaN), so NaN sorts above +inf.
  - Numeric BODY is a class byte, then the exponent, then base-100 digit pairs and a terminator. Trailing
    zeros are stripped, so `1.0` and `1.00` encode the same.
  - Text BODY escapes `0x00` as `00 FF` and ends with `00 01`. bpchar strips trailing spaces first.
  - Interval BODY is `cmp_span` as i128.
  - Array, jsonb and record BODYs follow PG's btree comparison.
  - SQL-equal values MUST encode identically. Unorderable types (json, point...) give 42883.
  - Index entry layout `[index relid u32 BE][key][rowid u64 BE if non-unique]` is ARCHITECTURE §4.
- **Binary wire format (`io::send/recv`).** PG's `*send`/`*recv`, big-endian, byte-identical to
  PG 18.6. Examples:
  - numeric: NBASE-10000 header and digits
  - timestamp: i64 µs since 2000-01-01
  - interval: µs, days, months
  - jsonb: version byte 1 followed by the text
  - arrays: ndim, hasnull, elemtype, (dim, lbound)*, then length-prefixed elements
- **Text I/O** is byte-exact with PG 18.6 under `IoConfig`. Every family's module doc lists oracle fixtures.

Changing either on-disk format needs a new format version byte or an index rebuild, plus an amendment.

## Error conventions

- Every fallible API returns `pgarbor_common::Result<T>` = `Result<T, PgError>`. Never panic on user input.
  No `unwrap` on data paths, and no `todo!`/`unimplemented!`/`panic!`.
- Messages, SQLSTATEs, DETAILs and HINTs are PG 18.6's, verbatim. The module docs quote the
  oracle-verified texts. Examples:
  - 22P02 `invalid input syntax for type integer: "x"`
  - 22003 `value "99999999999" is out of range for type integer`
  - 22008 `date/time field value out of range: "25:00"`
  - 42883 `function foo(integer) does not exist` + HINT
  - 42725 `operator is not unique: unknown + unknown`
- Type names in messages come from `names::display_name` (= PG `format_type_be`).
- Warnings that PG emits mid-call (typmod precision reduced, etc.) are reported either by return value
  (`typmod_in`'s `Vec<PgError>` with severity Warning) or through `SessionContext::notice`.
- **Not implemented yet**: `PgError::unimplemented("<LEASE>", "<what>")` → 0A000
  `<what> is not supported by PGArbor yet`, DETAIL `pgarbor-unimplemented: <LEASE>`. This is the ONLY
  allowed placeholder. Each leased file uses only its own lease ID. Builtin procs that a module does not
  implement are reported through that module's `unsupported(proc)`, so the harness can inventory gaps by
  lease.
- Internal invariant breaks: `PgError::internal` (XX000). Corrupt bytes: `PgError::corruption` (XX001).
- PG's error CONTEXT lines (`PL/pgSQL function ...`, `JSON data, line 1: ...`) are not modelled;
  amendment 02 proposes a `context` field.

## Function, operator and aggregate implementation ownership

`gen/gen_tables.py` (run with `python3 crates/pgarbor-types/gen/gen_tables.py`; `-v <Module>` prints
the classification) regenerates `oids.rs` and `catalog_data/*.rs` from `contracts/reference/pg18/*.csv`
plus `gen/pg_proc_args.csv`. It assigns each proc and aggregate an `ImplModule`; do not hand-edit the
generated files.
- `func::implementation(oid)` routes each proc through its module's `lookup(proc)`.
- If `lookup` returns `None`, the module's `unsupported(proc)` supplies the 0A000 error.
- `ImplModule::External` procs are catalog, session or transaction functions, such as `current_setting`,
  `nextval`, `pg_backend_pid`, `txid_current` and the `pg_get_*` family. They resolve to
  `FuncImpl::External`, and `FuncHandle::call` forwards them to `SessionContext::call_external(_set)`,
  which pgarbor-exec implements.
- `ImplModule::Unsupported` procs (geometric, network, range, tsearch, xml, …) are 0A000 through
  `extra_types` (TYP-42).

To move a proc to another module, edit `classify()` in the generator and regenerate. That is an
architect change, not a worker change.

## How downstream crates use it

- **pgarbor-catalog** implements `TypeEnv` over a catalog snapshot.
  - Builtin rows come from `catalog_data`. User enums, domains, composites and casts come from storage.
  - `type_by_name(None, name)` and `reg_lookup` MUST honour `search_path`.
  - It falls back to `registry::builtin_env()` for builtin OIDs.
  - It serves `pg_type`/`pg_proc`/`pg_operator`/`pg_cast`/`pg_aggregate` rows from `catalog_data`.
- **pgarbor-sql / pgarbor-plan (binder)**
  - turns sqlparser types into `names::TypeName` and calls `resolve_type_name`
  - resolves calls with `resolve::func::resolve_function` (candidates from `builtin_candidates` plus user
    functions)
  - resolves operators with `ops::resolve_operator`
  - plans casts with `cast::plan_cast`
  - picks UNION/CASE/COALESCE/VALUES/ARRAY types with `resolve::coerce::select_common_type`
  - uses `typmod::enforce_typmod` for assignment coercion
- **pgarbor-exec**
  - implements `SessionContext`: clocks, PRNG, notices and External procs
  - builds `FuncCtx`/`IoCtx` from session GUCs (`IoConfig`, `datetime::tz::parse_timezone_setting` for
    `SET TimeZone`)
  - caches `FuncHandle`s in plans and evaluates with `logic`, `compare`, `cast::apply_cast` and
    `agg::make_aggregate`
  - sorts with `compare::cmp_sort` and hashes with `compare::hash_datum`
- **pgarbor-core** stores rows with `tuple::encode_row/decode_row/decode_columns` and index keys with
  `keycodec::encode_key`.
- **Wire layer (pgarbor-cli serve)** uses `io::output` for text-format results and `io::send/recv` for
  binary format. It reports column types as `TypeId` (OID + typmod), and its ParameterStatus values must
  match `IoConfig`.

## Non-goals and known gaps (v1)

- **Collations other than C/code-point; locales** (`lc_monetary`, `lc_time`): `to_char` uses English names.
- **IANA time zones.** Only UTC, fixed offsets, POSIX-style `UTC+3` strings and the PG `Default`
  abbreviation set are supported. Named zones such as `America/New_York` give 0A000 (TYP-25), not 22023.
  See amendment 04 (oracle tzdata `2026c`).
- **Datum variants for inet/cidr/macaddr, bit/varbit, geometric types, ranges/multiranges, tid, tsvector, xml,
  money (only via Int8 in TYP-42).** These need a Datum amendment (03); until then they are 0A000.
- **Regex.** PG's ARE engine is re-implemented (TYP-17, opus, amendment 01). This is the largest single
  risk to parity.
- **Error CONTEXT lines** (amendment 02).
- **Anonymous record types** for `json_to_record` and friends: the column definition list must come from
  the binder through `FuncCtx.result_type` + `TypeEnv::composite_fields`. Input of anonymous `record`
  gives 0A000.
- **`version()`** reports `PostgreSQL 18.6 (PGArbor …)`, which differs from the oracle string by design.
- **Float transcendental functions** use Rust `f64` methods (the platform libm). Last-ulp differences from glibc (e.g. `cbrt`, `exp`
  at extremes) are accepted and documented in tests.
- **`keycodec`** lives here, not in pgarbor-common as ARCHITECTURE §1 suggests, because it needs
  `Datum` and `compare`. pgarbor-common keeps only the type-agnostic prefix helpers (`kv::prefix_upper_bound`,
  `SpaceId`).

## North-star order

The first vertical slice is
`CREATE TABLE t(id int, name text, amount numeric(10,2), ts timestamptz); INSERT ...; SELECT ... WHERE ... ORDER BY`
over the wire. The leases JSON lists the slice's 27 leases first, tagged `[north-star]`, in a topological
order of `depends_on_leases`:

1. Values, ordering and storage: TYP-49 compare, TYP-04 int, TYP-13 text I/O, TYP-03 bool, TYP-09 numeric I/O,
   TYP-06 float, TYP-50 keycodec, TYP-51 tuple.
2. Type names and the timestamptz column:
   - TYP-15 format/quote_ident
   - TYP-21 calendar, TYP-24 encode, TYP-25 tz, TYP-22 lexer, TYP-23 decode, TYP-28 timestamp
   - TYP-02 typmod, TYP-01 names
3. Binding: TYP-52/TYP-53 casts, TYP-57 polymorphic, TYP-54 coerce, TYP-55 function resolution, TYP-56 operators.
4. WHERE operators: TYP-05 int ops, TYP-14 text functions/comparisons, TYP-10 numeric arithmetic,
   TYP-12 numeric functions/comparisons.

`depends_on_leases` is an ordering hint for integration. It does not block granting: speculative work waits
only on frozen contracts (RFC 003 §2).

A few edges were deliberately left out so the graph stays acyclic and the north-star set stays small:
- TYP-57 → TYP-54
- TYP-49 → TYP-34/TYP-41
- TYP-02 → TYP-27/TYP-29
- TYP-28 → TYP-29
- TYP-12 → TYP-11

In each of these, the leased code calls the other lease's frozen signature normally. Its tests for that
part are marked `#[ignore = "needs TYP-xx"]` until the other lease lands (see each lease's `GATED` note).
Everything else can proceed in parallel.
