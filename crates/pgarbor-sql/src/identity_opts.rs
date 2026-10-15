//! Identity column option lists: `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [( seq_opt_list )]`.
//!
//! PG 18 `gram.y` accepts a `SeqOptList` in any order inside the parentheses, and that list includes
//! `SEQUENCE NAME any_name` (`SeqOptElem`). The sqlparser 0.63 adapter cannot parse either: its
//! identity option parser is fixed-order and has no sequence name. So before sqlparser sees a chunk,
//! [`extract`] removes every identity option list from the token stream, parses it here, and returns
//! one [`IdentityGroup`] per `AS IDENTITY` occurrence, in source order. The binder consumes the groups
//! in the same order as the identity columns it binds (`Binder::identity_groups`).
//!
//! An `AS IDENTITY` without a parenthesized list still yields an (empty) group, so the counts match.

use sqlparser::ast::{ObjectNamePart, SequenceOptions as AstSeqOpt};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan};
use sqlparser::tokenizer::Span;

use crate::source::SourceMap;
use crate::syntax_error;
use pgarbor_common::Result;

/// The parsed identity option list of one `AS IDENTITY` occurrence.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdentityGroup {
    /// `SEQUENCE NAME <name>` when the name is a single (possibly quoted) identifier.
    pub sequence_name: Option<String>,
    /// `SEQUENCE NAME` was given a qualified name (`schema.name`). Not implemented yet.
    pub sequence_name_qualified: bool,
    /// The other options, in written order (`sequence.rs`-style: `IncrementBy(expr, has_by)`, ...).
    pub options: Vec<AstSeqOpt>,
}

fn is_word(t: &Token, word: &str) -> bool {
    matches!(t, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word))
}

/// Remove the identity option lists from `toks` and parse them.
///
/// Returns the reduced token vector (the identity parentheses are gone, so sqlparser reads a plain
/// `GENERATED ... AS IDENTITY`) and the groups in source order. Syntax errors inside a list are
/// translated with the same rules as the statement parser.
pub fn extract(toks: &[TokenWithSpan], src: &SourceMap) -> Result<(Vec<TokenWithSpan>, Vec<IdentityGroup>)> {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_))).collect();
    let n = sig.len();
    let t = |k: usize| &toks[sig[k]].token;
    let mut remove = vec![false; toks.len()];
    let mut groups = Vec::new();
    let mut k = 0;
    while k < n {
        if !is_word(t(k), "GENERATED") {
            k += 1;
            continue;
        }
        let mut j = k + 1;
        if j < n && is_word(t(j), "ALWAYS") {
            j += 1;
        } else if j + 1 < n && is_word(t(j), "BY") && is_word(t(j + 1), "DEFAULT") {
            j += 2;
        } else {
            k += 1;
            continue;
        }
        if !(j + 1 < n && is_word(t(j), "AS") && is_word(t(j + 1), "IDENTITY")) {
            k += 1;
            continue;
        }
        let after = j + 2;
        if after >= n || !matches!(t(after), Token::LParen) {
            groups.push(IdentityGroup::default());
            k = after;
            continue;
        }
        // Matching right parenthesis.
        let mut depth = 0usize;
        let mut end = None;
        for m in after..n {
            match t(m) {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(m);
                        break;
                    }
                }
                _ => {}
            }
        }
        // An unbalanced list is left for sqlparser to report.
        let Some(end) = end else {
            k += 1;
            continue;
        };
        let mut inner: Vec<TokenWithSpan> = (after + 1..end).map(|m| toks[sig[m]].clone()).collect();
        let eof_loc = toks[sig[end]].span.start;
        inner.push(TokenWithSpan::new(Token::EOF, Span::new(eof_loc, eof_loc)));
        let dialect = PostgreSqlDialect {};
        let mut p = Parser::new(&dialect).with_tokens_with_locations(inner.clone());
        match parse_group(&mut p) {
            Ok(g) => groups.push(g),
            Err(e) => return Err(syntax_error::translate_parser_error(&e, src, &inner)?),
        }
        for m in after..=end {
            remove[sig[m]] = true;
        }
        k = end + 1;
    }
    let out = toks.iter().enumerate().filter(|(i, _)| !remove[*i]).map(|(_, t)| t.clone()).collect();
    Ok((out, groups))
}

