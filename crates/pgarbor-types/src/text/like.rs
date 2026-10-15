//! LIKE / ILIKE / SIMILAR TO pattern support (TYPE-13). Lease TYP-16.
//!
//! PG18 reference: `utils/adt/like.c`, `like_match.c` (`MatchText`, `do_like_escape`),
//! `regexp.c` (`similar_escape_internal`), planner prefix logic `utils/adt/like_support.c`.
//! Owned procs (ImplModule::Like): `textlike textnlike texticlike texticnlike namelike namenlike
//! nameiclike nameicnlike bytealike byteanlike like_escape like_escape_bytea similar_escape
//! similar_to_escape_1 similar_to_escape_2`.
//!
//! LIKE semantics: `%` matches any sequence (including empty), `_` exactly one CHARACTER (one BYTE
//! for bytea), the escape character (default `\`) makes the next char literal. Matching is on the whole
//! string. A pattern ending with the escape char → 22025 `LIKE pattern must not end with escape character`.
//! `x LIKE p ESCAPE e` is bound as `textlike(x, like_escape(p, e))`: `like_escape` rewrites p to use `\`;
//! e must be '' (disables escaping: `\` becomes literal, so double it) or one character, else 22025
//! `invalid escape string` with HINT `Escape string must be empty or one character.`.
//! ILIKE: case-insensitive using the same simple lowercase mapping as `lower()`. NOT LIKE = negation.
//! Example: `'abc' LIKE 'a%'` → true; `'a_c' LIKE 'a\_c'` → true; `'abc' LIKE 'a\_c'` → false.
//!
//! SIMILAR TO: `x SIMILAR TO p [ESCAPE e]` is bound as `textregexeq(x, similar_to_escape(p[, e]))`.
//! `similar_to_escape` converts the SQL-standard pattern to an anchored POSIX ARE:
//! result = `^(?:` + converted + `)$`; `%` → `.*`, `_` → `.`, regex metachars `.` `^` `$` are escaped,
//! `[...]` copied through (with `^` negation kept), and the escape-double-quote separators (`#"` with
//! ESCAPE '#') split the pattern into three parts emitted as `(?:a){1,1}?(b){1,1}(?:c)` — copy the exact
//! algorithm from `similar_escape_internal`. Oracle: `similar_to_escape('a%b_')` → `^(?:a.*b.)$`;
//! `similar_to_escape('a#"b#"c', '#')` → `^(?:a){1,1}?(b){1,1}(?:c)$`; `similar_to_escape('[^a].$')` →
//! `^(?:[^a]\.\$)$`.
//!
//! The "must not end with escape character" error is raised only when matching actually reaches the
//! trailing escape: `'x' LIKE 'a\'` is simply false, `'ab\' LIKE 'ab\'` errors (oracle-verified).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// `s LIKE pattern` with `\` as escape. Example: `like("abc", "a%")` → `Ok(true)`.
// CONTRACT
#[allow(unused_variables)]
pub fn like(s: &str, pattern: &str) -> Result<bool> {
    Err(PgError::unimplemented("TYP-16", "LIKE"))
}

/// `s ILIKE pattern` with `\` as escape.
// CONTRACT
#[allow(unused_variables)]
pub fn ilike(s: &str, pattern: &str) -> Result<bool> {
    Err(PgError::unimplemented("TYP-16", "ILIKE"))
}

/// Byte-wise LIKE for bytea.
// CONTRACT
#[allow(unused_variables)]
pub fn like_bytes(s: &[u8], pattern: &[u8]) -> Result<bool> {
    Err(PgError::unimplemented("TYP-16", "bytea LIKE"))
}

/// Planner helper: the literal prefix of a LIKE pattern (with `\` escapes) and whether the pattern is
/// exactly that literal (no wildcards). `"abc%"` → `("abc", false)`; `"ab\\%c"` → `("ab%c", true)`;
/// `"%x"` → `("", false)`. Used for index range scans (`prefix <= x < successor(prefix)`).
// CONTRACT
#[allow(unused_variables)]
pub fn like_fixed_prefix(pattern: &str) -> Result<(String, bool)> {
    Err(PgError::unimplemented("TYP-16", "LIKE prefix extraction"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Like-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-16", format!("function {}", proc.name))
}
