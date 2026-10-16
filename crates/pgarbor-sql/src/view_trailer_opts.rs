//! Trailing clause of `CREATE [MATERIALIZED] VIEW`: `WITH [NO] DATA` (materialized views, gram.y
//! `opt_with_data`) and `WITH [CASCADED | LOCAL] CHECK OPTION` (plain views, gram.y `opt_check_option`).
//!
//! sqlparser 0.63 `CreateView` carries neither clause, and its parser stops the statement at a trailing
//! `WITH`, so before sqlparser sees a chunk, [`extract`] removes the clause from the END of the tokens and
//! returns it as a [`ViewTrailer`]. PostgreSQL accepts `WITH [NO] DATA` only after a materialized view's
//! query and `WITH CHECK OPTION` only after a plain view's query; any other trailing `WITH` is left in place
//! so sqlparser reports the syntax error at it, as PostgreSQL does. The binder reads the trailer from
//! `Binder::view_trailer` (see `binder::ddl_view`).
use sqlparser::tokenizer::{Token, TokenWithSpan};

/// The trailing clause of one `CREATE [MATERIALIZED] VIEW` statement. Both fields are `None` when the
/// statement has no such clause.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewTrailer {
    /// `Some(true)` for `WITH DATA`, `Some(false)` for `WITH NO DATA`.
    pub with_data: Option<bool>,
    /// `"cascaded"` or `"local"` (lower case). A bare `WITH CHECK OPTION` means `"cascaded"`.
    pub check_option: Option<String>,
}

/// Upper-cased text of an unquoted word token (`None` for any other token).
fn upper_word(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    }
}

/// `Some(materialized)` when the significant tokens start with `CREATE [OR REPLACE] [TEMP|TEMPORARY|RECURSIVE]
/// [MATERIALIZED] VIEW`; `None` for any other statement.
fn view_kind(toks: &[TokenWithSpan], sig: &[usize]) -> Option<bool> {
    let word = |k: usize| sig.get(k).and_then(|&i| upper_word(&toks[i].token));
    if word(0).as_deref() != Some("CREATE") {
        return None;
    }
    let mut materialized = false;
    let mut k = 1;
    while let Some(w) = word(k) {
        match w.as_str() {
            "OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "RECURSIVE" => {}
            "MATERIALIZED" => materialized = true,
            "VIEW" => return Some(materialized),
            _ => return None,
        }
        k += 1;
    }
    None
}

/// Remove the trailing `WITH [NO] DATA` (materialized views) or `WITH [CASCADED|LOCAL] CHECK OPTION` (plain
/// views) of a chunk and return the reduced tokens with the clause. Other chunks, and chunks without such a
/// clause, come back unchanged with `None`. `toks` may end with `Token::EOF`, which is kept.
pub fn extract(toks: &[TokenWithSpan]) -> (Vec<TokenWithSpan>, Option<ViewTrailer>) {
    let sig: Vec<usize> = (0..toks.len()).filter(|&i| !matches!(toks[i].token, Token::Whitespace(_) | Token::EOF)).collect();
    let Some(materialized) = view_kind(toks, &sig) else {
        return (toks.to_vec(), None);
    };
    let n = sig.len();
    // The word `back` significant tokens from the end (1 = last token), `None` for other tokens.
    let last = |back: usize| n.checked_sub(back).and_then(|k| upper_word(&toks[sig[k]].token));
    let is = |back: usize, word: &str| last(back).as_deref() == Some(word);

    let found: Option<(usize, ViewTrailer)> = if materialized {
        if is(1, "DATA") && is(2, "WITH") {
            Some((2, ViewTrailer { with_data: Some(true), check_option: None }))
        } else if is(1, "DATA") && is(2, "NO") && is(3, "WITH") {
            Some((3, ViewTrailer { with_data: Some(false), check_option: None }))
        } else {
            None
        }
    } else if is(1, "OPTION") && is(2, "CHECK") && is(3, "WITH") {
        Some((3, ViewTrailer { with_data: None, check_option: Some("cascaded".to_string()) }))
    } else if is(1, "OPTION") && is(2, "CHECK") && is(4, "WITH") && (is(3, "CASCADED") || is(3, "LOCAL")) {
        let level = last(3).unwrap_or_default().to_ascii_lowercase();
        Some((4, ViewTrailer { with_data: None, check_option: Some(level) }))
    } else {
        None
    };

    let Some((cut, trailer)) = found else {
        return (toks.to_vec(), None);
    };
    let first_cut = n - cut;
    let removed: Vec<usize> = sig[first_cut..].to_vec();
    let out: Vec<TokenWithSpan> = toks
        .iter()
        .enumerate()
        .filter(|(i, _)| !removed.contains(i))
        .map(|(_, t)| t.clone())
        .collect();
    (out, Some(trailer))
}
