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
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

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

/// Parse a flags string (`"gi"`). Errors per module docs.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_flags(flags: &str) -> Result<RegexFlags> {
    Err(PgError::unimplemented("TYP-17", "regular expression flags"))
}

/// Compile an ARE. Errors: 2201B `invalid regular expression: ...`.
// CONTRACT
#[allow(unused_variables)]
pub fn compile(pattern: &str, flags: RegexFlags) -> Result<Regex> {
    Err(PgError::unimplemented("TYP-17", "regular expressions"))
}

/// First match at or after byte offset `start` (must be a char boundary), or None.
// CONTRACT
#[allow(unused_variables)]
pub fn find_at(re: &Regex, subject: &str, start: usize) -> Result<Option<RegexMatch>> {
    Err(PgError::unimplemented("TYP-17", "regular expression matching"))
}

/// True if `re` matches anywhere in `subject` (the `~` operator).
// CONTRACT
#[allow(unused_variables)]
pub fn is_match(re: &Regex, subject: &str) -> Result<bool> {
    Err(PgError::unimplemented("TYP-17", "regular expression matching"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Regex-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-17", format!("function {}", proc.name))
}
