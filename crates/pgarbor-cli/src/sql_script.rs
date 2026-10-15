//! Incremental SQL script splitter shared by `shell`, `exec` and `import` (lease CLI-04).
//! It recognizes statement boundaries without attempting to parse SQL.
use crate::ctx::CliError;

/// One executable unit.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptItem {
    pub kind: ItemKind,
    /// Statement text without the trailing `;` (for `Meta`: the whole line).
    pub text: String,
    pub line: u64,
    pub index: u64,
}

/// Item kinds.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Statement,
    /// `text` is the `COPY ... FROM stdin` statement; the payload follows.
    CopyFromStdin { data: Vec<u8> },
    /// A backslash command line.
    Meta,
}

/// What an interactive prompt should show.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitState {
    Empty,
    InStatement,
    InSingleQuote,
    InDoubleQuote,
    InDollarQuote(String),
    InBlockComment(u32),
    InParen(u32),
    InCopyData,
}

#[derive(Debug, Default)]
pub struct ScriptSplitter {
    pub(crate) buf: String,
    pub(crate) line_no: u64,
    pub(crate) item_no: u64,
    start_line: u64,
    copy_data: Option<Vec<u8>>,
    copy_sql: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LexState {
    Normal,
    Single(bool), // backslash escapes enabled (E'...')
    Double,
    Dollar(String),
    Block(u32),
    LineComment,
}

fn identifier_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '_' || c == '$' }

fn dollar_delimiter(chars: &[char], i: usize) -> Option<String> {
    if chars.get(i) != Some(&'$') || (i > 0 && identifier_char(chars[i - 1])) { return None; }
    let mut j = i + 1;
    if chars.get(j) == Some(&'$') { return Some("$$".to_owned()); }
    if !chars.get(j).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') { return None; }
    j += 1;
    while chars.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') { j += 1; }
    if chars.get(j) == Some(&'$') { Some(chars[i..=j].iter().collect()) } else { None }
}

fn lex(text: &str) -> (Vec<usize>, LexState, u32, bool) {
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut state = LexState::Normal;
    let mut parens = 0u32;
    let mut semis = Vec::new();
    let mut meaningful = false;
    while i < c.len() {
        match &mut state {
            LexState::Normal => {
                if c[i].is_whitespace() { i += 1; continue; }
                if c[i] == '-' && c.get(i + 1) == Some(&'-') { state = LexState::LineComment; i += 2; continue; }
                if c[i] == '/' && c.get(i + 1) == Some(&'*') { state = LexState::Block(1); i += 2; continue; }
                if c[i] == '\'' {
                    let escape = i > 0 && matches!(c[i - 1], 'e' | 'E') && (i < 2 || !identifier_char(c[i - 2]));
                    state = LexState::Single(escape); meaningful = true; i += 1; continue;
                }
                if c[i] == '"' { state = LexState::Double; meaningful = true; i += 1; continue; }
                if c[i] == '$' {
                    if let Some(tag) = dollar_delimiter(&c, i) { i += tag.chars().count(); state = LexState::Dollar(tag); meaningful = true; continue; }
                }
                match c[i] {
                    '(' => { parens = parens.saturating_add(1); meaningful = true; }
                    ')' => { parens = parens.saturating_sub(1); meaningful = true; }
                    ';' if parens == 0 => semis.push(i),
                    _ => meaningful = true,
                }
                i += 1;
            }
            LexState::Single(escape) => {
                if *escape && c[i] == '\\' { i = (i + 2).min(c.len()); continue; }
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') { i += 2; } else { state = LexState::Normal; i += 1; }
                } else { i += 1; }
            }
            LexState::Double => {
                if c[i] == '"' { if c.get(i + 1) == Some(&'"') { i += 2; } else { state = LexState::Normal; i += 1; } } else { i += 1; }
            }
            LexState::Dollar(tag) => {
                if c[i..].starts_with(&tag.chars().collect::<Vec<_>>()) { i += tag.chars().count(); state = LexState::Normal; } else { i += 1; }
            }
            LexState::Block(n) => {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') { *n = n.saturating_add(1); i += 2; }
                else if c[i] == '*' && c.get(i + 1) == Some(&'/') { *n -= 1; i += 2; if *n == 0 { state = LexState::Normal; } }
                else { i += 1; }
            }
            LexState::LineComment => { if c[i] == '\n' { state = LexState::Normal; } i += 1; }
        }
    }
    let depth = parens;
    (semis, state, depth, meaningful)
}

