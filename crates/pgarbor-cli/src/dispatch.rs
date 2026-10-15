//! Command routing. Fully implemented, no lease.
use crate::cli::{Cli, Command};
use crate::cmd;
use crate::ctx::{CliResult, Ctx};
use crate::exit::Exit;
use crate::paths;

/// Run the parsed command line.
pub fn dispatch(ctx: &mut Ctx, cli: &Cli) -> CliResult<Exit> {
    match &cli.command {
        Command::Init(a) => cmd::init::run(ctx, a),
        Command::Serve(a) => cmd::serve::run(ctx, a),
        Command::Shell(a) => cmd::shell::run(ctx, a),
        Command::Exec(a) => cmd::exec::run(ctx, a),
        Command::Status(a) => cmd::status::run(ctx, a),
        Command::Inspect(a) => cmd::inspect::run(ctx, a),
        Command::Verify(a) => cmd::verify::run(ctx, a),
        Command::Compact(a) => cmd::compact::run(ctx, a),
        Command::Doctor(a) => cmd::doctor::run(ctx, a),
        Command::Version => cmd::version::run(ctx),
        Command::Dump(a) => cmd::dump::run(ctx, a),
        Command::Import(a) => cmd::import::run(ctx, a),
        Command::Branch(b) => cmd::branch::run(ctx, b),
    }
}

/// Shared by commands taking `[FILE] --db`.
pub fn db_path_of(file: &Option<std::path::PathBuf>, db: &crate::cli::DbFlag) -> CliResult<std::path::PathBuf> {
    paths::resolve_db(file.as_deref(), db.db.as_deref())
}
