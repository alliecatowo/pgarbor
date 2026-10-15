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
use crate::context::FuncCtx;
use crate::datum::{type_mismatch, Datum};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result, SqlState};

/// Recursion limit for `match_units` (PostgreSQL recurses once per `%` in the pattern and reports
/// `54001 stack depth limit exceeded` when the C stack runs out; we bound the depth explicitly).
const MAX_MATCH_DEPTH: u32 = 4096;

/// A "character" unit of the matcher: `char` for text, `u8` for bytea.
trait Unit: Copy + Eq {
    const BS: Self;
    const PCT: Self;
    const USC: Self;
}

impl Unit for char {
    const BS: char = '\\';
    const PCT: char = '%';
    const USC: char = '_';
}

impl Unit for u8 {
    const BS: u8 = b'\\';
    const PCT: u8 = b'%';
    const USC: u8 = b'_';
}

/// Result of `match_units`: `Abort` means no suffix of the text can match the rest of the pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Matched {
    True,
    False,
    Abort,
}

fn escape_end_error() -> PgError {
    PgError::new(SqlState::INVALID_ESCAPE_SEQUENCE, "LIKE pattern must not end with escape character")
}

fn invalid_escape_error() -> PgError {
    PgError::new(SqlState::INVALID_ESCAPE_SEQUENCE, "invalid escape string")
        .with_hint("Escape string must be empty or one character.")
}

fn stack_depth_error() -> PgError {
    PgError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
}

/// Port of `MatchText` (like_match.c), deterministic-collation paths only (v1 collates by code point).
fn match_units<U: Unit>(t: &[U], p: &[U], depth: u32) -> Result<Matched> {
    // Fast path for the match-everything pattern.
    if p.len() == 1 && p[0] == U::PCT {
        return Ok(Matched::True);
    }
    if depth > MAX_MATCH_DEPTH {
        return Err(stack_depth_error());
    }
    let mut ti = 0usize;
    let mut pi = 0usize;
    while ti < t.len() && pi < p.len() {
        let pc = p[pi];
        if pc == U::PCT {
            // Collapse runs of '%' and '_' in the pattern: N '_' plus '%' = N '_' then '%'.
            pi += 1;
            while pi < p.len() {
                let c = p[pi];
                if c == U::PCT {
                    pi += 1;
                } else if c == U::USC {
                    if ti >= t.len() {
                        return Ok(Matched::Abort);
                    }
                    ti += 1;
                    pi += 1;
                } else {
                    break;
                }
            }
            // Trailing '%' matches any remaining text.
            if pi >= p.len() {
                return Ok(Matched::True);
            }
            // The first remaining pattern unit is a literal, so only positions starting with it
            // need a recursive attempt.
            let firstpat = if p[pi] == U::BS {
                if p.len() - pi < 2 {
                    return Err(escape_end_error());
                }
                p[pi + 1]
            } else {
                p[pi]
            };
            while ti < t.len() {
                if t[ti] == firstpat {
                    let m = match_units(&t[ti..], &p[pi..], depth + 1)?;
                    if m != Matched::False {
                        return Ok(m);
                    }
                }
                ti += 1;
            }
            // No position matched, so no later start can either.
            return Ok(Matched::Abort);
        } else if pc == U::USC {
            ti += 1;
            pi += 1;
            continue;
        } else if pc == U::BS {
            // Next pattern unit must match literally, whatever it is.
            pi += 1;
            if pi >= p.len() {
                return Err(escape_end_error());
            }
            if p[pi] != t[ti] {
                return Ok(Matched::False);
            }
        } else if pc != t[ti] {
            return Ok(Matched::False);
        }
        ti += 1;
        pi += 1;
    }
    if ti < t.len() {
        // End of pattern, but not of text.
        return Ok(Matched::False);
    }
    // End of text: the rest of the pattern must be zero or more '%'.
    while pi < p.len() && p[pi] == U::PCT {
        pi += 1;
    }
    if pi >= p.len() {
        Ok(Matched::True)
    } else {
        Ok(Matched::Abort)
    }
}

