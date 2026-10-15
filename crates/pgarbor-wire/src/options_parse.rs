//! Startup `options` parameter and startup GUC assembly (lease WIR-07). PG:
//! `postmaster/postmaster.c` `process_startup_options`, `utils/misc/guc.c` `ParseLongOption`,
//! `pg_split_opts` (`port/...`).
//!
//! ## `options` syntax
//! The value is a command-line fragment. It is split into tokens on unescaped whitespace (space,
//! tab, newline, CR, FF, VT); a backslash makes the next character literal (`\ ` is a space inside a
//! token, `\\` a backslash; a trailing lone backslash is kept as a backslash). Then:
//! * `-c name=value` (two tokens) and `-cname=value` (one token) set a GUC;
//! * `--name=value` sets a GUC; any `-` in `name` becomes `_` (`--statement-timeout=5s`);
//! * `-c name` without `=` or `--name` without `=`: FATAL `42601`
//!   `invalid command-line argument for server process: <token>` (no quotes around the token);
//! * any other token (`-d 5`, `-B 100`, ...): the same error. (PG accepts a few server switches;
//!   PGArbor accepts only `-c` and `--`.)
//! * `name` is case-insensitive in PG GUC lookup; this function does NOT validate names or values
//!   (the session does, with PG's own errors) and preserves the case as given.
//!
//! ## Assembly order ([`build_startup_gucs`])
//! 1. `defaults` (`ServerConfig::default_gucs`) in order;
//! 2. every startup parameter that is not reserved, in packet order. Reserved (handled elsewhere):
//!    `user`, `database`, `options`, `replication`, `application_name`, and anything starting with
//!    `_pq_.` (protocol options). Everything else (e.g. `client_encoding`, `DateStyle`, `TimeZone`,
//!    `search_path`, `extra_float_digits`) is forwarded and validated by the session;
//! 3. the `-c`/`--` settings of `options`, in order.
//! Later duplicates simply come later in the list (the session applies them in order).
use pgarbor_common::{PgError, Result, Severity, SqlState};

/// Split an `options` string into tokens (module docs, escaping rules).
/// Example: `split_options("-c search_path=a\\ b  -c x=1")` -> `["-c", "search_path=a b", "-c", "x=1"]`.
pub fn split_options(options: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut escaped = false;

    for ch in options.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if is_option_whitespace(ch) {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn is_option_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

/// Parse `options` into `(name, value)` pairs (module docs, error cases).
/// Examples: `"-c DateStyle=ISO,MDY --statement-timeout=5s"` -> `[("DateStyle","ISO,MDY"),
/// ("statement_timeout","5s")]`; `"-cwork_mem=64kB"` -> `[("work_mem","64kB")]`; `""` -> `[]`;
/// `"-c foo"` -> FATAL 42601 `invalid command-line argument for server process: foo`.
pub fn parse_options(options: &str) -> Result<Vec<(String, String)>> {
    let tokens = split_options(options)?;
    let mut pairs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-c" {
            i += 1;
            if i >= tokens.len() {
                return Err(invalid_option(token));
            }
            let argument = &tokens[i];
            let Some((name, value)) = argument.split_once('=') else {
                return Err(invalid_option(argument));
            };
            pairs.push((name.to_string(), value.to_string()));
        } else if let Some(argument) = token.strip_prefix("-c") {
            let Some((name, value)) = argument.split_once('=') else {
                return Err(invalid_option(token));
            };
            pairs.push((name.to_string(), value.to_string()));
        } else if let Some(argument) = token.strip_prefix("--") {
            let Some((name, value)) = argument.split_once('=') else {
                return Err(invalid_option(token));
            };
            pairs.push((name.replace('-', "_"), value.to_string()));
        } else {
            return Err(invalid_option(token));
        }
        i += 1;
    }
    Ok(pairs)
}

/// Assemble the `SessionOptions::startup_gucs` list (module docs, assembly order).
pub fn build_startup_gucs(
    defaults: &[(String, String)],
    params: &[(String, String)],
    options: &str,
) -> Result<Vec<(String, String)>> {
    let option_gucs = parse_options(options)?;
    let mut gucs = Vec::with_capacity(defaults.len() + params.len() + option_gucs.len());
    gucs.extend(defaults.iter().cloned());
    gucs.extend(
        params
            .iter()
            .filter(|(name, _)| !is_reserved_startup_param(name))
            .cloned(),
    );
    gucs.extend(option_gucs);
    Ok(gucs)
}

fn invalid_option(token: &str) -> PgError {
    PgError::new(
        SqlState::SYNTAX_ERROR,
        format!("invalid command-line argument for server process: {token}"),
    )
    .with_severity(Severity::Fatal)
}

/// True for startup parameter names that are not GUCs (see module docs).
pub fn is_reserved_startup_param(name: &str) -> bool {
    matches!(name, "user" | "database" | "options" | "replication" | "application_name") || name.starts_with("_pq_.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_escaped_and_supported_whitespace() {
        assert_eq!(
            split_options("-c search_path=a\\ b  -c x=1\t--y=z\nq\rff\x0cw\x0bv").unwrap(),
            vec!["-c", "search_path=a b", "-c", "x=1", "--y=z", "q", "ff", "w", "v"]
        );
        assert_eq!(split_options("x\\\\y tail\\").unwrap(), vec!["x\\y", "tail\\"]);
    }

    #[test]
    fn parses_short_and_long_forms() {
        assert_eq!(
            parse_options("-c DateStyle=ISO,MDY -cstatement_timeout=5s --work-mem=64kB").unwrap(),
            vec![
                ("DateStyle".into(), "ISO,MDY".into()),
                ("statement_timeout".into(), "5s".into()),
                ("work_mem".into(), "64kB".into())
            ]
        );
        assert_eq!(parse_options("").unwrap(), Vec::<(String, String)>::new());
    }

    #[test]
    fn malformed_options_are_fatal_syntax_errors() {
        for options in ["-c foo", "--foo", "-d 5", "-c"] {
            let err = parse_options(options).unwrap_err();
            assert_eq!(err.sqlstate, SqlState::SYNTAX_ERROR);
            assert_eq!(err.severity, Severity::Fatal);
        }
        assert_eq!(
            parse_options("-c foo").unwrap_err().message,
            "invalid command-line argument for server process: foo"
        );
    }

    #[test]
    fn startup_gucs_preserve_order_and_filter_reserved_parameters() {
        let defaults = vec![("statement_timeout".into(), "1s".into())];
        let params = vec![
            ("user".into(), "alice".into()),
            ("DateStyle".into(), "ISO".into()),
            ("_pq_.x".into(), "y".into()),
            ("application_name".into(), "client".into()),
        ];
        assert_eq!(
            build_startup_gucs(&defaults, &params, "-c statement_timeout=5s").unwrap(),
            vec![
                ("statement_timeout".into(), "1s".into()),
                ("DateStyle".into(), "ISO".into()),
                ("statement_timeout".into(), "5s".into()),
            ]
        );
    }
}
