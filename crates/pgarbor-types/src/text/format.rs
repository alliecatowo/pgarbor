//! format(), quoting, concat and radix conversions (TYPE-13). Lease TYP-15.
//!
//! PG18 reference: `utils/adt/varlena.c` (`text_format`, `text_concat`, `text_concat_ws`,
//! `to_hex32/64`, `to_oct32/64`, `to_bin32/64`), `quote.c`, `ruleutils.c` (`quote_identifier`),
//! keyword list: `contracts/reference/pg18/pg_keywords.csv`.
//! Owned procs (ImplModule::TextFormat): `text_format`, `text_format_nv` (format with only the format
//! string), `quote_ident`, `quote_literal`, `quote_nullable`, SQL-language `quote_literal(anyelement)`
//! (oid 1285) and `quote_nullable(anyelement)` (oid 1290) (convert with `crate::io::output` of
//! `ctx.arg_types[0]` first), `text_concat` (`concat(VARIADIC "any")`), `text_concat_ws`,
//! `to_hex32 to_hex64 to_oct32 to_oct64 to_bin32 to_bin64` (two's complement of the unsigned
//! reinterpretation: `to_hex(-1)` = `ffffffff`, lowercase).
//!
//! `concat`/`concat_ws`/`format` are `VARIADIC "any"`: they receive the individual argument Datums and
//! their types in `ctx.arg_types`; non-text values are rendered with `crate::io::output`; NULLs are
//! skipped by concat/concat_ws (concat_ws with NULL separator → NULL; it is not strict).
//! `concat('a', NULL, 1)` = `a1`; `concat_ws(',', 'a', NULL, 'b')` = `a,b`.
//!
//! `format(fmt, ...)`: specifiers `%[argpos$][flags][width]type` with types `s` (value as text, NULL →
//! empty), `I` (`quote_ident`; NULL → 22004 `null values cannot be formatted as an SQL identifier`),
//! `L` (`quote_literal`, NULL → `NULL` unquoted), `%%` literal. Flag `-` left-justifies; width may be
//! `*` / `*n$`. Positional and sequential args may mix: after `%2$s` the next plain `%s` uses arg 3.
//! Errors (22023): `too few arguments for format()`, `unrecognized format() type specifier "z"` with
//! HINT `For a single "%" use "%%".`, `unterminated format() type specifier`, and
//! `format specifies argument 0, but arguments are numbered from 1`.
//! Oracle: `format('%2$s %1$s %s', 'a', 'b')` = `b a b`.
//!
//! `quote_ident(s)`: returned unquoted only if s matches `[a-z_][a-z0-9_$]*` AND is not a keyword
//! whose category (pg_keywords.csv `catcode`) is anything but `U` (unreserved); otherwise wrapped in `"`
//! with embedded `"` doubled. `quote_ident('select')` = `"select"`, `quote_ident('Abc')` = `"Abc"`.
//! `quote_literal(s)`: `'` + s with `'` doubled + `'`; if s contains a backslash, backslashes are doubled
//! and the result is prefixed with `E` (`quote_literal(E'a''b\\c')` = `E'a''b\\\\c'`).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// PG `quote_identifier` (also used by reg* output and catalog DDL rendering downstream).
// CONTRACT
#[allow(unused_variables)]
pub fn quote_identifier(ident: &str) -> Result<String> {
    Err(PgError::unimplemented("TYP-15", "quote_ident"))
}

/// PG `quote_literal` text form.
// CONTRACT
#[allow(unused_variables)]
pub fn quote_literal(s: &str) -> Result<String> {
    Err(PgError::unimplemented("TYP-15", "quote_literal"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a TextFormat-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-15", format!("function {}", proc.name))
}
