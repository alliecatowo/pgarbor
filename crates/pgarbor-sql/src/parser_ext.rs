//! Hand parser for the extensibility statements (amendment pgarbor-sql-05), a child module of
//! `parser_extra` so it shares its token cursor `Cx`. Grammar: PostgreSQL 18.6 `gram.y`
//! (`DefineStmt` for OPERATOR / AGGREGATE / COLLATION, `AlterOperatorStmt`, `RemoveOperStmt`,
//! `RemoveAggrStmt`, `CreateOpClassStmt`, `CreateOpFamilyStmt`, `AlterOpFamilyStmt`, `DropOpClassStmt`,
//! `DropOpFamilyStmt`, `CreateCastStmt`, `DropCastStmt`, `CreateConversionStmt`, `AlterCollationStmt`,
//! `CreateExtensionStmt`, `AlterExtensionStmt`, `AlterExtensionContentsStmt`, `RenameStmt`,
//! `AlterObjectSchemaStmt`, `AlterOwnerStmt`, `DropStmt`, `CommentStmt`).
//!
//! Operator names (`all_Op`) are read from the source text of adjacent operator-character tokens,
//! with PostgreSQL's lexer rules: `!=` means `<>`; a multi-character operator that ends in `+` or `-`
//! loses that suffix unless it contains one of `~ ! @ # ^ & | \` ? %`.
use super::{is_eof, is_word, parse_drop_behavior, parse_if_exists, role_spec, Cx};
use crate::diag;
use crate::ext_stmt::*;
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{ArgMode, DataType, Ident, ObjectName, ObjectNamePart, OperateFunctionArg};
use sqlparser::tokenizer::{Token, TokenWithSpan};

/// Characters of PG's `op_chars`.
const OP_CHARS: &str = "~!@#^&|`?+-*/%<>=";

/// Head words (upper-cased) of the chunk.
fn heads(tokens: &[TokenWithSpan]) -> Vec<String> {
    super::head_words(tokens)
}

/// Does this chunk belong to this parser?
pub(super) fn is_ext_statement(tokens: &[TokenWithSpan]) -> bool {
    let h = heads(tokens);
    let at = |i: usize, w: &str| h.get(i).is_some_and(|x| x == w);
    let verb = at(0, "CREATE") || at(0, "ALTER") || at(0, "DROP");
    if verb && (at(1, "OPERATOR") || at(1, "AGGREGATE") || at(1, "COLLATION") || at(1, "CONVERSION") || at(1, "EXTENSION"))
    {
        return true;
    }
    if (at(0, "CREATE") || at(0, "DROP")) && at(1, "CAST") {
        return true;
    }
    if at(0, "CREATE") && at(1, "DEFAULT") && at(2, "CONVERSION") {
        return true;
    }
    if at(0, "COMMENT") && at(1, "ON") {
        return ["OPERATOR", "AGGREGATE", "CAST", "CONVERSION", "COLLATION", "EXTENSION"].iter().any(|w| at(2, w));
    }
    false
}

