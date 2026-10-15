//! `pgarbor import` (lease CLI-28): run a SQL script / dump into a branch.
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use pgarbor::{SessionOptions, StatementResult};
use pgarbor_common::PgError;

use crate::cli::ImportArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::open::{self, OpenSpec};
use crate::paths;
use crate::reports::{ImportReport, ScriptErrorJson};
use crate::sql_script::{ItemKind, ScriptItem, ScriptSplitter};

pub fn run(ctx: &mut Ctx, args: &ImportArgs) -> CliResult<Exit> {
    let target = paths::resolve_target(None, &args.target)?;
    let mut spec = OpenSpec::existing();
    spec.create = args.create;
    spec.initial_branch = target.branch.clone().unwrap_or_else(|| "main".to_string());
    let db = open::open_database(&target.db_path, &spec)?;
    let branch = paths::bind_branch(&db, &target)?;
    let mut session = db.session(&branch, SessionOptions {
        application_name: "pgarbor import".to_string(),
        ..SessionOptions::default()
    }).map_err(CliError::from)?;

    let input: Box<dyn BufRead> = if args.input == Path::new("-") {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        let file = File::open(&args.input).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::not_found(format!("input file \"{}\" does not exist", args.input.display()))
            } else { CliError::io("cannot read import input", &e) }
        })?;
        Box::new(BufReader::new(file))
    };

    let mut splitter = ScriptSplitter::new();
    let mut statements_run = 0u64;
    let mut copy_rows = 0u64;
    let mut errors = Vec::new();
    let mut failed = false;
    let mut committed = false;
    if args.single_transaction {
        let out = session.simple_query_raw("BEGIN");
        if let Some(e) = out.error { return Err(CliError::from(e)); }
    }

    let mut process = |item: ScriptItem, session: &mut pgarbor::Session, ctx: &mut Ctx,
                       statements_run: &mut u64, copy_rows: &mut u64, errors: &mut Vec<ScriptErrorJson>,
                       failed: &mut bool| -> CliResult<bool> {
        let copying = if let ItemKind::CopyFromStdin { data } = &item.kind {
            session.set_copy_channel(Some(Box::new(pgarbor::MemChannel { input: Some(data.clone()), output: Vec::new() })));
            true
        } else { false };
        match item.kind {
            ItemKind::Meta => {
                if !matches!(item.text.trim(), "\\." | "\\connect" | "\\c" | "\\restrict" | "\\unrestrict") {
                    ctx.info(&format!("ignoring psql meta-command {}", item.text));
                }
                Ok(true)
            }
            ItemKind::CopyFromStdin { .. } | ItemKind::Statement => {
                let out = session.simple_query_raw(&item.text);
                if copying { session.set_copy_channel(None); }
                if let Some(e) = out.error {
                    errors.push(ScriptErrorJson { statement: item.index, line: item.line,
                        sqlstate: e.sqlstate.code().to_string(), message: e.message.clone() });
                    if !ctx.json { crate::render::print_error(ctx, &item.text, &e)?; }
                    *failed = true;
                    Ok(args.continue_on_error)
                } else {
                    *statements_run += 1;
                    for result in out.statements {
                        if let StatementResult::Command { tag } = result.result {
                            if let Some(n) = tag.strip_prefix("COPY ").and_then(|s| s.parse::<u64>().ok()) { *copy_rows += n; }
                        }
                    }
                    Ok(true)
                }
            }
        }
    };

    let mut line = String::new();
    let mut stop = false;
    let mut reader = input;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| CliError::io("cannot read import input", &e))?;
        if n == 0 { break; }
        let items = splitter.push_line(&line)?;
        for item in items {
            if !process(item, &mut session, ctx, &mut statements_run, &mut copy_rows, &mut errors, &mut failed)? { stop = true; break; }
        }
        if stop { break; }
    }
    if !stop {
        for item in splitter.finish()? {
            if !process(item, &mut session, ctx, &mut statements_run, &mut copy_rows, &mut errors, &mut failed)? { stop = true; break; }
        }
    }
    if args.single_transaction {
        if failed || stop {
            let _ = session.simple_query_raw("ROLLBACK");
        } else {
            let out = session.simple_query_raw("COMMIT");
            committed = out.error.is_none();
            if let Some(e) = out.error {
                errors.push(ScriptErrorJson { statement: statements_run + 1, line: 1,
                    sqlstate: e.sqlstate.code().to_string(), message: e.message.clone() });
                failed = true;
                let _ = session.simple_query_raw("ROLLBACK");
            }
        }
    } else { committed = !failed; }
    if ctx.json {
        ctx.emit_json(&ImportReport { branch, statements_run, copy_rows, errors, committed })?;
    } else if !ctx.quiet {
        ctx.out_line(&format!("imported {statements_run} statements, {copy_rows} rows copied into branch \"{branch}\""));
    }
    Ok(if failed { Exit::FAILURE } else { Exit::OK })
}
