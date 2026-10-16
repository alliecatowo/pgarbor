//! Tokenizer for PL/pgSQL bodies (PG `pl_scanner.c` on top of the core SQL scanner `scan.l`).
//! Only token boundaries matter: SQL fragments are passed on as source text.
use pgarbor_common::{PgError, Result, SqlState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokKind {
    /// Unquoted word (`text` = folded to lower case).
    Word,
    /// `"Quoted"` identifier (`text` = unquoted, case kept).
    QuotedIdent,
    /// String constant of any form (`text` = decoded value for plain/E/dollar strings).
    Str,
    Number,
    /// `$n`.
    Param,
    /// Operator or punctuation: `;` `,` `(` `)` `[` `]` `.` `..` `:=` `::` `=` `<<` `>>` and operator runs.
    Op,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tok {
    pub kind: TokKind,
    pub text: String,
    /// Byte range in the body.
    pub start: usize,
    pub end: usize,
    /// 1-based line number of `start`.
    pub line: u32,
}

impl Tok {
    pub fn is_word(&self, w: &str) -> bool {
        self.kind == TokKind::Word && self.text == w
    }
    pub fn is_op(&self, op: &str) -> bool {
        self.kind == TokKind::Op && self.text == op
    }
    /// A word or quoted identifier.
    pub fn is_ident(&self) -> bool {
        matches!(self.kind, TokKind::Word | TokKind::QuotedIdent)
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (c as u32) >= 0x80
}

fn is_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || (c as u32) >= 0x80
}

const OP_CHARS: &str = "~!@#^&|`?+-*/%<>=";

/// Syntax error at byte `at` of the body (position resolved by the caller).
pub fn lex_error(msg: &str, at: usize) -> PgError {
    let mut e = PgError::new(SqlState::SYNTAX_ERROR, msg.to_string());
    // Temporarily carry the byte offset; `parse` converts it to a character position.
    e.internal_position = Some(at as u32);
    e
}

/// Tokenize `src` (comments and whitespace dropped).
pub fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line: u32 = 1;
    let n = bytes.len();
    let count_lines = |s: &str| s.bytes().filter(|b| *b == b'\n').count() as u32;
    while i < n {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // -- comment
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // /* nested comment */
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            let mut depth = 0;
            loop {
                if i >= n {
                    return Err(lex_error("unterminated /* comment", start));
                }
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        let start = i;
        let start_line = line;
        let ch = src[i..].chars().next().unwrap_or(' ');
        // String prefixes: E'..', B'..', X'..', N'..', U&'..'
        let lower = c.to_ascii_lowercase();
        if (lower == b'e' || lower == b'b' || lower == b'x' || lower == b'n') && bytes.get(i + 1) == Some(&b'\'') {
            let escapes = lower == b'e';
            let (val, end) = read_quoted(src, i + 1, escapes)?;
            line += count_lines(&src[i..end]);
            out.push(Tok { kind: TokKind::Str, text: val, start, end, line: start_line });
            i = end;
            continue;
        }
        if lower == b'u' && bytes.get(i + 1) == Some(&b'&') && bytes.get(i + 2) == Some(&b'\'') {
            let (val, end) = read_quoted(src, i + 2, false)?;
            line += count_lines(&src[i..end]);
            out.push(Tok { kind: TokKind::Str, text: val, start, end, line: start_line });
            i = end;
            continue;
        }
        if lower == b'u' && bytes.get(i + 1) == Some(&b'&') && bytes.get(i + 2) == Some(&b'"') {
            let (val, end) = read_quoted_ident(src, i + 2)?;
            out.push(Tok { kind: TokKind::QuotedIdent, text: val, start, end, line: start_line });
            i = end;
            continue;
        }
        if c == b'\'' {
            let (val, end) = read_quoted(src, i, false)?;
            line += count_lines(&src[i..end]);
            out.push(Tok { kind: TokKind::Str, text: val, start, end, line: start_line });
            i = end;
            continue;
        }
        if c == b'"' {
            let (val, end) = read_quoted_ident(src, i)?;
            line += count_lines(&src[i..end]);
            out.push(Tok { kind: TokKind::QuotedIdent, text: val, start, end, line: start_line });
            i = end;
            continue;
        }
        if c == b'$' {
            // $n parameter
            let mut j = i + 1;
            while j < n && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                out.push(Tok { kind: TokKind::Param, text: src[i..j].to_string(), start, end: j, line: start_line });
                i = j;
                continue;
            }
            // $tag$ dollar quote
            let mut k = i + 1;
            while k < n {
                let ck = src[k..].chars().next().unwrap_or(' ');
                if ck == '$' {
                    break;
                }
                if !(is_ident_cont(ck) && ck != '$') || (k == i + 1 && ck.is_ascii_digit()) {
                    break;
                }
                k += ck.len_utf8();
            }
            if k < n && bytes[k] == b'$' {
                let tag = &src[i..=k];
                let body_start = k + 1;
                match src[body_start..].find(tag) {
                    Some(rel) => {
                        let val = src[body_start..body_start + rel].to_string();
                        let end = body_start + rel + tag.len();
                        line += count_lines(&src[i..end]);
                        out.push(Tok { kind: TokKind::Str, text: val, start, end, line: start_line });
                        i = end;
                        continue;
                    }
                    None => return Err(lex_error("unterminated dollar-quoted string", i)),
                }
            }
            // Lone `$`: treat as an operator character.
            out.push(Tok { kind: TokKind::Op, text: "$".into(), start, end: i + 1, line: start_line });
            i += 1;
            continue;
        }
        if is_ident_start(ch) {
            let mut j = i;
            while j < n {
                let cj = src[j..].chars().next().unwrap_or(' ');
                if !is_ident_cont(cj) {
                    break;
                }
                j += cj.len_utf8();
            }
            let word = src[i..j].to_lowercase();
            out.push(Tok { kind: TokKind::Word, text: word, start, end: j, line: start_line });
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut j = i;
            while j < n && bytes[j].is_ascii_digit() {
                j += 1;
            }
            // `1..10` is integer `1` followed by `..`
            if j < n && bytes[j] == b'.' && bytes.get(j + 1) != Some(&b'.') {
                j += 1;
                while j < n && bytes[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j < n && (bytes[j] == b'e' || bytes[j] == b'E') {
                let mut k = j + 1;
                if k < n && (bytes[k] == b'+' || bytes[k] == b'-') {
                    k += 1;
                }
                if k < n && bytes[k].is_ascii_digit() {
                    while k < n && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    j = k;
                }
            }
            // Underscore digit separators / trailing identifier chars are left to the SQL parser.
            while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            out.push(Tok { kind: TokKind::Number, text: src[i..j].to_string(), start, end: j, line: start_line });
            i = j;
            continue;
        }
        // Punctuation and operators.
        let two = src.get(i..i + 2).unwrap_or("");
        let single = |t: &str| t.to_string();
        let (text, len) = match c {
            b';' | b',' | b'(' | b')' | b'[' | b']' => (single(&src[i..i + 1]), 1),
            b'.' if two == ".." => ("..".to_string(), 2),
            b'.' => (".".to_string(), 1),
            b':' if two == ":=" => (":=".to_string(), 2),
            b':' if two == "::" => ("::".to_string(), 2),
            b':' => (":".to_string(), 1),
            _ if OP_CHARS.as_bytes().contains(&c) => {
                let mut j = i;
                while j < n && OP_CHARS.as_bytes().contains(&bytes[j]) {
                    // Stop before the start of a comment.
                    if (bytes[j] == b'-' && bytes.get(j + 1) == Some(&b'-')) || (bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*')) {
                        if j > i {
                            break;
                        }
                    }
                    j += 1;
                }
                let mut op = &src[i..j];
                // PG: `<<` and `>>` are always split out as label brackets by pl_scanner when they are
                // the whole operator; longer runs keep their meaning.
                // Trailing `+`/`-` of a multi-character operator belong to the next token unless the
                // operator contains one of ~ ! @ # % ^ & | ` ? (scan.l rule).
                while op.len() > 1 && (op.ends_with('+') || op.ends_with('-')) && !op.chars().any(|ch| "~!@#%^&|`?".contains(ch)) {
                    op = &op[..op.len() - 1];
                }
                (op.to_string(), op.len())
            }
            _ => (src[i..i + ch.len_utf8()].to_string(), ch.len_utf8()),
        };
        out.push(Tok { kind: TokKind::Op, text, start, end: i + len, line: start_line });
        i += len;
    }
    Ok(out)
}

/// Read a '...' string starting at the quote at `q`; returns (decoded value, end byte).
fn read_quoted(src: &str, q: usize, escapes: bool) -> Result<(String, usize)> {
    let bytes = src.as_bytes();
    let mut i = q + 1;
    let mut val = String::new();
    loop {
        if i >= bytes.len() {
            return Err(lex_error("unterminated quoted string", q));
        }
        let c = bytes[i];
        if c == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                val.push('\'');
                i += 2;
                continue;
            }
            // Adjacent string continuation across a newline ('a'\n'b') is rare in PL bodies; not merged.
            return Ok((val, i + 1));
        }
        if escapes && c == b'\\' {
            if let Some(&nx) = bytes.get(i + 1) {
                match nx {
                    b'n' => val.push('\n'),
                    b't' => val.push('\t'),
                    b'r' => val.push('\r'),
                    b'b' => val.push('\u{8}'),
                    b'f' => val.push('\u{c}'),
                    _ => {
                        let ch = src[i + 1..].chars().next().unwrap_or('\\');
                        val.push(ch);
                        i += 1 + ch.len_utf8();
                        continue;
                    }
                }
                i += 2;
                continue;
            }
        }
        let ch = src[i..].chars().next().unwrap_or(' ');
        val.push(ch);
        i += ch.len_utf8();
    }
}

fn read_quoted_ident(src: &str, q: usize) -> Result<(String, usize)> {
    let bytes = src.as_bytes();
    let mut i = q + 1;
    let mut val = String::new();
    loop {
        if i >= bytes.len() {
            return Err(lex_error("unterminated quoted identifier", q));
        }
        if bytes[i] == b'"' {
            if bytes.get(i + 1) == Some(&b'"') {
                val.push('"');
                i += 2;
                continue;
            }
            return Ok((val, i + 1));
        }
        let ch = src[i..].chars().next().unwrap_or(' ');
        val.push(ch);
        i += ch.len_utf8();
    }
}
