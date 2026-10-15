//! Process context, output sinks and the CLI error type. Fully implemented, no lease.
//!
//! * [`Ctx`] carries the global flags and the two output sinks (injectable for tests). Commands
//!   write success output with [`Ctx::out_line`] / [`Ctx::emit_json`] and diagnostics with
//!   [`Ctx::info`] (suppressed by `--quiet`), [`Ctx::debug`] (needs `-v`).
//! * [`CliError`] is what commands return. `render` prints it: human form
//!   `pgarbor: error: <message>` (+ `  detail:` / `  hint:` lines) on stderr, or with `--json` one JSON
//!   object per error on stderr (stdout stays clean):
//!   `{"error":{"kind":"locked","exit_code":4,"sqlstate":"55006","message":"...","detail":null,"hint":null}}`.
use std::io::Write;

use pgarbor_common::PgError;

use crate::exit::{classify, Exit};

/// Result type of every command: `Ok(exit status)` or an error to render.
pub type CliResult<T = Exit> = Result<T, CliError>;

/// Global flags + sinks.
pub struct Ctx {
    pub json: bool,
    pub quiet: bool,
    pub verbose: u8,
    pub out: Box<dyn Write>,
    pub err: Box<dyn Write>,
}

impl Ctx {
    /// Real stdout/stderr.
    pub fn stdio(json: bool, quiet: bool, verbose: u8) -> Ctx {
        Ctx { json, quiet, verbose, out: Box::new(std::io::stdout()), err: Box::new(std::io::stderr()) }
    }
    /// Sinks supplied by a test.
    pub fn with_sinks(json: bool, out: Box<dyn Write>, err: Box<dyn Write>) -> Ctx {
        Ctx { json, quiet: false, verbose: 0, out, err }
    }
    /// One line on stdout (flushed: harnesses wait on lines like `pgarbor: listening on ...`).
    pub fn out_line(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }
    /// Informational line on stderr unless `--quiet` or `--json`.
    pub fn info(&mut self, line: &str) {
        if !self.quiet && !self.json {
            let _ = writeln!(self.err, "{line}");
        }
    }
    /// Debug line on stderr when `-v`.
    pub fn debug(&mut self, line: &str) {
        if self.verbose > 0 {
            let _ = writeln!(self.err, "pgarbor: {line}");
        }
    }
    /// Pretty-printed JSON document on stdout.
    pub fn emit_json<T: serde::Serialize>(&mut self, value: &T) -> CliResult<()> {
        let text = serde_json::to_string_pretty(value).map_err(|e| CliError::internal(format!("json serialization failed: {e}")))?;
        self.out_line(&text);
        Ok(())
    }
    /// One compact JSON line on stdout (event streams such as the `serve` listening event).
    pub fn emit_json_line<T: serde::Serialize>(&mut self, value: &T) -> CliResult<()> {
        let text = serde_json::to_string(value).map_err(|e| CliError::internal(format!("json serialization failed: {e}")))?;
        self.out_line(&text);
        Ok(())
    }
}

/// A failed command.
#[derive(Debug, Clone)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub sqlstate: Option<String>,
}

impl CliError {
    pub fn new(exit: Exit, message: impl Into<String>) -> CliError {
        CliError { exit, message: message.into(), detail: None, hint: None, sqlstate: None }
    }
    pub fn failure(message: impl Into<String>) -> CliError {
        CliError::new(Exit::FAILURE, message)
    }
    pub fn usage(message: impl Into<String>) -> CliError {
        CliError::new(Exit::USAGE, message)
    }
    pub fn not_found(message: impl Into<String>) -> CliError {
        CliError::new(Exit::NOT_FOUND, message)
    }
    pub fn refused(message: impl Into<String>) -> CliError {
        CliError::new(Exit::REFUSED, message)
    }
    pub fn internal(message: impl Into<String>) -> CliError {
        CliError::new(Exit::INTERNAL, message)
    }
    pub fn data(message: impl Into<String>) -> CliError {
        CliError::new(Exit::DATA_ERR, message)
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> CliError {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> CliError {
        self.detail = Some(detail.into());
        self
    }
    /// I/O failure with context: `<context>: <os error>`.
    pub fn io(context: &str, err: &std::io::Error) -> CliError {
        let exit = match err.kind() {
            std::io::ErrorKind::NotFound => Exit::NOT_FOUND,
            std::io::ErrorKind::AlreadyExists => Exit::EXISTS,
            std::io::ErrorKind::AddrInUse => Exit::ADDR_IN_USE,
            std::io::ErrorKind::PermissionDenied => Exit::REFUSED,
            _ => Exit::FAILURE,
        };
        let message = if exit == Exit::ADDR_IN_USE { format!("{context}: address in use") } else { format!("{context}: {err}") };
        CliError::new(exit, message)
    }

    /// Print per the module docs.
    pub fn render(&self, ctx: &mut Ctx) {
        if ctx.json {
            let v = serde_json::json!({
                "error": {
                    "kind": self.exit.name(),
                    "exit_code": self.exit.code(),
                    "sqlstate": self.sqlstate,
                    "message": self.message,
                    "detail": self.detail,
                    "hint": self.hint,
                }
            });
            let _ = writeln!(ctx.err, "{v}");
        } else {
            let _ = writeln!(ctx.err, "pgarbor: error: {}", self.message);
            if let Some(d) = &self.detail {
                let _ = writeln!(ctx.err, "  detail: {d}");
            }
            if let Some(h) = &self.hint {
                let _ = writeln!(ctx.err, "  hint: {h}");
            }
        }
        let _ = ctx.err.flush();
    }
}

impl From<PgError> for CliError {
    fn from(e: PgError) -> CliError {
        let exit = classify(&e);
        let mut message = e.message.clone();
        if exit == Exit::CORRUPT && !message.to_ascii_lowercase().contains("corrupt") {
            message = format!("database file is corrupt: {message}");
        }
        CliError { exit, message, detail: e.detail.clone(), hint: e.hint.clone(), sqlstate: Some(e.sqlstate.code().to_string()) }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> CliError {
        CliError::io("I/O error", &e)
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
