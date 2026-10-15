//! json / jsonb text and binary I/O (TYPE-08, TYPE-15). Lease TYP-33.
//!
//! PG18 reference: `utils/adt/json.c` (`json_in/out/recv/send`), `jsonb.c` (`jsonb_in`, `jsonb_out`
//! via `JsonbToCString`, `jsonb_recv`/`jsonb_send` with version byte), `jsonb_util.c` (normalization).
//!
//! - json input: `parse::validate_json`, store the original text verbatim (`Datum::Json`).
//!   Output: verbatim. Binary send/recv: raw UTF-8 text (no version byte).
//! - jsonb input: `parse::parse_jsonb` → `Datum::Jsonb`. Output (`format_jsonb`): canonical form
//!   `{"a": 1, "b": [1, 2]}` (", " and ": " separators; strings via `parse::escape_json`; numbers via
//!   `numeric::io::format_numeric` (so `1.50` stays `1.50`, `1e3` prints `1000`)).
//!   Oracle: `'{"b":1,"aa":[1,2.50,{"c":null}],"a":true, "b":2}'::jsonb` →
//!   `{"a": true, "b": 2, "aa": [1, 2.50, {"c": null}]}`; `'1.000e-2'::jsonb` → `0.01000`; `'-0'::jsonb` → `0`.
//! - jsonb binary: send = version byte 0x01 followed by the canonical text; recv requires version 1
//!   (else XX000 `unsupported jsonb version number N`, as PG's elog) then parses the text.
//! - `jsonb_pretty` format (used by json::build): 4-space indentation, `{` newline, `"k": v`, items
//!   separated by `,\n`, closing bracket on its own line; empty containers print `{}` / `[]`.
use super::Jsonb;
use crate::context::IoCtx;
use crate::datum::{Datum, TypeId};
use pgarbor_common::{PgError, Result};

/// Canonical jsonb text (`jsonb_out`).
// CONTRACT
#[allow(unused_variables)]
pub fn format_jsonb(j: &Jsonb) -> Result<String> {
    Err(PgError::unimplemented("TYP-33", "jsonb output"))
}

/// Indented jsonb text (`jsonb_pretty`).
// CONTRACT
#[allow(unused_variables)]
pub fn format_jsonb_pretty(j: &Jsonb) -> Result<String> {
    Err(PgError::unimplemented("TYP-33", "jsonb_pretty"))
}

/// `json_in` / `jsonb_in` by `ty.oid`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-33", "json input"))
}

/// `json_out` / `jsonb_out`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-33", "json output"))
}

/// `json_recv` / `jsonb_recv`.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-33", "json binary input"))
}

/// `json_send` / `jsonb_send`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-33", "json binary output"))
}
