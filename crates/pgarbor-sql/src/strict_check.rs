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
use sqlparser::tokenizer::{Token, TokenWithSpan};

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
    let sig: Vec<&TokenWithSpan> = toks.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
    let word = |i: usize| -> String {
        match sig.get(i).map(|t| &t.token) {
            Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
            _ => String::new(),
        }
    };
    // PostgreSQL gram.y does not have the SQL-standard window null-treatment clause.
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
                        | Token::Mod | Token::Eq | Token::Neq | Token::Lt | Token::Gt | Token::LtEq | Token::GtEq if depth == 1 => {
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
}
