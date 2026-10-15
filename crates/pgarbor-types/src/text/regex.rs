//! POSIX regular expressions (PostgreSQL ARE flavor) and the regexp_* functions (TYPE-13).
//! Lease TYP-17 (opus). See amendment request `contracts/amendments/pgarbor-types-01-regex-engine.md`.
//!
//! PG18 reference: `src/backend/regex/*` (Henry Spencer's ARE engine: `regcomp.c`, `regexec.c`),
//! `utils/adt/regexp.c` (SQL functions, flag parsing, `regexp_matches` setup, replacement strings),
//! docs: "9.7.3 POSIX Regular Expressions" (ARE syntax, escapes `\d \s \w \m \M \y \Y`, `[[:alpha:]]`,
//! back references `\1`, lookahead/lookbehind `(?=` `(?!` `(?<=` `(?<!`, non-greedy quantifiers,
//! embedded options `(?i)`, `***=` literal prefix).
//!
//! No regex crate is a dependency (frozen deps). This lease writes a self-contained backtracking
//! engine implementing ARE semantics, including PG's leftmost-LONGEST rule for the overall match when
//! the RE's greediness is "greedy" (PG determines a whole-RE greediness from its first quantifier;
//! see regex README "Greediness"). Matching works on chars (code points), case-insensitivity via the
//! simple lowercase mapping.
//!
//! Owned procs (ImplModule::Regex): `textregexeq textregexne texticregexeq texticregexne nameregexeq
//! nameregexne nameicregexeq nameicregexne` (`~ !~ ~* !~*`), `textregexsubstr` (`substring(s from re)`:
//! first parenthesized subexpression if any, else whole match; NULL if no match), SQL-language
//! `substring(text, text, text)` (oid 2074, SIMILAR form: `textregexsubstr(s, similar_to_escape(p, e))`),
//! `textregexreplace_noopt textregexreplace textregexreplace_extended* ` (replacement escapes `\1`..`\9`, `\&`,
//! `\\`; flag `g` replaces all; PG16 start/N args), `regexp_match(_no_flags)` (text[] of captures or the
//! whole match; NULL when none), `regexp_matches(_no_flags)` (SRF of text[]; flag `g` = all matches),
//! `regexp_split_to_array(_no_flags)`, `regexp_split_to_table(_no_flags)` (SRF), `regexp_count*`,
//! `regexp_instr*`, `regexp_like*`, `regexp_substr*`.
//!
//! Flags (string arg): `i` case-insensitive, `c` case-sensitive (default), `g` global (only where
//! allowed; `regexp_match` with `g` → 22023 `regexp_match() does not support the "global" option`
//! with HINT `Use the regexp_matches function instead.`), `n`/`m` newline-sensitive, `s` (default),
//! `p` partial newline-sensitive, `w` inverse partial, `x` expanded syntax, `q` literal pattern,
//! `b` BRE, `e` ERE, `t` tight. Unknown → 22023 `invalid regular expression option: "z"`.
//! Compile errors → 2201B `invalid regular expression: <reason>` with PG's reason strings, e.g.
//! `parentheses () not balanced`, `brackets [] not balanced`, `invalid repetition count(s)`,
//! `quantifier operand invalid`, `invalid escape \ sequence`, `invalid backreference number`.
//!
//! Implementation notes (TYP-17):
//! - The engine parses the pattern to an AST, compiles it to a small instruction set and runs it
//!   on an explicit backtracking stack (no recursion except for look-around bodies), over `char`s.
//! - Overall match length follows PG: when the RE is "greedy" (decided by its first quantifier) the
//!   leftmost start is kept and every alternative path is explored to find the LONGEST match (paths
//!   ending at the end of the subject short-circuit); when the first quantifier is non-greedy the first
//!   path in priority order wins. `is_match` only needs existence, so it stops at the first path.
//! - `RegexFlags` has no field for the `e` (plain ERE) option; it is encoded as `basic && literal`
//!   (a combination that is otherwise meaningless). See `contract_breaks` in the lease report.
//! - Backtracking is bounded by a step budget; exhaustion raises 2201B
//!   `regular expression failed: regular expression is too complex`.
use crate::array::Array;
use crate::catalog_data::BuiltinProc;
use crate::context::FuncCtx;
use crate::datum::Datum;
use crate::func::{FuncImpl, SetIter};
use crate::oids;
use pgarbor_common::{PgError, Result, SqlState};
use std::cell::RefCell;
use std::sync::Arc;

/// Parsed regex flags (see module docs).
// CONTRACT
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegexFlags {
    pub case_insensitive: bool,
    pub global: bool,
    pub newline_sensitive: bool,
    pub partial_newline: bool,
    pub inverse_partial_newline: bool,
    pub expanded: bool,
    pub literal: bool,
    pub basic: bool,
}

/// A compiled regular expression. Opaque: the private fields belong to lease TYP-17.
// CONTRACT
#[derive(Clone, Debug)]
pub struct Regex {
    pattern: String,
    flags: RegexFlags,
    prog: Arc<Program>,
}

/// One match: byte ranges into the subject (`None` for non-participating groups).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegexMatch {
    /// Whole-match byte range.
    pub whole: (usize, usize),
    /// Parenthesized capture groups 1..=n in order.
    pub groups: Vec<Option<(usize, usize)>>,
}

impl Regex {
    /// The source pattern.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
    /// The flags it was compiled with.
    pub fn flags(&self) -> RegexFlags {
        self.flags
    }
}

// ---------------------------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------------------------

const E_BADPAT: &str = "invalid regexp (reg version 0.8)";
const E_ECOLLATE: &str = "invalid collating element";
const E_ECTYPE: &str = "invalid character class";
const E_ESCAPE: &str = "invalid escape \\ sequence";
const E_SUBREG: &str = "invalid backreference number";
const E_BRACK: &str = "brackets [] not balanced";
const E_PAREN: &str = "parentheses () not balanced";
const E_BRACE: &str = "braces {} not balanced";
const E_BADBR: &str = "invalid repetition count(s)";
const E_ERANGE: &str = "invalid character range";
const E_BADRPT: &str = "quantifier operand invalid";
const E_BADOPT: &str = "invalid embedded option";
const E_TOOBIG: &str = "regular expression is too complex";

type PResult<T> = std::result::Result<T, &'static str>;

fn compile_error(msg: &str) -> PgError {
    PgError::new(SqlState::INVALID_REGULAR_EXPRESSION, format!("invalid regular expression: {msg}"))
}

fn exec_error(msg: &str) -> PgError {
    PgError::new(SqlState::INVALID_REGULAR_EXPRESSION, format!("regular expression failed: {msg}"))
}

fn bad_param(name: &str, v: i32) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"{name}\": {v}"))
}

// ---------------------------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    Are,
    Ere,
    Bre,
    Quote,
}

/// Parse a flags string (`"gi"`). Errors per module docs.
// CONTRACT
pub fn parse_flags(flags: &str) -> Result<RegexFlags> {
    let mut f = RegexFlags::default();
    let mut flavor = Flavor::Are;
    let mut nlstop = false;
    let mut nlanch = false;
    for ch in flags.chars() {
        match ch {
            'g' => f.global = true,
            'b' => flavor = Flavor::Bre,
            'c' => f.case_insensitive = false,
            'e' => flavor = Flavor::Ere,
            'i' => f.case_insensitive = true,
            'm' | 'n' => {
                nlstop = true;
                nlanch = true;
            }
            'p' => {
                nlstop = true;
                nlanch = false;
            }
            'q' => flavor = Flavor::Quote,
            's' => {
                nlstop = false;
                nlanch = false;
            }
            't' => f.expanded = false,
            'w' => {
                nlstop = false;
                nlanch = true;
            }
            'x' => f.expanded = true,
            other => {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid regular expression option: \"{other}\""),
                ))
            }
        }
    }
    f.newline_sensitive = nlstop && nlanch;
    f.partial_newline = nlstop && !nlanch;
    f.inverse_partial_newline = nlanch && !nlstop;
    match flavor {
        Flavor::Are => {}
        Flavor::Bre => f.basic = true,
        Flavor::Quote => f.literal = true,
        Flavor::Ere => {
            f.basic = true;
            f.literal = true;
        }
    }
    Ok(f)
}

fn flag_bits(f: &RegexFlags) -> u16 {
    (f.case_insensitive as u16)
        | (f.global as u16) << 1
        | (f.newline_sensitive as u16) << 2
        | (f.partial_newline as u16) << 3
        | (f.inverse_partial_newline as u16) << 4
        | (f.expanded as u16) << 5
        | (f.literal as u16) << 6
        | (f.basic as u16) << 7
}

// ---------------------------------------------------------------------------------------------
// Character sets and classes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    Word,
}

fn class_by_name(name: &str) -> Option<Class> {
    Some(match name {
        "alnum" => Class::Alnum,
        "alpha" => Class::Alpha,
        "ascii" => Class::Ascii,
        "blank" => Class::Blank,
        "cntrl" => Class::Cntrl,
        "digit" => Class::Digit,
        "graph" => Class::Graph,
        "lower" => Class::Lower,
        "print" => Class::Print,
        "punct" => Class::Punct,
        "space" => Class::Space,
        "upper" => Class::Upper,
        "xdigit" => Class::Xdigit,
        _ => return None,
    })
}

