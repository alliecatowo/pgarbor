//! Token rewrites for table hierarchies (amendment pgarbor-sql-05), applied before sqlparser:
//!
//! * `ONLY name` / `ONLY (name)` in a FROM list, after `JOIN`, as the UPDATE / DELETE target or in
//!   `DELETE ... USING`: sqlparser 0.63 reads `ONLY` as a table name. The keyword is removed and the
//!   name gets an extra leading qualifier [`ONLY_MARKER`] (a quoted identifier holding a NUL, which
//!   PostgreSQL identifiers cannot contain). `names::qual_name` drops the marker; the binder asks
//!   [`crate::names::is_only_marked`] to learn that inheritance children are excluded.
//! * `name *` in the same positions (explicit "include descendants", the default): the `*` is removed.
//! * `CREATE TABLE ... PARTITION OF parent (col [WITH OPTIONS] options, ...)`: partition column
//!   entries have no type; [`PARTITION_COLUMN_MARKER`] is inserted as a placeholder type name, which
//!   the CREATE TABLE binder recognizes.
use sqlparser::tokenizer::{Token, TokenWithSpan, Word};

/// Leading qualifier marking a relation written with `ONLY`.
pub const ONLY_MARKER: &str = "\0pgarbor only";
/// Placeholder type of a `PARTITION OF` column entry.
pub const PARTITION_COLUMN_MARKER: &str = "\0pgarbor partition column";

fn bare(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    }
}

fn marker_word(value: &str) -> Token {
    Token::Word(Word { value: value.to_string(), quote_style: Some('"'), keyword: sqlparser::keywords::Keyword::NoKeyword })
}

/// Words that cannot start a relation name after `ONLY` in a select list (`SELECT a, only FROM t`).
const NOT_A_NAME: &[&str] = &[
    "FROM", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "UNION", "INTERSECT", "EXCEPT", "AS", "AND", "OR",
    "IS", "IN", "NOT", "LIKE", "BETWEEN", "FETCH", "FOR", "WINDOW", "INTO", "RETURNING",
];

/// Clause keywords tracked per parenthesis depth to know whether a `,` continues a FROM list.
const CLAUSES: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "WINDOW", "UNION", "INTERSECT", "EXCEPT",
    "RETURNING", "SET", "VALUES", "ON", "USING", "JOIN", "UPDATE", "INTO", "DELETE", "FETCH", "FOR",
];

fn from_like(clause: Option<&str>) -> bool {
    matches!(clause, Some("FROM" | "JOIN" | "ON" | "USING"))
}

/// `ONLY` and `name *` rewrites (module docs).
pub fn mark_only_relations(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let has_only = toks.iter().any(|t| bare(&t.token).as_deref() == Some("ONLY"));
    let has_star = toks.iter().any(|t| matches!(t.token, Token::Mul));
    if !has_only && !has_star {
        return toks;
    }
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    // Statement kinds where sqlparser already understands ONLY (ALTER TABLE, TRUNCATE, LOCK).
    let first = sig.first().and_then(|&i| bare(&toks[i].token));
    if matches!(first.as_deref(), Some("ALTER" | "TRUNCATE" | "LOCK")) {
        return toks;
    }
    let mut remove = vec![false; toks.len()];
    // token index -> tokens to insert before it
    let mut insert_before: Vec<(usize, Vec<TokenWithSpan>)> = Vec::new();
    let mut clause: Vec<Option<String>> = vec![None];
    let mut k = 0usize;
    while k < sig.len() {
        let i = sig[k];
        let t = &toks[i].token;
        match t {
            Token::LParen => clause.push(None),
            Token::RParen => {
                if clause.len() > 1 {
                    clause.pop();
                }
            }
            _ => {}
        }
        let w = bare(t);
        let prev = if k > 0 { Some(&toks[sig[k - 1]].token) } else { None };
        let prev_word = prev.and_then(bare);
        let cur_clause = clause.last().cloned().flatten();
        let relation_position = |prev_word: &Option<String>, prev: Option<&Token>| -> bool {
            matches!(prev_word.as_deref(), Some("FROM" | "JOIN" | "UPDATE" | "USING"))
                || (matches!(prev, Some(Token::Comma)) && from_like(cur_clause.as_deref()))
        };
        if w.as_deref() == Some("ONLY") && relation_position(&prev_word, prev) {
            // Name start: a word, or `(` name `)`.
            let next = sig.get(k + 1).copied();
            let (name_start, paren) = match next.map(|n| &toks[n].token) {
                Some(Token::Word(nw)) => {
                    let upper = nw.value.to_ascii_uppercase();
                    if nw.quote_style.is_none() && NOT_A_NAME.contains(&upper.as_str()) {
                        k += 1;
                        continue;
                    }
                    (k + 1, None)
                }
                Some(Token::LParen) => match sig.get(k + 2).map(|&n| &toks[n].token) {
                    Some(Token::Word(_)) => (k + 2, Some(k + 1)),
                    _ => {
                        k += 1;
                        continue;
                    }
                },
                _ => {
                    k += 1;
                    continue;
                }
            };
            // End of the qualified name.
            let mut end = name_start;
            while matches!(sig.get(end + 1).map(|&n| &toks[n].token), Some(Token::Period))
                && matches!(sig.get(end + 2).map(|&n| &toks[n].token), Some(Token::Word(_)))
            {
                end += 2;
            }
            if let Some(p) = paren {
                if !matches!(sig.get(end + 1).map(|&n| &toks[n].token), Some(Token::RParen)) {
                    k += 1;
                    continue;
                }
                remove[sig[p]] = true;
                remove[sig[end + 1]] = true;
            }
            remove[i] = true;
            let span = toks[sig[name_start]].span;
            insert_before.push((
                sig[name_start],
                vec![TokenWithSpan::new(marker_word(ONLY_MARKER), span), TokenWithSpan::new(Token::Period, span)],
            ));
            k = end + 1 + usize::from(paren.is_some());
            continue;
        }
        if matches!(t, Token::Mul) && k >= 2 {
            // `name *`: walk back over the qualified name.
            let mut s = k - 1;
            if matches!(toks[sig[s]].token, Token::Word(_)) {
                while s >= 2
                    && matches!(toks[sig[s - 1]].token, Token::Period)
                    && matches!(toks[sig[s - 2]].token, Token::Word(_))
                {
                    s -= 2;
                }
                let before = if s > 0 { Some(&toks[sig[s - 1]].token) } else { None };
                let before_word = before.and_then(bare);
                let after = sig.get(k + 1).map(|&n| &toks[n].token);
                let after_ok = match after {
                    None | Some(Token::EOF) | Some(Token::Comma) | Some(Token::RParen) | Some(Token::SemiColon) => true,
                    Some(Token::Word(_)) => true,
                    _ => false,
                };
                if after_ok && relation_position(&before_word, before) {
                    remove[i] = true;
                }
            }
        }
        if let Some(word) = &w {
            if CLAUSES.contains(&word.as_str()) {
                if let Some(c) = clause.last_mut() {
                    *c = Some(word.clone());
                }
            }
        }
        k += 1;
    }
    if !remove.iter().any(|r| *r) && insert_before.is_empty() {
        return toks;
    }
    let mut out = Vec::with_capacity(toks.len() + insert_before.len() * 2);
    for (i, t) in toks.into_iter().enumerate() {
        if let Some((_, ins)) = insert_before.iter().find(|(at, _)| *at == i) {
            out.extend(ins.iter().cloned());
        }
        if !remove[i] {
            out.push(t);
        }
    }
    out
}

