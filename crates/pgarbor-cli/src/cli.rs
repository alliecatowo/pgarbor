//! The command line grammar (clap derive). FROZEN: argv is a contract (RFC 001 sections 3 and 10,
//! `contracts/amendments/harness-02-cli-serve-contract.md`). Fully implemented, no lease.
//!
//! ```text
//! pgarbor [--json] [-q] [-v] <command>
//!
//! init|create <FILE> [--branch main] [--page-size N] [--if-not-exists]
//! serve [FILE] [--db FILE] [--branch B] [--handle H] [--host 127.0.0.1] [--port 5432|0] [--socket-dir DIR]
//!       [--auth auto|trust|password|md5|scram-sha-256] [--max-connections N] [--allow-nonloopback]
//!       [--transcript FILE] [--read-only]
//! shell [FILE] [target flags] [--user U] [--database D] [-c SQL] [-f FILE]
//! exec  [FILE] [target flags] (-c SQL | -f FILE | stdin) [--continue-on-error] [--single-transaction]
//! status|inspect|verify|compact|doctor [FILE]
//! version
//! dump [FILE] [target flags] [-o OUT] [--schema-only | --data-only] [--copy]
//! import <INPUT|-> [target flags] [--create] [--continue-on-error] [--single-transaction]
//! branch create <NAME> [--from B] [--snapshot LABEL] [--copy-snapshots]
//! branch list [--all] [--space] | link <BRANCH> <HANDLE> | resolve <HANDLE> | rename <OLD> <NEW>
//! branch delete <NAME> [--purge] [--allow-default] [--yes] | reset <NAME> (--to-snapshot L | --to-branch B | --to-parent) [--yes]
//! branch compare <A> <B> | detach <NAME> --out F [--strategy auto|reflink|copy] | export <NAME> --out F
//! branch exec [NAME] [target flags] [--force-env] -- <COMMAND>...   |  branch serve <NAME> [serve flags]
//! branch snapshot create|list|drop | restore <NAME_OR_ID> | purge | set-default <NAME>
//! ```
//! Database resolution (see `paths.rs`): positional FILE, else `--db`, else `$PGARBOR_DB`, else the
//! master database named by `--handle` / the nearest `.pgabranch`. `clap`'s `env` feature is NOT
//! enabled, so environment fallbacks are done by hand in `paths.rs`.
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

