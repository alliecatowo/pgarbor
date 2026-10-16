//! Recursive-descent PL/pgSQL parser (PG `pl_gram.y`). Produces [`PlFunction`]; SQL fragments are kept
//! as text. Errors carry the BYTE offset of the offending token in the body ([`PlParseError::offset`]);
//! the caller turns it into a statement position.
use std::collections::HashSet;

use pgarbor_common::{PgError, SqlState};

use super::ast::*;
use super::errcodes;
use super::lex::{tokenize, Tok, TokKind};

/// A compile error with the byte offset (in the body) it refers to.
#[derive(Clone, Debug)]
pub struct PlParseError {
    pub error: PgError,
    /// Byte offset of the error position (the error is reported with a cursor position).
    pub offset: Option<usize>,
    /// Byte offset of the last token read (for the `near line N` CONTEXT of position-less errors).
    pub near: usize,
}

type PResult<T> = std::result::Result<T, PlParseError>;

fn perr(e: PgError, offset: Option<usize>) -> PlParseError {
    PlParseError { error: e, offset, near: offset.unwrap_or(0) }
}

/// Words that end a statement list.
const LIST_END: &[&str] = &["end", "else", "elsif", "elseif", "when", "exception"];

/// PL/pgSQL reserved words (pl_reserved_kwlist.h).
pub const RESERVED: &[&str] = &[
    "all", "begin", "by", "case", "declare", "else", "end", "execute", "for", "foreach", "from", "if", "in", "into",
    "loop", "not", "null", "or", "strict", "then", "to", "using", "when", "while",
];

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    pos: usize,
    /// Cursor names declared in the enclosing blocks (for FOR-over-cursor detection).
    cursors: Vec<HashSet<String>>,
    /// Enclosing blocks and loops, innermost last: (label, is_loop). For EXIT / CONTINUE validation.
    scopes: Vec<(Option<String>, bool)>,
}

