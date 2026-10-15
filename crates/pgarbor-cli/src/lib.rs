//! The `pgarbor` command-line tool as a library (so integration tests can call it in-process).
//! `main.rs` is a thin wrapper around [`run_from_env`].
//!
//! CONTRACT: the grammar (`cli`), exit codes (`exit`), JSON schemas (`reports`) and the
//! `// CONTRACT` items elsewhere are frozen at `contracts-v1`; see `contracts/crates/pgarbor-cli.md`
//! and `contracts/leases/pgarbor-cli.json` (CLI-01..).
//!
//! ## Layout
//! | area | modules |
//! |---|---|
//! | frozen interface | [`cli`], [`exit`], [`ctx`], [`reports`], [`signals`], [`confirm`], [`dispatch`] |
//! | helpers | [`paths`], [`open`], [`table`], [`render`], [`humanize`], [`sql_script`], [`transcript_file`] |
//! | serving | [`serve_registry`], [`control`], [`control_client`] |
//! | commands | [`cmd`] (init, serve, shell, exec, status, inspect, verify, compact, doctor, version, dump, import, branch/*) |
//!
//! ## Entry points
//! * [`run_from_env`]: parse `std::env::args_os`, run, return the process `ExitCode`.
//! * [`run_with`]: same with explicit args and sinks (tests).
//! Clap usage errors exit 64 (not clap's default 2: exit 2 is reserved for "address in use" by
//! harness-02); `--help`/`--version` exit 0 and print to stdout.
#![allow(dead_code)]

pub mod cli;
pub mod cmd;
pub mod confirm;
pub mod control;
pub mod control_client;
pub mod ctx;
pub mod dispatch;
pub mod exit;
pub mod humanize;
pub mod open;
pub mod paths;
pub mod render;
pub mod reports;
pub mod serve_registry;
pub mod signals;
pub mod sql_script;
pub mod table;
pub mod transcript_file;

use std::ffi::OsString;

use clap::Parser;

use crate::cli::Cli;
use crate::ctx::Ctx;
use crate::exit::Exit;

/// Parse and run with explicit arguments (`args[0]` is the program name) and output sinks.
/// Returns the exit status instead of terminating the process.
pub fn run_with<I, T>(args: I, ctx_factory: impl FnOnce(&Cli) -> Ctx) -> Exit
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => Exit::USAGE.code(),
            };
            let _ = e.print();
            return Exit(code);
        }
    };
    let mut ctx = ctx_factory(&cli);
    match dispatch::dispatch(&mut ctx, &cli) {
        Ok(exit) => exit,
        Err(err) => {
            err.render(&mut ctx);
            err.exit
        }
    }
}

/// Real process entry (see `main.rs`).
pub fn run_from_env() -> std::process::ExitCode {
    let exit = run_with(std::env::args_os(), |cli| Ctx::stdio(cli.json, cli.quiet, cli.verbose));
    exit.into()
}