fn class_has(cl: Class, c: char) -> bool {
    match cl {
        Class::Alnum => c.is_alphanumeric(),
        Class::Alpha => c.is_alphabetic(),
        Class::Ascii => c.is_ascii(),
        Class::Blank => c == ' ' || c == '\t',
        Class::Cntrl => c.is_control(),
        Class::Digit => c.is_ascii_digit(),
        Class::Graph => !c.is_whitespace() && !c.is_control(),
        Class::Lower => c.is_lowercase(),
        Class::Print => !c.is_control(),
        Class::Punct => {
            if c.is_ascii() {
                c.is_ascii_punctuation()
            } else {
                !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()
            }
        }
        Class::Space => c.is_whitespace(),
        Class::Upper => c.is_uppercase(),
        Class::Xdigit => c.is_ascii_hexdigit(),
        Class::Word => c.is_alphanumeric() || c == '_',
    }
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn upper(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let mut u = c.to_uppercase();
    match (u.next(), u.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

#[derive(Debug)]
struct CharSet {
    neg: bool,
    ranges: Vec<(char, char)>,
    classes: Vec<(Class, bool)>,
}

impl CharSet {
    fn has(&self, c: char) -> bool {
        self.ranges.iter().any(|&(a, b)| a <= c && c <= b) || self.classes.iter().any(|&(cl, neg)| class_has(cl, c) != neg)
    }
    fn matches(&self, c: char, icase: bool, nlstop: bool) -> bool {
        let mut m = self.has(c);
        if !m && icase {
            let l = fold(c);
            if l != c {
                m = self.has(l);
            }
            if !m {
                let u = upper(c);
                if u != c {
                    m = self.has(u);
                }
            }
        }
        if self.neg {
            if nlstop && c == '\n' {
                false
            } else {
                !m
            }
        } else {
            m
        }
    }
}

// ---------------------------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssertKind {
    Bol,
    Eol,
    StartStr,
    EndStr,
    WordStart,
    WordEnd,
    WordBoundary,
    NotWordBoundary,
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(char),
    Any,
    Set(Arc<CharSet>),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Group(Box<Node>, usize),
    Repeat(Box<Node>, u32, Option<u32>, bool),
    Shortest(Box<Node>),
    Assert(AssertKind),
    Backref(usize),
    Look { ahead: bool, neg: bool, node: Box<Node> },
}

enum Esc {
    Ch(char),
    Cls(Class, bool),
    Assert(AssertKind),
    Backref(usize),
}

const DUPMAX: u32 = 255;

struct Parser {
    c: Vec<char>,
    i: usize,
    flavor: Flavor,
    expanded: bool,
    nopen: usize,
    defined: Vec<bool>,
    first_pref: Option<bool>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn peek_at(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }
    fn eof(&self) -> bool {
        self.i >= self.c.len()
    }

    fn skip_ws(&mut self) {
        if !self.expanded || self.flavor == Flavor::Quote {
            return;
        }
        loop {
            while let Some(ch) = self.peek() {
                if matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r') {
                    self.i += 1;
                } else {
                    break;
                }
            }
            if self.peek() == Some('#') {
                while let Some(ch) = self.peek() {
                    if ch == '\n' {
                        break;
                    }
                    self.i += 1;
                }
            } else {
                break;
            }
        }
    }

    fn parse_alt(&mut self, in_group: bool, look: bool) -> PResult<Node> {
        if self.flavor == Flavor::Quote {
            let rest: Vec<Node> = self.c[self.i..].iter().map(|&c| Node::Char(c)).collect();
            self.i = self.c.len();
            return Ok(match rest.len() {
                0 => Node::Empty,
                1 => rest.into_iter().next().unwrap_or(Node::Empty),
                _ => Node::Cat(rest),
            });
        }
        let mut alts: Vec<Node> = Vec::new();
        loop {
            let b = self.parse_branch(in_group, look)?;
            alts.push(b);
            self.skip_ws();
            if self.flavor != Flavor::Bre && self.peek() == Some('|') {
                self.i += 1;
                continue;
            }
            break;
        }
        if alts.len() == 1 {
            Ok(alts.pop().unwrap_or(Node::Empty))
        } else {
            Ok(Node::Alt(alts))
        }
    }

    fn parse_branch(&mut self, in_group: bool, look: bool) -> PResult<Node> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            self.skip_ws();
            let ch = match self.peek() {
                None => break,
                Some(c) => c,
            };
            if self.flavor == Flavor::Bre {
                if ch == '\\' && self.peek_at(1) == Some(')') {
                    if in_group {
                        break;
                    }
                    return Err(E_PAREN);
                }
            } else {
                if ch == '|' {
                    break;
                }
                if ch == ')' {
                    if in_group {
                        break;
                    }
                    if self.flavor == Flavor::Are {
                        return Err(E_PAREN);
                    }
                }
            }
            let empty = items.is_empty();
            let after_bol = matches!(items.last(), Some(Node::Assert(AssertKind::Bol)));
            if let Some(n) = self.parse_piece(in_group, look, empty, after_bol)? {
                items.push(n);
            }
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Cat(items),
        })
    }

    fn parse_group(&mut self, cap: bool, look: bool) -> PResult<Node> {
        let no = if cap {
            self.nopen += 1;
            self.defined.push(false);
            self.nopen
        } else {
            0
        };
        let inner = self.parse_alt(true, look)?;
        if self.flavor == Flavor::Bre {
            if self.peek() == Some('\\') && self.peek_at(1) == Some(')') {
                self.i += 2;
            } else {
                return Err(E_PAREN);
            }
        } else if self.peek() == Some(')') {
            self.i += 1;
        } else {
            return Err(E_PAREN);
        }
        if cap {
            self.defined[no] = true;
            Ok(Node::Group(Box::new(inner), no))
        } else {
            Ok(inner)
        }
    }

    fn check_backref(&self, k: usize, look: bool) -> PResult<Node> {
        if look || k == 0 || k > self.nopen || !self.defined[k] {
            return Err(E_SUBREG);
        }
        Ok(Node::Backref(k))
    }

    /// One atom or constraint, with its quantifier. `Ok(None)` for a comment group.
    fn parse_piece(&mut self, in_group: bool, look: bool, items_empty: bool, after_bol: bool) -> PResult<Option<Node>> {
        let _ = in_group;
        let ch = self.c[self.i];
        self.i += 1;
        let atom: Node;
        match self.flavor {
            Flavor::Are | Flavor::Ere => match ch {
                '^' => return Ok(Some(Node::Assert(AssertKind::Bol))),
                '$' => return Ok(Some(Node::Assert(AssertKind::Eol))),
                '(' => {
                    let mut cap = true;
                    let mut lk: Option<(bool, bool)> = None;
                    if self.flavor == Flavor::Are && self.peek() == Some('?') {
                        self.i += 1;
                        let c2 = self.peek().ok_or(E_BADRPT)?;
                        self.i += 1;
                        match c2 {
                            ':' => cap = false,
                            '#' => {
                                while let Some(x) = self.peek() {
                                    if x == ')' {
                                        break;
                                    }
                                    self.i += 1;
                                }
                                if self.peek().is_some() {
                                    self.i += 1;
                                }
                                return Ok(None);
                            }
                            '=' => lk = Some((true, false)),
                            '!' => lk = Some((true, true)),
                            '<' => {
                                let c3 = self.peek().ok_or(E_BADRPT)?;
                                self.i += 1;
                                match c3 {
                                    '=' => lk = Some((false, false)),
                                    '!' => lk = Some((false, true)),
                                    _ => return Err(E_BADRPT),
                                }
                            }
                            _ => return Err(E_BADRPT),
                        }
                    }
                    if let Some((ahead, neg)) = lk {
                        let inner = self.parse_alt(true, true)?;
                        if self.peek() != Some(')') {
                            return Err(E_PAREN);
                        }
                        self.i += 1;
                        return Ok(Some(Node::Look { ahead, neg, node: Box::new(inner) }));
                    }
                    atom = self.parse_group(cap && !look, look)?;
                }
                '[' => {
                    if let Some(n) = self.try_word_bracket() {
                        return Ok(Some(n));
                    }
                    atom = self.parse_bracket()?;
                }
                '.' => atom = Node::Any,
                '\\' => {
                    if self.eof() {
                        return Err(E_ESCAPE);
                    }
                    if self.flavor == Flavor::Ere {
                        let x = self.c[self.i];
                        self.i += 1;
                        atom = Node::Char(x);
                    } else {
                        match self.lexescape(false)? {
                            Esc::Ch(c) => atom = Node::Char(c),
                            Esc::Cls(cl, neg) => {
                                atom = Node::Set(Arc::new(CharSet { neg: false, ranges: Vec::new(), classes: vec![(cl, neg)] }))
                            }
                            Esc::Assert(k) => return Ok(Some(Node::Assert(k))),
                            Esc::Backref(k) => atom = self.check_backref(k, look)?,
                        }
                    }
                }
                '*' | '+' | '?' => return Err(E_BADRPT),
                '{' => {
                    self.skip_ws();
                    if self.peek().map_or(false, |d| d.is_ascii_digit()) {
                        return Err(E_BADRPT);
                    }
                    atom = Node::Char('{');
                }
                c => atom = Node::Char(c),
            },
            Flavor::Bre => match ch {
                '*' => {
                    if items_empty || after_bol {
                        atom = Node::Char('*');
                    } else {
                        return Err(E_BADRPT);
                    }
                }
                '^' => {
                    if items_empty {
                        return Ok(Some(Node::Assert(AssertKind::Bol)));
                    }
                    atom = Node::Char('^');
                }
                '$' => {
                    if self.eof() || (self.peek() == Some('\\') && self.peek_at(1) == Some(')')) {
                        return Ok(Some(Node::Assert(AssertKind::Eol)));
                    }
                    atom = Node::Char('$');
                }
                '[' => {
                    if let Some(n) = self.try_word_bracket() {
                        return Ok(Some(n));
                    }
                    atom = self.parse_bracket()?;
                }
                '.' => atom = Node::Any,
                '\\' => {
                    if self.eof() {
                        return Err(E_ESCAPE);
                    }
                    let d = self.c[self.i];
                    self.i += 1;
                    match d {
                        '{' => return Err(E_BADRPT),
                        '(' => atom = self.parse_group(!look, look)?,
                        '<' => return Ok(Some(Node::Assert(AssertKind::WordStart))),
                        '>' => return Ok(Some(Node::Assert(AssertKind::WordEnd))),
                        '1'..='9' => atom = self.check_backref(d as usize - '0' as usize, look)?,
                        other => atom = Node::Char(other),
                    }
                }
                c => atom = Node::Char(c),
            },
            Flavor::Quote => atom = Node::Char(ch),
        }
        Ok(Some(self.apply_quant(atom)?))
    }

    /// `[[:<:]]` and `[[:>:]]` (the opening `[` is already consumed).
    fn try_word_bracket(&mut self) -> Option<Node> {
        if self.peek() == Some('[')
            && self.peek_at(1) == Some(':')
            && matches!(self.peek_at(2), Some('<') | Some('>'))
            && self.peek_at(3) == Some(':')
            && self.peek_at(4) == Some(']')
            && self.peek_at(5) == Some(']')
        {
            let k = if self.peek_at(2) == Some('<') { AssertKind::WordStart } else { AssertKind::WordEnd };
            self.i += 6;
            return Some(Node::Assert(k));
        }
        None
    }

    fn lazy_suffix(&mut self) -> bool {
        if self.flavor == Flavor::Are && self.peek() == Some('?') {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn scan_num(&mut self) -> PResult<u32> {
        let mut n: u32 = 0;
        while let Some(d) = self.peek() {
            if let Some(v) = d.to_digit(10) {
                n = n.saturating_mul(10).saturating_add(v);
                self.i += 1;
            } else {
                break;
            }
        }
        if n > DUPMAX {
            return Err(E_BADBR);
        }
        Ok(n)
    }

    fn parse_bound(&mut self, bre: bool) -> PResult<(u32, Option<u32>, bool)> {
        self.skip_ws();
        let m = self.scan_num()?;
        let mut comma = false;
        let mut n = Some(m);
        self.skip_ws();
        if self.peek() == Some(',') {
            comma = true;
            self.i += 1;
            self.skip_ws();
            if self.peek().map_or(false, |d| d.is_ascii_digit()) {
                n = Some(self.scan_num()?);
            } else {
                n = None;
            }
            if let Some(nv) = n {
                if m > nv {
                    return Err(E_BADBR);
                }
            }
        }
        self.skip_ws();
        if bre {
            if self.peek() == Some('\\') && self.peek_at(1) == Some('}') {
                self.i += 2;
            } else if self.eof() {
                return Err(E_BRACE);
            } else {
                return Err(E_BADBR);
            }
        } else {
            match self.peek() {
                Some('}') => self.i += 1,
                None => return Err(E_BRACE),
                _ => return Err(E_BADBR),
            }
        }
        Ok((m, n, comma))
    }

    fn apply_quant(&mut self, atom: Node) -> PResult<Node> {
        self.skip_ws();
        // (min, max, lazy, has_preference, comma_form)
        let q: (u32, Option<u32>, bool, bool, bool);
        match (self.flavor, self.peek()) {
            (Flavor::Are, Some('*')) | (Flavor::Ere, Some('*')) | (Flavor::Bre, Some('*')) => {
                self.i += 1;
                let lazy = self.lazy_suffix();
                q = (0, None, lazy, true, false);
            }
            (Flavor::Are, Some('+')) | (Flavor::Ere, Some('+')) => {
                self.i += 1;
                let lazy = self.lazy_suffix();
                q = (1, None, lazy, true, false);
            }
            (Flavor::Are, Some('?')) | (Flavor::Ere, Some('?')) => {
                self.i += 1;
                let lazy = self.lazy_suffix();
                q = (0, Some(1), lazy, true, false);
            }
            (Flavor::Are, Some('{')) | (Flavor::Ere, Some('{')) => {
                // '{' starts a bound only when a digit follows (possibly after whitespace).
                let save = self.i;
                self.i += 1;
                self.skip_ws();
                if self.peek().map_or(false, |d| d.is_ascii_digit()) {
                    let (m, n, comma) = self.parse_bound(false)?;
                    let lazy = self.lazy_suffix();
                    q = (m, n, lazy, comma, comma);
                } else {
                    self.i = save;
                    return Ok(atom);
                }
            }
            (Flavor::Bre, Some('\\')) if self.peek_at(1) == Some('{') => {
                self.i += 2;
                let (m, n, comma) = self.parse_bound(true)?;
                q = (m, n, false, comma, comma);
            }
            _ => return Ok(atom),
        }
        let (m, n, lazy, pref, comma) = q;
        if pref && self.first_pref.is_none() {
            self.first_pref = Some(!lazy);
        }
        if m == 0 && n == Some(0) {
            return Ok(Node::Empty);
        }
        if m == 1 && n == Some(1) {
            if comma && lazy {
                return Ok(Node::Shortest(Box::new(atom)));
            }
            return Ok(atom);
        }
        Ok(Node::Repeat(Box::new(atom), m, n, !lazy))
    }

    fn lexdigits(&mut self, base: u32, minlen: usize, maxlen: usize) -> PResult<u32> {
        let mut n: u32 = 0;
        let mut len = 0usize;
        while len < maxlen {
            let d = match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) if d < base => d,
                _ => break,
            };
            self.i += 1;
            n = n.saturating_mul(base).saturating_add(d);
            len += 1;
        }
        if len < minlen {
            return Err(E_ESCAPE);
        }
        Ok(n)
    }

    /// ARE backslash escape (the backslash is consumed; not at EOF).
    fn lexescape(&mut self, in_bracket: bool) -> PResult<Esc> {
        let _ = in_bracket;
        let c = self.c[self.i];
        self.i += 1;
        if !c.is_ascii_alphanumeric() {
            return Ok(Esc::Ch(c));
        }
        Ok(match c {
            'a' => Esc::Ch('\x07'),
            'A' => Esc::Assert(AssertKind::StartStr),
            'b' => Esc::Ch('\x08'),
            'B' => Esc::Ch('\\'),
            'c' => {
                if self.eof() {
                    return Err(E_ESCAPE);
                }
                let x = self.c[self.i];
                self.i += 1;
                Esc::Ch(char::from_u32((x as u32) & 0o37).ok_or(E_ESCAPE)?)
            }
            'd' => Esc::Cls(Class::Digit, false),
            'D' => Esc::Cls(Class::Digit, true),
            'e' => Esc::Ch('\x1b'),
            'f' => Esc::Ch('\x0c'),
            'm' => Esc::Assert(AssertKind::WordStart),
            'M' => Esc::Assert(AssertKind::WordEnd),
            'n' => Esc::Ch('\n'),
            'r' => Esc::Ch('\r'),
            's' => Esc::Cls(Class::Space, false),
            'S' => Esc::Cls(Class::Space, true),
            't' => Esc::Ch('\t'),
            'u' => {
                let v = self.lexdigits(16, 4, 4)?;
                Esc::Ch(char::from_u32(v).ok_or(E_ESCAPE)?)
            }
            'U' => {
                let v = self.lexdigits(16, 8, 8)?;
                Esc::Ch(char::from_u32(v).ok_or(E_ESCAPE)?)
            }
            'v' => Esc::Ch('\x0b'),
            'w' => Esc::Cls(Class::Word, false),
            'W' => Esc::Cls(Class::Word, true),
            'x' => {
                let v = self.lexdigits(16, 1, 255)?;
                Esc::Ch(char::from_u32(v).ok_or(E_ESCAPE)?)
            }
            'y' => Esc::Assert(AssertKind::WordBoundary),
            'Y' => Esc::Assert(AssertKind::NotWordBoundary),
            'Z' => Esc::Assert(AssertKind::EndStr),
            '1'..='9' | '0' => {
                let save = self.i; // just after the first digit
                if c != '0' {
                    let mut j = save - 1;
                    let mut val: u64 = 0;
                    let mut cnt = 0;
                    while cnt < 255 && j < self.c.len() && self.c[j].is_ascii_digit() {
                        val = (val * 10 + self.c[j] as u64 - '0' as u64).min(1 << 30);
                        j += 1;
                        cnt += 1;
                    }
                    if j == save || (val > 0 && (val as usize) <= self.nopen) {
                        self.i = j;
                        return Ok(Esc::Backref(val as usize));
                    }
                }
                // octal escape
                self.i = save - 1;
                let mut v: u32 = 0;
                let mut len = 0;
                while len < 3 {
                    match self.peek().and_then(|ch| ch.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            self.i += 1;
                            len += 1;
                        }
                        None => break,
                    }
                }
                if len < 1 {
                    return Err(E_ESCAPE);
                }
                if v > 0xff {
                    self.i -= 1;
                    v >>= 3;
                }
                Esc::Ch(char::from_u32(v).ok_or(E_ESCAPE)?)
            }
            _ => return Err(E_ESCAPE),
        })
    }

    /// Read `[.x.]`, `[=x=]` or `[:x:]` content up to the closing `<delim>]`.
    fn bracket_item_text(&mut self, delim: char) -> PResult<String> {
        let mut s = String::new();
        loop {
            let c = self.peek().ok_or(E_BRACK)?;
            if c == delim && self.peek_at(1) == Some(']') {
                self.i += 2;
                return Ok(s);
            }
            self.i += 1;
            s.push(c);
        }
    }

    fn collating_element(name: &str, err: &'static str) -> PResult<char> {
        let mut it = name.chars();
        if let (Some(c), None) = (it.next(), it.clone().next()) {
            return Ok(c);
        }
        let c = match name {
            "NUL" => '\0',
            "tab" => '\t',
            "newline" => '\n',
            "vertical-tab" => '\x0b',
            "form-feed" => '\x0c',
            "carriage-return" => '\r',
            "space" => ' ',
            "exclamation-mark" => '!',
            "quotation-mark" => '"',
            "number-sign" => '#',
            "dollar-sign" => '$',
            "percent-sign" => '%',
            "ampersand" => '&',
            "apostrophe" => '\'',
            "left-parenthesis" => '(',
            "right-parenthesis" => ')',
            "asterisk" => '*',
            "plus-sign" => '+',
            "comma" => ',',
            "hyphen" | "hyphen-minus" => '-',
            "period" | "full-stop" => '.',
            "slash" | "solidus" => '/',
            "zero" => '0',
            "one" => '1',
            "two" => '2',
            "three" => '3',
            "four" => '4',
            "five" => '5',
            "six" => '6',
            "seven" => '7',
            "eight" => '8',
            "nine" => '9',
            "colon" => ':',
            "semicolon" => ';',
            "less-than-sign" => '<',
            "equals-sign" => '=',
            "greater-than-sign" => '>',
            "question-mark" => '?',
            "commercial-at" => '@',
            "left-square-bracket" => '[',
            "backslash" | "reverse-solidus" => '\\',
            "right-square-bracket" => ']',
            "circumflex" | "circumflex-accent" => '^',
            "underscore" | "low-line" => '_',
            "grave-accent" => '`',
            "left-brace" | "left-curly-bracket" => '{',
            "vertical-line" => '|',
            "right-brace" | "right-curly-bracket" => '}',
            "tilde" => '~',
            "DEL" => '\x7f',
            _ => return Err(err),
        };
        Ok(c)
    }

    /// Bracket expression; the opening `[` is already consumed.
    fn parse_bracket(&mut self) -> PResult<Node> {
        let neg = if self.peek() == Some('^') {
            self.i += 1;
            true
        } else {
            false
        };
        let mut set = CharSet { neg, ranges: Vec::new(), classes: Vec::new() };
        let mut first = true;
        loop {
            let c = self.peek().ok_or(E_BRACK)?;
            self.i += 1;
            if c == ']' && !first {
                break;
            }
            let was_first = first;
            first = false;
            // Start element of a (possible) range.
            let start: char;
            match c {
                '[' => match self.peek() {
                    None => return Err(E_BRACK),
                    Some('.') => {
                        self.i += 1;
                        let name = self.bracket_item_text('.')?;
                        if name.is_empty() {
                            return Err(E_ECOLLATE);
                        }
                        start = Self::collating_element(&name, E_ECOLLATE)?;
                    }
                    Some('=') => {
                        self.i += 1;
                        let name = self.bracket_item_text('=')?;
                        if name.is_empty() {
                            return Err(E_ECOLLATE);
                        }
                        let ch = Self::collating_element(&name, E_ECOLLATE)?;
                        set.ranges.push((ch, ch));
                        continue;
                    }
                    Some(':') => {
                        self.i += 1;
                        let name = self.bracket_item_text(':')?;
                        if name.is_empty() {
                            return Err(E_ECTYPE);
                        }
                        let cl = class_by_name(&name).ok_or(E_ECTYPE)?;
                        set.classes.push((cl, false));
                        continue;
                    }
                    Some(_) => start = '[',
                },
                '\\' if self.flavor == Flavor::Are => {
                    if self.eof() {
                        return Err(E_ESCAPE);
                    }
                    match self.lexescape(true)? {
                        Esc::Ch(ch) => start = ch,
                        Esc::Cls(cl, n) => {
                            set.classes.push((cl, n));
                            continue;
                        }
                        _ => return Err(E_ESCAPE),
                    }
                }
                '-' => {
                    if was_first || self.peek() == Some(']') {
                        start = '-';
                    } else {
                        return Err(E_ERANGE);
                    }
                }
                other => start = other,
            }
            // Range?
            if self.peek() == Some('-') && self.peek_at(1) != Some(']') && self.peek_at(1).is_some() {
                self.i += 1;
                let c3 = self.peek().ok_or(E_BRACK)?;
                self.i += 1;
                let end: char = match c3 {
                    '[' => match self.peek() {
                        None => return Err(E_BRACK),
                        Some('.') => {
                            self.i += 1;
                            let name = self.bracket_item_text('.')?;
                            if name.is_empty() {
                                return Err(E_ECOLLATE);
                            }
                            Self::collating_element(&name, E_ECOLLATE)?
                        }
                        Some('=') | Some(':') => return Err(E_ERANGE),
                        Some(_) => '[',
                    },
                    '\\' if self.flavor == Flavor::Are => {
                        if self.eof() {
                            return Err(E_ESCAPE);
                        }
                        match self.lexescape(true)? {
                            Esc::Ch(ch) => ch,
                            Esc::Cls(..) => return Err(E_ERANGE),
                            _ => return Err(E_ESCAPE),
                        }
                    }
                    other => other,
                };
                if start > end {
                    return Err(E_ERANGE);
                }
                set.ranges.push((start, end));
            } else {
                set.ranges.push((start, start));
            }
        }
        Ok(Node::Set(Arc::new(set)))
    }
}