/// Parse a routine body.
pub fn parse_body(src: &str) -> std::result::Result<PlFunction, PlParseError> {
    let toks = tokenize(src).map_err(|e| {
        let off = e.internal_position.map(|p| p as usize);
        let mut e = e;
        e.internal_position = None;
        perr(e, off)
    })?;
    let mut p = Parser { src, toks, pos: 0, cursors: Vec::new(), scopes: Vec::new() };
    let options = p.parse_directives()?;
    let label = p.opt_label()?;
    let block = p.parse_block(label, true)?;
    // Optional trailing semicolon, then end of input.
    if p.peek().is_some_and(|t| t.is_op(";")) {
        p.pos += 1;
    }
    if let Some(t) = p.peek() {
        return Err(p.syntax_at(t.start, Some(t)));
    }
    Ok(PlFunction { options, block })
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.toks.get(self.pos + k)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn at_word(&self, w: &str) -> bool {
        self.peek().is_some_and(|t| t.is_word(w))
    }
    fn at_op(&self, o: &str) -> bool {
        self.peek().is_some_and(|t| t.is_op(o))
    }
    fn eof_offset(&self) -> usize {
        self.src.len()
    }

    /// `syntax error at or near "tok"` (or `at end of input`).
    fn syntax_at(&self, offset: usize, tok: Option<&Tok>) -> PlParseError {
        let msg = match tok {
            Some(t) => format!("syntax error at or near \"{}\"", &self.src[t.start..t.end]),
            None => "syntax error at end of input".to_string(),
        };
        perr(PgError::new(SqlState::SYNTAX_ERROR, msg), Some(offset))
    }
    fn syntax_here(&self) -> PlParseError {
        match self.peek() {
            Some(t) => self.syntax_at(t.start, Some(t)),
            None => self.syntax_at(self.eof_offset(), None),
        }
    }
    /// A plain message error at the current token (`yyerror(msg)`: `msg at or near "tok"`).
    fn msg_here(&self, msg: &str) -> PlParseError {
        match self.peek() {
            Some(t) => perr(
                PgError::new(SqlState::SYNTAX_ERROR, format!("{msg} at or near \"{}\"", &self.src[t.start..t.end])),
                Some(t.start),
            ),
            None => perr(PgError::new(SqlState::SYNTAX_ERROR, format!("{msg} at end of input")), Some(self.eof_offset())),
        }
    }
    fn msg_at_tok(&self, msg: &str, t: &Tok) -> PlParseError {
        perr(
            PgError::new(SqlState::SYNTAX_ERROR, format!("{msg} at or near \"{}\"", &self.src[t.start..t.end])),
            Some(t.start),
        )
    }

    fn expect_word(&mut self, w: &str) -> PResult<Tok> {
        match self.peek() {
            Some(t) if t.is_word(w) => {
                let t = t.clone();
                self.pos += 1;
                Ok(t)
            }
            _ => Err(self.syntax_here()),
        }
    }
    fn expect_op(&mut self, o: &str) -> PResult<Tok> {
        match self.peek() {
            Some(t) if t.is_op(o) => {
                let t = t.clone();
                self.pos += 1;
                Ok(t)
            }
            _ => Err(self.syntax_here()),
        }
    }
    fn expect_semi(&mut self) -> PResult<()> {
        self.expect_op(";").map(|_| ())
    }

    /// An error without a cursor position, attributed to the last token read.
    fn nopos(&self, e: PgError) -> PlParseError {
        let near = self.pos.checked_sub(1).and_then(|i| self.toks.get(i)).map(|t| t.start).unwrap_or(0);
        PlParseError { error: e, offset: None, near }
    }

    fn line(&self) -> u32 {
        self.peek().map(|t| t.line).unwrap_or_else(|| self.toks.last().map(|t| t.line).unwrap_or(1))
    }

    /// `#variable_conflict ...` / `#print_strict_params on|off` at the start of the body.
    fn parse_directives(&mut self) -> PResult<PlOptions> {
        let mut o = PlOptions::default();
        while self.at_op("#") {
            let hash = self.next();
            let name = self.next().filter(|t| t.kind == TokKind::Word);
            match name.as_ref().map(|t| t.text.as_str()) {
                Some("variable_conflict") => {
                    let v = self.next();
                    o.variable_conflict = match v.as_ref().map(|t| t.text.as_str()) {
                        Some("error") => Some(VarConflict::Error),
                        Some("use_variable") => Some(VarConflict::UseVariable),
                        Some("use_column") => Some(VarConflict::UseColumn),
                        _ => return Err(self.syntax_here()),
                    };
                }
                Some("print_strict_params") => {
                    let v = self.next();
                    o.print_strict_params = match v.as_ref().map(|t| t.text.as_str()) {
                        Some("on") | Some("true") => Some(true),
                        Some("off") | Some("false") => Some(false),
                        _ => return Err(self.syntax_here()),
                    };
                }
                _ => {
                    let at = hash.map(|t| t.start).unwrap_or(0);
                    return Err(perr(PgError::new(SqlState::SYNTAX_ERROR, "syntax error"), Some(at)));
                }
            }
        }
        Ok(o)
    }

    /// `<<label>>`.
    fn opt_label(&mut self) -> PResult<Option<String>> {
        if !self.at_op("<<") {
            return Ok(None);
        }
        self.pos += 1;
        let name = match self.next() {
            Some(t) if t.is_ident() => t.text,
            _ => return Err(self.syntax_here()),
        };
        self.expect_op(">>")?;
        Ok(Some(name))
    }

    /// `[DECLARE decls] BEGIN stmts [EXCEPTION handlers] END [label]` (the `;` is read by the caller for
    /// nested blocks).
    fn parse_block(&mut self, label: Option<String>, top: bool) -> PResult<PlBlock> {
        let line = self.line();
        let mut decls = Vec::new();
        self.cursors.push(HashSet::new());
        if self.at_word("declare") {
            self.pos += 1;
            self.parse_decls(&mut decls)?;
        }
        if !self.at_word("begin") {
            return Err(self.syntax_here());
        }
        self.pos += 1;
        // `BEGIN;` is a syntax error at the semicolon (PG: proc_sect cannot start with ';').
        self.scopes.push((label.clone(), false));
        let body = self.parse_stmts();
        self.scopes.pop();
        let body = body?;
        let mut handlers = None;
        if self.at_word("exception") {
            self.pos += 1;
            handlers = Some(self.parse_handlers()?);
        }
        self.expect_word("end")?;
        self.check_end_label(&label)?;
        self.cursors.pop();
        let _ = top;
        Ok(PlBlock { label, decls, body, handlers, line })
    }

    /// Optional end label after END / END LOOP; must match the block label.
    fn check_end_label(&mut self, label: &Option<String>) -> PResult<()> {
        let Some(t) = self.peek().cloned() else { return Ok(()) };
        if !t.is_ident() || (t.kind == TokKind::Word && RESERVED.contains(&t.text.as_str())) {
            return Ok(());
        }
        self.pos += 1;
        match label {
            None => Err(perr(
                PgError::new(SqlState::SYNTAX_ERROR, format!("end label \"{}\" specified for unlabeled block", t.text)),
                Some(t.start),
            )),
            Some(l) if *l != t.text => Err(perr(
                PgError::new(SqlState::SYNTAX_ERROR, format!("end label \"{}\" differs from block's label \"{}\"", t.text, l)),
                Some(t.start),
            )),
            Some(_) => Ok(()),
        }
    }

    fn parse_decls(&mut self, decls: &mut Vec<PlDecl>) -> PResult<()> {
        let mut names: HashSet<String> = HashSet::new();
        loop {
            let Some(t) = self.peek().cloned() else {
                return Err(self.syntax_here());
            };
            if t.is_word("begin") {
                return Ok(());
            }
            if t.is_word("declare") {
                self.pos += 1;
                continue;
            }
            if t.is_op("<<") {
                return Err(perr(
                    PgError::new(SqlState::SYNTAX_ERROR, "block label must be placed before DECLARE, not after"),
                    Some(t.start),
                ));
            }
            if !t.is_ident() {
                return Err(self.syntax_here());
            }
            self.pos += 1;
            let name = t.text.clone();
            let line = t.line;
            if !names.insert(name.clone()) {
                return Err(self.msg_at_tok("duplicate declaration", &t));
            }
            // ALIAS FOR
            if self.at_word("alias") {
                self.pos += 1;
                self.expect_word("for")?;
                let target = match self.next() {
                    Some(a) if a.kind == TokKind::Param => vec![a.text.clone()],
                    Some(a) if a.is_ident() => {
                        let mut parts = vec![a.text.clone()];
                        while self.at_op(".") {
                            self.pos += 1;
                            match self.next() {
                                Some(b) if b.is_ident() => parts.push(b.text),
                                _ => return Err(self.syntax_here()),
                            }
                        }
                        parts
                    }
                    _ => return Err(self.syntax_here()),
                };
                self.expect_semi()?;
                decls.push(PlDecl::Alias { name, target, line });
                continue;
            }
            // [NO] SCROLL CURSOR / CURSOR
            let mut scroll = None;
            if self.at_word("no") && self.peek_at(1).is_some_and(|t| t.is_word("scroll")) {
                self.pos += 2;
                scroll = Some(false);
            } else if self.at_word("scroll") {
                self.pos += 1;
                scroll = Some(true);
            }
            if self.at_word("cursor") {
                self.pos += 1;
                let mut args = Vec::new();
                if self.at_op("(") {
                    self.pos += 1;
                    loop {
                        let an = match self.next() {
                            Some(a) if a.is_ident() => a.text,
                            _ => return Err(self.syntax_here()),
                        };
                        let ty = self.read_type(&[",", ")"])?;
                        args.push((an, ty));
                        if self.at_op(",") {
                            self.pos += 1;
                            continue;
                        }
                        self.expect_op(")")?;
                        break;
                    }
                }
                if !(self.at_word("for") || self.at_word("is")) {
                    return Err(self.syntax_here());
                }
                self.pos += 1;
                let query = self.read_until_semi("missing SQL statement")?;
                if let Some(s) = self.cursors.last_mut() {
                    s.insert(name.clone());
                }
                decls.push(PlDecl::Cursor { name, scroll, args, query, line });
                continue;
            }
            if scroll.is_some() {
                return Err(self.syntax_here());
            }
            let mut constant = false;
            if self.at_word("constant") {
                self.pos += 1;
                constant = true;
            }
            let ty_off = self.peek().map(|t| t.start).unwrap_or(self.eof_offset());
            let ty = self.read_type(&[";", ":=", "=", "default", "not", "collate"])?;
            let mut collate = None;
            if self.at_word("collate") {
                self.pos += 1;
                match self.next() {
                    Some(c) if c.is_ident() => collate = Some(c.text),
                    _ => return Err(self.syntax_here()),
                }
            }
            let mut not_null = false;
            if self.at_word("not") {
                self.pos += 1;
                self.expect_word("null")?;
                not_null = true;
            }
            let mut default = None;
            if self.at_op(":=") || self.at_op("=") || self.at_word("default") {
                self.pos += 1;
                default = Some(self.read_expr(&[";"], ";")?.0);
            } else {
                self.expect_semi()?;
            }
            if not_null && default.is_none() {
                return Err(perr(
                    PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("variable \"{name}\" must have a default value, since it's declared NOT NULL"),
                    ),
                    Some(ty_off),
                ));
            }
            decls.push(PlDecl::Var { name, constant, ty, collate, not_null, default, line, offset: t.start });
        }
    }

    /// Read a data type up to (not including) one of the `until` tokens at depth 0.
    fn read_type(&mut self, until: &[&str]) -> PResult<PlType> {
        let start_pos = self.pos;
        let mut depth = 0i32;
        while let Some(t) = self.peek() {
            if depth == 0 && until.iter().any(|u| t.is_op(u) || t.is_word(u)) {
                break;
            }
            if t.is_op("(") || t.is_op("[") {
                depth += 1;
            } else if t.is_op(")") || t.is_op("]") {
                depth -= 1;
            } else if t.is_op(";") {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start_pos {
            return Err(self.msg_here("missing data type declaration"));
        }
        let toks = &self.toks[start_pos..self.pos];
        // name%TYPE / name%ROWTYPE
        if let Some(pct) = toks.iter().position(|t| t.is_op("%")) {
            let name: Vec<String> = toks[..pct].iter().filter(|t| t.is_ident()).map(|t| t.text.clone()).collect();
            let kind = toks.get(pct + 1).map(|t| t.text.clone()).unwrap_or_default();
            let offset = toks[0].start;
            let array = toks[pct + 2..].iter().any(|t| t.is_op("["));
            return match kind.as_str() {
                "type" => Ok(PlType::VarType { name, array, offset }),
                "rowtype" => Ok(PlType::RowType { name, offset }),
                _ => Err(self.msg_at_tok("syntax error", &toks[pct])),
            };
        }
        let start = toks[0].start;
        let end = toks[toks.len() - 1].end;
        Ok(PlType::Named(PlSql { text: self.src[start..end].to_string(), offset: start }))
    }

    /// Read SQL text up to one of `until` (operators or words) at paren depth 0. Returns the fragment and
    /// the terminator token (consumed). `expected` names the terminator in errors.
    fn read_expr(&mut self, until: &[&str], expected: &str) -> PResult<(PlSql, Tok)> {
        self.read_construct(until, expected, true)
    }

    fn read_construct(&mut self, until: &[&str], expected: &str, is_expr: bool) -> PResult<(PlSql, Tok)> {
        let start_pos = self.pos;
        let mut depth = 0i32;
        loop {
            let Some(t) = self.peek().cloned() else {
                if depth != 0 {
                    return Err(self.msg_here("mismatched parentheses"));
                }
                let what = if is_expr { "expression" } else { "statement" };
                return Err(perr(
                    PgError::new(SqlState::SYNTAX_ERROR, format!("missing \"{expected}\" at end of SQL {what}")),
                    Some(self.eof_offset()),
                ));
            };
            if depth == 0 && until.iter().any(|u| t.is_op(u) || t.is_word(u)) {
                break;
            }
            if t.is_op("(") || t.is_op("[") {
                depth += 1;
            } else if t.is_op(")") || t.is_op("]") {
                depth -= 1;
                if depth < 0 {
                    return Err(self.msg_at_tok("mismatched parentheses", &t));
                }
            } else if t.is_op(";") {
                if depth != 0 {
                    return Err(self.msg_at_tok("mismatched parentheses", &t));
                }
                let what = if is_expr { "expression" } else { "statement" };
                return Err(perr(
                    PgError::new(SqlState::SYNTAX_ERROR, format!("missing \"{expected}\" at end of SQL {what}")),
                    Some(t.start),
                ));
            }
            self.pos += 1;
        }
        let term = self.next().ok_or_else(|| self.syntax_here())?;
        if self.pos - 1 == start_pos {
            let msg = if is_expr { "missing expression" } else { "missing SQL statement" };
            return Err(self.msg_at_tok(msg, &term));
        }
        let start = self.toks[start_pos].start;
        let end = self.toks[self.pos - 2].end;
        Ok((PlSql { text: self.src[start..end].to_string(), offset: start }, term))
    }

    /// SQL statement text up to `;` (consumed).
    fn read_until_semi(&mut self, _what: &str) -> PResult<PlSql> {
        Ok(self.read_construct(&[";"], ";", false)?.0)
    }

    fn parse_stmts(&mut self) -> PResult<Vec<PlStmt>> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Ok(out),
                Some(t) if t.kind == TokKind::Word && LIST_END.contains(&t.text.as_str()) => return Ok(out),
                _ => {}
            }
            out.push(self.parse_stmt()?);
        }
    }

    fn parse_handlers(&mut self) -> PResult<Vec<PlHandler>> {
        let mut out = Vec::new();
        while self.at_word("when") {
            let line = self.line();
            self.pos += 1;
            let mut conds = Vec::new();
            loop {
                let t = self.next().ok_or_else(|| self.syntax_here())?;
                if t.is_word("sqlstate") {
                    let s = self.next().ok_or_else(|| self.syntax_here())?;
                    if s.kind != TokKind::Str {
                        return Err(self.syntax_at(s.start, Some(&s)));
                    }
                    if s.text.len() != 5 || !s.text.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) {
                        return Err(self.msg_at_tok("invalid SQLSTATE code", &s));
                    }
                    conds.push(PlCondition::SqlState(s.text));
                } else if t.is_word("others") {
                    conds.push(PlCondition::Others);
                } else if t.is_ident() {
                    let lower = t.text.to_ascii_lowercase();
                    let mut found = false;
                    for (n, s) in errcodes::CONDITIONS {
                        if *n == lower {
                            conds.push(PlCondition::SqlState((*s).to_string()));
                            found = true;
                        }
                    }
                    if !found {
                        return Err(self.nopos(PgError::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("unrecognized exception condition \"{}\"", t.text),
                        )));
                    }
                } else {
                    return Err(self.syntax_at(t.start, Some(&t)));
                }
                if self.at_word("or") {
                    self.pos += 1;
                    continue;
                }
                break;
            }
            self.expect_word("then")?;
            let body = self.parse_stmts()?;
            out.push(PlHandler { conditions: conds, body, line });
        }
        if out.is_empty() {
            return Err(self.syntax_here());
        }
        Ok(out)
    }

    fn parse_stmt(&mut self) -> PResult<PlStmt> {
        let start = self.peek().map(|t| t.start).unwrap_or(0);
        let mut s = self.parse_stmt_inner()?;
        s.offset = start;
        Ok(s)
    }

    fn parse_stmt_inner(&mut self) -> PResult<PlStmt> {
        let t = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(self.syntax_here()),
        };
        let line = t.line;
        let mk = |kind| Ok(PlStmt { kind, line, offset: 0 });
        if t.is_op("<<") {
            let label = self.opt_label()?;
            let nt = self.peek().cloned();
            let nline = nt.as_ref().map(|t| t.line).unwrap_or(line);
            return match nt {
                Some(n) if n.is_word("begin") || n.is_word("declare") => {
                    let b = self.parse_block(label, false)?;
                    self.expect_semi()?;
                    Ok(PlStmt { kind: PlStmtKind::Block(b), line: nline, offset: 0 })
                }
                Some(n) if n.is_word("loop") || n.is_word("while") || n.is_word("for") || n.is_word("foreach") => {
                    let mut s = self.parse_stmt_with_label(label)?;
                    s.line = nline;
                    Ok(s)
                }
                _ => Err(self.syntax_here()),
            };
        }
        if t.is_op(";") {
            return Err(self.syntax_here());
        }
        if !t.is_ident() {
            return Err(self.syntax_here());
        }
        // Assignment: target := expr / target = expr.
        if t.kind == TokKind::QuotedIdent || !matches!(t.text.as_str(), "begin" | "declare") {
            if let Some(stmt) = self.try_assignment()? {
                return mk(stmt);
            }
        }
        if t.kind == TokKind::QuotedIdent {
            return self.parse_sql_stmt();
        }
        match t.text.as_str() {
            "begin" | "declare" => {
                let b = self.parse_block(None, false)?;
                self.expect_semi()?;
                mk(PlStmtKind::Block(b))
            }
            "if" => self.parse_if(),
            "case" => self.parse_case(),
            "loop" | "while" | "for" | "foreach" => self.parse_stmt_with_label(None),
            "exit" | "continue" => {
                self.pos += 1;
                let is_continue = t.text == "continue";
                let mut label = None;
                if let Some(l) = self.peek().cloned() {
                    if l.is_ident() && !l.is_word("when") {
                        self.pos += 1;
                        label = Some(l.text);
                    }
                }
                let word = if is_continue { "CONTINUE" } else { "EXIT" };
                match &label {
                    Some(l) => match self.scopes.iter().rev().find(|(sl, _)| sl.as_deref() == Some(l.as_str())) {
                        None => {
                            return Err(perr(
                                PgError::new(
                                    SqlState::SYNTAX_ERROR,
                                    format!("there is no label \"{l}\" attached to any block or loop enclosing this statement"),
                                ),
                                Some(t.start),
                            ))
                        }
                        Some((_, false)) if is_continue => {
                            return Err(perr(
                                PgError::new(SqlState::SYNTAX_ERROR, format!("block label \"{l}\" cannot be used in CONTINUE")),
                                Some(t.start),
                            ))
                        }
                        Some(_) => {}
                    },
                    None if !self.scopes.iter().any(|(_, is_loop)| *is_loop) => {
                        return Err(perr(
                            PgError::new(SqlState::SYNTAX_ERROR, format!("{word} cannot be used outside a loop, unless it has a label")),
                            Some(t.start),
                        ))
                    }
                    None => {}
                }
                let mut cond = None;
                if self.at_word("when") {
                    self.pos += 1;
                    cond = Some(self.read_expr(&[";"], ";")?.0);
                } else {
                    self.expect_semi()?;
                }
                mk(PlStmtKind::Exit { is_continue, label, cond })
            }
            "return" => self.parse_return(),
            "raise" => self.parse_raise(),
            "assert" => {
                self.pos += 1;
                let (cond, term) = self.read_expr(&[",", ";"], ", or ;")?;
                let message = if term.is_op(",") { Some(self.read_expr(&[";"], ";")?.0) } else { None };
                mk(PlStmtKind::Assert { cond, message })
            }
            "perform" => {
                self.pos += 1;
                let q = self.read_until_semi("missing SQL statement")?;
                mk(PlStmtKind::Perform { query: q })
            }
            "execute" => {
                self.pos += 1;
                let (query, mut term) = self.read_expr(&["into", "using", ";"], "INTO or USING or ;")?;
                let mut into = None;
                let mut using = Vec::new();
                loop {
                    if term.is_word("into") {
                        if into.is_some() {
                            return Err(self.msg_at_tok("syntax error", &term));
                        }
                        let (strict, targets) = self.read_into_targets()?;
                        into = Some((strict, targets));
                        term = self.next().ok_or_else(|| self.syntax_here())?;
                        if !(term.is_word("using") || term.is_op(";")) {
                            return Err(self.syntax_at(term.start, Some(&term)));
                        }
                        continue;
                    }
                    if term.is_word("using") {
                        if !using.is_empty() {
                            return Err(self.msg_at_tok("syntax error", &term));
                        }
                        loop {
                            let (e, t2) = self.read_expr(&[",", ";", "into"], ", or ; or INTO")?;
                            using.push(e);
                            term = t2;
                            if term.is_op(",") {
                                continue;
                            }
                            break;
                        }
                        continue;
                    }
                    break;
                }
                mk(PlStmtKind::DynExecute { query, into, using })
            }
            "get" => self.parse_getdiag(),
            "open" => self.parse_open(),
            "fetch" | "move" => self.parse_fetch(t.text == "move"),
            "close" => {
                self.pos += 1;
                let c = self.next().filter(|c| c.is_ident()).ok_or_else(|| self.syntax_here())?;
                self.expect_semi()?;
                mk(PlStmtKind::Close { cursor: c.text })
            }
            "null" => {
                self.pos += 1;
                self.expect_semi()?;
                mk(PlStmtKind::Null)
            }
            "commit" | "rollback" => {
                // `ROLLBACK TO ...`, `COMMIT PREPARED`: plain SQL (rejected at run time).
                let nt = self.peek_at(1).cloned();
                if nt.as_ref().is_some_and(|n| n.is_op(";") || n.is_word("and")) {
                    self.pos += 1;
                    let mut chain = false;
                    if self.at_word("and") {
                        self.pos += 1;
                        if self.at_word("no") {
                            self.pos += 1;
                        } else {
                            chain = true;
                        }
                        self.expect_word("chain")?;
                    }
                    self.expect_semi()?;
                    return if t.text == "commit" { mk(PlStmtKind::Commit { chain }) } else { mk(PlStmtKind::Rollback { chain }) };
                }
                self.parse_sql_stmt()
            }
            _ => self.parse_sql_stmt(),
        }
    }

    /// `target [.field|[sub]]* {:= | =} expr ;` when the statement has that shape.
    fn try_assignment(&mut self) -> PResult<Option<PlStmtKind>> {
        let save = self.pos;
        let Some(first) = self.peek().cloned() else { return Ok(None) };
        if first.kind == TokKind::Word && RESERVED.contains(&first.text.as_str()) {
            return Ok(None);
        }
        // Look ahead for the shape without consuming.
        let mut k = self.pos + 1;
        loop {
            match self.toks.get(k) {
                Some(t) if t.is_op(".") => {
                    if self.toks.get(k + 1).is_some_and(|n| n.is_ident()) {
                        k += 2;
                        continue;
                    }
                    return Ok(None);
                }
                Some(t) if t.is_op("[") => {
                    let mut depth = 0;
                    while let Some(t) = self.toks.get(k) {
                        if t.is_op("[") || t.is_op("(") {
                            depth += 1;
                        } else if t.is_op("]") || t.is_op(")") {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        } else if t.is_op(";") {
                            return Ok(None);
                        }
                        k += 1;
                    }
                    k += 1;
                    continue;
                }
                Some(t) if t.is_op(":=") || t.is_op("=") => break,
                _ => return Ok(None),
            }
        }
        let target = self.read_target()?;
        if !(self.at_op(":=") || self.at_op("=")) {
            self.pos = save;
            return Ok(None);
        }
        self.pos += 1;
        let expr = self.read_expr(&[";"], ";")?.0;
        let end = expr.offset + expr.text.len();
        let full = PlSql { text: self.src[target.offset..end].to_string(), offset: target.offset };
        Ok(Some(PlStmtKind::Assign { target, expr, full }))
    }

    /// `name[.name]*[subscripts]*`.
    fn read_target(&mut self) -> PResult<PlTarget> {
        let first = self.next().filter(|t| t.is_ident()).ok_or_else(|| self.syntax_here())?;
        let mut name = vec![first.text.clone()];
        let mut end = first.end;
        let mut subscripts = Vec::new();
        loop {
            if self.at_op(".") && self.peek_at(1).is_some_and(|t| t.is_ident()) && subscripts.is_empty() {
                self.pos += 1;
                let t = self.next().ok_or_else(|| self.syntax_here())?;
                end = t.end;
                name.push(t.text);
                continue;
            }
            if self.at_op("[") {
                self.pos += 1;
                let (e, term) = self.read_expr(&["]"], "]")?;
                end = term.end;
                subscripts.push(e);
                continue;
            }
            break;
        }
        Ok(PlTarget { name, subscripts, text: self.src[first.start..end].to_string(), offset: first.start })
    }

    /// `[STRICT] target [, target ...]` after INTO.
    fn read_into_targets(&mut self) -> PResult<(bool, Vec<PlTarget>)> {
        let mut strict = false;
        if self.at_word("strict") {
            self.pos += 1;
            strict = true;
        }
        let mut targets = vec![self.read_target()?];
        while self.at_op(",") {
            self.pos += 1;
            targets.push(self.read_target()?);
        }
        Ok((strict, targets))
    }

    /// A SQL statement up to `;`, with a PL/pgSQL INTO clause cut out (replaced by spaces).
    fn parse_sql_stmt(&mut self) -> PResult<PlStmt> {
        let first = self.peek().cloned().ok_or_else(|| self.syntax_here())?;
        let line = first.line;
        let start = first.start;
        let first_word = first.text.clone();
        let mut depth = 0i32;
        let mut prev: Option<Tok> = None;
        let mut into: Option<(bool, Vec<PlTarget>)> = None;
        let mut into_range: Option<(usize, usize)> = None;
        let mut in_routine = false;
        let mut begin_depth = 0i32;
        let mut idx = 0usize;
        loop {
            let Some(t) = self.peek().cloned() else {
                return Err(perr(
                    PgError::new(SqlState::SYNTAX_ERROR, "unexpected end of function definition at end of input"),
                    Some(self.eof_offset()),
                ));
            };
            if idx == 1 && first_word == "create" && (t.is_word("function") || t.is_word("procedure")) {
                in_routine = true;
            }
            if idx == 3 && first_word == "create" && (t.is_word("function") || t.is_word("procedure")) {
                in_routine = true;
            }
            idx += 1;
            if t.is_op("(") {
                depth += 1;
            } else if t.is_op(")") && depth > 0 {
                depth -= 1;
            }
            if in_routine && depth == 0 {
                if t.is_word("begin") || t.is_word("case") {
                    begin_depth += 1;
                } else if t.is_word("end") && begin_depth > 0 {
                    begin_depth -= 1;
                }
            }
            if t.is_op(";") && depth == 0 && begin_depth == 0 {
                break;
            }
            if t.is_word("into") {
                let skip = prev.as_ref().is_some_and(|p| p.is_word("insert") || p.is_word("merge")) || first_word == "import";
                if !skip {
                    if into.is_some() {
                        return Err(self.msg_at_tok("INTO specified more than once", &t));
                    }
                    self.pos += 1;
                    let targets = self.read_into_targets()?;
                    let end = self.peek().map(|n| n.start).unwrap_or(self.eof_offset());
                    into_range = Some((t.start, end));
                    into = Some(targets);
                    prev = Some(t);
                    continue;
                }
            }
            prev = Some(t);
            self.pos += 1;
        }
        let semi = self.next().ok_or_else(|| self.syntax_here())?;
        let mut text = String::new();
        match into_range {
            Some((a, b)) => {
                text.push_str(&self.src[start..a]);
                text.push_str(&" ".repeat(self.src[a..b].chars().count()));
                text.push_str(&self.src[b..semi.start]);
            }
            None => text.push_str(&self.src[start..semi.start]),
        }
        let text = text.trim_end().to_string();
        Ok(PlStmt { kind: PlStmtKind::ExecSql { sql: PlSql { text, offset: start }, into, first_word }, line, offset: 0 })
    }

    fn parse_if(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        let mut arms = Vec::new();
        let cond = self.read_expr(&["then"], "THEN")?.0;
        let body = self.parse_stmts()?;
        arms.push((cond, body));
        let mut else_body = None;
        loop {
            if self.at_word("elsif") || self.at_word("elseif") {
                self.pos += 1;
                let c = self.read_expr(&["then"], "THEN")?.0;
                let b = self.parse_stmts()?;
                arms.push((c, b));
                continue;
            }
            if self.at_word("else") {
                self.pos += 1;
                else_body = Some(self.parse_stmts()?);
            }
            break;
        }
        self.expect_word("end")?;
        self.expect_word("if")?;
        self.expect_semi()?;
        Ok(PlStmt { kind: PlStmtKind::If { arms, else_body }, line, offset: 0 })
    }

    fn parse_case(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        let expr = if self.at_word("when") { None } else { Some(self.read_expr(&["when"], "WHEN")?.0) };
        if expr.is_some() {
            // read_expr consumed WHEN; step back so the arm loop sees it.
            self.pos -= 1;
        }
        let mut arms = Vec::new();
        while self.at_word("when") {
            self.pos += 1;
            let c = self.read_expr(&["then"], "THEN")?.0;
            let b = self.parse_stmts()?;
            arms.push((c, b));
        }
        let mut else_body = None;
        if self.at_word("else") {
            self.pos += 1;
            else_body = Some(self.parse_stmts()?);
        }
        self.expect_word("end")?;
        self.expect_word("case")?;
        self.expect_semi()?;
        Ok(PlStmt { kind: PlStmtKind::Case { expr, arms, else_body }, line, offset: 0 })
    }

    /// `END LOOP [label];`
    fn end_loop(&mut self, label: &Option<String>) -> PResult<()> {
        self.expect_word("end")?;
        self.expect_word("loop")?;
        self.check_end_label(label)?;
        self.expect_semi()
    }

    fn parse_stmt_with_label(&mut self, label: Option<String>) -> PResult<PlStmt> {
        self.scopes.push((label.clone(), true));
        let r = self.parse_stmt_with_label_inner(label);
        self.scopes.pop();
        r
    }

    fn parse_stmt_with_label_inner(&mut self, label: Option<String>) -> PResult<PlStmt> {
        let t = self.peek().cloned().ok_or_else(|| self.syntax_here())?;
        let line = t.line;
        self.pos += 1;
        match t.text.as_str() {
            "loop" => {
                let body = self.parse_stmts()?;
                self.end_loop(&label)?;
                Ok(PlStmt { kind: PlStmtKind::Loop { label, body }, line, offset: 0 })
            }
            "while" => {
                let cond = self.read_expr(&["loop"], "LOOP")?.0;
                let body = self.parse_stmts()?;
                self.end_loop(&label)?;
                Ok(PlStmt { kind: PlStmtKind::While { label, cond, body }, line, offset: 0 })
            }
            "for" => self.parse_for(label, line),
            "foreach" => {
                let mut targets = vec![self.read_target()?];
                while self.at_op(",") {
                    self.pos += 1;
                    targets.push(self.read_target()?);
                }
                let mut slice = 0u32;
                if self.at_word("slice") {
                    self.pos += 1;
                    let n = self.next().ok_or_else(|| self.syntax_here())?;
                    if n.kind != TokKind::Number {
                        return Err(self.syntax_at(n.start, Some(&n)));
                    }
                    slice = n.text.parse().map_err(|_| self.syntax_at(n.start, Some(&n)))?;
                }
                self.expect_word("in")?;
                self.expect_word("array")?;
                let array = self.read_expr(&["loop"], "LOOP")?.0;
                let body = self.parse_stmts()?;
                self.end_loop(&label)?;
                Ok(PlStmt { kind: PlStmtKind::Foreach { label, targets, slice, array, body }, line, offset: 0 })
            }
            _ => Err(self.syntax_at(t.start, Some(&t))),
        }
    }

    fn parse_for(&mut self, label: Option<String>, line: u32) -> PResult<PlStmt> {
        let mut targets = vec![self.read_target()?];
        while self.at_op(",") {
            self.pos += 1;
            targets.push(self.read_target()?);
        }
        self.expect_word("in")?;
        // FOR ... IN EXECUTE
        if self.at_word("execute") {
            self.pos += 1;
            let (query, term) = self.read_expr(&["loop", "using"], "LOOP or USING")?;
            let mut using = Vec::new();
            if term.is_word("using") {
                loop {
                    let (e, t2) = self.read_expr(&[",", "loop"], ", or LOOP")?;
                    using.push(e);
                    if t2.is_op(",") {
                        continue;
                    }
                    break;
                }
            }
            let body = self.parse_stmts()?;
            self.end_loop(&label)?;
            return Ok(PlStmt { kind: PlStmtKind::ForQuery { label, targets, query: PlQuery::Execute { query, using }, body }, line, offset: 0 });
        }
        // FOR r IN cursor_name [(args)] LOOP
        if let Some(c) = self.peek().cloned() {
            let is_cursor = c.is_ident() && self.cursors.iter().any(|s| s.contains(&c.text));
            let follows = self.peek_at(1).is_some_and(|n| n.is_word("loop") || n.is_op("("));
            if is_cursor && follows {
                self.pos += 1;
                let mut args = None;
                if self.at_op("(") {
                    let open = self.next().ok_or_else(|| self.syntax_here())?;
                    let (a, _) = self.read_expr(&[")"], ")")?;
                    let _ = open;
                    args = Some(a);
                }
                self.expect_word("loop")?;
                if targets.len() != 1 {
                    return Err(perr(
                        PgError::new(SqlState::SYNTAX_ERROR, "cursor FOR loop must have only one target variable"),
                        Some(targets[1].offset),
                    ));
                }
                let target = targets[0].name.join(".");
                let body = self.parse_stmts()?;
                self.end_loop(&label)?;
                return Ok(PlStmt { kind: PlStmtKind::ForCursor { label, target, cursor: c.text, args, body }, line, offset: 0 });
            }
        }
        let mut reverse = false;
        if self.at_word("reverse") {
            self.pos += 1;
            reverse = true;
        }
        // Integer loop when `..` appears at depth 0 before LOOP.
        let mut k = self.pos;
        let mut depth = 0;
        let mut dotdot = None;
        while let Some(t) = self.toks.get(k) {
            if t.is_op("(") || t.is_op("[") {
                depth += 1;
            } else if t.is_op(")") || t.is_op("]") {
                depth -= 1;
            } else if depth == 0 && (t.is_word("loop") || t.is_op(";")) {
                break;
            } else if depth == 0 && t.is_op("..") {
                dotdot = Some(k);
                break;
            }
            k += 1;
        }
        if dotdot.is_some() {
            if targets.len() != 1 {
                return Err(perr(
                    PgError::new(SqlState::SYNTAX_ERROR, "integer FOR loop must have only one target variable"),
                    Some(targets[0].offset),
                ));
            }
            let (lower, _) = self.read_expr(&[".."], "..")?;
            let (upper, term) = self.read_expr(&["loop", "by"], "LOOP")?;
            let step = if term.is_word("by") { Some(self.read_expr(&["loop"], "LOOP")?.0) } else { None };
            let body = self.parse_stmts()?;
            self.end_loop(&label)?;
            let var = targets[0].name.join(".");
            return Ok(PlStmt { kind: PlStmtKind::ForInt { label, var, reverse, lower, upper, step, body }, line, offset: 0 });
        }
        if reverse {
            return Err(self.nopos(PgError::new(SqlState::SYNTAX_ERROR, "cannot specify REVERSE in query FOR loop")));
        }
        let (query, _) = self.read_construct(&["loop"], "LOOP", false)?;
        let body = self.parse_stmts()?;
        self.end_loop(&label)?;
        Ok(PlStmt { kind: PlStmtKind::ForQuery { label, targets, query: PlQuery::Static(query), body }, line, offset: 0 })
    }

    fn parse_return(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        if self.at_op(";") {
            self.pos += 1;
            return Ok(PlStmt { kind: PlStmtKind::Return { expr: None }, line, offset: 0 });
        }
        if self.at_word("next") {
            self.pos += 1;
            if self.at_op(";") {
                self.pos += 1;
                return Ok(PlStmt { kind: PlStmtKind::ReturnNext { expr: None }, line, offset: 0 });
            }
            let e = self.read_expr(&[";"], ";")?.0;
            return Ok(PlStmt { kind: PlStmtKind::ReturnNext { expr: Some(e) }, line, offset: 0 });
        }
        if self.at_word("query") {
            self.pos += 1;
            if self.at_word("execute") {
                self.pos += 1;
                let (query, term) = self.read_expr(&["using", ";"], "USING or ;")?;
                let mut using = Vec::new();
                if term.is_word("using") {
                    loop {
                        let (e, t2) = self.read_expr(&[",", ";"], ", or ;")?;
                        using.push(e);
                        if t2.is_op(",") {
                            continue;
                        }
                        break;
                    }
                }
                return Ok(PlStmt { kind: PlStmtKind::ReturnQuery { query: PlQuery::Execute { query, using } }, line, offset: 0 });
            }
            let q = self.read_until_semi("missing SQL statement")?;
            return Ok(PlStmt { kind: PlStmtKind::ReturnQuery { query: PlQuery::Static(q) }, line, offset: 0 });
        }
        let e = self.read_expr(&[";"], ";")?.0;
        Ok(PlStmt { kind: PlStmtKind::Return { expr: Some(e) }, line, offset: 0 })
    }

    fn parse_raise(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        let mut level = RaiseLevel::Exception;
        let mut what = RaiseWhat::Nothing;
        let mut options = Vec::new();
        if self.at_op(";") {
            self.pos += 1;
            return Ok(PlStmt { kind: PlStmtKind::Raise { level, what, options }, line, offset: 0 });
        }
        if let Some(t) = self.peek() {
            let lv = match t.text.as_str() {
                "exception" if t.kind == TokKind::Word => Some(RaiseLevel::Exception),
                "warning" if t.kind == TokKind::Word => Some(RaiseLevel::Warning),
                "notice" if t.kind == TokKind::Word => Some(RaiseLevel::Notice),
                "info" if t.kind == TokKind::Word => Some(RaiseLevel::Info),
                "log" if t.kind == TokKind::Word => Some(RaiseLevel::Log),
                "debug" if t.kind == TokKind::Word => Some(RaiseLevel::Debug),
                _ => None,
            };
            if let Some(l) = lv {
                level = l;
                self.pos += 1;
            }
        }
        let t = self.peek().cloned().ok_or_else(|| self.msg_here("unexpected end of function definition"))?;
        let mut term_is_using = false;
        if t.kind == TokKind::Str {
            self.pos += 1;
            let format = t.text.clone();
            let mut args = Vec::new();
            let mut tok = self.next().ok_or_else(|| self.syntax_here())?;
            if !(tok.is_op(",") || tok.is_op(";") || tok.is_word("using")) {
                return Err(self.syntax_at(tok.start, Some(&tok)));
            }
            while tok.is_op(",") {
                let (e, t2) = self.read_expr(&[",", ";", "using"], ", or ; or USING")?;
                args.push(e);
                tok = t2;
            }
            term_is_using = tok.is_word("using");
            // check_raise_parameters
            let mut expected = 0usize;
            let b = format.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' {
                    if b.get(i + 1) == Some(&b'%') {
                        i += 1;
                    } else {
                        expected += 1;
                    }
                }
                i += 1;
            }
            if expected < args.len() {
                return Err(self.nopos(PgError::new(SqlState::SYNTAX_ERROR, "too many parameters specified for RAISE")));
            }
            if expected > args.len() {
                return Err(self.nopos(PgError::new(SqlState::SYNTAX_ERROR, "too few parameters specified for RAISE")));
            }
            what = RaiseWhat::Format { format, args };
        } else if t.is_word("using") {
            self.pos += 1;
            term_is_using = true;
        } else {
            self.pos += 1;
            if t.is_word("sqlstate") {
                let s = self.next().ok_or_else(|| self.syntax_here())?;
                if s.kind != TokKind::Str {
                    return Err(self.syntax_at(s.start, Some(&s)));
                }
                if s.text.len() != 5 || !s.text.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) {
                    return Err(self.msg_at_tok("invalid SQLSTATE code", &s));
                }
                what = RaiseWhat::SqlState(s.text);
            } else if t.kind == TokKind::Word && !RESERVED.contains(&t.text.as_str()) {
                match errcodes::sqlstate_of(&t.text) {
                    Some(code) => what = RaiseWhat::Condition { name: t.text.clone(), sqlstate: code.to_string() },
                    None => {
                        return Err(self.nopos(PgError::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("unrecognized exception condition \"{}\"", t.text),
                        )))
                    }
                }
            } else {
                return Err(self.syntax_at(t.start, Some(&t)));
            }
            let tok = self.next().ok_or_else(|| self.syntax_here())?;
            if tok.is_word("using") {
                term_is_using = true;
            } else if !tok.is_op(";") {
                return Err(self.syntax_at(tok.start, Some(&tok)));
            }
        }
        if term_is_using {
            loop {
                let o = self.next().ok_or_else(|| self.msg_here("unexpected end of function definition"))?;
                let opt = match o.text.as_str() {
                    "errcode" => RaiseOption::Errcode,
                    "message" => RaiseOption::Message,
                    "detail" => RaiseOption::Detail,
                    "hint" => RaiseOption::Hint,
                    "column" => RaiseOption::Column,
                    "constraint" => RaiseOption::Constraint,
                    "datatype" => RaiseOption::Datatype,
                    "table" => RaiseOption::Table,
                    "schema" => RaiseOption::Schema,
                    _ => return Err(self.msg_at_tok("unrecognized RAISE statement option", &o)),
                };
                let eq = self.next().ok_or_else(|| self.syntax_here())?;
                if !(eq.is_op("=") || eq.is_op(":=")) {
                    return Err(self.msg_at_tok("syntax error, expected \"=\"", &eq));
                }
                let (e, term) = self.read_expr(&[",", ";"], ", or ;")?;
                options.push((opt, e));
                if term.is_op(";") {
                    break;
                }
            }
        }
        Ok(PlStmt { kind: PlStmtKind::Raise { level, what, options }, line, offset: 0 })
    }

    fn parse_getdiag(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        let mut stacked = false;
        if self.at_word("current") {
            self.pos += 1;
        } else if self.at_word("stacked") {
            self.pos += 1;
            stacked = true;
        }
        self.expect_word("diagnostics")?;
        let mut items = Vec::new();
        loop {
            let target = self.read_target()?;
            if !(self.at_op("=") || self.at_op(":=")) {
                return Err(self.syntax_here());
            }
            self.pos += 1;
            let it = self.next().ok_or_else(|| self.syntax_here())?;
            let item = match it.text.as_str() {
                "row_count" => DiagItem::RowCount,
                "pg_context" => DiagItem::PgContext,
                "pg_routine_oid" => DiagItem::PgRoutineOid,
                "returned_sqlstate" => DiagItem::ReturnedSqlstate,
                "message_text" => DiagItem::MessageText,
                "pg_exception_detail" => DiagItem::PgExceptionDetail,
                "pg_exception_hint" => DiagItem::PgExceptionHint,
                "pg_exception_context" => DiagItem::PgExceptionContext,
                "column_name" => DiagItem::ColumnName,
                "constraint_name" => DiagItem::ConstraintName,
                "pg_datatype_name" => DiagItem::PgDatatypeName,
                "table_name" => DiagItem::TableName,
                "schema_name" => DiagItem::SchemaName,
                _ => return Err(self.msg_at_tok("unrecognized GET DIAGNOSTICS item", &it)),
            };
            let stacked_only = !matches!(item, DiagItem::RowCount | DiagItem::PgContext | DiagItem::PgRoutineOid);
            if stacked && !stacked_only {
                return Err(perr(
                    PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("diagnostics item {} is not allowed in GET STACKED DIAGNOSTICS", it.text.to_uppercase()),
                    ),
                    Some(it.start),
                ));
            }
            if !stacked && stacked_only {
                return Err(perr(
                    PgError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("diagnostics item {} is not allowed in GET CURRENT DIAGNOSTICS", it.text.to_uppercase()),
                    ),
                    Some(it.start),
                ));
            }
            items.push((target, item));
            if self.at_op(",") {
                self.pos += 1;
                continue;
            }
            self.expect_semi()?;
            break;
        }
        Ok(PlStmt { kind: PlStmtKind::GetDiag { stacked, items }, line, offset: 0 })
    }

    fn parse_open(&mut self) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        let c = self.next().filter(|c| c.is_ident()).ok_or_else(|| self.syntax_here())?;
        let mut scroll = None;
        if self.at_word("no") && self.peek_at(1).is_some_and(|t| t.is_word("scroll")) {
            self.pos += 2;
            scroll = Some(false);
        } else if self.at_word("scroll") {
            self.pos += 1;
            scroll = Some(true);
        }
        if self.at_word("for") {
            self.pos += 1;
            if self.at_word("execute") {
                self.pos += 1;
                let (query, term) = self.read_expr(&["using", ";"], "USING or ;")?;
                let mut using = Vec::new();
                if term.is_word("using") {
                    loop {
                        let (e, t2) = self.read_expr(&[",", ";"], ", or ;")?;
                        using.push(e);
                        if t2.is_op(",") {
                            continue;
                        }
                        break;
                    }
                }
                return Ok(PlStmt {
                    kind: PlStmtKind::Open { cursor: c.text, scroll, query: Some(PlQuery::Execute { query, using }), args: None },
                    line,
                    offset: 0,
                });
            }
            let q = self.read_until_semi("missing SQL statement")?;
            return Ok(PlStmt { kind: PlStmtKind::Open { cursor: c.text, scroll, query: Some(PlQuery::Static(q)), args: None }, line, offset: 0 });
        }
        if scroll.is_some() {
            return Err(self.msg_here("syntax error, expected \"FOR\""));
        }
        let mut args = None;
        if self.at_op("(") {
            self.pos += 1;
            let (a, _) = self.read_expr(&[")"], ")")?;
            args = Some(a);
        }
        self.expect_semi()?;
        Ok(PlStmt { kind: PlStmtKind::Open { cursor: c.text, scroll: None, query: None, args }, line, offset: 0 })
    }

    fn parse_fetch(&mut self, is_move: bool) -> PResult<PlStmt> {
        let line = self.line();
        self.pos += 1;
        // Direction (everything through FROM/IN), PG read_fetch_direction.
        let mut direction = FetchDirection::Next;
        let t = self.peek().cloned().ok_or_else(|| self.msg_here("unexpected end of function definition"))?;
        let mut check_from = true;
        let word = if t.kind == TokKind::Word { t.text.as_str() } else { "" };
        let read_count = |p: &mut Parser| -> PResult<PlSql> { Ok(p.read_expr(&["from", "in"], "FROM or IN")?.0) };
        match word {
            "next" => {
                self.pos += 1;
            }
            "prior" => {
                self.pos += 1;
                direction = FetchDirection::Prior;
            }
            "first" => {
                self.pos += 1;
                direction = FetchDirection::First;
            }
            "last" => {
                self.pos += 1;
                direction = FetchDirection::Last;
            }
            "absolute" => {
                self.pos += 1;
                direction = FetchDirection::Absolute(read_count(self)?);
                check_from = false;
            }
            "relative" => {
                self.pos += 1;
                direction = FetchDirection::Relative(read_count(self)?);
                check_from = false;
            }
            "all" => {
                self.pos += 1;
                direction = FetchDirection::ForwardAll;
            }
            "forward" | "backward" => {
                self.pos += 1;
                let fwd = word == "forward";
                let nt = self.peek().cloned();
                if nt.as_ref().is_some_and(|n| n.is_word("all")) {
                    self.pos += 1;
                    direction = if fwd { FetchDirection::ForwardAll } else { FetchDirection::BackwardAll };
                } else if nt.as_ref().is_some_and(|n| n.is_word("from") || n.is_word("in")) {
                    direction = if fwd { FetchDirection::Forward(None) } else { FetchDirection::Backward(None) };
                } else if nt.as_ref().is_some_and(|n| n.is_ident() && self.peek_at(1).is_some_and(|x| x.is_word("into") || x.is_op(";"))) {
                    // `FORWARD cursor INTO ...`: no count.
                    direction = if fwd { FetchDirection::Forward(None) } else { FetchDirection::Backward(None) };
                    check_from = false;
                } else {
                    let n = read_count(self)?;
                    direction = if fwd { FetchDirection::Forward(Some(n)) } else { FetchDirection::Backward(Some(n)) };
                    check_from = false;
                }
            }
            "from" | "in" => {
                check_from = false;
                self.pos += 1;
            }
            _ => {
                // No direction: the token is the cursor name.
                check_from = false;
            }
        }
        if check_from {
            if self.at_word("from") || self.at_word("in") {
                self.pos += 1;
            } else if !(word == "next" || word == "prior" || word == "first" || word == "last" || word == "all") {
                return Err(self.msg_here("expected FROM or IN"));
            }
        }
        let c = self.next().filter(|c| c.is_ident()).ok_or_else(|| self.syntax_here())?;
        let mut targets = Vec::new();
        if !is_move {
            if matches!(direction, FetchDirection::ForwardAll | FetchDirection::BackwardAll)
                || matches!(&direction, FetchDirection::Forward(Some(_)) | FetchDirection::Backward(Some(_)))
                    && !matches!(&direction, FetchDirection::Forward(Some(s)) | FetchDirection::Backward(Some(s)) if s.text.trim() == "1")
            {
                return Err(perr(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "FETCH statement cannot return multiple rows"), Some(t.start)));
            }
            self.expect_word("into")?;
            let (_, ts) = self.read_into_targets()?;
            targets = ts;
        }
        self.expect_semi()?;
        Ok(PlStmt { kind: PlStmtKind::Fetch { cursor: c.text, direction, targets, is_move }, line, offset: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_block() {
        let f = parse_body("DECLARE x int := 1; BEGIN x := x + 1; RAISE NOTICE 'x = %', x; RETURN x; END").expect("parse");
        assert_eq!(f.block.decls.len(), 1);
        assert_eq!(f.block.body.len(), 3);
        assert!(matches!(f.block.body[0].kind, PlStmtKind::Assign { .. }));
    }

    #[test]
    fn into_is_cut_out_of_select() {
        let f = parse_body("BEGIN SELECT a INTO STRICT v FROM t; END").expect("parse");
        match &f.block.body[0].kind {
            PlStmtKind::ExecSql { sql, into: Some((true, t)), .. } => {
                assert_eq!(t.len(), 1);
                assert!(sql.text.starts_with("SELECT a "));
                assert!(sql.text.ends_with("FROM t"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn insert_into_is_not_a_target() {
        let f = parse_body("BEGIN INSERT INTO t VALUES (1) RETURNING id INTO v; END").expect("parse");
        assert!(matches!(&f.block.body[0].kind, PlStmtKind::ExecSql { into: Some(_), .. }));
    }

    #[test]
    fn for_loops() {
        let f = parse_body("BEGIN FOR i IN REVERSE 10..1 BY 2 LOOP NULL; END LOOP; FOR r IN SELECT 1 LOOP END LOOP; END").expect("parse");
        assert!(matches!(&f.block.body[0].kind, PlStmtKind::ForInt { reverse: true, step: Some(_), .. }));
        assert!(matches!(&f.block.body[1].kind, PlStmtKind::ForQuery { .. }));
    }

    #[test]
    fn end_label_mismatch() {
        let e = parse_body("<<a>> BEGIN END b").err().expect("error");
        assert!(e.error.message.contains("differs"));
    }

    #[test]
    fn raise_parameter_count() {
        assert!(parse_body("BEGIN RAISE NOTICE '% %', 1; END").is_err());
        assert!(parse_body("BEGIN RAISE NOTICE '%%', 1; END").is_err());
        assert!(parse_body("BEGIN RAISE NOTICE '%', 1; END").is_ok());
    }
}
