//! Identifier and qualified-name helpers shared by the parser adapter and the binder.
//! Fully implemented, frozen (v1).
//!
//! PostgreSQL rules (`scansup.c` `downcase_identifier`, `truncate_identifier`):
//! * An UNQUOTED identifier is folded to lower case (ASCII letters only; other characters are left
//!   alone; sqlparser keeps the original case).
//! * A quoted identifier keeps its text verbatim (`"A"` stays `A`).
//! * An identifier longer than 63 BYTES is truncated at a character boundary and a NOTICE
//!   `identifier "<orig>" will be truncated to "<trunc>"` (00000, severity NOTICE) is raised
//!   (verified: `create table <70 chars>` -> NOTICE, not an error).
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{Ident, ObjectName, ObjectNamePart};

use crate::source::SourceMap;

/// PG `NAMEDATALEN - 1`.
pub const MAX_IDENT_BYTES: usize = 63;

/// Fold an identifier per PG rules (no truncation).
pub fn fold_ident(id: &Ident) -> String {
    match id.quote_style {
        Some(_) => id.value.clone(),
        None => id.value.to_ascii_lowercase(),
    }
}

/// Truncate to 63 bytes at a char boundary; `None` when no truncation was needed.
pub fn truncate_ident(name: &str) -> Option<String> {
    if name.len() <= MAX_IDENT_BYTES {
        return None;
    }
    let mut end = MAX_IDENT_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Some(name[..end].to_string())
}

/// Fold + truncate; pushes the truncation NOTICE onto `notices`.
pub fn ident_name(id: &Ident, notices: &mut Vec<PgError>) -> String {
    let folded = fold_ident(id);
    match truncate_ident(&folded) {
        Some(t) => {
            let mut n = PgError::notice(format!(
                "identifier \"{folded}\" will be truncated to \"{t}\""
            ));
            n.sqlstate = SqlState::NAME_TOO_LONG;
            notices.push(n);
            t
        }
        None => folded,
    }
}

/// A possibly qualified name `[[catalog.]schema.]name`, folded.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QualName {
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    /// Error position (1-based char offset) of the first part.
    pub pos: Option<u32>,
}

impl QualName {
    /// Unqualified name.
    pub fn simple(name: impl Into<String>) -> QualName {
        QualName { catalog: None, schema: None, name: name.into(), pos: None }
    }
    /// `schema.name` or `name` as written (for messages).
    pub fn display(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }
    pub fn schema_str(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

/// Convert an `ObjectName` (1..3 parts) to a [`QualName`].
///
/// Errors: 4+ parts -> `42601 improper qualified name (too many dotted names): a.b.c.d`;
/// function-valued parts (`ObjectNamePart::Function`) -> 42601 syntax error.
/// Cross-database references (3 parts whose first part is not `current_database`) are rejected by
/// the caller (`diag::cross_database`) because only it knows the database name.
pub fn qual_name(name: &ObjectName, src: &SourceMap, notices: &mut Vec<PgError>) -> Result<QualName> {
    let mut parts: Vec<&Ident> = Vec::with_capacity(name.0.len());
    for p in &name.0 {
        match p {
            ObjectNamePart::Identifier(i) => parts.push(i),
            ObjectNamePart::Function(f) => {
                return Err(PgError::syntax("syntax error", src.pos_of_span(f.name.span)));
            }
        }
    }
    let pos = parts.first().and_then(|i| src.pos_of_span(i.span));
    let folded: Vec<String> = parts.iter().map(|i| ident_name(i, notices)).collect();
    match folded.len() {
        0 => Err(PgError::syntax("syntax error", pos)),
        1 => Ok(QualName { catalog: None, schema: None, name: folded[0].clone(), pos }),
        2 => Ok(QualName { catalog: None, schema: Some(folded[0].clone()), name: folded[1].clone(), pos }),
        3 => Ok(QualName {
            catalog: Some(folded[0].clone()),
            schema: Some(folded[1].clone()),
            name: folded[2].clone(),
            pos,
        }),
        _ => {
            let mut e = PgError::new(
                SqlState::SYNTAX_ERROR,
                format!("improper qualified name (too many dotted names): {}", folded.join(".")),
            );
            e.position = pos;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_and_truncation() {
        let up = Ident::new("FooBar");
        assert_eq!(fold_ident(&up), "foobar");
        let q = Ident::with_quote('"', "FooBar");
        assert_eq!(fold_ident(&q), "FooBar");
        let long = "a".repeat(70);
        assert_eq!(truncate_ident(&long).unwrap().len(), 63);
        let mut n = Vec::new();
        let id = Ident::new(long.clone());
        assert_eq!(ident_name(&id, &mut n).len(), 63);
        assert_eq!(n.len(), 1);
    }
}