/// `PARTITION OF parent ( ... )` column entries get a placeholder type (module docs).
pub fn partition_column_types(toks: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let word = |k: usize| sig.get(k).and_then(|&i| bare(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return toks;
    }
    let Some(of) = (1..sig.len().saturating_sub(1)).find(|&k| word(k).as_deref() == Some("PARTITION") && word(k + 1).as_deref() == Some("OF"))
    else {
        return toks;
    };
    // Skip the qualified parent name.
    let mut k = of + 2;
    if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::Word(_))) {
        return toks;
    }
    while matches!(sig.get(k + 1).map(|&i| &toks[i].token), Some(Token::Period)) {
        k += 2;
    }
    k += 1;
    if !matches!(sig.get(k).map(|&i| &toks[i].token), Some(Token::LParen)) {
        return toks;
    }
    let mut remove = vec![false; toks.len()];
    let mut inserts: Vec<(usize, TokenWithSpan)> = Vec::new();
    let mut depth = 0usize;
    let mut element_start = true;
    let mut m = k;
    while m < sig.len() {
        let i = sig[m];
        match &toks[i].token {
            Token::LParen => {
                depth += 1;
                if depth == 1 {
                    element_start = true;
                    m += 1;
                    continue;
                }
            }
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Token::Comma if depth == 1 => {
                element_start = true;
                m += 1;
                continue;
            }
            _ => {}
        }
        if element_start && depth == 1 {
            element_start = false;
            let is_constraint = matches!(
                word(m).as_deref(),
                Some("CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "EXCLUDE" | "LIKE")
            );
            if !is_constraint && matches!(toks[i].token, Token::Word(_)) {
                let mut after = m + 1;
                if word(after).as_deref() == Some("WITH") && word(after + 1).as_deref() == Some("OPTIONS") {
                    remove[sig[after]] = true;
                    remove[sig[after + 1]] = true;
                    after += 2;
                }
                let at = sig.get(after).copied().unwrap_or(toks.len());
                inserts.push((at, TokenWithSpan::new(marker_word(PARTITION_COLUMN_MARKER), toks[i].span)));
            }
        }
        m += 1;
    }
    if inserts.is_empty() {
        return toks;
    }
    let mut out = Vec::with_capacity(toks.len() + inserts.len() * 2);
    let n = toks.len();
    for (i, t) in toks.into_iter().enumerate() {
        for (at, ins) in &inserts {
            if *at == i {
                out.push(TokenWithSpan::new(Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), ins.span));
                out.push(ins.clone());
                out.push(TokenWithSpan::new(Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), ins.span));
            }
        }
        if !remove[i] {
            out.push(t);
        }
    }
    let _ = n;
    out
}

#[cfg(test)]
mod tests {
    use crate::parser::{parse, Parsed};

    fn text(sql: &str) -> String {
        match &parse(sql).expect("parse")[0].parsed {
            Parsed::Ast(s) => s.to_string(),
            Parsed::Extra(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn only_becomes_marker() {
        let t = text("SELECT * FROM ONLY a JOIN ONLY (s.b) ON true, ONLY c");
        assert_eq!(t.matches("pgarbor only").count(), 3, "{t}");
        assert!(!text("SELECT * FROM a*").contains('*') || text("SELECT * FROM a*").matches('*').count() == 1);
        let t = text("UPDATE ONLY t SET a = 1");
        assert!(t.contains("pgarbor only"), "{t}");
        let t = text("DELETE FROM ONLY t");
        assert!(t.contains("pgarbor only"), "{t}");
        assert!(!text("SELECT a, only FROM t").contains("pgarbor only"));
    }

    #[test]
    fn partition_columns_get_marker_type() {
        let t = text("CREATE TABLE p1 PARTITION OF p (a WITH OPTIONS NOT NULL, CONSTRAINT c CHECK (a > 0)) FOR VALUES IN (1)");
        assert!(t.contains("pgarbor partition column"), "{t}");
    }
}
