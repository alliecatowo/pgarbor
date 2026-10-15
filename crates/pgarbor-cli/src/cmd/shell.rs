//! `pgarbor shell` (lease CLI-08): a small psql-like REPL over the EMBEDDED engine (no network).
//!
//! The terminal loop deliberately uses only standard input/output. See the CLI contract for the
//! prompt and command behavior.
use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Instant;

use pgarbor::{Session, SessionOptions, TxnStatus};
use pgarbor_common::PgError;

use crate::cli::ShellArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{open, paths, render, sql_script};
use crate::cmd::shell_meta::{self, MetaOutcome, ShellSettings};
use crate::sql_script::{ItemKind, ScriptItem, ScriptSplitter, SplitState};

pub fn run(ctx: &mut Ctx, args: &ShellArgs) -> CliResult<Exit> {
    if args.command.is_some() && args.script.is_some() {
        return Err(CliError::usage("cannot use -c and -f together"));
    }
    let interactive = args.command.is_none() && args.script.is_none() && std::io::stdin().is_terminal();
    if interactive && ctx.json {
        return Err(CliError::usage("--json is not supported for the interactive shell"));
    }

    let target = paths::resolve_target(args.file.as_deref(), &args.target)?;
    let spec = open::OpenSpec { read_only: args.read_only, ..open::OpenSpec::existing() };
    let db = open::open_database(&target.db_path, &spec)?;
    let branch = paths::bind_branch(&db, &target)?;
    let mut session = db.session(&branch, SessionOptions {
        user: args.user.clone(),
        database: args.database.clone(),
        application_name: "pgarbor shell".to_string(),
        ..SessionOptions::default()
    }).map_err(CliError::from)?;

    let result = if let Some(sql) = &args.command {
        run_script(ctx, &mut session, sql, crate::cli::FormatArg::Table, false)
    } else if let Some(path) = &args.script {
        match std::fs::read_to_string(path) {
            Ok(text) => run_script(ctx, &mut session, &text, crate::cli::FormatArg::Table, false),
            Err(e) => {
                let err = CliError::io(&format!("could not read {}", path.display()), &e);
                let status = err.exit;
                err.render(ctx);
                status
            }
        }
    } else if interactive {
        interactive_loop(ctx, &mut session, &args.database, &branch)?
    } else {
        let mut text = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut text) {
            let err = CliError::io("could not read stdin", &e);
            let status = err.exit;
            err.render(ctx);
            session.close();
            let _ = db.close();
            return Ok(status);
        }
        run_script(ctx, &mut session, &text, crate::cli::FormatArg::Table, false)
    };
    session.close();
    db.close().map_err(CliError::from)?;
    Ok(result)
}

fn run_script(ctx: &mut Ctx, session: &mut Session, text: &str, fmt: crate::cli::FormatArg, continue_on_error: bool) -> Exit {
    let items = match sql_script::split_script(text) {
        Ok(items) => items,
        Err(e) => { e.render(ctx); return e.exit; }
    };
    let mut failed = false;
    let mut settings = ShellSettings::default();
    for item in items {
        match item.kind {
            ItemKind::Statement => {
                let started = Instant::now();
                let out = session.simple_query_raw(&item.text);
                for statement in &out.statements {
                    if let Err(e) = render::print_result(ctx, session, statement, fmt) {
                        e.render(ctx);
                        failed = true;
                    }
                }
                if settings.timing {
                    ctx.info(&format!("Time: {:.3} ms", started.elapsed().as_secs_f64() * 1000.0));
                }
                if let Some(err) = out.error {
                    let _ = render::print_error(ctx, &item.text, &err);
                    failed = true;
                    if !continue_on_error { return Exit::FAILURE; }
                }
            }
            ItemKind::CopyFromStdin { .. } => {
                let e = PgError::unimplemented("CLI-08", "COPY FROM stdin is not supported in the shell yet");
                let _ = render::print_error(ctx, &item.text, &e);
                return Exit::UNSUPPORTED;
            }
            ItemKind::Meta => {
                let branch = session.branch_name();
                match shell_meta::run_meta(ctx, session, &mut settings, &branch, &item.text) {
                    Ok(MetaOutcome::Continue) => (),
                    Ok(MetaOutcome::Quit) => return if failed { Exit::FAILURE } else { Exit::OK },
                    Err(e) => { e.render(ctx); failed = true; if !continue_on_error { return e.exit; } }
                }
            }
        }
    }
    if failed { Exit::FAILURE } else { Exit::OK }
}

