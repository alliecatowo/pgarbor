//! `pgarbor dump` (lease CLI-26): pg_dump-like plain SQL schema + data.
//!
//! The catalog query surface is still being implemented by the embedded SQL leases. This command
//! refuses to write a partial dump until those catalog queries can provide a complete inventory.
use pgarbor_common::PgError;
use crate::cli::DumpArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

/// Run a consistent plain SQL dump. A partial dump is never presented as successful output.
pub fn run(ctx: &mut Ctx, args: &DumpArgs) -> CliResult<Exit> {
    if ctx.json && args.output.is_none() {
        return Err(CliError::usage("--json requires --output for dump"));
    }
    if args.copy {
        return Err(CliError::from(PgError::unimplemented(
            "CLI-26",
            "dump --copy needs COPY streaming (not implemented yet)",
        )));
    }
    Err(CliError::from(PgError::unimplemented(
        "CLI-26",
        "dump catalog inventory queries are not available yet",
    )))
}

/// PostgreSQL `quote_ident`: leaves simple lowercase non-keywords bare; otherwise quotes and
/// doubles embedded double quotes.
pub fn quote_ident(s: &str) -> Result<String, CliError> {
    const RESERVED: &[&str] = &[
        "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "both",
        "case", "cast", "check", "collate", "column", "constraint", "create", "current_catalog",
        "current_date", "current_role", "current_time", "current_timestamp", "current_user", "default",
        "deferrable", "desc", "distinct", "do", "else", "end", "except", "false", "fetch", "for",
        "foreign", "from", "grant", "group", "having", "in", "initially", "intersect", "into",
        "lateral", "leading", "limit", "localtime", "localtimestamp", "not", "null", "offset", "on",
        "only", "or", "order", "placing", "primary", "references", "returning", "select",
        "session_user", "some", "symmetric", "system_user", "table", "then", "to", "trailing", "true",
        "union", "unique", "user", "using", "variadic", "when", "where", "window", "with",
    ];
    let mut chars = s.chars();
    let first_ok = chars.next().is_some_and(|c| c == '_' || c.is_ascii_lowercase());
    let body_ok = s.chars().all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit());
    if first_ok && body_ok && !RESERVED.contains(&s) {
        Ok(s.to_owned())
    } else {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            out.push(c);
            if c == '"' { out.push('"'); }
        }
        out.push('"');
        Ok(out)
    }
}

/// PostgreSQL literal quoting under `standard_conforming_strings = on`.
pub fn quote_literal(s: &str) -> Result<String, CliError> {
    let escaped = s.contains('\\');
    let mut out = String::with_capacity(s.len() + if escaped { 3 } else { 2 });
    if escaped { out.push('E'); }
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if escaped => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_quoting() {
        for (input, expected) in [("simple_name", "simple_name"), ("select", "\"select\""),
            ("Mixed", "\"Mixed\""), ("a\"b", "\"a\"\"b\"")] {
            assert_eq!(quote_ident(input).expect("quote identifier"), expected);
        }
    }

    #[test]
    fn literal_quoting() {
        assert_eq!(quote_literal("plain").expect("quote literal"), "'plain'");
        assert_eq!(quote_literal("it's").expect("quote literal"), "'it''s'");
        assert_eq!(quote_literal("a\\b").expect("quote literal"), "E'a\\\\b'");
    }
}