/// Top-level parser.
// CONTRACT
#[derive(Parser, Debug)]
#[command(name = "pgarbor", version, about = "PGArbor: a single-file, branch-native, PostgreSQL-compatible database", propagate_version = true)]
pub struct Cli {
    /// Machine-readable output: one JSON document on stdout for success, one JSON object on stderr
    /// for errors.
    #[arg(long, global = true)]
    pub json: bool,
    /// Suppress informational output.
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// More diagnostics on stderr (repeat for more).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

/// Where the database / branch comes from (flags shared by many commands).
// CONTRACT
#[derive(Args, Debug, Clone, Default)]
pub struct TargetFlags {
    /// Master database file (same as the positional FILE where one exists).
    #[arg(long = "db", value_name = "FILE")]
    pub db: Option<PathBuf>,
    /// Branch to bind to (default: the database's default branch).
    #[arg(short = 'b', long, value_name = "NAME")]
    pub branch: Option<String>,
    /// A `.pgabranch` handle naming the master database and branch.
    #[arg(long, value_name = "HANDLE")]
    pub handle: Option<PathBuf>,
}

/// Just `--db` (branch administration commands name the branch positionally).
// CONTRACT
#[derive(Args, Debug, Clone, Default)]
pub struct DbFlag {
    #[arg(long = "db", value_name = "FILE")]
    pub db: Option<PathBuf>,
}

/// Listener options shared by `serve` and `branch serve`.
// CONTRACT
#[derive(Args, Debug, Clone)]
pub struct ServeOpts {
    /// Interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// TCP port; 0 picks a free port and prints the real one.
    #[arg(long, default_value_t = 5432)]
    pub port: u16,
    /// Also listen on a Unix socket `<DIR>/.s.PGSQL.<port>` (unix only).
    #[arg(long, value_name = "DIR")]
    pub socket_dir: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = AuthArg::Auto)]
    pub auth: AuthArg,
    #[arg(long, default_value_t = 100)]
    pub max_connections: usize,
    /// Allow non-loopback `--host` (requires --auth password|md5|scram-sha-256; there is no TLS yet).
    #[arg(long)]
    pub allow_nonloopback: bool,
    /// Append every wire message of every connection to this JSONL file (diagnostics).
    #[arg(long, value_name = "FILE")]
    pub transcript: Option<PathBuf>,
    /// Open the database read-only (shared lock; every write fails with 25006).
    #[arg(long)]
    pub read_only: bool,
    /// Do not write the `<db>.serve.json` discovery file and control socket.
    #[arg(long)]
    pub no_registry: bool,
    /// Default `statement_timeout` for every session (PG duration syntax, e.g. `30s`).
    #[arg(long, value_name = "DURATION")]
    pub statement_timeout: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthArg {
    Auto,
    Trust,
    Password,
    Md5,
    #[value(name = "scram-sha-256")]
    ScramSha256,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelArg {
    None,
    Roots,
    Full,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyArg {
    Auto,
    Reflink,
    Copy,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatArg {
    Table,
    Csv,
    Json,
}

/// Every command.
// CONTRACT
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new database file (alias: create).
    #[command(visible_alias = "create")]
    Init(InitArgs),
    /// Serve a branch over the PostgreSQL wire protocol.
    Serve(ServeArgs),
    /// Interactive SQL shell (psql-like) over the embedded engine.
    Shell(ShellArgs),
    /// Run SQL from -c, -f or stdin and print results.
    Exec(ExecArgs),
    /// One-screen summary of a database file.
    Status(FileArgs),
    /// Detailed structure and space report.
    Inspect(InspectArgs),
    /// Verify checksums and structure.
    Verify(VerifyArgs),
    /// Rewrite the file keeping only reachable pages.
    Compact(CompactArgs),
    /// Diagnose environment and file problems with explanations.
    Doctor(FileArgs),
    /// Print version and build information.
    Version,
    /// Write a plain-SQL dump of one branch.
    Dump(DumpArgs),
    /// Run a SQL dump / script into a branch.
    Import(ImportArgs),
    /// Branch management.
    #[command(subcommand)]
    Branch(BranchCommand),
}

// CONTRACT
#[derive(Args, Debug)]
pub struct InitArgs {
    pub file: PathBuf,
    /// Name of the first branch.
    #[arg(long, default_value = "main")]
    pub branch: String,
    /// Page size in bytes (power of two, 4096..=65536).
    #[arg(long, value_name = "BYTES")]
    pub page_size: Option<u32>,
    /// Succeed (exit 0) if the file already is a valid PGArbor database.
    #[arg(long)]
    pub if_not_exists: bool,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct ServeArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub target: TargetFlags,
    #[command(flatten)]
    pub opts: ServeOpts,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct ShellArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub target: TargetFlags,
    #[arg(long, default_value = "postgres")]
    pub user: String,
    #[arg(long, default_value = "postgres")]
    pub database: String,
    /// Run this SQL and exit (like psql -c).
    #[arg(short = 'c', long, value_name = "SQL")]
    pub command: Option<String>,
    /// Run this script file and exit (like psql -f).
    #[arg(short = 'f', long, value_name = "FILE")]
    pub script: Option<PathBuf>,
    #[arg(long)]
    pub read_only: bool,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct ExecArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub target: TargetFlags,
    #[arg(long, default_value = "postgres")]
    pub user: String,
    #[arg(long, default_value = "postgres")]
    pub database: String,
    #[arg(short = 'c', long, value_name = "SQL")]
    pub command: Option<String>,
    #[arg(short = 'f', long, value_name = "FILE")]
    pub script: Option<PathBuf>,
    /// Keep going after an error (exit status is still 1 at the end).
    #[arg(long)]
    pub continue_on_error: bool,
    /// Wrap the whole script in BEGIN/COMMIT.
    #[arg(long)]
    pub single_transaction: bool,
    #[arg(long, value_enum, default_value_t = FormatArg::Table)]
    pub format: FormatArg,
    #[arg(long)]
    pub read_only: bool,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct FileArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct InspectArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub db: DbFlag,
    /// Logical vs physical space, reclaimable bytes, per-branch retained bytes.
    #[arg(long)]
    pub space: bool,
    /// Decode one page (id) and print its header, kind and keys.
    #[arg(long, value_name = "ID")]
    pub page: Option<u64>,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct VerifyArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub db: DbFlag,
    #[arg(long, value_enum, default_value_t = LevelArg::Full)]
    pub level: LevelArg,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct CompactArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct DumpArgs {
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub target: TargetFlags,
    /// Output file (default stdout).
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    #[arg(long, conflicts_with = "data_only")]
    pub schema_only: bool,
    #[arg(long)]
    pub data_only: bool,
    /// Emit data as `COPY ... FROM stdin` blocks instead of INSERT statements.
    #[arg(long)]
    pub copy: bool,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct ImportArgs {
    /// SQL file to run, or `-` for stdin.
    pub input: PathBuf,
    #[command(flatten)]
    pub target: TargetFlags,
    /// Create the database file if it does not exist.
    #[arg(long)]
    pub create: bool,
    #[arg(long)]
    pub continue_on_error: bool,
    /// Run everything in one transaction (all or nothing).
    #[arg(long)]
    pub single_transaction: bool,
}

/// `pgarbor branch ...`
// CONTRACT
#[derive(Subcommand, Debug)]
pub enum BranchCommand {
    Create(BranchCreateArgs),
    List(BranchListArgs),
    /// Write a `.pgabranch` handle file.
    Link(BranchLinkArgs),
    /// Validate a handle against its master database.
    Resolve(BranchResolveArgs),
    /// Run a command with DATABASE_URL pointing at a branch listener.
    Exec(BranchExecArgs),
    /// Serve a named branch (same as `serve --branch NAME`).
    Serve(BranchServeArgs),
    Rename(BranchRenameArgs),
    Delete(BranchDeleteArgs),
    Reset(BranchResetArgs),
    Compare(BranchCompareArgs),
    /// Produce an independent standalone `.pga` for a branch (reflink when possible).
    Detach(BranchDetachArgs),
    /// Streaming compact export of a branch into a new standalone `.pga`.
    Export(BranchExportArgs),
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Restore a deleted (retained) branch.
    Restore(BranchRestoreArgs),
    /// Permanently drop retained deleted branches.
    Purge(BranchPurgeArgs),
    /// Change the default branch.
    SetDefault(BranchSetDefaultArgs),
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchCreateArgs {
    pub name: String,
    /// Source branch (default: the default branch).
    #[arg(long)]
    pub from: Option<String>,
    /// Create from this named snapshot of the source branch.
    #[arg(long, value_name = "LABEL")]
    pub snapshot: Option<String>,
    #[arg(long)]
    pub copy_snapshots: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchListArgs {
    /// Include deleted (retained) branches.
    #[arg(long)]
    pub all: bool,
    /// Add retained / exclusive bytes per branch.
    #[arg(long)]
    pub space: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchLinkArgs {
    pub branch: String,
    /// Handle file to write (conventionally `*.pgabranch`).
    pub handle: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
    /// Store an absolute path to the master database in the handle.
    #[arg(long)]
    pub absolute: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchResolveArgs {
    pub handle: PathBuf,
    /// Look for the master database here (overrides the handle's hint).
    #[command(flatten)]
    pub db: DbFlag,
    /// After finding the master at a new location, rewrite the handle's path hint.
    #[arg(long)]
    pub rewrite_hint: bool,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchExecArgs {
    /// Branch name (omit when --handle is given).
    pub branch: Option<String>,
    #[command(flatten)]
    pub target: BranchTarget,
    #[arg(long, default_value = "postgres")]
    pub user: String,
    #[arg(long, default_value = "postgres")]
    pub database: String,
    /// Name of the environment variable to set.
    #[arg(long, default_value = "DATABASE_URL")]
    pub env_name: String,
    /// Overwrite an existing DATABASE_URL (refused by default).
    #[arg(long)]
    pub force_env: bool,
    /// Never reuse a running listener of the same database; always start an owned one.
    #[arg(long)]
    pub no_reuse: bool,
    /// The command to run, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

/// `--db` + `--handle` for commands whose branch is positional.
// CONTRACT
#[derive(Args, Debug, Clone, Default)]
pub struct BranchTarget {
    #[arg(long = "db", value_name = "FILE")]
    pub db: Option<PathBuf>,
    #[arg(long, value_name = "HANDLE")]
    pub handle: Option<PathBuf>,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchServeArgs {
    pub branch: Option<String>,
    #[command(flatten)]
    pub target: BranchTarget,
    #[command(flatten)]
    pub opts: ServeOpts,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchRenameArgs {
    pub old: String,
    pub new: String,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchDeleteArgs {
    pub name: String,
    /// Do not retain the data for `restore` (space is reclaimed by `compact`).
    #[arg(long)]
    pub purge: bool,
    #[arg(long)]
    pub allow_default: bool,
    /// Also end sessions currently using the branch.
    #[arg(long)]
    pub force: bool,
    /// Do not ask for confirmation (required when stdin is not a terminal).
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchResetArgs {
    pub name: String,
    #[arg(long, value_name = "LABEL")]
    pub to_snapshot: Option<String>,
    #[arg(long, value_name = "BRANCH")]
    pub to_branch: Option<String>,
    #[arg(long)]
    pub to_parent: bool,
    /// Save the current head under this snapshot label first.
    #[arg(long, value_name = "LABEL")]
    pub recovery_snapshot: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchCompareArgs {
    pub a: String,
    pub b: String,
    #[arg(long, default_value_t = 10_000_000)]
    pub max_differences: u64,
    #[arg(long, default_value_t = 5)]
    pub samples: usize,
    /// Skip the page-level shared/exclusive trace (faster on large files).
    #[arg(long)]
    pub no_pages: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchDetachArgs {
    pub name: String,
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
    #[arg(long, value_enum, default_value_t = StrategyArg::Auto)]
    pub strategy: StrategyArg,
    /// Branch name inside the new file.
    #[arg(long, default_value = "main")]
    pub target_name: String,
    #[arg(long)]
    pub include_snapshots: bool,
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchExportArgs {
    pub name: String,
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
    #[arg(long, default_value = "main")]
    pub target_name: String,
    #[arg(long)]
    pub include_snapshots: bool,
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

/// `pgarbor branch snapshot ...`
// CONTRACT
#[derive(Subcommand, Debug)]
pub enum SnapshotCommand {
    Create { branch: String, label: String, #[command(flatten)] db: DbFlag },
    List { branch: String, #[command(flatten)] db: DbFlag },
    Drop { branch: String, label: String, #[command(flatten)] db: DbFlag },
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchRestoreArgs {
    /// Branch name or id of a deleted branch.
    pub name_or_id: String,
    #[arg(long)]
    pub as_name: Option<String>,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchPurgeArgs {
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[command(flatten)]
    pub db: DbFlag,
}

// CONTRACT
#[derive(Args, Debug)]
pub struct BranchSetDefaultArgs {
    pub name: String,
    #[command(flatten)]
    pub db: DbFlag,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("pgarbor").chain(args.iter().copied()))
    }

    #[test]
    fn command_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn harness_contract_argv_parses() {
        let c = parse(&["create", "x.pga"]).unwrap();
        assert!(matches!(c.command, Command::Init(_)));
        let c = parse(&["serve", "x.pga", "--host", "127.0.0.1", "--port", "0", "--branch", "main"]).unwrap();
        match c.command {
            Command::Serve(s) => {
                assert_eq!(s.opts.port, 0);
                assert_eq!(s.target.branch.as_deref(), Some("main"));
            }
            _ => panic!("not serve"),
        }
    }

    #[test]
    fn branch_exec_takes_trailing_command() {
        let c = parse(&["branch", "exec", "feature/auth", "--db", "d.pga", "--", "npm", "test"]).unwrap();
        match c.command {
            Command::Branch(BranchCommand::Exec(e)) => {
                assert_eq!(e.branch.as_deref(), Some("feature/auth"));
                assert_eq!(e.command.len(), 2);
            }
            _ => panic!("not branch exec"),
        }
    }
}
