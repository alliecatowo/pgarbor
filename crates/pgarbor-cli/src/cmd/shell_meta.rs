//! Backslash commands of the shell (lease CLI-09). Text-level reimplementation of the useful part of
//! `psql` (`psql/command.c`, `psql/describe.c`); all catalog access is plain SQL through the session.
use std::{io::Write, time::Instant};

use pgarbor::Session;

use crate::ctx::{CliError, Ctx};

/// What the shell loop should do next.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaOutcome {
    Continue,
    Quit,
}

/// Mutable toggles owned by the shell loop.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ShellSettings {
    pub timing: bool,
    pub expanded: bool,
}

/// Run one backslash line (`line` starts with `\\`, no trailing newline).
pub fn run_meta(
    ctx: &mut Ctx,
    session: &mut Session,
    settings: &mut ShellSettings,
    branch: &str,
    line: &str,
) -> Result<MetaOutcome, CliError> {
    let (command, plus, arg) = parse_command(line);
    let arg = arg.trim();
    match command.as_str() {
        "q" | "quit" => Ok(MetaOutcome::Quit),
        "?" => {
            for row in HELP {
                ctx.out_line(row);
            }
            Ok(MetaOutcome::Continue)
        }
        "conninfo" => {
            ctx.out_line(&format!(
                "You are connected to database \"{}\" as user \"{}\" on branch \"{}\" (file {}).",
                session.database_name(), session.user_name(), branch, session.database().path().display()
            ));
            Ok(MetaOutcome::Continue)
        }
        "timing" => {
            settings.timing = !settings.timing;
            ctx.out_line(if settings.timing { "Timing is on." } else { "Timing is off." });
            Ok(MetaOutcome::Continue)
        }
        "x" => {
            settings.expanded = !settings.expanded;
            ctx.out_line(if settings.expanded { "Expanded display is on." } else { "Expanded display is off." });
            Ok(MetaOutcome::Continue)
        }
        "echo" => {
            ctx.out_line(arg);
            Ok(MetaOutcome::Continue)
        }
        "set" | "unset" => {
            write_diagnostic(ctx, &unknown_command_message(&command));
            Ok(MetaOutcome::Continue)
        }
        "branch" if arg.is_empty() => {
            ctx.out_line(&format!("Branch: {}", session.branch_name()));
            Ok(MetaOutcome::Continue)
        }
        "branch" if arg == "list" => {
            let branches = session.database().list_branches(false).map_err(CliError::from)?;
            ctx.out_line("Name | Parent | Head | Default");
            for item in branches {
                ctx.out_line(&format!("{} | {} | {} | {}", item.name, item.parent.as_deref().unwrap_or(""), item.head_seq, item.is_default));
            }
            Ok(MetaOutcome::Continue)
        }
        "l" => run_query(ctx, session, "SELECT datname AS \"Name\", pg_catalog.pg_get_userbyid(datdba) AS \"Owner\", pg_catalog.pg_encoding_to_char(encoding) AS \"Encoding\" FROM pg_catalog.pg_database ORDER BY 1", settings),
        "dn" => run_query(ctx, session, "SELECT nspname AS \"Name\", pg_catalog.pg_get_userbyid(nspowner) AS \"Owner\" FROM pg_catalog.pg_namespace WHERE nspname !~ '^pg_' AND nspname <> 'information_schema' ORDER BY 1", settings),
        "dt" | "di" | "dv" | "ds" | "d" => {
            if command == "d" && !arg.is_empty() {
                return describe_relation(ctx, session, arg, settings);
            }
            let kinds = match command.as_str() {
                "dt" => "r", "di" => "i", "dv" => "v", "ds" => "S", _ => "rivS",
            };
            let pattern = if arg.is_empty() { String::new() } else { pattern_to_regex(arg) };
            let schema_filter = if arg.contains('.') { "" } else { "AND pg_catalog.pg_table_is_visible(c.oid)" };
            let pattern_target = if arg.contains('.') { "(n.nspname || '.' || c.relname)" } else { "c.relname" };
            let relation_filter = if pattern.is_empty() { String::new() } else { format!("AND {pattern_target} ~ {}", sql_literal(&pattern)) };
            let sql = format!("SELECT n.nspname AS \"Schema\", c.relname AS \"Name\", CASE c.relkind WHEN 'r' THEN 'table' WHEN 'i' THEN 'index' WHEN 'v' THEN 'view' WHEN 'S' THEN 'sequence' ELSE c.relkind::text END AS \"Type\", pg_catalog.pg_get_userbyid(c.relowner) AS \"Owner\" FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ({}) {} {} ORDER BY 1, 2", kinds.chars().map(|ch| sql_literal(&ch.to_string())).collect::<Vec<_>>().join(","), schema_filter, relation_filter);
            let _ = plus;
            run_query(ctx, session, &sql, settings)
        }
        "i" => {
            // Script inclusion is handled by the shell loop; this command reports the unsupported
            // operation explicitly because run_meta has no input-reader handle.
            Err(CliError::from(pgarbor_common::PgError::unimplemented("CLI-09", "\\i requires shell input access")))
        }
        _ => {
            write_diagnostic(ctx, &unknown_command_message(&command));
            write_diagnostic(ctx, "Try \\? for help.");
            Ok(MetaOutcome::Continue)
        }
    }
}

