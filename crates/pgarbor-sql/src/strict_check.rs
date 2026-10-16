//! Token-level grammar checks for statements whose operands `sqlparser` accepts more loosely than
//! PostgreSQL 18's `gram.y`. Each check runs before the statement goes to `sqlparser` and only
//! rejects input that PostgreSQL's grammar rejects, at the token where bison would report the
//! error (`syntax error at or near "tok"`, or `syntax error at end of input`). Valid input falls
//! through unchanged, so the AST path stays the single source of truth for everything else.
//!
//! Productions (PostgreSQL 18 `src/backend/parser/gram.y`):
//! * `LISTEN ColId`, `UNLISTEN {ColId | '*'}`, `NOTIFY ColId notify_payload`;
//! * `EXECUTE name execute_param_clause` with `name: ColId` and `'(' expr_list ')'` (non-empty);
//! * `RESET var_name` with `var_name: ColId | var_name '.' ColId`;
//! * `ALTER SCHEMA name {RENAME TO | OWNER TO}`.
//! `ColId` is an identifier, an unreserved keyword or a column-name keyword; reserved and
//! type-or-function-name keywords (`kwlist.h`) must be quoted.
use pgarbor_common::Result;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan};

use crate::diag;
use crate::source::SourceMap;
use crate::syntax_error::token_problem;

/// Reserved and type_func_name keywords of PostgreSQL 18 (`src/include/parser/kwlist.h`), lower case.
/// These are not valid unquoted `ColId`s.
const NON_COLID_KEYWORDS: &[&str] = &[
    "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "authorization",
    "binary", "both", "case", "cast", "check", "collate", "collation", "column", "concurrently",
    "constraint", "create", "cross", "current_catalog", "current_date", "current_role",
    "current_schema", "current_time", "current_timestamp", "current_user", "default", "deferrable",
    "desc", "distinct", "do", "else", "end", "except", "false", "fetch", "for", "foreign", "freeze",
    "from", "full", "grant", "group", "having", "ilike", "in", "initially", "inner", "intersect",
    "into", "is", "isnull", "join", "lateral", "leading", "left", "like", "limit", "localtime",
    "localtimestamp", "natural", "not", "notnull", "null", "offset", "on", "only", "or", "order",
    "outer", "overlaps", "placing", "primary", "references", "returning", "right", "select",
    "session_user", "similar", "some", "symmetric", "system_user", "table", "tablesample", "then",
    "to", "trailing", "true", "union", "unique", "user", "using", "variadic", "verbose", "when",
    "where", "window", "with",
];