/// Parse the chunk.
pub(super) fn parse(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let h = heads(&cx.tokens);
    let at = |i: usize, w: &str| h.get(i).is_some_and(|x| x == w);
    if at(0, "COMMENT") {
        return parse_comment(cx);
    }
    let stmt = if at(0, "CREATE") {
        cx.expect_word("CREATE")?;
        let or_replace = if cx.word("OR") {
            cx.expect_word("REPLACE")?;
            true
        } else {
            false
        };
        if cx.word("OPERATOR") {
            if cx.word("FAMILY") {
                let name = cx.object_name()?;
                cx.expect_word("USING")?;
                let method = cx.ident()?;
                ExtStmt::CreateOpFamily { name, method }
            } else if cx.word("CLASS") {
                parse_create_opclass(cx)?
            } else {
                let name = oper_name(cx)?;
                let defs = definition(cx)?;
                ExtStmt::CreateOperator { name, defs }
            }
        } else if at_word(cx, "AGGREGATE") {
            let pos = pos_here(cx);
            cx.expect_word("AGGREGATE")?;
            parse_create_aggregate(cx, or_replace, pos)?
        } else if cx.word("COLLATION") {
            let if_not_exists = if_not_exists(cx)?;
            let name = cx.object_name()?;
            if cx.word("FROM") {
                let from = cx.object_name()?;
                ExtStmt::CreateCollation { if_not_exists, name, from: Some(from), defs: Vec::new() }
            } else {
                let defs = definition(cx)?;
                ExtStmt::CreateCollation { if_not_exists, name, from: None, defs }
            }
        } else if cx.word("CAST") {
            parse_create_cast(cx)?
        } else if cx.word("DEFAULT") || at_word(cx, "CONVERSION") {
            let is_default = at(1, "DEFAULT");
            cx.expect_word("CONVERSION")?;
            let name = cx.object_name()?;
            cx.expect_word("FOR")?;
            let for_encoding = sconst(cx)?;
            cx.expect_word("TO")?;
            let to_encoding = sconst(cx)?;
            cx.expect_word("FROM")?;
            let func = cx.object_name()?;
            ExtStmt::CreateConversion { is_default, name, for_encoding, to_encoding, func }
        } else if cx.word("EXTENSION") {
            parse_create_extension(cx)?
        } else {
            return Err(cx.fail_here());
        }
    } else if at(0, "ALTER") {
        cx.expect_word("ALTER")?;
        if cx.word("OPERATOR") {
            if cx.word("FAMILY") {
                parse_alter_opfamily(cx)?
            } else if cx.word("CLASS") {
                let name = cx.object_name()?;
                cx.expect_word("USING")?;
                let method = cx.ident()?;
                let action = alter_act(cx, true, true, true)?;
                ExtStmt::AlterOpClass { name, method, action }
            } else {
                parse_alter_operator(cx)?
            }
        } else if cx.word("AGGREGATE") {
            let sig = agg_sig(cx)?;
            let action = alter_act(cx, true, true, true)?;
            ExtStmt::AlterAggregate { sig, action }
        } else if cx.word("COLLATION") {
            let name = cx.object_name()?;
            if cx.word("REFRESH") {
                cx.expect_word("VERSION")?;
                ExtStmt::AlterCollation { name, action: None }
            } else {
                let action = alter_act(cx, true, true, true)?;
                ExtStmt::AlterCollation { name, action: Some(action) }
            }
        } else if cx.word("CONVERSION") {
            let name = cx.object_name()?;
            let action = alter_act(cx, true, true, true)?;
            ExtStmt::AlterConversion { name, action }
        } else if cx.word("EXTENSION") {
            parse_alter_extension(cx)?
        } else {
            return Err(cx.fail_here());
        }
    } else {
        cx.expect_word("DROP")?;
        if cx.word("OPERATOR") {
            if cx.word("FAMILY") || at(2, "CLASS") {
                let family = !at(2, "CLASS");
                if !family {
                    cx.expect_word("CLASS")?;
                }
                let if_exists = parse_if_exists(cx)?;
                let names = cx.comma_list(|c| c.object_name())?;
                cx.expect_word("USING")?;
                let method = cx.ident()?;
                let cascade = parse_drop_behavior(cx);
                if family {
                    ExtStmt::DropOpFamily { if_exists, names, method, cascade }
                } else {
                    ExtStmt::DropOpClass { if_exists, names, method, cascade }
                }
            } else {
                let if_exists = parse_if_exists(cx)?;
                let sigs = cx.comma_list(oper_sig)?;
                let cascade = parse_drop_behavior(cx);
                ExtStmt::DropOperator { if_exists, sigs, cascade }
            }
        } else if cx.word("AGGREGATE") {
            let if_exists = parse_if_exists(cx)?;
            let sigs = cx.comma_list(agg_sig)?;
            let cascade = parse_drop_behavior(cx);
            ExtStmt::DropAggregate { if_exists, sigs, cascade }
        } else if cx.word("CAST") {
            let if_exists = parse_if_exists(cx)?;
            cx.expect_token(&Token::LParen)?;
            let source = cx.data_type()?;
            cx.expect_word("AS")?;
            let target = cx.data_type()?;
            cx.expect_token(&Token::RParen)?;
            let cascade = parse_drop_behavior(cx);
            ExtStmt::DropCast { if_exists, source, target, cascade }
        } else if cx.word("CONVERSION") {
            let if_exists = parse_if_exists(cx)?;
            let names = cx.comma_list(|c| c.object_name())?;
            let cascade = parse_drop_behavior(cx);
            ExtStmt::DropConversion { if_exists, names, cascade }
        } else if cx.word("COLLATION") {
            let if_exists = parse_if_exists(cx)?;
            let names = cx.comma_list(|c| c.object_name())?;
            let cascade = parse_drop_behavior(cx);
            ExtStmt::DropCollation { if_exists, names, cascade }
        } else if cx.word("EXTENSION") {
            let if_exists = parse_if_exists(cx)?;
            let names = cx.comma_list(|c| c.ident())?;
            let cascade = parse_drop_behavior(cx);
            ExtStmt::DropExtension { if_exists, names, cascade }
        } else {
            return Err(cx.fail_here());
        }
    };
    cx.finish()?;
    Ok(stmt)
}

// ------------------------------------------------------------------------------------------------
// small helpers

fn at_word(cx: &Cx<'_>, kw: &str) -> bool {
    is_word(&cx.peek(), kw)
}

fn pos_here(cx: &Cx<'_>) -> Option<u32> {
    let t = cx.peek();
    cx.src.pos_of_span(t.span.clone())
}