fn copy_from_stdin(sql: &str) -> bool {
    let mut clean = String::new();
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut state = LexState::Normal;
    while i < chars.len() {
        match &mut state {
            LexState::Normal => {
                if chars[i] == '-' && chars.get(i + 1) == Some(&'-') { state = LexState::LineComment; i += 2; }
                else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') { state = LexState::Block(1); i += 2; }
                else if chars[i] == '\'' { state = LexState::Single(false); clean.push(' '); i += 1; }
                else if chars[i] == '"' { state = LexState::Double; clean.push(' '); i += 1; }
                else if chars[i] == '$' { if let Some(tag) = dollar_delimiter(&chars, i) { state = LexState::Dollar(tag.clone()); clean.push(' '); i += tag.len(); } else { clean.push(chars[i]); i += 1; } }
                else { clean.push(chars[i]); i += 1; }
            }
            LexState::Single(_) => { if chars[i] == '\'' { if chars.get(i + 1) == Some(&'\'') { i += 2; } else { state = LexState::Normal; i += 1; } } else { i += 1; } }
            LexState::Double => { if chars[i] == '"' { if chars.get(i + 1) == Some(&'"') { i += 2; } else { state = LexState::Normal; i += 1; } } else { i += 1; } }
            LexState::Dollar(tag) => { if chars[i..].starts_with(&tag.chars().collect::<Vec<_>>()) { i += tag.len(); state = LexState::Normal; } else { i += 1; } }
            LexState::Block(n) => { if chars[i] == '/' && chars.get(i + 1) == Some(&'*') { *n += 1; i += 2; } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') { *n -= 1; i += 2; if *n == 0 { state = LexState::Normal; } } else { i += 1; } }
            LexState::LineComment => { if chars[i] == '\n' { clean.push(' '); state = LexState::Normal; } i += 1; }
        }
    }
    let words: Vec<String> = clean.split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
    words.first().is_some_and(|w| w == "COPY") && words.windows(2).any(|w| w[0] == "FROM" && w[1] == "STDIN")
}

impl ScriptSplitter {
    pub fn new() -> ScriptSplitter { ScriptSplitter::default() }

    pub fn push_line(&mut self, line: &str) -> Result<Vec<ScriptItem>, CliError> {
        self.line_no = self.line_no.saturating_add(1);
        if let Some(data) = &mut self.copy_data {
            if line.trim_end_matches(['\n', '\r']) == "\\." {
                let bytes = match self.copy_data.take() { Some(bytes) => bytes, None => Vec::new() };
                let text = match self.copy_sql.take() { Some(text) => text, None => String::new() };
                self.buf.clear();
                self.item_no += 1;
                return Ok(vec![ScriptItem { kind: ItemKind::CopyFromStdin { data: bytes }, text, line: self.start_line, index: self.item_no }]);
            }
            data.extend_from_slice(line.as_bytes());
            return Ok(Vec::new());
        }
        if self.buf.trim().is_empty() {
            if let Some(pos) = line.find(|c: char| !c.is_whitespace()) {
                if line[pos..].starts_with('\\') {
                    self.buf.clear();
                    self.item_no += 1;
                    return Ok(vec![ScriptItem { kind: ItemKind::Meta, text: line.trim_end_matches(['\n', '\r']).to_owned(), line: self.line_no, index: self.item_no }]);
                }
            }
        }
        if self.buf.is_empty() { self.start_line = self.line_no; }
        self.buf.push_str(line);
        let (semis, _, _, _) = lex(&self.buf);
        let mut items = Vec::new();
        let mut consumed_chars = 0usize;
        for semi in semis {
            let prefix: String = self.buf.chars().take(semi).skip(consumed_chars).collect();
            let (_, _, _, meaningful) = lex(&prefix);
            if meaningful {
                let sql = prefix.trim().to_owned();
                if copy_from_stdin(&sql) {
                    self.copy_sql = Some(sql);
                    self.buf.clear();
                    self.copy_data = Some(Vec::new());
                    return Ok(items);
                }
                self.item_no += 1;
                let line = self.start_line.saturating_add(self.buf.chars().take(semi).filter(|c| *c == '\n').count() as u64);
                items.push(ScriptItem { kind: ItemKind::Statement, text: sql, line, index: self.item_no });
            }
            consumed_chars = semi + 1;
        }
        if consumed_chars > 0 {
            let chars: Vec<char> = self.buf.chars().collect();
            let removed_newlines = chars.iter().take(consumed_chars).filter(|c| **c == '\n').count() as u64;
            let remainder: String = chars.into_iter().skip(consumed_chars).collect();
            let leading_newlines = remainder.chars().take_while(|c| c.is_whitespace()).filter(|c| *c == '\n').count() as u64;
            self.buf = remainder.trim_start().to_owned();
            self.start_line = self.start_line.saturating_add(removed_newlines + leading_newlines);
        }
        Ok(items)
    }

    pub fn finish(&mut self) -> Result<Vec<ScriptItem>, CliError> {
        if self.copy_data.is_some() { return Err(CliError::data("unterminated COPY data block").with_detail(format!("line {}", self.start_line))); }
        let (_, state, _depth, meaningful) = lex(&self.buf);
        let message = match state {
            LexState::Single(_) | LexState::Double => Some("unterminated quoted string at end of input"),
            LexState::Dollar(_) => Some("unterminated dollar-quoted string"),
            LexState::Block(_) => Some("unterminated /* comment"),
            _ => None,
        };
        if let Some(m) = message { return Err(CliError::data(m).with_detail(format!("line {}", self.start_line))); }
        if meaningful {
            self.item_no += 1;
            let item = ScriptItem { kind: ItemKind::Statement, text: std::mem::take(&mut self.buf).trim().to_owned(), line: self.start_line, index: self.item_no };
            Ok(vec![item])
        } else { self.buf.clear(); Ok(Vec::new()) }
    }

    pub fn state(&self) -> SplitState {
        if self.copy_data.is_some() { return SplitState::InCopyData; }
        if self.buf.is_empty() { return SplitState::Empty; }
        let (_, state, depth, meaningful) = lex(&self.buf);
        match state {
            LexState::Single(_) => SplitState::InSingleQuote,
            LexState::Double => SplitState::InDoubleQuote,
            LexState::Dollar(tag) => SplitState::InDollarQuote(tag),
            LexState::Block(n) => SplitState::InBlockComment(n),
            LexState::LineComment => if meaningful { SplitState::InStatement } else { SplitState::Empty },
            LexState::Normal if depth > 0 => SplitState::InParen(depth),
            LexState::Normal if meaningful => SplitState::InStatement,
            _ => SplitState::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_boundaries_and_quoted_semicolons() {
        let items = split_script("select ';'; select 2 -- ;\n; select $$;$$; select (1; 2);").expect("valid script");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].text, "select ';'");
        assert_eq!(items[1].text, "select 2 -- ;");
        assert_eq!(items[2].text, "select $$;$$");
        assert_eq!(items[3].text, "select (1; 2)");
    }

    #[test]
    fn handles_meta_trailing_and_nested_comments() {
        let items = split_script("\\echo hi\n/* outer /* nested */ end */ select 1").expect("valid script");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::Meta);
        assert_eq!(items[1].text, "/* outer /* nested */ end */ select 1");
    }

    #[test]
    fn copy_block_collects_raw_lines() {
        let items = split_script("COPY t FROM stdin;\na\tb\n\\.\n").expect("valid COPY");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "COPY t FROM stdin");
        assert_eq!(items[0].kind, ItemKind::CopyFromStdin { data: b"a\tb\n".to_vec() });
    }

    #[test]
    fn unterminated_quote_reports_data_error() {
        let err = split_script("select 'open").expect_err("unterminated quote");
        assert_eq!(err.exit, crate::exit::Exit::DATA_ERR);
        assert!(err.message.contains("unterminated quoted string"));
    }

    #[test]
    fn prompt_state_tracks_lexical_context() {
        let mut splitter = ScriptSplitter::new();
        splitter.push_line("select (1\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InParen(1));
        splitter.push_line(" + 2)\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InStatement);
    }

    #[test]
    fn handles_escape_quotes_and_semicolons_in_nested_comments() {
        let items = split_script(r"select E'it\'s; ok'; /* outer /* ; */ still */ select 2;")
            .expect("valid escaped string and nested comment");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, r"select E'it\'s; ok'");
        assert!(items[1].text.contains("/* outer /* ; */ still */"));
    }

    #[test]
    fn prompt_state_reports_quote_comment_and_dollar_tag() {
        let mut splitter = ScriptSplitter::new();
        splitter.push_line("select 'open\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InSingleQuote);
        splitter.buf.clear();
        splitter.push_line("select \"open\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InDoubleQuote);
        splitter.buf.clear();
        splitter.push_line("select $body$open\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InDollarQuote("$body$".to_owned()));
        splitter.buf.clear();
        splitter.push_line("/* outer /* inner\n").expect("line");
        assert_eq!(splitter.state(), SplitState::InBlockComment(2));
    }

    #[test]
    fn unterminated_constructs_report_the_buffer_start_line() {
        for sql in ["select 'open", "select \"open", "select $tag$open", "select /* open"] {
            let err = split_script(sql).expect_err("unterminated lexical construct");
            assert_eq!(err.exit, crate::exit::Exit::DATA_ERR);
            assert!(err.detail.as_deref().is_some_and(|detail| detail.contains("line 1")));
        }
    }
}

pub fn split_script(text: &str) -> Result<Vec<ScriptItem>, CliError> {
    let mut splitter = ScriptSplitter::new();
    let mut result = Vec::new();
    for line in text.split_inclusive('\n') { result.extend(splitter.push_line(line)?); }
    result.extend(splitter.finish()?);
    Ok(result)
}