fn interactive_loop(ctx: &mut Ctx, session: &mut Session, database: &str, branch: &str) -> CliResult<Exit> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut splitter = ScriptSplitter::new();
    let mut settings = ShellSettings::default();
    let mut line = String::new();
    loop {
        let superuser = session.setting("is_superuser").map(|s| s != "off").unwrap_or(true);
        let prompt = prompt(splitter.state(), database, branch, session.txn_status(), superuser);
        write!(ctx.err, "{prompt}").map_err(CliError::from)?;
        ctx.err.flush().map_err(CliError::from)?;
        line.clear();
        let count = input.read_line(&mut line).map_err(CliError::from)?;
        if count == 0 {
            match splitter.finish() {
                Ok(items) => if process_items(ctx, session, &mut settings, branch, items)? { return Ok(Exit::OK); },
                Err(e) => { e.render(ctx); return Ok(e.exit); }
            }
            writeln!(ctx.err).map_err(CliError::from)?;
            return Ok(Exit::OK);
        }
        let items = match splitter.push_line(&line) {
            Ok(v) => v,
            Err(e) => { e.render(ctx); return Ok(e.exit); }
        };
        if process_items(ctx, session, &mut settings, branch, items)? { return Ok(Exit::OK); }
    }
}

fn process_items(ctx: &mut Ctx, session: &mut Session, settings: &mut ShellSettings, branch: &str, items: Vec<ScriptItem>) -> CliResult<bool> {
    for item in items {
        match item.kind {
            ItemKind::Statement => {
                let start = Instant::now();
                let out = session.simple_query_raw(&item.text);
                for statement in &out.statements {
                    render::print_result(ctx, session, statement, crate::cli::FormatArg::Table)?;
                }
                if let Some(err) = out.error {
                    render::print_error(ctx, &item.text, &err)?;
                }
                if settings.timing { ctx.info(&format!("Time: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0)); }
            }
            ItemKind::CopyFromStdin { .. } => {
                let err = PgError::unimplemented("CLI-08", "COPY FROM stdin is not supported in the shell yet");
                render::print_error(ctx, &item.text, &err)?;
            }
            ItemKind::Meta => if shell_meta::run_meta(ctx, session, settings, branch, &item.text)? == MetaOutcome::Quit { return Ok(true); },
        }
    }
    Ok(false)
}

fn prompt(state: SplitState, database: &str, branch: &str, txn_status: TxnStatus, superuser: bool) -> String {
    let txn = match txn_status { TxnStatus::Idle => "", TxnStatus::InTransaction => "*", TxnStatus::Failed => "!" };
    let marker = if superuser { "=# " } else { "=> " };
    match state {
        SplitState::Empty => format!("{database}[{branch}]{txn}{marker}"),
        SplitState::InStatement => format!("{database}[{branch}]{txn}-# "),
        SplitState::InSingleQuote | SplitState::InDoubleQuote => format!("{database}[{branch}]{txn}...'# "),
        SplitState::InDollarQuote(_) => format!("{database}[{branch}]{txn}...$# "),
        SplitState::InBlockComment(_) => format!("{database}[{branch}]{txn}...*# "),
        SplitState::InParen(_) => format!("{database}[{branch}]{txn}...(# "),
        SplitState::InCopyData => format!("{database}[{branch}]{txn}COPY> "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_formats_primary_states() {
        assert_eq!(prompt(SplitState::Empty, "postgres", "main", TxnStatus::Idle, true), "postgres[main]=# ");
        assert_eq!(prompt(SplitState::Empty, "postgres", "main", TxnStatus::InTransaction, true), "postgres[main]*=# ");
        assert_eq!(prompt(SplitState::Empty, "postgres", "main", TxnStatus::Failed, false), "postgres[main]!=> ");
        assert_eq!(prompt(SplitState::InStatement, "db", "b", TxnStatus::Idle, true), "db[b]-# ");
        assert_eq!(prompt(SplitState::InSingleQuote, "db", "b", TxnStatus::Idle, true), "db[b]...'# ");
        assert_eq!(prompt(SplitState::InDoubleQuote, "db", "b", TxnStatus::Idle, true), "db[b]...'# ");
        assert_eq!(prompt(SplitState::InDollarQuote("tag".into()), "db", "b", TxnStatus::Idle, true), "db[b]...$# ");
        assert_eq!(prompt(SplitState::InBlockComment(1), "db", "b", TxnStatus::Idle, true), "db[b]...*# ");
        assert_eq!(prompt(SplitState::InParen(1), "db", "b", TxnStatus::Idle, true), "db[b]...(# ");
    }
}