fn if_not_exists(cx: &mut Cx<'_>) -> Result<bool> {
    if at_word(cx, "IF") {
        cx.expect_word("IF")?;
        cx.expect_word("NOT")?;
        cx.expect_word("EXISTS")?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn sconst(cx: &mut Cx<'_>) -> Result<String> {
    match cx.string_const() {
        Some(s) => Ok(s),
        None => Err(cx.fail_here()),
    }
}

/// `Iconst`.
fn iconst(cx: &mut Cx<'_>) -> Result<i32> {
    let t = cx.peek();
    match &t.token {
        Token::Number(n, _) if n.bytes().all(|b| b.is_ascii_digit()) => {
            let v = n.parse::<i32>().map_err(|_| cx.fail_at(&t))?;
            cx.p.next_token();
            Ok(v)
        }
        _ => Err(cx.fail_at(&t)),
    }
}

/// Source text of a token.
fn token_text(cx: &Cx<'_>, t: &TokenWithSpan) -> Option<String> {
    let s = cx.src.byte_offset(t.span.start)?;
    let e = cx.src.byte_offset(t.span.end)?;
    Some(cx.src.slice(s, e).to_string())
}

/// Is the token made only of operator characters (by its source text)?
fn op_token_text(cx: &Cx<'_>, t: &TokenWithSpan) -> Option<String> {
    if matches!(t.token, Token::EOF | Token::Whitespace(_) | Token::LParen | Token::RParen | Token::Comma) {
        return None;
    }
    match &t.token {
        Token::Word(w) if w.quote_style.is_some() => return None,
        Token::Word(w) if !w.value.chars().all(|c| OP_CHARS.contains(c)) => return None,
        _ => {}
    }
    let text = token_text(cx, t)?;
    if !text.is_empty() && text.chars().all(|c| OP_CHARS.contains(c)) {
        Some(text)
    } else {
        None
    }
}

/// `all_Op`: read one operator (adjacent operator-character tokens), applying the lexer rules.
fn all_op(cx: &mut Cx<'_>) -> Result<(String, Option<u32>)> {
    let first = cx.peek();
    let pos = cx.src.pos_of_span(first.span.clone());
    let mut toks: Vec<(String, TokenWithSpan)> = Vec::new();
    // Collect adjacent operator tokens without consuming.
    let mut idx = 0usize;
    loop {
        let t = cx.p.peek_nth_token(idx);
        let Some(text) = op_token_text(cx, &t) else { break };
        if let Some((_, prev)) = toks.last() {
            if prev.span.end != t.span.start {
                break;
            }
        }
        // A comment start ends the operator (`--`, `/*`): the tokenizer already split them out.
        toks.push((text, t));
        idx += 1;
    }
    if toks.is_empty() {
        return Err(cx.fail_at(&first));
    }
    let full: String = toks.iter().map(|(s, _)| s.as_str()).collect();
    // PG scan.l: trim trailing '+'/'-' of a multi-char operator without special characters.
    let mut keep = full.len();
    if full.len() > 1 && !full.chars().any(|c| "~!@#^&|`?%".contains(c)) {
        let b = full.as_bytes();
        while keep > 1 && (b[keep - 1] == b'+' || b[keep - 1] == b'-') {
            keep -= 1;
        }
    }
    let mut consumed = 0usize;
    let mut n = 0usize;
    for (s, _) in &toks {
        if consumed + s.len() > keep {
            break;
        }
        consumed += s.len();
        n += 1;
    }
    if consumed != keep || n == 0 {
        return Err(cx.fail_at(&toks[n.min(toks.len() - 1)].1));
    }
    for _ in 0..n {
        cx.p.next_token();
    }
    let mut name = full[..keep].to_string();
    if name == "!=" {
        name = "<>".to_string();
    }
    Ok((name, pos))
}

/// `any_operator`: `[ColId .]* all_Op`.
fn oper_name(cx: &mut Cx<'_>) -> Result<OperName> {
    let pos = pos_here(cx);
    let mut schema: Option<Ident> = None;
    loop {
        let t = cx.peek();
        match &t.token {
            Token::Word(w) if op_token_text(cx, &t).is_none() => {
                let next = cx.p.peek_nth_token(1);
                if !matches!(next.token, Token::Period) {
                    return Err(cx.fail_at(&t));
                }
                if schema.is_some() {
                    // catalog.schema.op: PG accepts it grammatically; keep the last two parts.
                }
                schema = Some(Ident { value: w.value.clone(), quote_style: w.quote_style, span: t.span.clone() });
                cx.p.next_token();
                cx.p.next_token();
            }
            _ => break,
        }
    }
    let (name, _) = all_op(cx)?;
    Ok(OperName { schema, name, pos })
}

/// `qual_all_Op`: `all_Op | OPERATOR ( any_operator )`.
fn qual_all_op(cx: &mut Cx<'_>) -> Result<OperName> {
    if at_word(cx, "OPERATOR") && matches!(cx.p.peek_nth_token(1).token, Token::LParen) {
        cx.p.next_token();
        cx.p.next_token();
        let n = oper_name(cx)?;
        cx.expect_token(&Token::RParen)?;
        Ok(n)
    } else {
        let pos = pos_here(cx);
        let (name, _) = all_op(cx)?;
        Ok(OperName { schema: None, name, pos })
    }
}

/// `oper_argtypes`.
fn oper_argtypes(cx: &mut Cx<'_>) -> Result<(OperArg, OperArg)> {
    cx.expect_token(&Token::LParen)?;
    let left = oper_arg(cx)?;
    if !cx.token(&Token::Comma) {
        let t = cx.peek();
        if matches!(t.token, Token::RParen) {
            return Err(diag::syntax("missing argument", cx.src.pos_of_span(t.span.clone()))
                .with_hint("Use NONE to denote the missing argument of a unary operator."));
        }
        return Err(cx.fail_at(&t));
    }
    let right = oper_arg(cx)?;
    if left.is_none() && right.is_none() {
        // `(NONE, NONE)` is not in the grammar.
        return Err(cx.fail_here());
    }
    cx.expect_token(&Token::RParen)?;
    Ok((left, right))
}

fn oper_arg(cx: &mut Cx<'_>) -> Result<OperArg> {
    if at_word(cx, "NONE") {
        cx.p.next_token();
        Ok(None)
    } else {
        Ok(Some(cx.data_type()?))
    }
}

/// `operator_with_argtypes`.
fn oper_sig(cx: &mut Cx<'_>) -> Result<OperSig> {
    let name = oper_name(cx)?;
    let (left, right) = oper_argtypes(cx)?;
    Ok(OperSig { name, left, right })
}

/// `definition`: `( def_elem [, ...] )`.
fn definition(cx: &mut Cx<'_>) -> Result<Vec<DefElem>> {
    cx.expect_token(&Token::LParen)?;
    let list = cx.comma_list(def_elem)?;
    cx.expect_token(&Token::RParen)?;
    Ok(list)
}

/// `def_elem`: `ColLabel [= def_arg]`.
fn def_elem(cx: &mut Cx<'_>) -> Result<DefElem> {
    let t = cx.peek();
    let pos = cx.src.pos_of_span(t.span.clone());
    let name = match &t.token {
        Token::Word(w) if op_token_text(cx, &t).is_none() => {
            if w.quote_style.is_some() {
                w.value.clone()
            } else {
                w.value.to_ascii_lowercase()
            }
        }
        _ => return Err(cx.fail_at(&t)),
    };
    cx.p.next_token();
    let arg = if cx.token(&Token::Eq) { Some(def_arg(cx)?) } else { None };
    Ok(DefElem { name, arg, pos })
}

/// Reserved keywords that `def_arg` accepts as plain strings (a subset: the ones that can appear).
const RESERVED_DEF_WORDS: &[&str] = &[
    "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "both", "case", "cast", "check",
    "collate", "column", "constraint", "create", "current_catalog", "current_date", "current_role", "current_time",
    "current_timestamp", "current_user", "default", "deferrable", "desc", "distinct", "do", "else", "end", "except",
    "false", "fetch", "for", "foreign", "from", "grant", "group", "having", "in", "initially", "intersect", "into",
    "lateral", "leading", "limit", "localtime", "localtimestamp", "not", "null", "offset", "on", "only", "or",
    "order", "placing", "primary", "references", "returning", "select", "session_user", "some", "symmetric",
    "system_user", "table", "then", "to", "trailing", "true", "union", "unique", "user", "using", "variadic", "when",
    "where", "window", "with",
];

/// `def_arg`.
fn def_arg(cx: &mut Cx<'_>) -> Result<DefArg> {
    let t = cx.peek();
    if let Some(s) = cx.string_const() {
        return Ok(DefArg::String(s));
    }
    match &t.token {
        Token::Number(..) => return Ok(DefArg::Number(cx.numeric()?)),
        Token::Plus | Token::Minus if matches!(cx.p.peek_nth_token(1).token, Token::Number(..)) => {
            return Ok(DefArg::Number(cx.numeric()?));
        }
        _ => {}
    }
    if op_token_text(cx, &t).is_some() {
        return Ok(DefArg::Operator(qual_all_op(cx)?));
    }
    if let Token::Word(w) = &t.token {
        if w.quote_style.is_none() {
            let lower = w.value.to_ascii_lowercase();
            if lower == "operator" && matches!(cx.p.peek_nth_token(1).token, Token::LParen) {
                return Ok(DefArg::Operator(qual_all_op(cx)?));
            }
            if lower == "none" {
                cx.p.next_token();
                return Ok(DefArg::Keyword(lower));
            }
            if lower == "setof" && matches!(cx.p.peek_nth_token(1).token, Token::Word(_)) {
                cx.p.next_token();
                return Ok(DefArg::Setof(cx.data_type()?));
            }
            let next = cx.p.peek_nth_token(1);
            let ends = matches!(next.token, Token::Comma | Token::RParen | Token::EOF);
            if ends && RESERVED_DEF_WORDS.contains(&lower.as_str()) {
                cx.p.next_token();
                return Ok(DefArg::Keyword(lower));
            }
        }
    }
    Ok(DefArg::Type(cx.data_type()?))
}

/// `[IN|OUT|INOUT|VARIADIC] [name] type`, ending before `,`, `)` or `ORDER`.
fn func_arg(cx: &mut Cx<'_>) -> Result<OperateFunctionArg> {
    let mode = if cx.word("INOUT") {
        Some(ArgMode::InOut)
    } else if cx.word("IN") {
        if cx.word("OUT") { Some(ArgMode::InOut) } else { Some(ArgMode::In) }
    } else if cx.word("OUT") {
        Some(ArgMode::Out)
    } else if cx.word("VARIADIC") {
        Some(ArgMode::Variadic)
    } else {
        None
    };
    let first = cx.data_type()?;
    let ends = |cx: &Cx<'_>| matches!(cx.peek().token, Token::Comma | Token::RParen) || at_word(cx, "ORDER");
    let (name, data_type) = if ends(cx) {
        (None, first)
    } else {
        let id = match &first {
            DataType::Custom(obj, mods) if mods.is_empty() => match obj.0.as_slice() {
                [ObjectNamePart::Identifier(id)] => id.clone(),
                _ => return Err(cx.fail_here()),
            },
            _ => return Err(cx.fail_here()),
        };
        (Some(id), cx.data_type()?)
    };
    Ok(OperateFunctionArg { mode, name, data_type, default_expr: None })
}

/// `aggr_arg`: a `func_arg` whose mode is IN or VARIADIC (or none).
fn aggr_arg(cx: &mut Cx<'_>) -> Result<OperateFunctionArg> {
    let pos = pos_here(cx);
    let a = func_arg(cx)?;
    if matches!(a.mode, Some(ArgMode::Out) | Some(ArgMode::InOut)) {
        let e = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "aggregates cannot have output arguments");
        return Err(match pos {
            Some(p) => e.with_position(p),
            None => e,
        });
    }
    Ok(a)
}

/// `aggr_args`.
fn aggr_args(cx: &mut Cx<'_>) -> Result<AggrArgs> {
    cx.expect_token(&Token::LParen)?;
    if cx.token(&Token::Mul) {
        cx.expect_token(&Token::RParen)?;
        return Ok(AggrArgs { args: Vec::new(), order_by_at: -1, star: true });
    }
    if cx.word("ORDER") {
        cx.expect_word("BY")?;
        let args = cx.comma_list(aggr_arg)?;
        cx.expect_token(&Token::RParen)?;
        return Ok(AggrArgs { args, order_by_at: 0, star: false });
    }
    let mut direct = cx.comma_list(aggr_arg)?;
    if cx.word("ORDER") {
        let order_pos = pos_here(cx);
        cx.expect_word("BY")?;
        let ordered = cx.comma_list(aggr_arg)?;
        cx.expect_token(&Token::RParen)?;
        // PG makeOrderedSetArgs.
        let ndirect = direct.len() as i32;
        if let Some(last) = direct.last() {
            if last.mode == Some(ArgMode::Variadic) {
                let first_ordered = &ordered[0];
                if ordered.len() != 1
                    || first_ordered.mode != Some(ArgMode::Variadic)
                    || first_ordered.data_type != last.data_type
                {
                    let e = PgError::new(
                        SqlState::SYNTAX_ERROR,
                        "an ordered-set aggregate with a VARIADIC direct argument must have one VARIADIC aggregated argument of the same data type",
                    );
                    return Err(match order_pos {
                        Some(p) => e.with_position(p),
                        None => e,
                    });
                }
                return Ok(AggrArgs { args: direct, order_by_at: ndirect, star: false });
            }
        }
        direct.extend(ordered);
        return Ok(AggrArgs { args: direct, order_by_at: ndirect, star: false });
    }
    cx.expect_token(&Token::RParen)?;
    Ok(AggrArgs { args: direct, order_by_at: -1, star: false })
}

/// `aggregate_with_argtypes`.
fn agg_sig(cx: &mut Cx<'_>) -> Result<AggSig> {
    let pos = pos_here(cx);
    let name = cx.object_name()?;
    let args = aggr_args(cx)?;
    Ok(AggSig { name, args, pos })
}

/// RENAME TO / OWNER TO / SET SCHEMA.
fn alter_act(cx: &mut Cx<'_>, rename: bool, owner: bool, schema: bool) -> Result<AlterAct> {
    if rename && cx.word("RENAME") {
        cx.expect_word("TO")?;
        return Ok(AlterAct::Rename(cx.ident()?));
    }
    if owner && cx.word("OWNER") {
        cx.expect_word("TO")?;
        return Ok(AlterAct::Owner(role_spec(cx)?));
    }
    if schema && cx.word("SET") {
        cx.expect_word("SCHEMA")?;
        return Ok(AlterAct::SetSchema(cx.ident()?));
    }
    Err(cx.fail_here())
}

// ------------------------------------------------------------------------------------------------
// statements

fn parse_alter_operator(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let sig = oper_sig(cx)?;
    let action = if cx.word("OWNER") {
        cx.expect_word("TO")?;
        AlterOperatorAction::Owner(role_spec(cx)?)
    } else if cx.word("SET") {
        if cx.word("SCHEMA") {
            AlterOperatorAction::SetSchema(cx.ident()?)
        } else {
            cx.expect_token(&Token::LParen)?;
            let list = cx.comma_list(operator_def_elem)?;
            cx.expect_token(&Token::RParen)?;
            AlterOperatorAction::Set(list)
        }
    } else {
        return Err(cx.fail_here());
    };
    Ok(ExtStmt::AlterOperator { sig, action })
}

/// `operator_def_elem`: `ColLabel = NONE | ColLabel = operator_def_arg | ColLabel`.
fn operator_def_elem(cx: &mut Cx<'_>) -> Result<DefElem> {
    let t = cx.peek();
    let pos = cx.src.pos_of_span(t.span.clone());
    let name = match &t.token {
        Token::Word(w) if op_token_text(cx, &t).is_none() => {
            if w.quote_style.is_some() {
                w.value.clone()
            } else {
                w.value.to_ascii_lowercase()
            }
        }
        _ => return Err(cx.fail_at(&t)),
    };
    cx.p.next_token();
    let arg = if cx.token(&Token::Eq) {
        if at_word(cx, "NONE") {
            cx.p.next_token();
            None
        } else {
            Some(def_arg(cx)?)
        }
    } else {
        None
    };
    Ok(DefElem { name, arg, pos })
}

fn parse_create_aggregate(cx: &mut Cx<'_>, or_replace: bool, pos: Option<u32>) -> Result<ExtStmt> {
    let name = cx.object_name()?;
    // Old style: `( IDENT = ...`.
    let old_style = matches!(cx.peek().token, Token::LParen)
        && matches!(cx.p.peek_nth_token(1).token, Token::Word(_))
        && matches!(cx.p.peek_nth_token(2).token, Token::Eq);
    if old_style {
        let defs = definition(cx)?;
        return Ok(ExtStmt::CreateAggregate { or_replace, name, args: None, defs, pos });
    }
    let args = aggr_args(cx)?;
    let defs = definition(cx)?;
    Ok(ExtStmt::CreateAggregate { or_replace, name, args: Some(args), defs, pos })
}

/// `opclass_item`.
fn opclass_item(cx: &mut Cx<'_>) -> Result<OpClassItem> {
    let pos = pos_here(cx);
    if cx.word("OPERATOR") {
        let number = iconst(cx)?;
        let op = oper_name(cx)?;
        let args = if matches!(cx.peek().token, Token::LParen) { Some(oper_argtypes(cx)?) } else { None };
        let order_family = if cx.word("FOR") {
            if cx.word("SEARCH") {
                None
            } else {
                cx.expect_word("ORDER")?;
                cx.expect_word("BY")?;
                Some(cx.object_name()?)
            }
        } else {
            None
        };
        return Ok(OpClassItem::Operator { number, op, args, order_family, pos });
    }
    if cx.word("FUNCTION") {
        let number = iconst(cx)?;
        let types = if matches!(cx.peek().token, Token::LParen) {
            cx.p.next_token();
            let list = cx.comma_list(oper_arg)?;
            cx.expect_token(&Token::RParen)?;
            Some(list)
        } else {
            None
        };
        let func = cx.object_name()?;
        let func_args = if cx.token(&Token::LParen) {
            if cx.token(&Token::RParen) {
                Some(Vec::new())
            } else {
                let l = cx.comma_list(func_arg)?;
                cx.expect_token(&Token::RParen)?;
                Some(l)
            }
        } else {
            None
        };
        return Ok(OpClassItem::Function { number, types, func, func_args, pos });
    }
    if cx.word("STORAGE") {
        return Ok(OpClassItem::Storage(cx.data_type()?));
    }
    Err(cx.fail_here())
}

fn parse_create_opclass(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let name = cx.object_name()?;
    let is_default = cx.word("DEFAULT");
    cx.expect_word("FOR")?;
    cx.expect_word("TYPE")?;
    let for_type = cx.data_type()?;
    cx.expect_word("USING")?;
    let method = cx.ident()?;
    let family = if cx.word("FAMILY") { Some(cx.object_name()?) } else { None };
    cx.expect_word("AS")?;
    let items = cx.comma_list(opclass_item)?;
    Ok(ExtStmt::CreateOpClass { name, is_default, for_type, method, family, items })
}

fn parse_alter_opfamily(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let name = cx.object_name()?;
    cx.expect_word("USING")?;
    let method = cx.ident()?;
    if cx.word("ADD") {
        let items = cx.comma_list(opclass_item)?;
        return Ok(ExtStmt::AlterOpFamilyAdd { name, method, items });
    }
    if cx.word("DROP") {
        let items = cx.comma_list(|cx| {
            let pos = pos_here(cx);
            let is_operator = if cx.word("OPERATOR") {
                true
            } else {
                cx.expect_word("FUNCTION")?;
                false
            };
            let number = iconst(cx)?;
            cx.expect_token(&Token::LParen)?;
            let types = cx.comma_list(|c| c.data_type().map(Some))?;
            cx.expect_token(&Token::RParen)?;
            Ok(OpFamilyDropItem { is_operator, number, types, pos })
        })?;
        return Ok(ExtStmt::AlterOpFamilyDrop { name, method, items });
    }
    let action = alter_act(cx, true, true, true)?;
    Ok(ExtStmt::AlterOpFamily { name, method, action })
}

fn parse_create_cast(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    cx.expect_token(&Token::LParen)?;
    let source = cx.data_type()?;
    cx.expect_word("AS")?;
    let target = cx.data_type()?;
    cx.expect_token(&Token::RParen)?;
    let method = if cx.word("WITHOUT") {
        cx.expect_word("FUNCTION")?;
        CastMethodAst::WithoutFunction
    } else {
        cx.expect_word("WITH")?;
        if cx.word("INOUT") {
            CastMethodAst::Inout
        } else {
            cx.expect_word("FUNCTION")?;
            let name = cx.object_name()?;
            let args = if cx.token(&Token::LParen) {
                if cx.token(&Token::RParen) {
                    Some(Vec::new())
                } else {
                    let l = cx.comma_list(func_arg)?;
                    cx.expect_token(&Token::RParen)?;
                    Some(l)
                }
            } else {
                None
            };
            CastMethodAst::Function { name, args }
        }
    };
    let context = if cx.word("AS") {
        if cx.word("IMPLICIT") {
            b'i'
        } else {
            cx.expect_word("ASSIGNMENT")?;
            b'a'
        }
    } else {
        b'e'
    };
    Ok(ExtStmt::CreateCast { source, target, method, context })
}

/// `NonReservedWord_or_Sconst`.
fn word_or_sconst(cx: &mut Cx<'_>) -> Result<String> {
    if let Some(s) = cx.string_const() {
        return Ok(s);
    }
    let t = cx.peek();
    match &t.token {
        Token::Word(w) => {
            let v = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_ascii_lowercase() };
            cx.p.next_token();
            Ok(v)
        }
        _ => Err(cx.fail_at(&t)),
    }
}