// ---------------------------------------------------------------------------------------------
// Program (compiled form)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Single {
    Char(char),
    Any,
    Set(Arc<CharSet>),
}

#[derive(Debug)]
enum Inst {
    One(Single),
    /// Repetition of a single-character matcher; `max == u32::MAX` means unbounded.
    Rep { one: Single, min: u32, max: u32, greedy: bool },
    /// Try the first target, on failure the second.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Assert(AssertKind),
    Backref(usize),
    /// Look-around: body follows at pc+1 and ends with `LookEnd`; continue at `end`.
    Look { ahead: bool, neg: bool, minw: usize, maxw: usize, end: usize },
    LookEnd,
    Mark(usize),
    EmptyCheck { slot: usize, exit: usize },
    Match,
}

#[derive(Debug)]
struct Program {
    insts: Vec<Inst>,
    nslots: usize,
    ngroups: usize,
    icase: bool,
    nlstop: bool,
    nlanch: bool,
    longest: bool,
    first_char: Option<char>,
    anchored: bool,
}

const MAX_INSTS: usize = 400_000;
const INF: usize = usize::MAX;

fn width(n: &Node) -> (usize, usize) {
    match n {
        Node::Empty | Node::Assert(_) | Node::Look { .. } => (0, 0),
        Node::Char(_) | Node::Any | Node::Set(_) => (1, 1),
        Node::Cat(v) => v.iter().fold((0usize, 0usize), |(a, b), x| {
            let (c, d) = width(x);
            (a.saturating_add(c), if b == INF || d == INF { INF } else { b.saturating_add(d) })
        }),
        Node::Alt(v) => {
            let mut lo = INF;
            let mut hi = 0usize;
            for x in v {
                let (c, d) = width(x);
                lo = lo.min(c);
                hi = hi.max(d);
            }
            (if lo == INF { 0 } else { lo }, hi)
        }
        Node::Group(x, _) | Node::Shortest(x) => width(x),
        Node::Repeat(x, m, mx, _) => {
            let (c, d) = width(x);
            let lo = c.saturating_mul(*m as usize);
            let hi = match mx {
                None => {
                    if d == 0 {
                        0
                    } else {
                        INF
                    }
                }
                Some(k) => {
                    if d == INF {
                        INF
                    } else {
                        d.saturating_mul(*k as usize)
                    }
                }
            };
            (lo, hi)
        }
        Node::Backref(_) => (0, INF),
    }
}

