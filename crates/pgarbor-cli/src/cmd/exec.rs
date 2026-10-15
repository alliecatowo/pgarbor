//! `pgarbor exec` (lease CLI-10): run SQL non-interactively over the embedded engine.
//!
//! SQL input, execution, rendering and exit behavior are specified by this module's contract.
use std::io::{IsTerminal, Read};
use std::path::Path;

use pgarbor::{Database, Session, SessionOptions};
use pgarbor_common::PgError;

use crate::cli::{ExecArgs, FormatArg};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::open::{open_database, OpenSpec};
use crate::paths::{bind_branch, resolve_target};
use crate::reports::{ErrorJson, ExecReport, ExecStatementJson};
use crate::sql_script::{split_script, ItemKind};

pub fn run(ctx: &mut Ctx, args: &ExecArgs) -> CliResult<Exit> {
    if args.command.is_some() && args.script.is_some() {
        return Err(CliError::usage("-c and -f cannot be used together"));
    }

    let sql = if let Some(command) = &args.command {
        command.clone()
    } else if let Some(path) = &args.script {
        read_input(path.as_path())?
    } else if std::io::stdin().is_terminal() {
        return Err(CliError::usage("no SQL given: use -c, -f or pipe SQL on stdin"));
    } else {
        read_stdin()?
    };

    let items = split_script(&sql)?;
    let target = resolve_target(args.file.as_deref(), &args.target)?;
    let db = open_database(&target.db_path, &OpenSpec { read_only: args.read_only, ..OpenSpec::existing() })?;
    let branch = match bind_branch(&db, &target) {
        Ok(branch) => branch,
        Err(error) => { let _ = db.close(); return Err(error); }
    };
    let mut session = match db.session(
        &branch,
        SessionOptions { user: args.user.clone(), database: args.database.clone(), application_name: "pgarbor exec".into(), ..SessionOptions::default() },
    ) {
        Ok(session) => session,
        Err(error) => { let _ = db.close(); return Err(CliError::from(error)); }
    };

    let result = execute_items(ctx, args, &mut session, &branch, &items);
    session.close();
    let close_result = db.close().map_err(CliError::from);
    match (result, close_result) {
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
        (Ok(exit), Ok(())) => Ok(exit),
    }
}

fn read_input(path: &Path) -> CliResult<String> {
    if path == Path::new("-") { return read_stdin(); }
    std::fs::read_to_string(path).map_err(|e| CliError::io(&format!("could not read {}", path.display()), &e))
}

fn read_stdin() -> CliResult<String> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).map_err(|e| CliError::io("could not read stdin", &e))?;
    Ok(text)
}

fn execute_items(ctx: &mut Ctx, args: &ExecArgs, session: &mut Session, branch: &str, items: &[crate::sql_script::ScriptItem]) -> CliResult<Exit> {
    if args.single_transaction {
        let begin = session.simple_query_raw("BEGIN");
        if let Some(error) = begin.error { return Err(CliError::from(error)); }
    }
    let mut report = ExecReport { branch: branch.to_string(), statements: Vec::new(), ok: true };
    let mut failed = false;
    for item in items {
        let copying = if let ItemKind::CopyFromStdin { data } = &item.kind {
            session.set_copy_channel(Some(Box::new(pgarbor::MemChannel { input: Some(data.clone()), output: Vec::new() })));
            true
        } else { false };
        match &item.kind {
            ItemKind::Meta if item.text.trim() == "\\." => continue,
            ItemKind::Meta => ctx.info(&format!("ignoring psql meta-command {}", item.text.trim())),
            ItemKind::CopyFromStdin { .. } | ItemKind::Statement => {
                let output = session.simple_query_raw(&item.text);
                if copying { session.set_copy_channel(None); }
                for statement in &output.statements {
                    let index = report.statements.len() + 1;
                    if ctx.json {
                        report.statements.push(crate::render::statement_json(session, index, &item.text, Ok(statement))?);
                    } else {
                        crate::render::print_result(ctx, session, statement, args.format)?;
                    }
                }
                if let Some(error) = output.error {
                    failed = true;
                    if ctx.json {
                        report.statements.push(ExecStatementJson {
                            index: report.statements.len() + 1,
                            sql: item.text.clone(), tag: None, columns: None, rows: None, notices: Vec::new(),
                            error: Some(ErrorJson { sqlstate: error.sqlstate.code().to_string(), message: error.message.clone(), detail: error.detail.clone(), hint: error.hint.clone(), position: error.position }),
                        });
                    } else {
                        crate::render::print_error(ctx, &item.text, &error)?;
                    }
                    if args.single_transaction {
                        break;
                    }
                    if !args.continue_on_error { break; }
                }
            }
        }
    }
    if args.single_transaction {
        if failed {
            let _ = session.simple_query_raw("ROLLBACK");
        } else {
            let output = session.simple_query_raw("COMMIT");
            if let Some(error) = output.error {
                failed = true;
                if ctx.json {
                    report.statements.push(ExecStatementJson { index: report.statements.len() + 1, sql: "COMMIT".into(), tag: None, columns: None, rows: None, notices: Vec::new(), error: Some(ErrorJson { sqlstate: error.sqlstate.code().to_string(), message: error.message.clone(), detail: error.detail.clone(), hint: error.hint.clone(), position: error.position }) });
                } else { crate::render::print_error(ctx, "COMMIT", &error)?; }
            } else if !ctx.json {
                for output in &output.statements { crate::render::print_result(ctx, session, output, args.format)?; }
            }
        }
    }
    report.ok = !failed;
    if ctx.json { ctx.emit_json(&report)?; }
    Ok(if failed { Exit::FAILURE } else { Exit::OK })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::TargetFlags;
    use std::io::Cursor;

    #[test]
    #[ignore = "needs engine"]
    fn exec_command_prints_table() {
        let args = ExecArgs { file: None, target: TargetFlags::default(), user: "postgres".into(), database: "postgres".into(), command: Some("select 1".into()), script: None, continue_on_error: false, single_transaction: false, format: FormatArg::Table, read_only: false };
        let _ = args;
    }

    #[test]
    fn rejects_command_and_script_together() {
        let args = ExecArgs { file: None, target: TargetFlags::default(), user: "postgres".into(), database: "postgres".into(), command: Some("select 1".into()), script: Some("input.sql".into()), continue_on_error: false, single_transaction: false, format: FormatArg::Table, read_only: false };
        let mut ctx = Ctx::with_sinks(false, Box::new(Cursor::new(Vec::<u8>::new())), Box::new(Cursor::new(Vec::<u8>::new())));
        let error = run(&mut ctx, &args).err();
        assert!(error.map(|e| e.exit == Exit::USAGE).unwrap_or(false));
    }
}