fn parse_create_extension(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let if_not_exists = if_not_exists(cx)?;
    let name = cx.ident()?;
    cx.word("WITH");
    let mut schema = None;
    let mut version = None;
    let mut cascade = false;
    loop {
        let t = cx.peek();
        if is_eof(&t) {
            break;
        }
        if cx.word("SCHEMA") {
            if schema.is_some() {
                return Err(super::conflicting(cx, &t));
            }
            schema = Some(cx.ident()?);
        } else if cx.word("VERSION") {
            if version.is_some() {
                return Err(super::conflicting(cx, &t));
            }
            version = Some(word_or_sconst(cx)?);
        } else if cx.word("FROM") {
            let e = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "CREATE EXTENSION ... FROM is no longer supported");
            return Err(match cx.src.pos_of_span(t.span.clone()) {
                Some(p) => e.with_position(p),
                None => e,
            });
        } else if cx.word("CASCADE") {
            if cascade {
                return Err(super::conflicting(cx, &t));
            }
            cascade = true;
        } else {
            return Err(cx.fail_at(&t));
        }
    }
    Ok(ExtStmt::CreateExtension { if_not_exists, name, schema, version, cascade })
}

fn parse_alter_extension(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    let name = cx.ident()?;
    if cx.word("UPDATE") {
        let mut version = None;
        while cx.word("TO") {
            let t = cx.peek();
            if version.is_some() {
                return Err(super::conflicting(cx, &t));
            }
            version = Some(word_or_sconst(cx)?);
        }
        return Ok(ExtStmt::AlterExtensionUpdate { name, version });
    }
    if cx.word("SET") {
        cx.expect_word("SCHEMA")?;
        let schema = cx.ident()?;
        return Ok(ExtStmt::AlterExtensionSchema { name, schema });
    }
    let add = if cx.word("ADD") {
        true
    } else {
        cx.expect_word("DROP")?;
        false
    };
    let object = object_ref(cx)?;
    Ok(ExtStmt::AlterExtensionMember { name, add, object })
}