const HELP: &[&str] = &[
    "\\?                   show this help",
    "\\conninfo            display connection information",
    "\\d [NAME]            describe relations or one relation",
    "\\dn                  list schemas",
    "\\di [PATTERN]        list indexes",
    "\\ds [PATTERN]        list sequences",
    "\\dv [PATTERN]        list views",
    "\\l                   list databases",
    "\\branch [list]       show current branch or list branches",
    "\\timing              toggle timing of statements",
    "\\x                   toggle expanded output",
    "\\echo TEXT           write text to standard output",
    "\\q                   quit",
];

fn parse_command(line: &str) -> (String, bool, String) {
    let body = line.strip_prefix('\\').unwrap_or(line).trim_start();
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    let head = &body[..end];
    let plus = head.ends_with('+');
    let name = if plus { &head[..head.len() - 1] } else { head };
    (name.to_owned(), plus, body[end..].trim_start().to_owned())
}

fn pattern_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c if "\\.+()|{}[]^$".contains(c) => { out.push('\\'); out.push(c); }
            c => out.push(c),
        }
    }
    out.push('$');
    out
}

fn sql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn unknown_command_message(command: &str) -> String {
    format!("invalid command \\{command}")
}

fn write_diagnostic(ctx: &mut Ctx, message: &str) {
    let _ = writeln!(ctx.err, "{message}");
    let _ = ctx.err.flush();
}

fn run_query(ctx: &mut Ctx, session: &mut Session, sql: &str, settings: &ShellSettings) -> Result<MetaOutcome, CliError> {
    let started = Instant::now();
    let output = session.simple_query_raw(sql);
    for statement in &output.statements {
        if settings.expanded {
            if let pgarbor::StatementResult::Rows { columns, rows, .. } = &statement.result {
                let table = crate::render::rows_to_table(session, columns, rows)?;
                ctx.out_line(&crate::table::render_expanded(&table)?);
            }
        } else {
            crate::render::print_result(ctx, session, statement, crate::cli::FormatArg::Table)?;
        }
    }
    if let Some(error) = output.error {
        crate::render::print_error(ctx, sql, &error)?;
    }
    if settings.timing {
        ctx.out_line(&format!("Time: {:.3} ms", started.elapsed().as_secs_f64() * 1000.0));
    }
    Ok(MetaOutcome::Continue)
}

fn describe_relation(ctx: &mut Ctx, session: &mut Session, name: &str, settings: &ShellSettings) -> Result<MetaOutcome, CliError> {
    let (schema, rel) = name.split_once('.').unwrap_or(("public", name));
    let sql = format!("SELECT a.attname AS \"Column\", pg_catalog.format_type(a.atttypid, a.atttypmod) AS \"Type\", CASE WHEN a.attnotnull THEN 'not null' ELSE '' END AS \"Modifiers\" FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relname = {} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum", sql_literal(schema), sql_literal(rel));
    run_query(ctx, session, &sql, settings)
}

#[cfg(test)]
mod tests {
    use super::{parse_command, pattern_to_regex, unknown_command_message};

    #[test]
    fn pattern_conversion_escapes_regex_syntax() {
        assert_eq!(pattern_to_regex("foo*"), "^foo.*$");
        assert_eq!(pattern_to_regex("a?b"), "^a.b$");
        assert_eq!(pattern_to_regex("a+b"), "^a\\+b$");
        assert_eq!(pattern_to_regex("public.foo"), "^public\\.foo$");
    }

    #[test]
    fn parses_plus_and_argument() {
        assert_eq!(parse_command("\\dt+ foo"), ("dt".to_string(), true, "foo".to_string()));
    }

    #[test]
    fn unknown_command_diagnostic() {
        assert_eq!(unknown_command_message("foo"), "invalid command \\foo");
    }
}
