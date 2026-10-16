//! Column lists of `ON DELETE SET NULL (cols)` and `ON DELETE SET DEFAULT (cols)` (PG 15+, gram.y
//! `key_action` with `opt_column_list`).
//!
//! sqlparser 0.63 `ReferentialAction` carries no column list, so before sqlparser sees a chunk, [`extract`]
//! removes each such parenthesized list from the token stream and returns one [`FkSetGroup`] per
//! `ON DELETE SET NULL|SET DEFAULT` occurrence, in source order. An occurrence without a list still yields
//! a group (with empty `columns`). The group names the referenced table of the nearest preceding
//! `REFERENCES`, so the binder can match each group to the foreign key it belongs to
//! (`Binder::fk_set_groups`, see `binder::ddl_constraint`).

use sqlparser::tokenizer::{Token, TokenWithSpan};

/// One `ON DELETE SET NULL|SET DEFAULT` occurrence of a statement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FkSetGroup {
    /// Last part of the referenced table name: lower-cased when unquoted, as written when quoted.
    pub ref_table: String,
    /// `true` for `SET DEFAULT`, `false` for `SET NULL`.
    pub set_default: bool,
    /// The column list, in written order; empty when the action has no list.
    pub columns: Vec<String>,
}

fn tok<'a>(toks: &'a [TokenWithSpan], sig: &[usize], k: usize) -> Option<&'a Token> {
    sig.get(k).map(|&i| &toks[i].token)
}

fn is_word(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word))
}

/// Normalized text of an identifier token (`None` for any other token).
fn ident_text(t: Option<&Token>) -> Option<String> {
    match t {
        Some(Token::Word(w)) => Some(if w.quote_style.is_none() { w.value.to_lowercase() } else { w.value.clone() }),
        _ => None,
    }
}

/// Last part of the (possibly qualified) name starting at significant token `k`.
fn name_last(toks: &[TokenWithSpan], sig: &[usize], mut k: usize) -> Option<String> {
    let mut last = ident_text(tok(toks, sig, k))?;
    k += 1;
    while matches!(tok(toks, sig, k), Some(Token::Period)) {
        last = ident_text(tok(toks, sig, k + 1))?;
        k += 2;
    }
    Some(last)
}

/// A parenthesized list of plain identifiers starting at significant token `open`: the names and the index
/// of the closing parenthesis. `None` when the tokens are not such a list (sqlparser then reports the error).
fn column_list(toks: &[TokenWithSpan], sig: &[usize], open: usize) -> Option<(Vec<String>, usize)> {
    if !matches!(tok(toks, sig, open), Some(Token::LParen)) {
        return None;
    }
    let mut names = Vec::new();
    let mut k = open + 1;
    loop {
        names.push(ident_text(tok(toks, sig, k))?);
        k += 1;
        match tok(toks, sig, k) {
            Some(Token::Comma) => k += 1,
            Some(Token::RParen) => return Some((names, k)),
            _ => return None,
        }
    }
}

/// Remove the `ON DELETE SET NULL|SET DEFAULT (cols)` lists from `toks` and return the reduced tokens with
/// one group per `ON DELETE SET NULL|SET DEFAULT` occurrence, in source order.
pub fn extract(toks: &[TokenWithSpan]) -> (Vec<TokenWithSpan>, Vec<FkSetGroup>) {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let mut remove = vec![false; toks.len()];
    let mut groups = Vec::new();
    let mut ref_table = String::new();
    for k in 0..sig.len() {
        if is_word(tok(toks, &sig, k), "REFERENCES") {
            if let Some(name) = name_last(toks, &sig, k + 1) {
                ref_table = name;
            }
            continue;
        }
        if !(is_word(tok(toks, &sig, k), "ON")
            && is_word(tok(toks, &sig, k + 1), "DELETE")
            && is_word(tok(toks, &sig, k + 2), "SET"))
        {
            continue;
        }
        let set_default = if is_word(tok(toks, &sig, k + 3), "NULL") {
            false
        } else if is_word(tok(toks, &sig, k + 3), "DEFAULT") {
            true
        } else {
            continue;
        };
        let mut group = FkSetGroup { ref_table: ref_table.clone(), set_default, columns: Vec::new() };
        if let Some((columns, close)) = column_list(toks, &sig, k + 4) {
            group.columns = columns;
            for r in remove[sig[k + 4]..=sig[close]].iter_mut() {
                *r = true;
            }
        }
        groups.push(group);
    }
    let out = toks
        .iter()
        .enumerate()
        .filter(|(i, _)| !remove[*i])
        .map(|(_, t)| t.clone())
        .collect();
    (out, groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::tokenizer::Tokenizer;

    fn lex(sql: &str) -> Vec<TokenWithSpan> {
        Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().expect("test SQL tokenizes")
    }

    #[test]
    fn set_null_column_list_is_removed_and_recorded() {
        let (out, groups) = extract(&lex(
            "CREATE TABLE posts (FOREIGN KEY (t, a) REFERENCES users ON DELETE SET NULL (zz), x int)",
        ));
        assert_eq!(
            groups,
            vec![FkSetGroup { ref_table: "users".into(), set_default: false, columns: vec!["zz".into()] }]
        );
        assert!(!out.iter().any(|t| matches!(&t.token, Token::Word(w) if w.value == "zz")), "list removed");
        assert!(out.iter().any(|t| matches!(&t.token, Token::Word(w) if w.value == "x")), "rest kept");
    }

    #[test]
    fn set_default_without_list_still_yields_a_group() {
        let (out, groups) = extract(&lex("CREATE TABLE c (a int REFERENCES public.p ON DELETE SET DEFAULT)"));
        assert_eq!(groups, vec![FkSetGroup { ref_table: "p".into(), set_default: true, columns: Vec::new() }]);
        assert_eq!(out.len(), lex("CREATE TABLE c (a int REFERENCES public.p ON DELETE SET DEFAULT)").len());
    }

    #[test]
    fn malformed_list_is_left_for_sqlparser() {
        let (out, groups) = extract(&lex("CREATE TABLE c (a int REFERENCES p ON DELETE SET NULL (1))"));
        assert_eq!(groups.len(), 1);
        assert!(groups[0].columns.is_empty());
        assert!(out.iter().any(|t| matches!(t.token, Token::LParen)));
    }
}