/// Simple (single-char) lowercase mapping, as used by `lower()`: a multi-char mapping keeps the char.
fn simple_lower(c: char) -> char {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// `s LIKE pattern` with `\` as escape. Example: `like("abc", "a%")` → `Ok(true)`.
// CONTRACT
pub fn like(s: &str, pattern: &str) -> Result<bool> {
    let t: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    Ok(match_units(&t, &p, 0)? == Matched::True)
}

/// `s ILIKE pattern` with `\` as escape.
// CONTRACT
pub fn ilike(s: &str, pattern: &str) -> Result<bool> {
    let t: Vec<char> = s.chars().map(simple_lower).collect();
    let p: Vec<char> = pattern.chars().map(simple_lower).collect();
    Ok(match_units(&t, &p, 0)? == Matched::True)
}

/// Byte-wise LIKE for bytea.
// CONTRACT
pub fn like_bytes(s: &[u8], pattern: &[u8]) -> Result<bool> {
    Ok(match_units(s, pattern, 0)? == Matched::True)
}

/// Planner helper: the literal prefix of a LIKE pattern (with `\` escapes) and whether the pattern is
/// exactly that literal (no wildcards). `"abc%"` → `("abc", false)`; `"ab\\%c"` → `("ab%c", true)`;
/// `"%x"` → `("", false)`. Used for index range scans (`prefix <= x < successor(prefix)`).
// CONTRACT
pub fn like_fixed_prefix(pattern: &str) -> Result<(String, bool)> {
    // Mirrors `Like_fixed_prefix` (like_support.c), case-sensitive variant.
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut pos = 0usize;
    while pos < chars.len() {
        let c = chars[pos];
        if c == '%' || c == '_' {
            break;
        }
        if c == '\\' {
            pos += 1;
            if pos >= chars.len() {
                break;
            }
        }
        out.push(chars[pos]);
        pos += 1;
    }
    Ok((out, pos == chars.len()))
}

/// Port of `do_like_escape` (like.c / like_match.c) over any unit type.
fn like_escape_units<U: Unit>(pat: &[U], esc: &[U]) -> Result<Vec<U>> {
    let mut out = Vec::with_capacity(pat.len() * 2);
    if esc.is_empty() {
        // No escape character wanted: double backslashes so they act as ordinary characters.
        for &c in pat {
            if c == U::BS {
                out.push(U::BS);
            }
            out.push(c);
        }
        return Ok(out);
    }
    if esc.len() > 1 {
        return Err(invalid_escape_error());
    }
    let e = esc[0];
    if e == U::BS {
        return Ok(pat.to_vec());
    }
    let mut afterescape = false;
    for &c in pat {
        if c == e && !afterescape {
            out.push(U::BS);
            afterescape = true;
        } else if c == U::BS {
            out.push(U::BS);
            if !afterescape {
                out.push(U::BS);
            }
            afterescape = false;
        } else {
            out.push(c);
            afterescape = false;
        }
    }
    Ok(out)
}

/// `like_escape(text, text)`: rewrite an ESCAPE-clause pattern to use `\`.
fn like_escape_text(pat: &str, esc: &str) -> Result<String> {
    let p: Vec<char> = pat.chars().collect();
    let e: Vec<char> = esc.chars().collect();
    Ok(like_escape_units(&p, &e)?.into_iter().collect())
}

/// Escape character of a SIMILAR TO conversion: `None` argument = default `\`; empty string = none.
fn similar_escape_char(esc: Option<&str>) -> Result<Option<char>> {
    match esc {
        None => Ok(Some('\\')),
        Some(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (None, _) => Ok(None),
                (Some(c), None) => Ok(Some(c)),
                _ => Err(invalid_escape_error()),
            }
        }
    }
}