/// The object designator of ALTER EXTENSION ADD/DROP and COMMENT ON.
fn object_ref(cx: &mut Cx<'_>) -> Result<ObjectRef> {
    let t = cx.peek();
    let kw = match &t.token {
        Token::Word(w) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
        _ => return Err(cx.fail_at(&t)),
    };
    cx.p.next_token();
    Ok(match kw.as_str() {
        "TABLE" | "SEQUENCE" | "VIEW" | "INDEX" => ObjectRef::Relation { keyword: kw, name: cx.object_name()? },
        "MATERIALIZED" => {
            cx.expect_word("VIEW")?;
            ObjectRef::Relation { keyword: "MATERIALIZED VIEW".into(), name: cx.object_name()? }
        }
        "FOREIGN" => {
            if cx.word("TABLE") {
                ObjectRef::Relation { keyword: "FOREIGN TABLE".into(), name: cx.object_name()? }
            } else {
                cx.expect_word("DATA")?;
                cx.expect_word("WRAPPER")?;
                cx.ident()?;
                ObjectRef::Other { keyword: "FOREIGN DATA WRAPPER".into() }
            }
        }
        "TYPE" | "DOMAIN" => ObjectRef::Type { keyword: kw, ty: cx.data_type()? },
        "SCHEMA" => ObjectRef::Schema(cx.ident()?),
        "FUNCTION" | "PROCEDURE" | "ROUTINE" => {
            let name = cx.object_name()?;
            let args = if cx.token(&Token::LParen) {
                if cx.token(&Token::RParen) {
                    Some(Vec::new())
                } else {
                    let l = cx.comma_list(func_arg)?;
                    cx.expect_token(&Token::RParen)?;
                    Some(l)
                }
            } else {
                None
            };
            ObjectRef::Routine { keyword: kw, name, args }
        }
        "AGGREGATE" => ObjectRef::Aggregate(agg_sig(cx)?),
        "OPERATOR" => {
            if cx.word("CLASS") {
                let name = cx.object_name()?;
                cx.expect_word("USING")?;
                ObjectRef::OpClass { name, method: cx.ident()? }
            } else if cx.word("FAMILY") {
                let name = cx.object_name()?;
                cx.expect_word("USING")?;
                ObjectRef::OpFamily { name, method: cx.ident()? }
            } else {
                ObjectRef::Operator(oper_sig(cx)?)
            }
        }
        "CAST" => {
            cx.expect_token(&Token::LParen)?;
            let source = cx.data_type()?;
            cx.expect_word("AS")?;
            let target = cx.data_type()?;
            cx.expect_token(&Token::RParen)?;
            ObjectRef::Cast { source, target }
        }
        "CONVERSION" => ObjectRef::Conversion(cx.object_name()?),
        "COLLATION" => ObjectRef::Collation(cx.object_name()?),
        "EXTENSION" => ObjectRef::Extension(cx.ident()?),
        "LANGUAGE" | "PROCEDURAL" | "SERVER" | "EVENT" | "ACCESS" | "TEXT" | "TRANSFORM" | "STATISTICS"
        | "PUBLICATION" | "DATABASE" | "ROLE" | "TABLESPACE" | "SUBSCRIPTION" => {
            // Consume the rest of the designator loosely; the binder reports 0A000 for these kinds.
            while !is_eof(&cx.peek()) && !is_word(&cx.peek(), "IS") {
                cx.p.next_token();
            }
            ObjectRef::Other { keyword: kw }
        }
        _ => return Err(cx.fail_at(&t)),
    })
}

fn parse_comment(cx: &mut Cx<'_>) -> Result<ExtStmt> {
    cx.expect_word("COMMENT")?;
    cx.expect_word("ON")?;
    let object = object_ref(cx)?;
    cx.expect_word("IS")?;
    let text = if cx.word("NULL") { None } else { Some(sconst(cx)?) };
    cx.finish()?;
    Ok(ExtStmt::Comment { object, text })
}

/// Helper used by tests: the folded name of an ObjectName part list.
#[allow(dead_code)]
pub(crate) fn name_parts(n: &ObjectName) -> Vec<String> {
    n.0.iter()
        .filter_map(|p| match p {
            ObjectNamePart::Identifier(id) => Some(id.value.clone()),
            _ => None,
        })
        .collect()
}
