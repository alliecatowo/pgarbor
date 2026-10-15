//! Literals: `sqlparser::ast::Value` -> typed constants (SQL-08).
//! PG references: `parse_expr.c` (`make_const`), `parse_node.c` (`make_const`), `scan.l` (literal
//! lexing rules), `gram.y` `AexprConst`, `parse_type.c`/`parse_coerce.c` for typed literals.
//!
//! ## Typing rules (PG `make_const`)
//! * Integer literal `123`: `int4` if it fits in i32, else `int8` if it fits in i64, else `numeric`.
//!   `-` is a separate unary operator (`-5` = `-(5)`, constant folded to `-5` int4; `-2147483648` is
//!   int4 after folding because PG's lexer/grammar `doNegate` folds the sign into the literal: do the
//!   same for a unary minus applied directly to a numeric literal).
//! * Decimal/exponent literal `1.5`, `1e3`, `1.` -> `numeric` with scale from the text (`1.50` has
//!   scale 2; `1e3` numeric 1000, scale 0).
//! * Hex/octal/binary integer literals (`0x1F`, `0o17`, `0b101`, PG 16+) and `1_000` underscores:
//!   int4/int8/numeric by magnitude; sqlparser exposes them as `Value::Number` text or
//!   `HexStringLiteral`; follow `scan.l` (`integer_junk`, `numeric_junk` -> 42601 `trailing junk
//!   after numeric literal at or near "1a"`).
//! * String literal `'abc'` (also `E'..'`, `$$..$$`, `U&'..'`, national `N'..'`): type `unknown`
//!   with value `Datum::Text(decoded)`; represented as `BoundExpr::constant(Datum::Text(s),
//!   TypeId::UNKNOWN)`. Conversion to a concrete type happens in `coerce` (position-aware).
//!   `E'..'` escapes decoded here: `\n \t \r \b \f \\ \' \" \ooo \xhh \uXXXX \UXXXXXXXX`,
//!   invalid -> 22P05? (`invalid Unicode escape`, 22025 `invalid escape string`...). `U&'\0041'`
//!   with optional `UESCAPE`. Standard strings (`standard_conforming_strings=on`, the pinned
//!   setting) treat backslash literally; sqlparser already folds `''`.
//! * `NULL` -> `Const(Datum::Null)` typed `unknown`.
//! * `TRUE`/`FALSE` -> `bool` constants. `X'1F'` hex-string and `B'101'` bit-string literals: bit
//!   types are unsupported -> 0A000 (`bit string literal`).
//! * Typed literal `DATE '2020-01-01'`, `TIMESTAMP '..'`, `INTERVAL '1 day' DAY TO SECOND`,
//!   `type 'str'` via `Expr::TypedString`: resolve the type (`types`), then convert the string with
//!   `pgarbor_types::io::input` NOW (constant) so errors carry the literal's position:
//!   `22007 invalid input syntax for type date: "xx"` at the string token. Special datetime inputs
//!   (`'now'`, `'today'`, ...) use `Binder::io_ctx()` (with the statement timestamp). Result is a
//!   `Const` of the target type (typmod applied by `input`).
//! * Placeholder values (`$1`) go to `params::bind_param`.
//!
//! Positions: errors use the `ValueWithSpan` span start.
use pgarbor_common::{PgError, Result};
use pgarbor_types::TypeId;
use sqlparser::ast::{DataType, Value, ValueWithSpan};

use super::context::Binder;
use crate::ir::BoundExpr;

/// Bind a literal value (typing rules above). `negate`: the literal is the direct operand of a unary
/// minus and the sign must be folded into the constant.
#[allow(unused_variables)]
pub fn bind_value(b: &mut Binder, v: &ValueWithSpan, negate: bool) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-08", "literal binding (literal::bind_value)"))
}

/// `type 'string'` / `DATE '..'`: resolve `data_type`, convert `value` (must be a string-like
/// literal: `SingleQuotedString`, `EscapedStringLiteral`, `DollarQuotedString`, ...) with
/// `io::input` and return the typed constant.
#[allow(unused_variables)]
pub fn bind_typed_string(b: &mut Binder, data_type: &DataType, value: &ValueWithSpan) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-08", "typed string literal (literal::bind_typed_string)"))
}

/// Classify numeric literal text into the narrowest of int4 / int8 / numeric and produce the
/// constant. `text` is the literal text WITHOUT sign (`"2147483648"` -> int8). Unit-testable helper.
#[allow(unused_variables)]
pub fn numeric_literal(b: &mut Binder, text: &str, negate: bool, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-08", "numeric literal typing (literal::numeric_literal)"))
}

/// Decode an `E'...'` escape-string body (the text between the quotes, still containing
/// backslash escapes) into the string value. Errors: 22025 `invalid escape string`? Use PG's
/// messages from `scan.l`: `invalid Unicode escape`, `invalid Unicode surrogate pair`,
/// `invalid hexadecimal digit`? (see `addunicode`, `check_escape_warning`), all 22P05 /
/// 22025 / 42601 as in the grammar; position = start of the string token.
#[allow(unused_variables)]
pub fn decode_escape_string(body: &str, pos: Option<u32>) -> Result<String> {
    Err(PgError::unimplemented("SQL-08", "E'' escape decoding (literal::decode_escape_string)"))
}

/// The `unknown`-typed constant for a string value (shared constructor, fully implemented).
pub fn unknown_text(s: String) -> BoundExpr {
    BoundExpr::constant(pgarbor_types::Datum::Text(s), TypeId::UNKNOWN)
}

/// Is this expression an `unknown`-typed string/NULL literal constant (awaiting coercion)?
pub fn is_unknown_literal(e: &BoundExpr) -> bool {
    e.ty.oid == pgarbor_types::oids::UNKNOWN && matches!(e.kind, crate::ir::ExprKind::Const(_))
}

#[allow(dead_code)]
fn _value_marker(_: &Value) {}