/// Port of `similar_escape_internal` (regexp.c): SQL SIMILAR TO pattern to anchored POSIX ARE.
fn similar_escape_impl(pat: &str, esc: Option<&str>) -> Result<String> {
    let e = similar_escape_char(esc)?;
    let mut out = String::with_capacity(pat.len() * 3 + 24);
    out.push_str("^(?:");
    let mut afterescape = false;
    let mut nquotes = 0u32;
    // Square-bracket nesting level and position inside a character class (see the C source).
    let mut bracket_depth = 0u32;
    let mut charclass_pos = 0u32;
    for pchar in pat.chars() {
        if afterescape {
            if pchar == '"' && bracket_depth < 1 {
                // Escape-double-quote separator between the SUBSTRING parts.
                match nquotes {
                    0 => out.push_str("){1,1}?("),
                    1 => out.push_str("){1,1}(?:"),
                    _ => {
                        return Err(PgError::new(
                            SqlState(*b"2200C"),
                            "SQL regular expression may not contain more than two escape-double-quote separators",
                        ))
                    }
                }
                nquotes += 1;
            } else {
                // Any character may be escaped (e.g. POSIX class escapes such as \d).
                out.push('\\');
                out.push(pchar);
                charclass_pos = 3;
            }
            afterescape = false;
        } else if e == Some(pchar) {
            // SQL escape character: not sent to the output.
            afterescape = true;
        } else if bracket_depth > 0 {
            // Inside a character class.
            if pchar == '\\' {
                out.push('\\');
            }
            out.push(pchar);
            if pchar == ']' && charclass_pos > 2 {
                bracket_depth -= 1;
            } else if pchar == '[' {
                bracket_depth += 1;
                charclass_pos = 3;
            } else if pchar == '^' {
                charclass_pos += 1;
            } else {
                charclass_pos = 3;
            }
        } else if pchar == '[' {
            out.push('[');
            bracket_depth = 1;
            charclass_pos = 1;
        } else if pchar == '%' {
            out.push_str(".*");
        } else if pchar == '_' {
            out.push('.');
        } else if pchar == '(' {
            out.push_str("(?:");
        } else if matches!(pchar, '\\' | '.' | '^' | '$') {
            out.push('\\');
            out.push(pchar);
        } else {
            out.push(pchar);
        }
    }
    out.push_str(")$");
    Ok(out)
}

/// `s SIMILAR TO pattern` escape conversion: `similar_to_escape(pattern[, escape])`. Strict callers
/// pass `Some(escape)`; `None` selects the default `\`.
fn similar_to_escape_text(pat: &str, esc: Option<&str>) -> Result<String> {
    similar_escape_impl(pat, esc)
}

// ---- scalar function wrappers (registry entry points) ----

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| PgError::internal(format!("missing function argument {}", i + 1)))
}

fn text_arg(d: &Datum) -> Result<&str> {
    d.as_str().ok_or_else(|| type_mismatch("text", d))
}

fn bytes_arg(d: &Datum) -> Result<&[u8]> {
    d.as_bytes().ok_or_else(|| type_mismatch("bytea", d))
}

fn text_pair(args: &[Datum]) -> Result<(&str, &str)> {
    Ok((text_arg(arg(args, 0)?)?, text_arg(arg(args, 1)?)?))
}

fn bytes_pair(args: &[Datum]) -> Result<(&[u8], &[u8])> {
    Ok((bytes_arg(arg(args, 0)?)?, bytes_arg(arg(args, 1)?)?))
}

fn like_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = text_pair(args)?;
    Ok(Datum::Bool(like(s, p)?))
}

fn not_like_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = text_pair(args)?;
    Ok(Datum::Bool(!like(s, p)?))
}

fn ilike_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = text_pair(args)?;
    Ok(Datum::Bool(ilike(s, p)?))
}

fn not_ilike_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = text_pair(args)?;
    Ok(Datum::Bool(!ilike(s, p)?))
}

fn bytea_like_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = bytes_pair(args)?;
    Ok(Datum::Bool(like_bytes(s, p)?))
}

fn bytea_not_like_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (s, p) = bytes_pair(args)?;
    Ok(Datum::Bool(!like_bytes(s, p)?))
}

fn like_escape_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (pat, esc) = text_pair(args)?;
    Ok(Datum::Text(like_escape_text(pat, esc)?))
}

fn like_escape_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (pat, esc) = bytes_pair(args)?;
    Ok(Datum::Bytea(like_escape_units(pat, esc)?))
}