fn has_choice(n: &Node) -> bool {
    match n {
        Node::Alt(_) => true,
        Node::Repeat(x, m, mx, _) => *mx != Some(*m) || has_choice(x),
        Node::Cat(v) => v.iter().any(has_choice),
        Node::Group(x, _) | Node::Shortest(x) => has_choice(x),
        _ => false,
    }
}

fn first_info(n: &Node) -> (Option<char>, bool) {
    match n {
        Node::Char(c) => (Some(*c), false),
        Node::Assert(AssertKind::Bol) => (None, true),
        Node::Cat(v) => v.first().map(first_info).unwrap_or((None, false)),
        Node::Group(x, _) | Node::Shortest(x) => first_info(x),
        Node::Repeat(x, m, _, _) if *m >= 1 => first_info(x),
        _ => (None, false),
    }
}

fn as_single(n: &Node) -> Option<Single> {
    match n {
        Node::Char(c) => Some(Single::Char(*c)),
        Node::Any => Some(Single::Any),
        Node::Set(s) => Some(Single::Set(s.clone())),
        _ => None,
    }
}

struct Emitter {
    insts: Vec<Inst>,
    ngroups: usize,
    nregs: usize,
}

impl Emitter {
    fn emit(&mut self, n: &Node, lazy: bool) -> PResult<()> {
        if self.insts.len() > MAX_INSTS {
            return Err(E_TOOBIG);
        }
        match n {
            Node::Empty => {}
            Node::Char(_) | Node::Any | Node::Set(_) => {
                if let Some(s) = as_single(n) {
                    self.insts.push(Inst::One(s));
                }
            }
            Node::Cat(v) => {
                for x in v {
                    self.emit(x, lazy)?;
                }
            }
            Node::Alt(v) => {
                let mut jumps: Vec<usize> = Vec::new();
                for (i, b) in v.iter().enumerate() {
                    if i + 1 < v.len() {
                        let split = self.insts.len();
                        self.insts.push(Inst::Split(split + 1, 0));
                        self.emit(b, lazy)?;
                        jumps.push(self.insts.len());
                        self.insts.push(Inst::Jmp(0));
                        let next = self.insts.len();
                        self.insts[split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(b, lazy)?;
                    }
                }
                let end = self.insts.len();
                for j in jumps {
                    self.insts[j] = Inst::Jmp(end);
                }
            }
            Node::Group(x, k) => {
                self.insts.push(Inst::Save(2 * k));
                self.emit(x, lazy)?;
                self.insts.push(Inst::Save(2 * k + 1));
            }
            Node::Shortest(x) => self.emit(x, true)?,
            Node::Assert(k) => self.insts.push(Inst::Assert(*k)),
            Node::Backref(k) => self.insts.push(Inst::Backref(*k)),
            Node::Look { ahead, neg, node } => {
                let (minw, maxw) = width(node);
                let idx = self.insts.len();
                self.insts.push(Inst::Look { ahead: *ahead, neg: *neg, minw, maxw, end: 0 });
                self.emit(node, lazy)?;
                self.insts.push(Inst::LookEnd);
                let end = self.insts.len();
                self.insts[idx] = Inst::Look { ahead: *ahead, neg: *neg, minw, maxw, end };
            }
            Node::Repeat(x, min, max, greedy) => {
                let greedy = *greedy && !lazy;
                if let Some(one) = as_single(x) {
                    let mx = max.unwrap_or(u32::MAX);
                    self.insts.push(Inst::Rep { one, min: *min, max: mx, greedy });
                    return Ok(());
                }
                if *max == Some(0) {
                    return Ok(());
                }
                for _ in 0..*min {
                    self.emit(x, lazy)?;
                }
                match max {
                    None => {
                        let slot = 2 * (self.ngroups + 1) + self.nregs;
                        self.nregs += 1;
                        let l = self.insts.len();
                        self.insts.push(Inst::Split(0, 0));
                        self.insts.push(Inst::Mark(slot));
                        self.emit(x, lazy)?;
                        let ec = self.insts.len();
                        self.insts.push(Inst::EmptyCheck { slot, exit: 0 });
                        self.insts.push(Inst::Jmp(l));
                        let exit = self.insts.len();
                        self.insts[l] = if greedy { Inst::Split(l + 1, exit) } else { Inst::Split(exit, l + 1) };
                        self.insts[ec] = Inst::EmptyCheck { slot, exit };
                    }
                    Some(mx) => {
                        let extra = mx.saturating_sub(*min);
                        let mut splits: Vec<usize> = Vec::new();
                        for _ in 0..extra {
                            let sp = self.insts.len();
                            self.insts.push(Inst::Split(0, 0));
                            self.emit(x, lazy)?;
                            splits.push(sp);
                        }
                        let end = self.insts.len();
                        for sp in splits {
                            self.insts[sp] = if greedy { Inst::Split(sp + 1, end) } else { Inst::Split(end, sp + 1) };
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn build(pattern: &str, flags: RegexFlags) -> PResult<Program> {
    let mut flavor = if flags.basic && flags.literal {
        Flavor::Ere
    } else if flags.basic {
        Flavor::Bre
    } else if flags.literal {
        Flavor::Quote
    } else {
        Flavor::Are
    };
    let mut expanded = flags.expanded;
    let mut icase = flags.case_insensitive;
    let mut nlstop = flags.newline_sensitive || flags.partial_newline;
    let mut nlanch = flags.newline_sensitive || flags.inverse_partial_newline;
    let c: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    if flavor != Flavor::Quote {
        if c.len() >= 4 && c[0] == '*' && c[1] == '*' && c[2] == '*' {
            match c[3] {
                '?' => return Err(E_BADPAT),
                '=' => {
                    flavor = Flavor::Quote;
                    expanded = false;
                    nlstop = false;
                    nlanch = false;
                    i = 4;
                }
                ':' => {
                    flavor = Flavor::Are;
                    i = 4;
                }
                _ => return Err(E_BADRPT),
            }
        }
        if flavor == Flavor::Are && c.len() >= i + 3 && c[i] == '(' && c[i + 1] == '?' && c[i + 2].is_ascii_alphabetic() {
            i += 2;
            while i < c.len() && c[i].is_ascii_alphabetic() {
                match c[i] {
                    'b' => flavor = Flavor::Bre,
                    'c' => icase = false,
                    'e' => flavor = Flavor::Ere,
                    'i' => icase = true,
                    'm' | 'n' => {
                        nlstop = true;
                        nlanch = true;
                    }
                    'p' => {
                        nlstop = true;
                        nlanch = false;
                    }
                    'q' => flavor = Flavor::Quote,
                    's' => {
                        nlstop = false;
                        nlanch = false;
                    }
                    't' => expanded = false,
                    'w' => {
                        nlstop = false;
                        nlanch = true;
                    }
                    'x' => expanded = true,
                    _ => return Err(E_BADOPT),
                }
                i += 1;
            }
            if i >= c.len() || c[i] != ')' {
                return Err(E_BADOPT);
            }
            i += 1;
            if flavor == Flavor::Quote {
                expanded = false;
                nlstop = false;
                nlanch = false;
            }
        }
    }
    let mut p = Parser { c, i, flavor, expanded, nopen: 0, defined: vec![true], first_pref: None };
    let node = p.parse_alt(false, false)?;
    if !p.eof() {
        // Only reachable through an unmatched close in BRE/ARE contexts already reported above.
        return Err(E_PAREN);
    }
    let ngroups = p.nopen;
    let mut em = Emitter { insts: Vec::new(), ngroups, nregs: 0 };
    em.emit(&node, false)?;
    em.insts.push(Inst::Match);
    let (fc, anch) = first_info(&node);
    let first_char = if icase { None } else { fc };
    let anchored = anch && !nlanch;
    let longest = p.first_pref != Some(false) && has_choice(&node);
    Ok(Program {
        insts: em.insts,
        nslots: 2 * (ngroups + 1) + em.nregs,
        ngroups,
        icase,
        nlstop,
        nlanch,
        longest,
        first_char,
        anchored,
    })
}

// ---------------------------------------------------------------------------------------------
// Matcher
// ---------------------------------------------------------------------------------------------

const UNSET: usize = usize::MAX;
const STEP_LIMIT: u64 = 50_000_000;
const STACK_LIMIT: usize = 4_000_000;

enum Bt {
    Branch(usize, usize),
    Restore(usize, usize),
    RepG { pc: usize, lo: usize, hi: usize },
    RepL { pc: usize, pos: usize, count: u32 },
}

struct Vm<'a> {
    p: &'a Program,
    s: &'a [char],
    slots: Vec<usize>,
    steps: u64,
    best: Option<(usize, Vec<usize>)>,
    longest: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl<'a> Vm<'a> {
    fn single(&self, one: &Single, c: char) -> bool {
        match one {
            Single::Char(x) => *x == c || (self.p.icase && fold(*x) == fold(c)),
            Single::Any => !(self.p.nlstop && c == '\n'),
            Single::Set(set) => set.matches(c, self.p.icase, self.p.nlstop),
        }
    }

    fn check_assert(&self, k: AssertKind, pos: usize) -> bool {
        let s = self.s;
        let n = s.len();
        let before = pos > 0 && is_word(s[pos - 1]);
        let after = pos < n && is_word(s[pos]);
        match k {
            AssertKind::Bol => pos == 0 || (self.p.nlanch && s[pos - 1] == '\n'),
            AssertKind::Eol => pos == n || (self.p.nlanch && s[pos] == '\n'),
            AssertKind::StartStr => pos == 0,
            AssertKind::EndStr => pos == n,
            AssertKind::WordStart => !before && after,
            AssertKind::WordEnd => before && !after,
            AssertKind::WordBoundary => before != after,
            AssertKind::NotWordBoundary => before == after,
        }
    }

    /// Run from `pc0` at `pos0`. `sub` is `None` for the top-level program and `Some(must_end)` for
    /// a look-around body.
    fn run(&mut self, pc0: usize, pos0: usize, sub: Option<Option<usize>>) -> PResult<bool> {
        let prog: &'a Program = self.p;
        let s: &'a [char] = self.s;
        let n = s.len();
        let mut stack: Vec<Bt> = Vec::new();
        let mut pc = pc0;
        let mut pos = pos0;
        'outer: loop {
            self.steps += 1;
            if self.steps > STEP_LIMIT || stack.len() > STACK_LIMIT {
                if sub.is_none() && self.best.is_some() {
                    return Ok(true);
                }
                return Err(E_TOOBIG);
            }
            let mut ok = true;
            match &prog.insts[pc] {
                Inst::One(one) => {
                    if pos < n && self.single(one, s[pos]) {
                        pos += 1;
                        pc += 1;
                    } else {
                        ok = false;
                    }
                }
                Inst::Rep { one, min, max, greedy } => {
                    let mn = *min as usize;
                    if *greedy {
                        let maxc = if *max == u32::MAX { usize::MAX } else { *max as usize };
                        let mut c = 0usize;
                        while c < maxc && pos + c < n && self.single(one, s[pos + c]) {
                            c += 1;
                        }
                        self.steps += c as u64;
                        if c < mn {
                            ok = false;
                        } else {
                            if c > mn {
                                stack.push(Bt::RepG { pc: pc + 1, lo: pos + mn, hi: pos + c - 1 });
                            }
                            pos += c;
                            pc += 1;
                        }
                    } else {
                        let mut good = pos + mn <= n;
                        if good {
                            for k in 0..mn {
                                if !self.single(one, s[pos + k]) {
                                    good = false;
                                    break;
                                }
                            }
                        }
                        if !good {
                            ok = false;
                        } else {
                            pos += mn;
                            if *max == u32::MAX || *min < *max {
                                stack.push(Bt::RepL { pc, pos, count: *min });
                            }
                            pc += 1;
                        }
                    }
                }
                Inst::Split(a, b) => {
                    stack.push(Bt::Branch(*b, pos));
                    pc = *a;
                }
                Inst::Jmp(t) => pc = *t,
                Inst::Save(k) => {
                    stack.push(Bt::Restore(*k, self.slots[*k]));
                    self.slots[*k] = pos;
                    pc += 1;
                }
                Inst::Mark(k) => {
                    stack.push(Bt::Restore(*k, self.slots[*k]));
                    self.slots[*k] = pos;
                    pc += 1;
                }
                Inst::EmptyCheck { slot, exit } => {
                    if self.slots[*slot] == pos {
                        pc = *exit;
                    } else {
                        pc += 1;
                    }
                }
                Inst::Assert(k) => {
                    if self.check_assert(*k, pos) {
                        pc += 1;
                    } else {
                        ok = false;
                    }
                }
                Inst::Backref(k) => {
                    let a = self.slots[2 * k];
                    let b = self.slots[2 * k + 1];
                    if a == UNSET || b == UNSET || b < a {
                        ok = false;
                    } else {
                        let len = b - a;
                        let mut good = pos + len <= n;
                        if good {
                            for t in 0..len {
                                let x = s[a + t];
                                let y = s[pos + t];
                                if x != y && !(prog.icase && fold(x) == fold(y)) {
                                    good = false;
                                    break;
                                }
                            }
                        }
                        if good {
                            pos += len;
                            pc += 1;
                        } else {
                            ok = false;
                        }
                    }
                }
                Inst::Look { ahead, neg, minw, maxw, end } => {
                    let found = if *ahead {
                        self.run(pc + 1, pos, Some(None))?
                    } else {
                        let mut f = false;
                        if pos >= *minw {
                            let lo = if *maxw == INF { 0 } else { pos.saturating_sub(*maxw) };
                            let mut st = pos - *minw;
                            loop {
                                if self.run(pc + 1, st, Some(Some(pos)))? {
                                    f = true;
                                    break;
                                }
                                if st <= lo {
                                    break;
                                }
                                st -= 1;
                            }
                        }
                        f
                    };
                    if found != *neg {
                        pc = *end;
                    } else {
                        ok = false;
                    }
                }
                Inst::LookEnd => match sub {
                    Some(me) => {
                        if me.map_or(true, |m| m == pos) {
                            return Ok(true);
                        }
                        ok = false;
                    }
                    None => ok = false,
                },
                Inst::Match => {
                    if self.longest {
                        let better = match &self.best {
                            None => true,
                            Some((e, _)) => pos > *e,
                        };
                        if better {
                            self.best = Some((pos, self.slots.clone()));
                        }
                        if pos == n {
                            return Ok(true);
                        }
                        ok = false;
                    } else {
                        self.best = Some((pos, self.slots.clone()));
                        return Ok(true);
                    }
                }
            }
            if ok {
                continue;
            }
            // Backtrack.
            loop {
                match stack.pop() {
                    None => return Ok(sub.is_none() && self.best.is_some()),
                    Some(Bt::Branch(p, q)) => {
                        pc = p;
                        pos = q;
                        continue 'outer;
                    }
                    Some(Bt::Restore(k, old)) => {
                        self.slots[k] = old;
                    }
                    Some(Bt::RepG { pc: p, lo, hi }) => {
                        if hi > lo {
                            stack.push(Bt::RepG { pc: p, lo, hi: hi - 1 });
                        }
                        pc = p;
                        pos = hi;
                        continue 'outer;
                    }
                    Some(Bt::RepL { pc: p, pos: q, count }) => {
                        if let Inst::Rep { one, max, .. } = &prog.insts[p] {
                            if (*max == u32::MAX || count < *max) && q < n && self.single(one, s[q]) {
                                stack.push(Bt::RepL { pc: p, pos: q + 1, count: count + 1 });
                                pc = p + 1;
                                pos = q + 1;
                                continue 'outer;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Search `s` for `prog` starting at char index `from`. Returns capture slots (char indices).
fn exec_slots(prog: &Program, s: &[char], from: usize, first_only: bool) -> Result<Option<Vec<usize>>> {
    let n = s.len();
    if from > n {
        return Ok(None);
    }
    let mut vm = Vm {
        p: prog,
        s,
        slots: vec![UNSET; prog.nslots],
        steps: 0,
        best: None,
        longest: prog.longest && !first_only,
    };
    let mut st = from;
    while st <= n {
        if let Some(fc) = prog.first_char {
            while st < n && s[st] != fc {
                st += 1;
            }
            if st >= n {
                break;
            }
        }
        if prog.anchored && st != 0 {
            break;
        }
        for x in vm.slots.iter_mut() {
            *x = UNSET;
        }
        vm.best = None;
        let hit = vm.run(0, st, None).map_err(exec_error)?;
        if hit {
            if let Some((end, mut slots)) = vm.best.take() {
                slots[0] = st;
                slots[1] = end;
                return Ok(Some(slots));
            }
        }
        st += 1;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

const CACHE_SIZE: usize = 32;

thread_local! {
    static CACHE: RefCell<Vec<(String, u16, Regex)>> = const { RefCell::new(Vec::new()) };
}

/// Compile an ARE. Errors: 2201B `invalid regular expression: ...`.
// CONTRACT
pub fn compile(pattern: &str, flags: RegexFlags) -> Result<Regex> {
    let bits = flag_bits(&flags);
    let hit = CACHE.with(|c| {
        c.borrow().iter().find(|(p, b, _)| *b == bits && p == pattern).map(|(_, _, r)| r.clone())
    });
    if let Some(r) = hit {
        return Ok(r);
    }
    let prog = build(pattern, flags).map_err(compile_error)?;
    let re = Regex { pattern: pattern.to_string(), flags, prog: Arc::new(prog) };
    CACHE.with(|c| {
        let mut v = c.borrow_mut();
        if v.len() >= CACHE_SIZE {
            v.remove(0);
        }
        v.push((pattern.to_string(), bits, re.clone()));
    });
    Ok(re)
}

/// Decoded subject: chars plus the byte offset of each char boundary.
struct Subj<'a> {
    text: &'a str,
    chars: Vec<char>,
    offs: Vec<usize>,
}

impl<'a> Subj<'a> {
    fn new(text: &'a str) -> Subj<'a> {
        let mut chars = Vec::with_capacity(text.len());
        let mut offs = Vec::with_capacity(text.len() + 1);
        for (b, ch) in text.char_indices() {
            offs.push(b);
            chars.push(ch);
        }
        offs.push(text.len());
        Subj { text, chars, offs }
    }
    fn n(&self) -> usize {
        self.chars.len()
    }
    fn slice(&self, a: usize, b: usize) -> &'a str {
        &self.text[self.offs[a]..self.offs[b]]
    }
}

/// First match at or after byte offset `start` (must be a char boundary), or None.
// CONTRACT
pub fn find_at(re: &Regex, subject: &str, start: usize) -> Result<Option<RegexMatch>> {
    let subj = Subj::new(subject);
    let from = subj.offs.partition_point(|&o| o < start);
    let slots = match exec_slots(&re.prog, &subj.chars, from, false)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let b = |i: usize| subj.offs[i];
    let whole = (b(slots[0]), b(slots[1]));
    let mut groups = Vec::with_capacity(re.prog.ngroups);
    for g in 1..=re.prog.ngroups {
        let (x, y) = (slots[2 * g], slots[2 * g + 1]);
        groups.push(if x == UNSET || y == UNSET { None } else { Some((b(x), b(y))) });
    }
    Ok(Some(RegexMatch { whole, groups }))
}

/// True if `re` matches anywhere in `subject` (the `~` operator).
// CONTRACT
pub fn is_match(re: &Regex, subject: &str) -> Result<bool> {
    let chars: Vec<char> = subject.chars().collect();
    Ok(exec_slots(&re.prog, &chars, 0, true)?.is_some())
}

// ---------------------------------------------------------------------------------------------
// SQL function layer
// ---------------------------------------------------------------------------------------------

fn txt(d: &Datum) -> Result<&str> {
    match d {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(PgError::internal(format!("regex function expected a text argument, got {other:?}"))),
    }
}

fn int(d: &Datum) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        Datum::Int2(v) => Ok(*v as i32),
        Datum::Int8(v) => Ok((*v).clamp(i32::MIN as i64, i32::MAX as i64) as i32),
        other => Err(PgError::internal(format!("regex function expected an integer argument, got {other:?}"))),
    }
}

fn opt_flags(d: Option<&Datum>) -> Result<RegexFlags> {
    match d {
        Some(x) => parse_flags(txt(x)?),
        None => parse_flags(""),
    }
}

fn no_global(f: &RegexFlags, fname: &str, hint: bool) -> Result<()> {
    if f.global {
        let mut e = PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{fname} does not support the \"global\" option"),
        );
        if hint {
            e = e.with_hint("Use the regexp_matches function instead.");
        }
        return Err(e);
    }
    Ok(())
}

fn positive(name: &str, v: i32) -> Result<()> {
    if v <= 0 {
        return Err(bad_param(name, v));
    }
    Ok(())
}

fn plain_flags(icase: bool) -> RegexFlags {
    RegexFlags { case_insensitive: icase, ..RegexFlags::default() }
}

fn regex_op(args: &[Datum], icase: bool, negate: bool) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let re = compile(p, plain_flags(icase))?;
    Ok(Datum::Bool(is_match(&re, s)? != negate))
}

fn textregexeq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    regex_op(args, false, false)
}
fn textregexne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    regex_op(args, false, true)
}
fn texticregexeq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    regex_op(args, true, false)
}
fn texticregexne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    regex_op(args, true, true)
}

fn textregexsubstr(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    regex_substring(s, p)
}

fn regex_substring(s: &str, p: &str) -> Result<Datum> {
    let re = compile(p, plain_flags(false))?;
    let subj = Subj::new(s);
    let slots = match exec_slots(&re.prog, &subj.chars, 0, false)? {
        Some(x) => x,
        None => return Ok(Datum::Null),
    };
    let (a, b) = if re.prog.ngroups > 0 { (slots[2], slots[3]) } else { (slots[0], slots[1]) };
    if a == UNSET || b == UNSET {
        return Ok(Datum::Null);
    }
    Ok(Datum::Text(subj.slice(a, b).to_string()))
}

/// SQL-language `substring(string, pattern, escape)`: `textregexsubstr(s, similar_to_escape(p, e))`.
fn substring_similar(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let conv = crate::func::call(1986, &[args[1].clone(), args[2].clone()], ctx)?;
    match conv {
        Datum::Null => Ok(Datum::Null),
        Datum::Text(p) => regex_substring(txt(&args[0])?, &p),
        other => Err(PgError::internal(format!("similar_to_escape returned {other:?}"))),
    }
}

/// All matches (char-index capture slots), mirroring `setup_regexp_matches`.
fn collect_matches(
    re: &Regex,
    subj: &Subj,
    start: usize,
    glob: bool,
    ignore_degenerate: bool,
) -> Result<Vec<Vec<usize>>> {
    let n = subj.n();
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut start = start;
    let mut prev_end = 0usize;
    while let Some(slots) = exec_slots(&re.prog, &subj.chars, start, false)? {
        let (so, eo) = (slots[0], slots[1]);
        if !ignore_degenerate || (so < n && eo > prev_end) {
            out.push(slots);
        }
        prev_end = eo;
        if !glob {
            break;
        }
        start = prev_end;
        if so == eo {
            start += 1;
        }
        if start > n {
            break;
        }
    }
    Ok(out)
}

/// Location of sub-pattern `j` (0-based) of a match; `use_sub` selects groups vs whole match.
fn loc(slots: &[usize], j: usize, use_sub: bool) -> Option<(usize, usize)> {
    let (a, b) = if use_sub { (slots[2 * (j + 1)], slots[2 * (j + 1) + 1]) } else { (slots[0], slots[1]) };
    if a == UNSET || b == UNSET {
        None
    } else {
        Some((a, b))
    }
}

fn text_array(items: Vec<Datum>) -> Datum {
    Datum::array(Array::from_vec(oids::TEXT, items))
}

fn match_array(subj: &Subj, slots: &[usize], nsub: usize) -> Datum {
    let use_sub = nsub > 0;
    let np = if use_sub { nsub } else { 1 };
    let mut items = Vec::with_capacity(np);
    for j in 0..np {
        items.push(match loc(slots, j, use_sub) {
            Some((a, b)) => Datum::Text(subj.slice(a, b).to_string()),
            None => Datum::Null,
        });
    }
    text_array(items)
}

fn regexp_match(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let flags = opt_flags(args.get(2))?;
    no_global(&flags, "regexp_match()", true)?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, 0, false, false)?;
    match ms.first() {
        None => Ok(Datum::Null),
        Some(m) => Ok(match_array(&subj, m, re.prog.ngroups)),
    }
}

fn regexp_matches(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let flags = opt_flags(args.get(2))?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, 0, flags.global, false)?;
    let rows: Vec<Result<Datum>> = ms.iter().map(|m| Ok(match_array(&subj, m, re.prog.ngroups))).collect();
    Ok(Box::new(rows.into_iter()))
}

fn split_pieces(args: &[Datum], fname: &str) -> Result<Vec<String>> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let flags = opt_flags(args.get(2))?;
    no_global(&flags, fname, false)?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, 0, true, true)?;
    let mut out = Vec::with_capacity(ms.len() + 1);
    let mut prev = 0usize;
    for m in &ms {
        out.push(subj.slice(prev, m[0]).to_string());
        prev = m[1];
    }
    out.push(subj.slice(prev, subj.n()).to_string());
    Ok(out)
}

fn regexp_split_to_array(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let pieces = split_pieces(args, "regexp_split_to_array()")?;
    Ok(text_array(pieces.into_iter().map(Datum::Text).collect()))
}

fn regexp_split_to_table(args: &[Datum], _ctx: &FuncCtx) -> Result<SetIter> {
    let pieces = split_pieces(args, "regexp_split_to_table()")?;
    let rows: Vec<Result<Datum>> = pieces.into_iter().map(|p| Ok(Datum::Text(p))).collect();
    Ok(Box::new(rows.into_iter()))
}

fn regexp_like(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let flags = opt_flags(args.get(2))?;
    no_global(&flags, "regexp_like()", false)?;
    let re = compile(p, flags)?;
    Ok(Datum::Bool(is_match(&re, s)?))
}

fn regexp_count(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let mut start = 1;
    if args.len() > 2 {
        start = int(&args[2])?;
        positive("start", start)?;
    }
    let flags = opt_flags(args.get(3))?;
    no_global(&flags, "regexp_count()", false)?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, (start - 1) as usize, true, false)?;
    Ok(Datum::Int4(ms.len() as i32))
}

fn regexp_instr(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let mut start = 1;
    let mut n = 1;
    let mut endoption = 0;
    let mut subexpr = 0;
    if args.len() > 2 {
        start = int(&args[2])?;
        positive("start", start)?;
    }
    if args.len() > 3 {
        n = int(&args[3])?;
        positive("n", n)?;
    }
    if args.len() > 4 {
        endoption = int(&args[4])?;
        if endoption != 0 && endoption != 1 {
            return Err(bad_param("endoption", endoption));
        }
    }
    if args.len() > 6 {
        subexpr = int(&args[6])?;
        if subexpr < 0 {
            return Err(bad_param("subexpr", subexpr));
        }
    }
    let flags = opt_flags(args.get(5))?;
    no_global(&flags, "regexp_instr()", false)?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, (start - 1) as usize, true, false)?;
    if n as usize > ms.len() {
        return Ok(Datum::Int4(0));
    }
    let nsub = re.prog.ngroups;
    let use_sub = subexpr > 0 && nsub > 0;
    let npat = if use_sub { nsub } else { 1 };
    if subexpr as usize > npat {
        return Ok(Datum::Int4(0));
    }
    let j = if subexpr > 0 { subexpr as usize - 1 } else { 0 };
    let m = &ms[n as usize - 1];
    match loc(m, j, use_sub) {
        Some((a, b)) => Ok(Datum::Int4((if endoption == 1 { b } else { a }) as i32 + 1)),
        None => Ok(Datum::Int4(0)),
    }
}

fn regexp_substr(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let mut start = 1;
    let mut n = 1;
    let mut subexpr = 0;
    if args.len() > 2 {
        start = int(&args[2])?;
        positive("start", start)?;
    }
    if args.len() > 3 {
        n = int(&args[3])?;
        positive("n", n)?;
    }
    if args.len() > 5 {
        subexpr = int(&args[5])?;
        if subexpr < 0 {
            return Err(bad_param("subexpr", subexpr));
        }
    }
    let flags = opt_flags(args.get(4))?;
    no_global(&flags, "regexp_substr()", false)?;
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let ms = collect_matches(&re, &subj, (start - 1) as usize, true, false)?;
    if n as usize > ms.len() {
        return Ok(Datum::Null);
    }
    let nsub = re.prog.ngroups;
    let use_sub = subexpr > 0 && nsub > 0;
    let npat = if use_sub { nsub } else { 1 };
    if subexpr as usize > npat {
        return Ok(Datum::Null);
    }
    let j = if subexpr > 0 { subexpr as usize - 1 } else { 0 };
    let m = &ms[n as usize - 1];
    match loc(m, j, use_sub) {
        Some((a, b)) => Ok(Datum::Text(subj.slice(a, b).to_string())),
        None => Ok(Datum::Null),
    }
}

/// `replace_text_regexp`: replace the `n`th match (all when `n == 0`), searching from char `search_start`.
fn replace_core(s: &str, p: &str, r: &str, flags: RegexFlags, search_start: usize, n: i32) -> Result<String> {
    let re = compile(p, flags)?;
    let subj = Subj::new(s);
    let len = subj.n();
    let rc: Vec<char> = r.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut data_pos = 0usize;
    let mut nmatches = 0i32;
    let mut search = search_start;
    while search <= len {
        let slots = match exec_slots(&re.prog, &subj.chars, search, false)? {
            Some(x) => x,
            None => break,
        };
        let (so, eo) = (slots[0], slots[1]);
        nmatches += 1;
        if n > 0 && nmatches != n {
            search = eo;
            if so == eo {
                search += 1;
            }
            continue;
        }
        if so > data_pos {
            out.push_str(subj.slice(data_pos, so));
        }
        append_replacement(&mut out, &rc, &subj, &slots);
        data_pos = eo;
        if n > 0 {
            break;
        }
        search = data_pos;
        if so == eo {
            search += 1;
        }
    }
    if data_pos < len {
        out.push_str(subj.slice(data_pos, len));
    }
    Ok(out)
}

fn append_replacement(out: &mut String, rc: &[char], subj: &Subj, slots: &[usize]) {
    let group = |idx: usize| -> Option<(usize, usize)> {
        if 2 * idx + 1 >= slots.len() {
            return None;
        }
        let (a, b) = (slots[2 * idx], slots[2 * idx + 1]);
        if a == UNSET || b == UNSET {
            None
        } else {
            Some((a, b))
        }
    };
    let mut k = 0usize;
    while k < rc.len() {
        let ch = rc[k];
        if ch != '\\' {
            out.push(ch);
            k += 1;
            continue;
        }
        k += 1;
        if k >= rc.len() {
            out.push('\\');
            break;
        }
        let d = rc[k];
        let span = match d {
            '1'..='9' => {
                k += 1;
                group(d as usize - '0' as usize)
            }
            '&' => {
                k += 1;
                group(0)
            }
            '\\' => {
                out.push('\\');
                k += 1;
                continue;
            }
            _ => {
                out.push('\\');
                continue;
            }
        };
        if let Some((a, b)) = span {
            out.push_str(subj.slice(a, b));
        }
    }
}

fn regexp_replace(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let s = txt(&args[0])?;
    let p = txt(&args[1])?;
    let r = txt(&args[2])?;
    match args.len() {
        3 => Ok(Datum::Text(replace_core(s, p, r, plain_flags(false), 0, 1)?)),
        4 if matches!(args[3], Datum::Text(_)) => {
            let opt = txt(&args[3])?;
            if let Some(first) = opt.chars().next() {
                if first.is_ascii_digit() {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("invalid regular expression option: \"{first}\""),
                    )
                    .with_hint(
                        "If you meant to use regexp_replace() with a start parameter, cast the fourth argument to integer explicitly.",
                    ));
                }
            }
            let flags = parse_flags(opt)?;
            Ok(Datum::Text(replace_core(s, p, r, flags, 0, if flags.global { 0 } else { 1 })?))
        }
        _ => {
            let start = int(&args[3])?;
            positive("start", start)?;
            let mut n = 1;
            if args.len() > 4 {
                n = int(&args[4])?;
                if n < 0 {
                    return Err(bad_param("n", n));
                }
            }
            let flags = opt_flags(args.get(5))?;
            if args.len() <= 4 {
                n = if flags.global { 0 } else { 1 };
            }
            Ok(Datum::Text(replace_core(s, p, r, flags, (start - 1) as usize, n)?))
        }
    }
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    if proc.oid == 2074 {
        return Some(FuncImpl::Scalar(substring_similar));
    }
    let name = proc.prosrc;
    Some(match name {
        "textregexeq" | "nameregexeq" => FuncImpl::Scalar(textregexeq),
        "textregexne" | "nameregexne" => FuncImpl::Scalar(textregexne),
        "texticregexeq" | "nameicregexeq" => FuncImpl::Scalar(texticregexeq),
        "texticregexne" | "nameicregexne" => FuncImpl::Scalar(texticregexne),
        "textregexsubstr" => FuncImpl::Scalar(textregexsubstr),
        "textregexreplace_noopt"
        | "textregexreplace"
        | "textregexreplace_extended"
        | "textregexreplace_extended_no_flags"
        | "textregexreplace_extended_no_n" => FuncImpl::Scalar(regexp_replace),
        "regexp_match" | "regexp_match_no_flags" => FuncImpl::Scalar(regexp_match),
        "regexp_matches" | "regexp_matches_no_flags" => FuncImpl::Set(regexp_matches),
        "regexp_split_to_array" | "regexp_split_to_array_no_flags" => FuncImpl::Scalar(regexp_split_to_array),
        "regexp_split_to_table" | "regexp_split_to_table_no_flags" => FuncImpl::Set(regexp_split_to_table),
        "regexp_like" | "regexp_like_no_flags" => FuncImpl::Scalar(regexp_like),
        "regexp_count" | "regexp_count_no_start" | "regexp_count_no_flags" => FuncImpl::Scalar(regexp_count),
        "regexp_instr"
        | "regexp_instr_no_start"
        | "regexp_instr_no_n"
        | "regexp_instr_no_endoption"
        | "regexp_instr_no_flags"
        | "regexp_instr_no_subexpr" => FuncImpl::Scalar(regexp_instr),
        "regexp_substr"
        | "regexp_substr_no_start"
        | "regexp_substr_no_n"
        | "regexp_substr_no_flags"
        | "regexp_substr_no_subexpr" => FuncImpl::Scalar(regexp_substr),
        _ => return None,
    })
}

