//! Incremental SQL script splitter shared by `shell`, `exec` and `import` (lease CLI-04).
//! Splits text into executable items the way psql's lexer (`psql/psqlscan.l`) and `pg_dump` plain
//! output require, WITHOUT parsing SQL.
//!
//! ## API
//! `ScriptSplitter::new()`, then `push_line(line)` once per input line (line includes its `\n`; the
//! last line may lack it), collecting returned [`ScriptItem`]s; `finish()` flushes a trailing
//! statement without `;`. `state()` tells an interactive prompt what is open.
//!
//! ## Statement boundaries
//! A statement ends at a `;` that is outside: single-quoted strings (`'it''s'`, also `E'..\'..'`
//! where `\` escapes the next char), double-quoted identifiers (`"a""b"`), dollar-quoted bodies
//! (`$$..$$`, `$tag$..$tag$`; a tag is `[A-Za-z_][A-Za-z0-9_]*` or empty, and `$` directly after an
//! identifier character is NOT a tag start), line comments (`-- ... \n`), block comments (nested
//! `/* /* */ */`) and parentheses (depth counting: a `;` inside `( ... )` does not end the statement;
//! this makes `CREATE RULE ... DO ALSO (a; b)` work).
//! The returned statement text excludes the final `;`, is trimmed, and keeps interior comments.
//! Statements consisting only of whitespace/comments are dropped.
//!
//! ## psql meta lines (only when the buffer is empty = at statement start)
//! A line whose first non-blank char is `\` yields `ItemKind::Meta(line_without_newline)` (e.g.
//! `\connect x`, `\echo hi`, `\.`, `\q`, `\d t`); `\\` inside a statement is not special (kept).
//! Dump files from pg_dump contain `\connect`, `\restrict`, `\unrestrict`: callers decide what to
//! do with metas (`import` ignores unknown ones with a warning).
//!
//! ## COPY FROM stdin blocks (`pg_dump` plain format)
//! After a completed statement whose first word is `COPY` (case-insensitive, after comments) and which
//! contains the word sequence `FROM STDIN` outside quotes, the splitter enters data mode: following
//! lines are data until a line that is exactly `\.` (optionally followed by `\r`). It then emits ONE
//! item `ItemKind::CopyFromStdin { data }` with `text` = the COPY statement and `data` = the raw
//! bytes of the data lines (each with its `\n`, WITHOUT the `\.` line). EOF inside data mode:
//! [`ScriptError`] `unterminated COPY data block`.
//!
//! ## Positions
//! `ScriptItem::line` is the 1-based line where the item's first non-whitespace char appears;
//! `index` counts items from 1.
//! Errors (returned by `finish()`): unterminated quote/comment/dollar quote at EOF:
//! `unterminated quoted string at end of input` / `unterminated /* comment` /
//! `unterminated dollar-quoted string`, with the starting line.
use pgarbor_common::PgError;
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
    /// Nothing buffered.
    Empty,
    /// Statement text buffered, not terminated (`-` continuation prompt).
    InStatement,
    InSingleQuote,
    InDoubleQuote,
    InDollarQuote(String),
    InBlockComment(u32),
    InParen(u32),
    /// Reading COPY data.
    InCopyData,
}

/// Splitter state machine (fields are private to the implementation; the worker may add private
/// fields freely).
#[derive(Debug, Default)]
pub struct ScriptSplitter {
    pub(crate) buf: String,
    pub(crate) line_no: u64,
    pub(crate) item_no: u64,
}

impl ScriptSplitter {
    pub fn new() -> ScriptSplitter {
        ScriptSplitter::default()
    }

    /// Feed one line (module docs). Returns the items completed by this line (0, 1 or several:
    /// `select 1; select 2;` yields two).
    #[allow(unused_variables)]
    pub fn push_line(&mut self, line: &str) -> Result<Vec<ScriptItem>, CliError> {
        Err(CliError::from(PgError::unimplemented("CLI-04", "script splitting (ScriptSplitter::push_line)")))
    }

    /// End of input: flush a trailing unterminated statement, or fail on unterminated constructs.
    pub fn finish(&mut self) -> Result<Vec<ScriptItem>, CliError> {
        Err(CliError::from(PgError::unimplemented("CLI-04", "script splitting end (ScriptSplitter::finish)")))
    }

    /// Current lexical state for prompts.
    pub fn state(&self) -> SplitState {
        SplitState::Empty
    }
}

/// Split a whole script (convenience over the incremental API: iterate `str::split_inclusive('\n')`).
#[allow(unused_variables)]
pub fn split_script(text: &str) -> Result<Vec<ScriptItem>, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-04", "whole-script splitting (sql_script::split_script)")))
}