/// `similar_escape(pattern, escape)`: legacy, NOT strict. NULL pattern → NULL; NULL escape → default.
fn similar_escape_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let pat_datum = arg(args, 0)?;
    if pat_datum.is_null() {
        return Ok(Datum::Null);
    }
    let pat = text_arg(pat_datum)?;
    let esc = match arg(args, 1)? {
        Datum::Null => None,
        d => Some(text_arg(d)?),
    };
    Ok(Datum::Text(similar_to_escape_text(pat, esc)?))
}

fn similar_to_escape_1_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let pat = text_arg(arg(args, 0)?)?;
    Ok(Datum::Text(similar_to_escape_text(pat, None)?))
}

fn similar_to_escape_2_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let (pat, esc) = text_pair(args)?;
    Ok(Datum::Text(similar_to_escape_text(pat, Some(esc))?))
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    Some(match proc.prosrc {
        "textlike" | "namelike" => FuncImpl::Scalar(like_fn),
        "textnlike" | "namenlike" => FuncImpl::Scalar(not_like_fn),
        "texticlike" | "nameiclike" => FuncImpl::Scalar(ilike_fn),
        "texticnlike" | "nameicnlike" => FuncImpl::Scalar(not_ilike_fn),
        "bytealike" => FuncImpl::Scalar(bytea_like_fn),
        "byteanlike" => FuncImpl::Scalar(bytea_not_like_fn),
        "like_escape" => FuncImpl::Scalar(like_escape_fn),
        "like_escape_bytea" => FuncImpl::Scalar(like_escape_bytea_fn),
        "similar_escape" => FuncImpl::Scalar(similar_escape_fn),
        "similar_to_escape_1" => FuncImpl::Scalar(similar_to_escape_1_fn),
        "similar_to_escape_2" => FuncImpl::Scalar(similar_to_escape_2_fn),
        _ => return None,
    })
}