/// 0A000 for a Regex-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-17", format!("function {}", proc.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;

    fn re(p: &str) -> Regex {
        compile(p, RegexFlags::default()).unwrap()
    }
    fn rei(p: &str) -> Regex {
        compile(p, parse_flags("i").unwrap()).unwrap()
    }
    fn m(p: &str, s: &str) -> bool {
        is_match(&re(p), s).unwrap()
    }
    fn whole(p: &str, s: &str) -> Option<String> {
        find_at(&re(p), s, 0).unwrap().map(|x| s[x.whole.0..x.whole.1].to_string())
    }
    fn t(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }
    fn call<R>(f: impl FnOnce(&FuncCtx) -> R) -> R {
        let env = crate::registry::builtin_env();
        let io = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx { env, io: &io, session: &sess, arg_types: &[], result_type: TypeId::TEXT, collation: 0 };
        f(&ctx)
    }
    fn arr(d: &Datum) -> Vec<Option<String>> {
        d.as_array()
            .unwrap()
            .items
            .iter()
            .map(|x| match x {
                Datum::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
    fn s(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    #[test]
    fn compile_errors() {
        let e = compile("(", RegexFlags::default()).unwrap_err();
        assert_eq!(e.sqlstate.code(), "2201B");
        assert_eq!(e.message, "invalid regular expression: parentheses () not balanced");
        let msg = |p: &str| compile(p, RegexFlags::default()).unwrap_err().message;
        assert_eq!(msg("["), "invalid regular expression: brackets [] not balanced");
        assert_eq!(msg("a{2,1}"), "invalid regular expression: invalid repetition count(s)");
        assert_eq!(msg("a{300}"), "invalid regular expression: invalid repetition count(s)");
        assert_eq!(msg("*a"), "invalid regular expression: quantifier operand invalid");
        assert_eq!(msg("a**"), "invalid regular expression: quantifier operand invalid");
        assert_eq!(msg("\\"), "invalid regular expression: invalid escape \\ sequence");
        assert_eq!(msg("\\q"), "invalid regular expression: invalid escape \\ sequence");
        assert_eq!(msg("(a)\\2"), "invalid regular expression: invalid backreference number");
        assert_eq!(msg("[z-a]"), "invalid regular expression: invalid character range");
        assert_eq!(msg("[[:foo:]]"), "invalid regular expression: invalid character class");
        assert_eq!(msg("a)"), "invalid regular expression: parentheses () not balanced");
        assert_eq!(msg("a{1"), "invalid regular expression: braces {} not balanced");
    }

    #[test]
    fn flags() {
        let e = parse_flags("z").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "invalid regular expression option: \"z\"");
        let f = parse_flags("gi").unwrap();
        assert!(f.global && f.case_insensitive && !f.newline_sensitive);
        let f = parse_flags("n").unwrap();
        assert!(f.newline_sensitive);
        let f = parse_flags("p").unwrap();
        assert!(f.partial_newline && !f.newline_sensitive);
        let f = parse_flags("w").unwrap();
        assert!(f.inverse_partial_newline);
        assert!(parse_flags("q").unwrap().literal);
        assert!(parse_flags("b").unwrap().basic);
    }

    #[test]
    fn basic_matching() {
        assert!(m("abc", "xxabcxx"));
        assert!(!m("abd", "xxabcxx"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "abcd"));
        assert!(m("a.c", "abc"));
        assert!(m("a|b", "b"));
        assert!(m("[[:digit:]]+", "ab12"));
        assert!(!m("[[:digit:]]+", "abcd"));
        assert!(m("\\d{3}", "a123"));
        assert!(!m("\\d{3}", "a12"));
        assert!(m("\\yfoo\\y", "a foo b"));
        assert!(!m("\\yfoo\\y", "afoob"));
        assert!(m("\\mfoo\\M", "a foo"));
        assert!(m("[]a]", "]"));
        assert!(m("[^]a]", "b"));
        assert!(m("[a-]", "-"));
        assert!(m("a{0}b", "b"));
        assert!(m("x*", ""));
        assert!(is_match(&rei("B"), "abc").unwrap());
        assert!(!is_match(&re("B"), "abc").unwrap());
        assert!(m("\\.", "a.b"));
        assert!(!m("\\.", "ab"));
    }

    #[test]
    fn lookaround_and_backrefs() {
        assert!(m("^(?=x)", "xy"));
        assert!(!m("^(?=y)", "xy"));
        assert!(m("^(?!y)", "xy"));
        assert!(m("(?<=a)b", "ab"));
        assert!(!m("(?<=a)b", "cb"));
        assert!(m("(?<!a)b", "cb"));
        assert!(m("(\\w)\\1", "aa"));
        assert!(!m("(\\w)\\1", "ab"));
        assert!(m("(a)\\1", "aa"));
        assert!(is_match(&rei("(a)\\1"), "aA").unwrap());
    }

    #[test]
    fn longest_semantics() {
        assert_eq!(whole("a|ab", "ab").as_deref(), Some("ab"));
        assert_eq!(whole("a*", "aaa").as_deref(), Some("aaa"));
        assert_eq!(whole("a*?", "aaa").as_deref(), Some(""));
        assert_eq!(whole("a+?", "aaa").as_deref(), Some("a"));
        assert_eq!(whole("a*(ab)?", "aab").as_deref(), Some("aab"));
        assert_eq!(whole("o.b", "foobar").as_deref(), Some("oob"));
        assert_eq!(whole("(a|ab)(c|bcd)", "abcd").as_deref(), Some("abcd"));
    }

    #[test]
    fn captures() {
        let r = re("(bar)(beque)");
        let mt = find_at(&r, "foobarbequebaz", 0).unwrap().unwrap();
        assert_eq!(mt.whole, (3, 11));
        assert_eq!(mt.groups, vec![Some((3, 6)), Some((6, 11))]);
        let r = re("(a)|(b)");
        let mt = find_at(&r, "b", 0).unwrap().unwrap();
        assert_eq!(mt.groups, vec![None, Some((0, 1))]);
    }

    #[test]
    fn find_at_offset() {
        let r = re("a");
        let mt = find_at(&r, "aXa", 1).unwrap().unwrap();
        assert_eq!(mt.whole, (2, 3));
        // ^ does not match at the search offset
        assert!(find_at(&re("^a"), "aXa", 2).unwrap().is_none());
        assert_eq!(find_at(&re("é"), "aé", 1).unwrap().unwrap().whole, (1, 3));
    }

    #[test]
    fn newline_modes() {
        let n = parse_flags("n").unwrap();
        assert!(is_match(&compile("^b", n).unwrap(), "a\nb").unwrap());
        assert!(!is_match(&compile("a.b", n).unwrap(), "a\nb").unwrap());
        assert!(is_match(&compile("a.b", RegexFlags::default()).unwrap(), "a\nb").unwrap());
        assert!(!is_match(&compile("a[^x]b", n).unwrap(), "a\nb").unwrap());
        assert!(!is_match(&re("^b"), "a\nb").unwrap());
    }

    #[test]
    fn expanded_literal_and_embedded() {
        let x = parse_flags("x").unwrap();
        assert!(is_match(&compile("a b # comment\n c", x).unwrap(), "abc").unwrap());
        let q = parse_flags("q").unwrap();
        assert!(is_match(&compile("a.c*", q).unwrap(), "xa.c*").unwrap());
        assert!(!is_match(&compile("a.c*", q).unwrap(), "abc").unwrap());
        assert!(m("***=a.c", "a.c"));
        assert!(!m("***=a.c", "abc"));
        assert!(m("(?i)ABC", "abc"));
        assert!(m("(?x) a b ", "ab"));
        assert!(m("(?:a)(?#comment)b", "ab"));
        let b = parse_flags("b").unwrap();
        assert!(is_match(&compile("a\\(b\\)\\1", b).unwrap(), "abb").unwrap());
        assert!(is_match(&compile("a+b", b).unwrap(), "a+b").unwrap());
        assert!(is_match(&compile("a\\{2\\}", b).unwrap(), "aa").unwrap());
    }

    #[test]
    fn similar_form() {
        // Output shape of similar_to_escape for substring(... similar ...)
        assert_eq!(whole("^(?:a){1,1}?(b+)(?:c)$", "abbc").as_deref(), Some("abbc"));
        let r = re("^(?:%){1,1}?(o.b){1,1}(?:.*)$".replace('%', ".*").as_str());
        let mt = find_at(&r, "foobar", 0).unwrap().unwrap();
        assert_eq!(mt.groups[0], Some((1, 4)));
    }

    #[test]
    fn budget_does_not_hang() {
        let subject = "a".repeat(24);
        let r = re("(a*)*b");
        let _ = is_match(&r, &subject);
        let r = re("(a|aa)+$");
        let _ = is_match(&r, &format!("{}b", "a".repeat(60)));
    }

    #[test]
    fn sql_functions() {
        call(|ctx| {
            let f = |name: &str, nargs: u16| -> &'static BuiltinProc {
                crate::catalog_data::PROCS
                    .iter()
                    .find(|p| p.name == name && p.nargs == nargs && p.module == crate::catalog_data::ImplModule::Regex)
                    .unwrap()
            };
            let scalar = |p: &'static BuiltinProc, args: &[Datum]| -> Result<Datum> {
                match lookup(p).unwrap() {
                    FuncImpl::Scalar(fp) => fp(args, ctx),
                    _ => panic!("not scalar"),
                }
            };
            let set = |p: &'static BuiltinProc, args: &[Datum]| -> Result<Vec<Datum>> {
                match lookup(p).unwrap() {
                    FuncImpl::Set(fp) => fp(args, ctx)?.collect(),
                    _ => panic!("not set"),
                }
            };
            // replace
            let r = scalar(f("regexp_replace", 4), &[t("foo"), t("o"), t("0"), t("g")]).unwrap();
            assert_eq!(r, t("f00"));
            let r = scalar(f("regexp_replace", 3), &[t("foo bar"), t("o"), t("0")]).unwrap();
            assert_eq!(r, t("f0o bar"));
            let e = scalar(f("regexp_replace", 4), &[t("foo bar"), t("o"), t("0"), t("z")]).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22023");
            assert_eq!(e.message, "invalid regular expression option: \"z\"");
            let r = scalar(f("regexp_replace", 3), &[t("hello world"), t("(\\w+) (\\w+)"), t("\\2 \\1 [\\&] \\\\")]).unwrap();
            assert_eq!(r, t("world hello [hello world] \\"));
            let r = scalar(f("regexp_replace", 6), &[t("abcabc"), t("b"), t("X"), Datum::Int4(3), Datum::Int4(1), t("")]).unwrap();
            assert_eq!(r, t("abcaXc"));
            let r = scalar(f("regexp_replace", 5), &[t("aaa"), t("a"), t("b"), Datum::Int4(1), Datum::Int4(0)]).unwrap();
            assert_eq!(r, t("bbb"));
            let r = scalar(f("regexp_replace", 3), &[t("abc"), t(""), t("-")]).unwrap();
            assert_eq!(r, t("-abc"));
            let r = scalar(f("regexp_replace", 4), &[t("abc"), t(""), t("-"), t("g")]).unwrap();
            assert_eq!(r, t("-a-b-c-"));
            // match
            let r = scalar(f("regexp_match", 2), &[t("foobarbequebaz"), t("(bar)(beque)")]).unwrap();
            assert_eq!(arr(&r), vec![s("bar"), s("beque")]);
            let r = scalar(f("regexp_match", 2), &[t("foobarbequebaz"), t("bar.*que")]).unwrap();
            assert_eq!(arr(&r), vec![s("barbeque")]);
            let r = scalar(f("regexp_match", 2), &[t("foo"), t("x")]).unwrap();
            assert!(r.is_null());
            let e = scalar(f("regexp_match", 3), &[t("foo"), t("x"), t("g")]).unwrap_err();
            assert_eq!(e.message, "regexp_match() does not support the \"global\" option");
            assert_eq!(e.hint.as_deref(), Some("Use the regexp_matches function instead."));
            // matches
            let rows = set(f("regexp_matches", 3), &[t("foobarbequebazilbarfbonk"), t("(b[^b]+)(b[^b]+)"), t("g")]).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(arr(&rows[0]), vec![s("bar"), s("beque")]);
            assert_eq!(arr(&rows[1]), vec![s("bazil"), s("barf")]);
            let rows = set(f("regexp_matches", 2), &[t("abab"), t("ab")]).unwrap();
            assert_eq!(rows.len(), 1);
            // split
            let r = scalar(f("regexp_split_to_array", 2), &[t("hello world"), t("\\s+")]).unwrap();
            assert_eq!(arr(&r), vec![s("hello"), s("world")]);
            let r = scalar(f("regexp_split_to_array", 2), &[t("abc"), t("")]).unwrap();
            assert_eq!(arr(&r), vec![s("a"), s("b"), s("c")]);
            let rows = set(f("regexp_split_to_table", 2), &[t("a,b,,c"), t(",")]).unwrap();
            assert_eq!(rows, vec![t("a"), t("b"), t(""), t("c")]);
            // count / instr / like / substr
            assert_eq!(scalar(f("regexp_count", 2), &[t("ABCABCABC"), t("A")]).unwrap(), Datum::Int4(3));
            assert_eq!(scalar(f("regexp_count", 3), &[t("ABCABCABC"), t("A"), Datum::Int4(2)]).unwrap(), Datum::Int4(2));
            assert_eq!(scalar(f("regexp_instr", 2), &[t("abcabc"), t("c")]).unwrap(), Datum::Int4(3));
            assert_eq!(scalar(f("regexp_instr", 4), &[t("abcabc"), t("c"), Datum::Int4(1), Datum::Int4(2)]).unwrap(), Datum::Int4(6));
            assert_eq!(scalar(f("regexp_like", 2), &[t("abc"), t("B")]).unwrap(), Datum::Bool(false));
            assert_eq!(scalar(f("regexp_like", 3), &[t("abc"), t("B"), t("i")]).unwrap(), Datum::Bool(true));
            assert_eq!(scalar(f("regexp_substr", 2), &[t("foobar"), t("o+")]).unwrap(), t("oo"));
            assert!(scalar(f("regexp_substr", 2), &[t("foobar"), t("x")]).unwrap().is_null());
            // substring
            let sub = crate::catalog_data::builtin_proc(2073).unwrap();
            assert_eq!(scalar(sub, &[t("foobar"), t("o.b")]).unwrap(), t("oob"));
            assert_eq!(scalar(sub, &[t("foobar"), t("o(.)b")]).unwrap(), t("o"));
            assert!(scalar(sub, &[t("foobar"), t("zzz")]).unwrap().is_null());
            // operators
            let eq = crate::catalog_data::builtin_proc(1254).unwrap();
            assert_eq!(scalar(eq, &[t("abc"), t("b")]).unwrap(), Datum::Bool(true));
            let e = scalar(eq, &[t("abc"), t("(")]).unwrap_err();
            assert_eq!(e.sqlstate.code(), "2201B");
            assert_eq!(e.message, "invalid regular expression: parentheses () not balanced");
            let ic = crate::catalog_data::builtin_proc(1238).unwrap();
            assert_eq!(scalar(ic, &[t("abc"), t("B")]).unwrap(), Datum::Bool(true));
        });
    }
}
