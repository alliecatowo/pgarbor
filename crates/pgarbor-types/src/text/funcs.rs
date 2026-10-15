//! String functions and character-type operators (TYPE-13). Lease TYP-14.
//!
//! PG18 reference: `utils/adt/varlena.c`, `varchar.c`, `name.c`, `char.c`, `oracle_compat.c`,
//! `formatting.c` (lower/upper/initcap), `ascii.c`.
//! Owned procs (ImplModule::Text; `grep 'module: ImplModule::Text' src/catalog_data/procs.rs`), by prosrc:
//! - comparison (codepoint/bytewise order for every collation in v1): `texteq textne text_lt text_le
//!   text_gt text_ge bttextcmp text_larger text_smaller text_pattern_* bttext_pattern_cmp`,
//!   name: `nameeq namene namelt namele namegt namege btnamecmp` and cross-type `nameeqtext namelttext ...
//!   texteqname textltname ... btnametextcmp bttextnamecmp`; bpchar: `bpchareq bpcharne bpcharlt ...
//!   bpcharcmp bpchar_larger bpchar_smaller bpchar_pattern_* btbpchar_pattern_cmp` (all compare with
//!   trailing spaces REMOVED: `'ab  '::char(4) = 'ab'` is true); "char": `chareq charne charlt ... btcharcmp`
//!   (unsigned byte order); hashes `hashtext hashname hashbpchar hashchar (+extended)` (bpchar hashes the
//!   trimmed value).
//! - `textcat` (`||`), `textlen` (char_length: characters), `bpcharlen` (characters, trailing spaces
//!   excluded: `length('ab  '::char(4))` = 2), `textoctetlen`/`bpcharoctetlen` (bytes; bpchar INCLUDES padding),
//!   `bit_length` (SQL-language, oid 1811: octet_length*8), `text_substr` / `text_substr_no_len`
//!   (`substr(s, start [, count])`, 1-based, start may be <= 0: `substr('abc', 0, 2)` = `a`; negative count →
//!   22011 `negative substring length not allowed`), `textoverlay(_no_len)`, `textpos` (position, 1-based,
//!   0 if absent, empty needle → 1), `text_starts_with`, `lower`, `upper`, `initcap` (word = run of
//!   alphanumerics), `casefold`, `ltrim/rtrim/btrim` (2-arg: trim chars set) and `ltrim1/rtrim1/btrim1`
//!   (spaces), `lpad`/`rpad` (truncate to length when longer; empty fill → no padding; SQL-language
//!   2-arg versions oids 879/880 pad with spaces), `repeat` (count <= 0 → ''), `replace_text`,
//!   `split_part` (field 0 → 22023 `field position must not be zero`; negative counts from the end),
//!   `translate`, `text_reverse`, `text_left`, `text_right` (negative n = all but |n|), `ascii`
//!   (code point of first char, 0 for ''), `chr` (0 → 54000 `null character not permitted`; invalid
//!   code point → 54000 `requested character too large for encoding: N`), `unistr`, `to_ascii_*`
//!   (unsupported → keep in `lookup` returning None), `unicode_normalize_func`, `unicode_is_normalized`,
//!   `unicode_assigned`;
//! - casts/conversions: `text_name`, `name_text`, `bpchar_name`, `name_bpchar`, `text_char`, `char_text`,
//!   `char_bpchar`, `chartoi4`, `i4tochar` (outside -128..=127 → 22003 `"char" out of range`),
//!   `nameconcatoid`;
//! - SQL-language: `textanycat` (oid 2003, `text || anynonarray`) and `anytextcat` (oid 2004): convert the
//!   non-text arg with `crate::io::output` of `ctx.arg_types[i]` and concatenate.
//!
//! Oracle: `'ab'::char(4) || 'x'` = `abx` (bpchar → text cast strips padding);
//! `length('ab  '::char(4))` = 2; `'abcd'::varchar(3)` = `abc` (explicit cast truncates).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Text-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-14", format!("function {}", proc.name))
}