/// 0A000 for a Like-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-16", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::registry::builtin_env;

    fn with_ctx<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &io,
            session: &session,
            arg_types: &[],
            result_type: TypeId::BOOL,
            collation: 0,
        };
        f(&ctx)
    }

    fn code_of(e: &PgError) -> &str {
        e.sqlstate.code()
    }

    #[test]
    fn like_basic() {
        assert_eq!(like("abc", "a%").unwrap(), true);
        assert_eq!(like("abc", "abc").unwrap(), true);
        assert_eq!(like("abc", "ab").unwrap(), false);
        assert_eq!(like("", "%").unwrap(), true);
        assert_eq!(like("", "_").unwrap(), false);
        assert_eq!(like("abc", "%%_c").unwrap(), true);
        assert_eq!(like("abc", "a_").unwrap(), false);
        assert_eq!(like("abcabc", "%bc").unwrap(), true);
        assert_eq!(like("abcabd", "%abc%").unwrap(), true);
        assert_eq!(like("héllo", "h_llo").unwrap(), true);
    }

    #[test]
    fn like_escapes() {
        assert_eq!(like("a_c", "a\\_c").unwrap(), true);
        assert_eq!(like("abc", "a\\_c").unwrap(), false);
        assert_eq!(like("x", "a\\").unwrap(), false);
        assert_eq!(like("", "a\\").unwrap(), false);
        let e = like("ab\\", "ab\\").unwrap_err();
        assert_eq!(code_of(&e), "22025");
        assert_eq!(e.message, "LIKE pattern must not end with escape character");
        let e = like("xyz", "%\\").unwrap_err();
        assert_eq!(code_of(&e), "22025");
    }

    #[test]
    fn ilike_case() {
        assert_eq!(ilike("ABC", "a%").unwrap(), true);
        assert_eq!(ilike("abc", "A_C").unwrap(), true);
        assert_eq!(ilike("ABC", "a\\_c").unwrap(), false);
    }

    #[test]
    fn bytea_like() {
        assert_eq!(like_bytes(b"abc", b"a%").unwrap(), true);
        assert_eq!(like_bytes(b"a\xffc", b"a_c").unwrap(), true);
        assert_eq!(like_bytes(b"ab\\", b"ab\\").unwrap_err().sqlstate.code(), "22025");
    }

    #[test]
    fn like_escape_rewrite() {
        let e = like_escape_text("x", "ab").unwrap_err();
        assert_eq!(code_of(&e), "22025");
        assert_eq!(e.message, "invalid escape string");
        assert_eq!(e.hint.as_deref(), Some("Escape string must be empty or one character."));
        assert_eq!(like_escape_text("a#_b", "#").unwrap(), "a\\_b");
        assert_eq!(like_escape_text("a\\b", "").unwrap(), "a\\\\b");
        assert_eq!(like_escape_text("a\\b", "\\").unwrap(), "a\\b");
        assert_eq!(like_escape_text("##x", "#").unwrap(), "\\#x");
    }

    #[test]
    fn similar_conversion() {
        assert_eq!(similar_to_escape_text("a%b_", None).unwrap(), "^(?:a.*b.)$");
        assert_eq!(similar_to_escape_text("a#\"b#\"c", Some("#")).unwrap(), "^(?:a){1,1}?(b){1,1}(?:c)$");
        assert_eq!(similar_to_escape_text("[^a].$", None).unwrap(), "^(?:[^a]\\.\\$)$");
        assert_eq!(similar_to_escape_text("(a|b)", None).unwrap(), "^(?:(?:a|b))$");
        assert_eq!(similar_to_escape_text("a", Some("")).unwrap(), "^(?:a)$");
        let e = similar_to_escape_text("a#\"b#\"c#\"", Some("#")).unwrap_err();
        assert_eq!(code_of(&e), "2200C");
        let e = similar_to_escape_text("a", Some("ab")).unwrap_err();
        assert_eq!(code_of(&e), "22025");
    }

    #[test]
    fn prefix_extraction() {
        assert_eq!(like_fixed_prefix("abc%").unwrap(), ("abc".to_string(), false));
        assert_eq!(like_fixed_prefix("ab\\%c").unwrap(), ("ab%c".to_string(), true));
        assert_eq!(like_fixed_prefix("%x").unwrap(), (String::new(), false));
        assert_eq!(like_fixed_prefix("abc").unwrap(), ("abc".to_string(), true));
        assert_eq!(like_fixed_prefix("ab_").unwrap(), ("ab".to_string(), false));
    }

    #[test]
    fn registry_wrappers() {
        with_ctx(|ctx| {
            let args = [Datum::text("abc"), Datum::text("a%")];
            assert_eq!(like_fn(&args, ctx).unwrap(), Datum::Bool(true));
            assert_eq!(not_like_fn(&args, ctx).unwrap(), Datum::Bool(false));
            let args = [Datum::Null, Datum::text("a%")];
            assert_eq!(similar_escape_fn(&args, ctx).unwrap(), Datum::Null);
            let args = [Datum::text("a%b_"), Datum::Null];
            assert_eq!(similar_escape_fn(&args, ctx).unwrap(), Datum::text("^(?:a.*b.)$"));
            let args = [Datum::text("x"), Datum::text("ab")];
            assert!(like_escape_fn(&args, ctx).is_err());
        });
        assert!(lookup_is_some("textlike"));
        assert!(lookup_is_some("similar_to_escape_1"));
        assert!(!lookup_is_some("int4pl"));
    }

    fn lookup_is_some(src: &'static str) -> bool {
        let proc = BuiltinProc {
            oid: 0,
            name: "t",
            namespace: 11,
            kind: crate::catalog_data::ProcKind::Function,
            ret: 16,
            retset: false,
            volatility: crate::catalog_data::Volatility::Immutable,
            strict: true,
            nargs: 2,
            ndefaults: 0,
            arg_types: &[25, 25],
            variadic: 0,
            prosrc: src,
            lang: 12,
            all_arg_types: &[],
            arg_modes: &[],
            arg_names: &[],
            arg_defaults: "",
            module: crate::catalog_data::ImplModule::Like,
        };
        lookup(&proc).is_some()
    }
}