/// PG `SeqOptList` (any order) up to the end of the list.
fn parse_group(p: &mut Parser) -> std::result::Result<IdentityGroup, ParserError> {
    let mut g = IdentityGroup::default();
    while !matches!(p.peek_token().token, Token::EOF) {
        if is_word(&p.peek_token().token, "SEQUENCE") {
            p.next_token();
            if !is_word(&p.peek_token().token, "NAME") {
                return p.expected("NAME", p.peek_token());
            }
            p.next_token();
            let name = p.parse_object_name(false)?;
            match name.0.as_slice() {
                [ObjectNamePart::Identifier(id)] => g.sequence_name = Some(id.value.clone()),
                _ => g.sequence_name_qualified = true,
            }
        } else if is_word(&p.peek_token().token, "INCREMENT") {
            p.next_token();
            let by = take_word(p, "BY");
            let v = p.parse_number()?;
            g.options.push(AstSeqOpt::IncrementBy(v, by));
        } else if take_word(p, "MINVALUE") {
            let v = p.parse_number()?;
            g.options.push(AstSeqOpt::MinValue(Some(v)));
        } else if take_word(p, "MAXVALUE") {
            let v = p.parse_number()?;
            g.options.push(AstSeqOpt::MaxValue(Some(v)));
        } else if is_word(&p.peek_token().token, "NO") {
            let next = p.peek_nth_token(1).token;
            let opt = if is_word(&next, "MINVALUE") {
                AstSeqOpt::MinValue(None)
            } else if is_word(&next, "MAXVALUE") {
                AstSeqOpt::MaxValue(None)
            } else if is_word(&next, "CYCLE") {
                AstSeqOpt::Cycle(false)
            } else {
                return p.expected("sequence option", p.peek_token());
            };
            p.next_token();
            p.next_token();
            g.options.push(opt);
        } else if take_word(p, "START") {
            let with = take_word(p, "WITH");
            let v = p.parse_number()?;
            g.options.push(AstSeqOpt::StartWith(v, with));
        } else if take_word(p, "CACHE") {
            let v = p.parse_number()?;
            g.options.push(AstSeqOpt::Cache(v));
        } else if take_word(p, "CYCLE") {
            g.options.push(AstSeqOpt::Cycle(true));
        } else {
            return p.expected("sequence option", p.peek_token());
        }
    }
    Ok(g)
}

fn take_word(p: &mut Parser, word: &str) -> bool {
    if is_word(&p.peek_token().token, word) {
        p.next_token();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;

    #[test]
    fn sequence_name_and_options_in_any_order() -> pgarbor_common::Result<()> {
        let sql = r#"CREATE TABLE t (id bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "t_id_seq" INCREMENT BY 1 START WITH 1 MINVALUE 1 MAXVALUE 9223372036854775807 CACHE 1), d int GENERATED BY DEFAULT AS IDENTITY, e int GENERATED ALWAYS AS IDENTITY (NO CYCLE START 7))"#;
        let stmts = parse(sql)?;
        let groups = &stmts[0].identity_groups;
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].sequence_name.as_deref(), Some("t_id_seq"));
        assert!(!groups[0].sequence_name_qualified);
        assert_eq!(groups[0].options.len(), 5, "{:?}", groups[0].options);
        assert_eq!(groups[1], Default::default());
        assert_eq!(groups[2].sequence_name, None);
        assert_eq!(groups[2].options.len(), 2);
        Ok(())
    }

    #[test]
    fn qualified_and_bad_sequence_names() {
        let stmts = parse("CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME s.q))").expect("parse");
        assert!(stmts[0].identity_groups[0].sequence_name_qualified);
        let err = parse("CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE x))").unwrap_err();
        assert_eq!(err.sqlstate.code(), "42601");
    }

    #[test]
    fn plain_identity_without_list_is_unchanged() -> pgarbor_common::Result<()> {
        let stmts = parse("ALTER TABLE t ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME x)")?;
        assert_eq!(stmts[0].identity_groups.len(), 1);
        assert_eq!(stmts[0].identity_groups[0].sequence_name.as_deref(), Some("x"));
        Ok(())
    }
}
