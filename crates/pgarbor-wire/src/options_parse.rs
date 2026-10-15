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
use pgarbor_common::{PgError, Result};

/// Split an `options` string into tokens (module docs, escaping rules).
/// Example: `split_options("-c search_path=a\\ b  -c x=1")` -> `["-c", "search_path=a b", "-c", "x=1"]`.
#[allow(unused_variables)]
pub fn split_options(options: &str) -> Result<Vec<String>> {
    Err(PgError::unimplemented("WIR-07", "split the options string (options_parse::split_options)"))
}

/// Parse `options` into `(name, value)` pairs (module docs, error cases).
/// Examples: `"-c DateStyle=ISO,MDY --statement-timeout=5s"` -> `[("DateStyle","ISO,MDY"),
/// ("statement_timeout","5s")]`; `"-cwork_mem=64kB"` -> `[("work_mem","64kB")]`; `""` -> `[]`;
/// `"-c foo"` -> FATAL 42601 `invalid command-line argument for server process: foo`.
#[allow(unused_variables)]
pub fn parse_options(options: &str) -> Result<Vec<(String, String)>> {
    Err(PgError::unimplemented("WIR-07", "parse the options string (options_parse::parse_options)"))
}

/// Assemble the `SessionOptions::startup_gucs` list (module docs, assembly order).
#[allow(unused_variables)]
pub fn build_startup_gucs(
    defaults: &[(String, String)],
    params: &[(String, String)],
    options: &str,
) -> Result<Vec<(String, String)>> {
    Err(PgError::unimplemented("WIR-07", "assemble startup GUCs (options_parse::build_startup_gucs)"))
}

/// True for startup parameter names that are not GUCs (see module docs).
pub fn is_reserved_startup_param(name: &str) -> bool {
    matches!(name, "user" | "database" | "options" | "replication" | "application_name") || name.starts_with("_pq_.")
}