/// Run the checks for one statement chunk (`toks` ends with `Token::EOF`).
pub fn check(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    numeric_junk(toks, src)?;
    let sig: Vec<&TokenWithSpan> = toks.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
    let word = |i: usize| -> String {
        match sig.get(i).map(|t| &t.token) {
            Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
            _ => String::new(),
        }
    };
    // gram.y `PREPARE name [(types)] AS PreparableStmt`: any other statement keyword is a syntax error at it.
    if word(0) == "PREPARE" && word(1) != "TRANSACTION" {
        let mut depth = 0usize;
        for i in 1..sig.len().saturating_sub(1) {
            match sig[i].token {
                Token::LParen => depth += 1,
                Token::RParen => depth = depth.saturating_sub(1),
                _ => {}
            }
            if depth == 0 && i >= 2 && word(i) == "AS" {
                let next = word(i + 1);
                if !next.is_empty()
                    && !matches!(next.as_str(), "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "VALUES" | "WITH" | "TABLE")
                {
                    return Err(at_token(sig[i + 1], src));
                }
                break;
            }
        }
    }
    // copy.c `defGetCopyHeaderChoice`: HEADER takes a Boolean (parse_bool spellings) or `match`.
    if word(0) == "COPY" {
        for i in 1..sig.len().saturating_sub(1) {
            if word(i) != "HEADER" || !matches!(sig[i - 1].token, Token::LParen | Token::Comma) {
                continue;
            }
            let value = match &sig[i + 1].token {
                Token::Word(w) => w.value.to_ascii_lowercase(),
                Token::SingleQuotedString(v) => v.to_ascii_lowercase(),
                Token::Number(n, _) => n.clone(),
                _ => continue,
            };
            let is_bool = !value.is_empty()
                && ["true", "false", "yes", "no", "on", "off"].iter().any(|w| w.starts_with(value.as_str()) && value != "o")
                || value == "1"
                || value == "0";
            if !is_bool && value != "match" {
                return Err(pgarbor_common::PgError::syntax("header requires a Boolean value or \"match\"", None));
            }
        }
    }
    // PostgreSQL gram.y does not have the SQL-standard window null-treatment clause.
    for i in 0..sig.len().saturating_sub(1) {
        // `SELECT select`, `SELECT where`, ...: a reserved word that cannot begin a target list.
        if word(i) == "SELECT" && matches!(word(i + 1).as_str(), "SELECT" | "WHERE" | "GROUP" | "HAVING" | "THEN" | "WHEN" | "ELSE") {
            return Err(at_token(sig[i + 1], src));
        }
    }
    for i in 1..sig.len().saturating_sub(1) {
        if matches!(sig[i - 1].token, Token::RParen)
            && matches!(word(i).as_str(), "IGNORE" | "RESPECT") && word(i + 1) == "NULLS" {
            return Err(at_token(sig[i + 1], src));
        }
        // Precision belongs to SECOND, not the other fields in an INTERVAL range.
        if word(i) == "TO" && matches!(word(i + 1).as_str(), "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE")
            && matches!(sig.get(i + 2).map(|t| &t.token), Some(Token::LParen))
            && (0..i).rev().take(4).any(|j| word(j) == "INTERVAL") {
            return Err(at_token(sig[i + 2], src));
        }
        if matches!(sig[i - 1].token, Token::LBracket) && matches!(sig[i].token, Token::RBracket)
            && matches!(sig[i + 1].token, Token::SingleQuotedString(_)) {
            return Err(at_token(sig[i], src));
        }
    }
    if word(0) == "CREATE" && matches!(word(1).as_str(), "INDEX" | "UNIQUE") {
        // At the index key-list level, operators/casts need an extra pair of parentheses.
        if let Some(on) = (1..sig.len()).find(|&i| word(i) == "ON") {
            if let Some(open) = (on + 1..sig.len()).find(|&i| matches!(sig[i].token, Token::LParen)) {
                let mut depth = 0usize;
                for t in &sig[open..] {
                    match t.token {
                        Token::LParen => depth += 1,
                        Token::RParen => { depth = depth.saturating_sub(1); if depth == 0 { break; } }
                        Token::DoubleColon | Token::StringConcat | Token::Plus | Token::Minus | Token::Mul | Token::Div
                        | Token::Mod | Token::Eq | Token::Neq | Token::Lt | Token::Gt | Token::LtEq | Token::GtEq
                        | Token::CustomBinaryOperator(_) if depth == 1 => {
                            return Err(at_token(t, src));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if word(0) == "CREATE" && word(1) == "TABLE" {
        // DEFAULT takes b_expr, which excludes a bare IN predicate (parenthesized a_expr is valid).
        let mut depth = 0usize;
        let mut default_depth = None;
        for (i, t) in sig.iter().enumerate() {
            match t.token {
                Token::LParen => depth += 1,
                Token::RParen => { if default_depth == Some(depth) { default_depth = None; } depth = depth.saturating_sub(1); }
                Token::Comma if default_depth == Some(depth) => default_depth = None,
                _ if word(i) == "DEFAULT" => default_depth = Some(depth),
                _ if word(i) == "IN" && default_depth == Some(depth) => return Err(at_token(t, src)),
                _ => {}
            }
        }
    }
    match word(0).as_str() {
        "LISTEN" | "NOTIFY" => {
            colid(&sig, 1, src)?;
        }
        "UNLISTEN" => {
            if !matches!(sig.get(1).map(|t| &t.token), Some(Token::Mul)) {
                colid(&sig, 1, src)?;
            }
        }
        "EXECUTE" => {
            let end = colid(&sig, 1, src)?;
            if matches!(sig.get(end).map(|t| &t.token), Some(Token::Period)) {
                return Err(at_token(sig[end], src));
            }
            if matches!(sig.get(end).map(|t| &t.token), Some(Token::LParen))
                && matches!(sig.get(end + 1).map(|t| &t.token), Some(Token::RParen))
            {
                return Err(at_token(sig[end + 1], src));
            }
        }
        "RESET" => {
            // `RESET TIME ZONE | TRANSACTION ISOLATION LEVEL | SESSION AUTHORIZATION` and `RESET ALL`
            // have their own rules in sqlparser; only `var_name` is checked here.
            let special = matches!(
                (word(1).as_str(), word(2).as_str()),
                ("TIME", "ZONE") | ("TRANSACTION", "ISOLATION") | ("SESSION", "AUTHORIZATION")
            );
            if !special && word(1) != "ALL" {
                let mut end = colid(&sig, 1, src)?;
                while matches!(sig.get(end).map(|t| &t.token), Some(Token::Period)) {
                    end = colid(&sig, end + 1, src)?;
                }
            }
        }
        "CREATE" if word(1) == "SCHEMA" => {
            // `CREATE SCHEMA [IF NOT EXISTS] ColId ...` or `... AUTHORIZATION RoleSpec ...`: a name that is
            // a reserved word (e.g. CURRENT_ROLE, only valid as a role_specification) is a syntax error.
            let start = if word(2) == "IF" && word(3) == "NOT" && word(4) == "EXISTS" { 5 } else { 2 };
            let at_eof = matches!(sig.get(start).map(|t| &t.token), None | Some(Token::EOF));
            if !at_eof && word(start) != "AUTHORIZATION" {
                colid(&sig, start, src)?;
            }
        }
        "COPY" => copy_clauses(&sig, src)?,
        "ALTER" if word(1) == "SCHEMA" => {
            let end = colid(&sig, 2, src)?;
            let next = word(end);
            if !matches!(next.as_str(), "RENAME" | "OWNER") {
                return Err(at_token(sig[end], src));
            }
        }
        _ => {}
    }
    Ok(())
}

/// PostgreSQL's scanner (`scan.l`: `integer_junk`, `numeric_junk`, `real_junk`, `hexfail`, `octfail`,
/// `binfail`) reads a numeric literal that runs straight into identifier characters as one bad token,
/// and rejects a bare `0x` / `0o` / `0b` prefix. sqlparser splits `123abc` into a number and a word,
/// so this runs on the raw token stream (whitespace and comments are tokens, so "directly followed by"
/// means the next token). The longest scanner match wins: a junk match always covers the whole
/// identifier run, so it beats `hexfail` / `octfail` / `binfail` except when the run is exactly the
/// prefix (`0b`, `0b_`, `0o`, `0x`), where the earlier prefix rule wins.
fn numeric_junk(toks: &[TokenWithSpan], src: &SourceMap) -> Result<()> {
    for (k, t) in toks.iter().enumerate() {
        let word = match toks.get(k + 1).map(|n| &n.token) {
            Some(Token::Word(w)) if w.quote_style.is_none() && w.value.starts_with(is_ident_start) => Some(w),
            _ => None,
        };
        let junk = |end: Location| -> pgarbor_common::PgError {
            let text = token_text(src, t.span.start, end);
            diag::syntax(format!("trailing junk after numeric literal at or near \"{text}\""), src.char_pos(t.span.start))
        };
        match &t.token {
            // `0b` / `0o` reach the tokenizer as `0` + word. A well-formed digit run (`0b101`) falls
            // through unchanged; a bare prefix (optionally with one `_`) is `binfail` / `octfail`;
            // anything else is trailing junk.
            Token::Number(n, _) if n == "0" && word.is_some_and(|w| is_radix_word(&w.value)) => {
                let w = match word {
                    Some(w) => w,
                    None => continue,
                };
                if radix_digits_ok(&w.value[1..], radix_of(&w.value)) {
                    continue;
                }
                if matches!(w.value.as_str(), "b" | "B" | "o" | "O" | "b_" | "B_" | "o_" | "O_") {
                    let what = if radix_of(&w.value) == 2 { "binary" } else { "octal" };
                    let text = token_text(src, t.span.start, toks[k + 1].span.end);
                    return Err(diag::syntax(format!("invalid {what} integer at or near \"{text}\""), src.char_pos(t.span.start)));
                }
                return Err(junk(toks[k + 1].span.end));
            }
            // `123L`: the tokenizer keeps the `L` suffix on the number, which PostgreSQL scans as junk.
            Token::Number(_, true) => return Err(junk(t.span.end)),
            Token::Number(..) if word.is_some() => return Err(junk(toks[k + 1].span.end)),
            Token::HexStringLiteral(digits) if is_hex_prefix(src, &t.span) => {
                if word.is_some() {
                    return Err(junk(toks[k + 1].span.end));
                }
                if digits.is_empty() {
                    let text = token_text(src, t.span.start, t.span.end);
                    return Err(diag::syntax(format!("invalid hexadecimal integer at or near \"{text}\""), src.char_pos(t.span.start)));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Identifier start characters of PostgreSQL's scanner (`ident_start`).
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

/// `b` / `o` (either case) followed by the rest of an identifier run: a candidate radix literal.
fn is_radix_word(value: &str) -> bool {
    matches!(value.as_bytes().first(), Some(b'b' | b'B' | b'o' | b'O'))
}

/// Radix of a `is_radix_word` value.
fn radix_of(value: &str) -> u32 {
    if matches!(value.as_bytes().first(), Some(b'b' | b'B')) { 2 } else { 8 }
}

/// Digits of a `0b` / `0o` literal: `(_?digit)+` in `scan.l`, so underscores only sit between digits.
fn radix_digits_ok(body: &str, radix: u32) -> bool {
    let mut digits = 0usize;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            match chars.peek() {
                Some(n) if n.is_digit(radix) => continue,
                _ => return false,
            }
        }
        if !c.is_digit(radix) {
            return false;
        }
        digits += 1;
    }
    digits > 0
}

/// Is the token a `0x` / `0X` hexadecimal integer (rather than a bit string `X'..'`)?
fn is_hex_prefix(src: &SourceMap, span: &sqlparser::tokenizer::Span) -> bool {
    let raw = token_text(src, span.start, span.end);
    raw.len() >= 2 && raw.as_bytes()[0] == b'0' && matches!(raw.as_bytes()[1], b'x' | b'X')
}

/// Source text between two locations (exclusive end, as `syntax_error::token_source`).
fn token_text(src: &SourceMap, start: Location, end: Location) -> &str {
    match (src.byte_offset(start), src.byte_offset(end)) {
        (Some(s), Some(e)) => src.slice(s, e),
        _ => "",
    }
}

/// Check a `ColId` at `sig[i]` and any `'.' ColId` continuation after it when `dotted` is set by the
/// caller; returns the index of the first token after the name. EOF and non-identifiers are errors.
fn colid(sig: &[&TokenWithSpan], i: usize, src: &SourceMap) -> Result<usize> {
    let t = match sig.get(i) {
        Some(t) => *t,
        None => return Err(diag::syntax_error_eof(src.end_pos())),
    };
    match &t.token {
        Token::EOF => return Err(diag::syntax_error_eof(src.end_pos())),
        Token::Word(w) if w.quote_style == Some('"') && !w.value.is_empty() => {}
        Token::Word(w) if w.quote_style == Some('"') => return Err(at_token(t, src)),
        Token::Word(w) if w.quote_style.is_none() => {
            let lower = w.value.to_ascii_lowercase();
            if NON_COLID_KEYWORDS.contains(&lower.as_str()) {
                return Err(at_token(t, src));
            }
        }
        _ => return Err(at_token(t, src)),
    }
    Ok(i + 1)
}

/// Option names `ProcessCopyOptions` (copy.c) accepts in PostgreSQL 18.
const COPY_OPTION_NAMES: &[&str] = &[
    "format", "freeze", "delimiter", "null", "default", "header", "quote", "escape", "force_quote",
    "force_not_null", "force_null", "on_error", "reject_limit", "encoding", "log_verbosity",
];

/// `COPY` grammar checks that `sqlparser` cannot make: `COPY (query)` only has a TO form (gram.y
/// `COPY '(' PreparableStmt ')' TO ...`), `WHERE` is rejected by the grammar action for COPY TO, and
/// `ProcessCopyOptions` rejects an unknown `WITH (...)` option name at that name's position.
fn copy_clauses(sig: &[&TokenWithSpan], src: &SourceMap) -> Result<()> {
    let kw = |i: usize, s: &str| {
        matches!(sig.get(i).map(|t| &t.token), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(s))
    };
    let is = |i: usize, f: fn(&Token) -> bool| sig.get(i).is_some_and(|t| f(&t.token));
    let close_of = |open: usize| -> Option<usize> {
        let mut depth = 0usize;
        for (j, t) in sig.iter().enumerate().skip(open) {
            match t.token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    };
    let mut i = 1;
    if is(1, |t| matches!(t, Token::LParen)) {
        let Some(close) = close_of(1) else { return Ok(()) };
        if !kw(close + 1, "TO") {
            return match sig.get(close + 1) {
                Some(t) => Err(at_token(t, src)),
                None => Ok(()),
            };
        }
        i = close + 1;
    } else {
        // Table form: skip the optional column list, then find the direction keyword.
        let mut depth = 0usize;
        let mut found = None;
        for (j, t) in sig.iter().enumerate().skip(1) {
            match t.token {
                Token::LParen => depth += 1,
                Token::RParen => depth = depth.saturating_sub(1),
                _ if depth == 0 && (kw(j, "FROM") || kw(j, "TO")) => {
                    found = Some(j);
                    break;
                }
                _ => {}
            }
        }
        match found {
            Some(j) => i = j,
            None => return Ok(()),
        }
    }
    let is_to = kw(i, "TO");
    let mut k = i + 1;
    k += if kw(k, "PROGRAM") { 2 } else { 1 };
    if kw(k, "WITH") {
        k += 1;
    }
    if is_to {
        let mut depth = 0usize;
        for (j, t) in sig.iter().enumerate().skip(k) {
            match t.token {
                Token::LParen => depth += 1,
                Token::RParen => depth = depth.saturating_sub(1),
                _ if depth == 0 && kw(j, "WHERE") => {
                    return Err(diag::syntax("WHERE clause not allowed with COPY TO", src.char_pos(t.span.start)));
                }
                _ => {}
            }
        }
    }
    if is(k, |t| matches!(t, Token::LParen)) {
        let Some(close) = close_of(k) else { return Ok(()) };
        let mut p = k + 1;
        while p < close {
            if let Token::Word(w) = &sig[p].token {
                let known = if w.quote_style.is_some() {
                    COPY_OPTION_NAMES.contains(&w.value.as_str())
                } else {
                    COPY_OPTION_NAMES.contains(&w.value.to_ascii_lowercase().as_str())
                };
                if !known {
                    return Err(diag::syntax(
                        format!("option \"{}\" not recognized", w.value),
                        src.char_pos(sig[p].span.start),
                    ));
                }
            }
            // Skip the value up to the next top-level comma.
            let mut depth = 0usize;
            while p < close {
                match sig[p].token {
                    Token::LParen => depth += 1,
                    Token::RParen => depth = depth.saturating_sub(1),
                    Token::Comma if depth == 0 => break,
                    _ => {}
                }
                p += 1;
            }
            p += 1;
        }
    }
    Ok(())
}

fn at_token(t: &TokenWithSpan, src: &SourceMap) -> pgarbor_common::PgError {
    token_problem(t, src)
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;

    /// PG error text and position for input that must be rejected.
    fn reject(sql: &str) -> (String, Option<u32>) {
        let e = parse(sql).expect_err(sql);
        assert_eq!(e.sqlstate.code(), "42601", "{sql}");
        (e.message.clone(), e.position)
    }

    #[test]
    fn unquoted_select_as_target_is_syntax_error() {
        assert_eq!(reject("SELECT select FROM t"), ("syntax error at or near \"select\"".into(), Some(8)));
        assert!(parse("SELECT \"select\" FROM t").is_ok());
    }

    #[test]
    fn copy_header_requires_boolean_or_match() {
        let (msg, pos) = reject("COPY t FROM STDIN WITH (header wrong_choice)");
        assert_eq!((msg.as_str(), pos), ("header requires a Boolean value or \"match\"", None));
        assert!(parse("COPY t FROM STDIN WITH (format csv, header on)").is_ok());
    }

    #[test]
    fn copy_grammar_errors_match_postgres() {
        assert_eq!(reject("COPY (SELECT 1 AS a) FROM STDIN"), ("syntax error at or near \"FROM\"".into(), Some(22)));
        assert_eq!(
            reject("COPY t TO STDOUT WHERE a > 0"),
            ("WHERE clause not allowed with COPY TO".into(), Some(18))
        );
        assert_eq!(
            reject("COPY t FROM STDIN WITH (nosuch true)"),
            ("option \"nosuch\" not recognized".into(), Some(25))
        );
        assert!(parse("COPY t FROM STDIN WITH (format csv, header true)").is_ok());
    }

    #[test]
    fn prepare_rejects_non_preparable_statement() {
        assert_eq!(reject("PREPARE p(text) AS NOTIFY $1"), ("syntax error at or near \"NOTIFY\"".into(), Some(20)));
        assert!(parse("PREPARE p AS SELECT 1").is_ok());
    }

    #[test]
    fn reserved_keyword_channel_is_syntax_error() {
        assert_eq!(reject("LISTEN select"), ("syntax error at or near \"select\"".into(), Some(8)));
        assert_eq!(reject("NOTIFY select"), ("syntax error at or near \"select\"".into(), Some(8)));
        assert_eq!(reject("UNLISTEN select"), ("syntax error at or near \"select\"".into(), Some(10)));
        assert!(parse("LISTEN \"select\"").is_ok());
        assert!(parse("UNLISTEN \"select\"").is_ok());
        assert!(parse("LISTEN virtual").is_ok());
    }

    #[test]
    fn literal_and_empty_channels_are_syntax_errors() {
        assert_eq!(reject("LISTEN 'ch'"), ("syntax error at or near \"'ch'\"".into(), Some(8)));
        assert_eq!(reject("UNLISTEN 'keep_me'"), ("syntax error at or near \"'keep_me'\"".into(), Some(10)));
        assert_eq!(reject("NOTIFY \"\""), ("zero-length delimited identifier at or near \"\"\"\"".into(), Some(8)));
        assert!(parse("UNLISTEN *").is_ok());
    }

    #[test]
    fn execute_empty_parens_is_syntax_error_at_close_paren() {
        assert_eq!(reject("EXECUTE p()"), ("syntax error at or near \")\"".into(), Some(11)));
        assert!(parse("EXECUTE p").is_ok());
        assert!(parse("EXECUTE p(1)").is_ok());
    }

    #[test]
    fn reset_names_are_checked_as_var_name() {
        assert_eq!(reject("RESET 'timezone'"), ("syntax error at or near \"'timezone'\"".into(), Some(7)));
        assert_eq!(reject("RESET extra_float_digits, statement_timeout"), ("syntax error at or near \",\"".into(), Some(25)));
        assert!(parse("RESET ALL").is_ok());
        assert!(parse("RESET search_path").is_ok());
    }

    #[test]
    fn postgres_expression_grammar_boundaries() {
        reject("SELECT lag(x) IGNORE NULLS OVER () FROM t");
        reject("SELECT lag(x) RESPECT NULLS OVER () FROM t");
        reject("SELECT CAST('1' AS interval DAY TO MINUTE(2))");
        reject("SELECT int4[] '{1}'");
        reject("CREATE INDEX ON t (a || b)");
        reject("CREATE INDEX ON t (a::text)");
        reject("CREATE TABLE t(b bool DEFAULT 1 IN (1,2))");
        for sql in ["SELECT lag(x) OVER () FROM t", "SELECT CAST('1' AS interval DAY TO SECOND(2))", "CREATE INDEX ON t ((a || b))", "CREATE INDEX ON t (lower(a))", "CREATE TABLE t(b bool DEFAULT (1 IN (1,2)))"] {
            assert!(parse(sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn alter_schema_if_exists_is_syntax_error() {
        assert_eq!(reject("ALTER SCHEMA IF EXISTS s RENAME TO t"), ("syntax error at or near \"EXISTS\"".into(), Some(17)));
        assert!(parse("ALTER SCHEMA s RENAME TO t").is_ok());
    }

    #[test]
    fn create_schema_name_must_be_colid() {
        assert_eq!(reject("CREATE SCHEMA CURRENT_ROLE"), ("syntax error at or near \"CURRENT_ROLE\"".into(), Some(15)));
        assert!(parse("CREATE SCHEMA s").is_ok());
        assert!(parse("CREATE SCHEMA IF NOT EXISTS s").is_ok());
        assert!(parse("CREATE SCHEMA AUTHORIZATION CURRENT_ROLE").is_ok());
    }

    #[test]
    fn numeric_literal_trailing_junk_is_syntax_error() {
        assert_eq!(reject("SELECT 123abc"), ("trailing junk after numeric literal at or near \"123abc\"".into(), Some(8)));
        assert_eq!(reject("SELECT 1b"), ("trailing junk after numeric literal at or near \"1b\"".into(), Some(8)));
        assert_eq!(reject("SELECT 0b0x"), ("trailing junk after numeric literal at or near \"0b0x\"".into(), Some(8)));
        assert_eq!(reject("SELECT 0x0y"), ("trailing junk after numeric literal at or near \"0x0y\"".into(), Some(8)));
        assert_eq!(reject("SELECT 0b"), ("invalid binary integer at or near \"0b\"".into(), Some(8)));
        assert_eq!(reject("SELECT 0o"), ("invalid octal integer at or near \"0o\"".into(), Some(8)));
        assert_eq!(reject("SELECT 0x"), ("invalid hexadecimal integer at or near \"0x\"".into(), Some(8)));
        assert!(parse("SELECT 1 AS abc, 1.5e3, 0 AS b, 1 abc").is_ok());
        assert!(parse("SELECT X'1F', 0x1F").is_ok());
    }
}
