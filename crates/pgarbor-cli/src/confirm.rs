//! Destructive-action confirmation ("no surprise destructive actions", RFC 001 section 10).
//! Fully implemented, no lease.
use std::io::{BufRead, IsTerminal, Write};

use crate::ctx::{CliError, CliResult, Ctx};

/// Succeed when `yes` is set. Otherwise, on an interactive terminal ask
/// `pgarbor: <what>. Type 'yes' to continue: ` on stderr and accept only the literal `yes`
/// (anything else: `CliError::refused("aborted")`). When stdin is not a terminal (CI, agents) refuse
/// immediately with exit 7 and the hint `re-run with --yes to confirm`.
pub fn require(ctx: &mut Ctx, yes: bool, what: &str) -> CliResult<()> {
    if yes {
        return Ok(());
    }
    if ctx.json || !std::io::stdin().is_terminal() {
        return Err(CliError::refused(format!("{what}: confirmation required")).with_hint("re-run with --yes to confirm"));
    }
    let _ = write!(ctx.err, "pgarbor: {what}. Type 'yes' to continue: ");
    let _ = ctx.err.flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) if line.trim() == "yes" => Ok(()),
        _ => Err(CliError::refused("aborted")),
    }
}
