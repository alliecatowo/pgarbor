//! COPY (SQL-42). PG references: `copy.c` (`DoCopy`, `ProcessCopyOptions`, `CopyGetAttnums`),
//! `copyfrom.c`, `copyto.c`, `gram.y` (`CopyStmt`, `copy_opt_list`, `copy_generic_opt_elem`).
//!
//! `bind_copy(b, stmt)` (`Statement::Copy { source: CopySource, to, target: CopyTarget, options:
//! Vec<CopyOption>, legacy_options: Vec<CopyLegacyOption>, values }`) -> `BoundStatement::Copy(Box<BoundCopy>)`.
//! * `CopySource::Table { table_name, columns }` -> `CopyTarget::Table { table: TargetTable (via
//!   `target_table::open_target_table(.., DmlKind::Copy, true)` for COPY FROM; a relation lookup with
//!   defaults not needed for COPY TO), columns: attnums }` (column list order; none = all live non-generated
//!   columns for FROM, all live columns for TO; unknown column 42703 `column "x" of relation "t" does not
//!   exist`; duplicate 42701 `column "x" specified more than once`; generated column in FROM list: 428C9?
//!   PG allows listing? no -> `column "g" is a generated column` DETAIL `Generated columns cannot be used in
//!   COPY.`). `CopySource::Query(q)` (COPY TO only: `(SELECT ...) TO ...`; FROM + query: 0A000/42601) ->
//!   `CopyTarget::Query(Box<BoundQuery>)` bound with `select::bind_query`; DML with RETURNING as source
//!   0A000.
//! * Endpoint: `CopyTarget::{Stdin, Stdout, File{name}, Program{command}}` -> `CopyEndpoint`; direction
//!   from `to`; `COPY FROM STDOUT`/`TO STDIN` are grammar errors (42601). Server-side `FILE`/`PROGRAM`
//!   privilege checks are the executor's (42501 `permission denied to COPY to or from a file` ...).
//! * Options (new `WITH (...)` list `CopyOption` and legacy `WITH CSV HEADER ...` `CopyLegacyOption`):
//!   `FORMAT text|csv|binary`, `FREEZE`, `DELIMITER 'c'` (single one-byte char: 22023 `COPY delimiter must
//!   be a single one-byte character`; not in binary), `NULL 'str'`, `DEFAULT 'str'` (FROM only),
//!   `HEADER [bool|MATCH]` (CSV/text; `MATCH` -> 0A000), `QUOTE`, `ESCAPE` (CSV only), `FORCE_QUOTE (cols)|*`
//!   (TO CSV), `FORCE_NOT_NULL (cols)`, `FORCE_NULL (cols)` (FROM CSV), `ON_ERROR`, `LOG_VERBOSITY`,
//!   `REJECT_LIMIT` (0A000 unless `stop`/default), `ENCODING 'x'` (only UTF8 accepted). Defaults resolved:
//!   text delimiter TAB, null `\N`, CSV delimiter `,`, null empty string, quote `"`, escape = quote. Option
//!   conflicts with PG messages (22023 `COPY QUOTE requires CSV mode` - same pattern for ESCAPE,
//!   FORCE_QUOTE, FORCE_NOT_NULL, FORCE_NULL; `COPY delimiter character must not appear in the NULL
//!   specification`; HEADER is accepted in text mode in PG 18 (oracle: `copy t to stdout (header)` prints a
//!   header row); `COPY FREEZE` cannot be used with COPY TO). Redshift/Snowflake legacy options (`IAM_ROLE`, `GZIP`, ...): 42601.
//! * `WHERE cond` (sqlparser: `where_clause`? if present) -> boolean over the target's columns in a
//!   `TableExpr` scope; COPY TO with WHERE: 0A000 `WHERE clause not allowed with COPY TO`.
//! * COPY of a view/matview/sequence/foreign table as FROM target: 42809 `cannot copy to view "v"` HINT
//!   `To enable copying to a view, provide an INSTEAD OF INSERT trigger.`; TO a view is allowed in PG 18
//!   only via query: `cannot copy from view "v"` HINT `Try the COPY (SELECT ...) TO variant.`
//!
//! ## Implementation notes (where the reference source decides the message or SQLSTATE)
//! * The lease text and the module docs above give some SQLSTATEs that differ from PG 18.6 `copy.c`.
//!   The implementation follows the PG 18.6 source: `QUOTE`/`ESCAPE`/`FORCE_*` "requires CSV mode",
//!   `COPY delimiter must be a single one-byte character` and `... quote must be ...` are 0A000
//!   (FEATURE_NOT_SUPPORTED); `COPY FORCE_QUOTE cannot be used with COPY FROM` is 0A000;
//!   `FORCE_NOT_NULL`/`FORCE_NULL`/`FREEZE cannot be used with COPY TO` are 22023; a generated column in
//!   a column list is 42P10 (INVALID_COLUMN_REFERENCE).
//! * `STDIN` and `STDOUT` are interchangeable in PG's grammar (`copy_file_name`); the direction selects the
//!   pipe (`DoCopy`), so `COPY t TO STDIN` writes to stdout and `COPY t FROM STDOUT` reads from stdin.
//!   The binder follows that instead of raising 42601.
//! * sqlparser 0.63 has no `WHERE` on `Statement::Copy`, no `DEFAULT`, `ON_ERROR`, `LOG_VERBOSITY`,
//!   `REJECT_LIMIT`, `FORCE_QUOTE *`, `HEADER MATCH`, so `BoundCopy::where_` and `CopyOptions::default_string`
//!   are always `None`, and `force_quote_all` is always `false`. Those constructs fail in the parser.
//! * sqlparser's `CopyLegacyOption::Escape` carries no character, so the legacy `ESCAPE` keyword is 0A000.
//! * Option errors carry no position (sqlparser's `CopyOption` has no spans). Column-list errors carry
//!   none either, as in PG's `CopyGetAttnums`.
//! * Quote and escape fields hold `"` and `"` when not given, in both modes (per the CopyOptions contract);
//!   only CSV mode uses them.
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::{CatalogEnv, RelKind, Relation};
use pgarbor_common::{PgError, Result, SqlState};
use sqlparser::ast::{
    CopyLegacyCsvOption, CopyLegacyOption, CopyOption, CopySource, CopyTarget as AstCopyTarget, Ident, ObjectName,
    Statement,
};

use super::context::Binder;
use super::select::{self, QueryCtx};
use super::target_table::{self, DmlKind};
use crate::diag;
use crate::ir::{BoundCopy, BoundStatement, CopyDirection, CopyEndpoint, CopyFormat, CopyOptions, CopyTarget};
use crate::names::qual_name;

/// The PG 18.6 `DefElem` forms COPY options reduce to, with names already folded.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OptDef {
    Format(String),
    Freeze(bool),
    Delimiter(String),
    Null(String),
    Header(bool),
    Quote(String),
    Escape(String),
    ForceQuote(Vec<String>),
    ForceNotNull(Vec<String>),
    ForceNull(Vec<String>),
    Encoding(String),
}

/// `CopyFormatOptions` after `ProcessCopyOptions` (column lists still by name).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedOptions {
    format: CopyFormat,
    delimiter: char,
    null_string: String,
    header: bool,
    quote: char,
    escape: char,
    encoding: Option<String>,
    freeze: bool,
    force_quote: Option<Vec<String>>,
    force_not_null: Option<Vec<String>>,
    force_null: Option<Vec<String>>,
}

/// A column as `CopyGetAttnums` sees it.
struct ColSpec<'s> {
    attnum: i16,
    name: &'s str,
    dropped: bool,
    generated: bool,
}

/// What a COPY table name resolves to.
enum CopyRelation<'a> {
    Relation(&'a Relation),
    Sequence(String),
}

/// Bind COPY.
pub fn bind_copy(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let (source, to, target, options, legacy_options, values) = match stmt {
        Statement::Copy { source, to, target, options, legacy_options, values } => {
            (source, *to, target, options, legacy_options, values)
        }
        _ => return Err(PgError::internal("binder: bind_copy called with a non-COPY statement")),
    };
    if !values.is_empty() {
        // sqlparser consumed `COPY ... FROM STDIN; <data>` as inline data; PG never reads it from the query text.
        return Err(diag::unsupported_construct("COPY data inline in the statement text", None));
    }
    let is_from = !to;
    let direction = if is_from { CopyDirection::From } else { CopyDirection::To };
    let defs = copy_option_defs(b, options, legacy_options)?;
    let endpoint = copy_endpoint(target, is_from);

    let (ir_target, resolved, forces) = match source {
        CopySource::Table { table_name, columns } => {
            let mut notices = Vec::new();
            let found = lookup_copy_relation(b, table_name, &mut notices)?;
            let rel = match found {
                CopyRelation::Sequence(name) => {
                    let msg = if is_from {
                        format!("cannot copy to sequence \"{name}\"")
                    } else {
                        format!("cannot copy from sequence \"{name}\"")
                    };
                    return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, msg));
                }
                CopyRelation::Relation(rel) => rel,
            };
            check_copy_relation_kind(rel, is_from)?;
            let resolved = resolve_options(&defs, is_from)?;
            let column_names = ident_names(b, columns);
            let table = if is_from {
                // Only FROM reaches the DML target path; its own name notices replace ours.
                let (table, _) = target_table::open_target_table(b, table_name, None, DmlKind::Copy, true)?;
                table
            } else {
                b.notices.append(&mut notices);
                target_table::build_target_table(b, rel, false)?
            };
            let specs: Vec<ColSpec<'_>> = table
                .columns
                .iter()
                .map(|c| ColSpec {
                    attnum: c.attnum,
                    name: c.name.as_str(),
                    dropped: c.dropped,
                    generated: c.generated.is_some(),
                })
                .collect();
            let attnums = copy_attnums(&specs, Some(table.name.as_str()), column_names.as_deref())?;
            let forces = resolve_forces(&specs, Some(table.name.as_str()), &resolved)?;
            (CopyTarget::Table { table: Box::new(table), columns: attnums }, resolved, forces)
        }
        CopySource::Query(q) => {
            if is_from {
                // gram.y: COPY (query) only exists in the TO form.
                return Err(diag::syntax_error_near("(", None));
            }
            let bq = select::bind_query(b, q, &QueryCtx { top_level: true, ..QueryCtx::default() })?;
            let resolved = resolve_options(&defs, false)?;
            let specs: Vec<ColSpec<'_>> = bq
                .output
                .iter()
                .enumerate()
                .map(|(i, c)| ColSpec {
                    attnum: (i + 1) as i16,
                    name: c.name.as_str(),
                    dropped: false,
                    generated: false,
                })
                .collect();
            let forces = resolve_forces(&specs, None, &resolved)?;
            (CopyTarget::Query(Box::new(bq)), resolved, forces)
        }
    };

    let copy_options = CopyOptions {
        format: resolved.format,
        delimiter: resolved.delimiter,
        null_string: resolved.null_string,
        header: resolved.header,
        quote: resolved.quote,
        escape: resolved.escape,
        force_quote_all: false,
        force_quote: forces.0,
        force_not_null: forces.1,
        force_null: forces.2,
        encoding: resolved.encoding,
        freeze: resolved.freeze,
        default_string: None,
    };
    Ok(BoundStatement::Copy(Box::new(BoundCopy {
        direction,
        target: ir_target,
        endpoint,
        options: copy_options,
        where_: None,
    })))
}

/// Endpoint from the sqlparser target. `STDIN`/`STDOUT` are interchangeable in PG's grammar; the direction
/// selects the pipe (`DoCopy`: a missing file name means stdin for FROM and stdout for TO).
fn copy_endpoint(target: &AstCopyTarget, is_from: bool) -> CopyEndpoint {
    match target {
        AstCopyTarget::Stdin | AstCopyTarget::Stdout => {
            if is_from {
                CopyEndpoint::Stdin
            } else {
                CopyEndpoint::Stdout
            }
        }
        AstCopyTarget::File { filename } => CopyEndpoint::File(filename.clone()),
        AstCopyTarget::Program { command } => CopyEndpoint::Program(command.clone()),
    }
}

/// Fold identifiers (pushes truncation NOTICEs). Empty input gives an empty list.
fn ident_names(b: &mut Binder, ids: &[Ident]) -> Option<Vec<String>> {
    if ids.is_empty() {
        return None;
    }
    Some(ids.iter().map(|id| b.ident(id)).collect())
}

/// The option list as `DefElem`s, in statement order. sqlparser keeps the parenthesized and the legacy
/// lists apart; PG's grammar accepts only one of them per statement.
fn copy_option_defs(b: &mut Binder, options: &[CopyOption], legacy: &[CopyLegacyOption]) -> Result<Vec<OptDef>> {
    if !options.is_empty() {
        if let Some(first) = legacy.first() {
            let msg = format!("syntax error at or near \"{}\"", legacy_keyword(first));
            return Err(PgError::new(SqlState::SYNTAX_ERROR, msg));
        }
    }
    let mut defs = Vec::with_capacity(options.len() + legacy.len());
    for opt in options {
        defs.push(new_option_def(b, opt));
    }
    for opt in legacy {
        legacy_option_defs(b, opt, &mut defs)?;
    }
    Ok(defs)
}

/// One parenthesized `WITH (...)` option.
fn new_option_def(b: &mut Binder, opt: &CopyOption) -> OptDef {
    match opt {
        CopyOption::Format(id) => OptDef::Format(b.ident(id)),
        CopyOption::Freeze(v) => OptDef::Freeze(*v),
        CopyOption::Delimiter(c) => OptDef::Delimiter(c.to_string()),
        CopyOption::Null(s) => OptDef::Null(s.clone()),
        CopyOption::Header(v) => OptDef::Header(*v),
        CopyOption::Quote(c) => OptDef::Quote(c.to_string()),
        CopyOption::Escape(c) => OptDef::Escape(c.to_string()),
        CopyOption::ForceQuote(ids) => OptDef::ForceQuote(ident_list(b, ids)),
        CopyOption::ForceNotNull(ids) => OptDef::ForceNotNull(ident_list(b, ids)),
        CopyOption::ForceNull(ids) => OptDef::ForceNull(ident_list(b, ids)),
        CopyOption::Encoding(s) => OptDef::Encoding(s.clone()),
    }
}

/// Legacy `WITH CSV HEADER ...` options: `CSV` is `FORMAT csv` plus its sub-options, as in `copy_opt_item`.
fn legacy_option_defs(b: &mut Binder, opt: &CopyLegacyOption, out: &mut Vec<OptDef>) -> Result<()> {
    match opt {
        CopyLegacyOption::Binary => out.push(OptDef::Format("binary".to_string())),
        CopyLegacyOption::Csv(subs) => {
            out.push(OptDef::Format("csv".to_string()));
            for sub in subs {
                match sub {
                    CopyLegacyCsvOption::Header => out.push(OptDef::Header(true)),
                    CopyLegacyCsvOption::Quote(c) => out.push(OptDef::Quote(c.to_string())),
                    CopyLegacyCsvOption::Escape(c) => out.push(OptDef::Escape(c.to_string())),
                    CopyLegacyCsvOption::ForceQuote(ids) => out.push(OptDef::ForceQuote(ident_list(b, ids))),
                    CopyLegacyCsvOption::ForceNotNull(ids) => out.push(OptDef::ForceNotNull(ident_list(b, ids))),
                }
            }
        }
        CopyLegacyOption::Delimiter(c) => out.push(OptDef::Delimiter(c.to_string())),
        CopyLegacyOption::Null(s) => out.push(OptDef::Null(s.clone())),
        CopyLegacyOption::Header => out.push(OptDef::Header(true)),
        CopyLegacyOption::Escape => {
            // sqlparser drops the escape character of the legacy form, so the option cannot be bound.
            return Err(diag::unsupported_construct("COPY legacy ESCAPE option", None));
        }
        other => {
            // Redshift/Snowflake-only options (IAM_ROLE, GZIP, ...) are not PG syntax.
            let msg = format!("syntax error at or near \"{}\"", legacy_keyword(other));
            return Err(PgError::new(SqlState::SYNTAX_ERROR, msg));
        }
    }
    Ok(())
}

fn ident_list(b: &mut Binder, ids: &[Ident]) -> Vec<String> {
    ids.iter().map(|id| b.ident(id)).collect()
}

/// The keyword text PG would print for a legacy option sqlparser accepted but PG does not know.
fn legacy_keyword(opt: &CopyLegacyOption) -> String {
    match opt {
        CopyLegacyOption::IamRole(_) => return "IAM_ROLE".to_string(),
        CopyLegacyOption::PartitionBy(_) => return "PARTITION".to_string(),
        _ => {}
    }
    let debug = format!("{opt:?}");
    let head = debug.split(|c: char| c == '(' || c == ' ' || c == '{').next().unwrap_or("");
    head.to_ascii_uppercase()
}

/// Resolve the table name the way `open_target_table` does, without relation-kind restrictions.
/// Returns the relation or the sequence name. Notices are pushed to `notices` (the caller decides).
fn lookup_copy_relation<'a>(
    b: &mut Binder<'a>,
    name: &ObjectName,
    notices: &mut Vec<PgError>,
) -> Result<CopyRelation<'a>> {
    let env: &'a CatalogEnv = b.env;
    let pos = b.pos(name);
    let qn = qual_name(name, b.src, notices)?;
    if let Some(cat) = qn.catalog.as_deref() {
        if cat != b.ctx.session.database.as_str() {
            let mut text = String::from(cat);
            if let Some(s) = qn.schema.as_deref() {
                text.push('.');
                text.push_str(s);
            }
            text.push('.');
            text.push_str(&qn.name);
            return Err(diag::cross_database(&text, pos));
        }
    }
    let schema = qn.schema.as_deref();
    let class = env.resolve_class(schema, &qn.name).map_err(|e| diag::with_position(e, pos))?;
    if let ClassRef::Sequence(_) = class {
        return Ok(CopyRelation::Sequence(qn.name.clone()));
    }
    let rel = env.resolve_relation(schema, &qn.name).map_err(|e| diag::with_position(e, pos))?;
    Ok(CopyRelation::Relation(rel))
}

const TRY_QUERY_VARIANT: &str = "Try the COPY (SELECT ...) TO variant.";

/// Relation-kind checks of `BeginCopyTo` (TO) and `CopyFrom` (FROM).
fn check_copy_relation_kind(rel: &Relation, is_from: bool) -> Result<()> {
    let name = rel.name.as_str();
    let wrong = |msg: String| PgError::new(SqlState::WRONG_OBJECT_TYPE, msg);
    if is_from {
        return match rel.kind {
            RelKind::Table | RelKind::ForeignTable | RelKind::PartitionedTable => Ok(()),
            RelKind::View => Err(wrong(format!("cannot copy to view \"{name}\""))
                .with_hint("To enable copying to a view, provide an INSTEAD OF INSERT trigger.")),
            RelKind::MatView => Err(wrong(format!("cannot copy to materialized view \"{name}\""))),
            _ => Err(wrong(format!("cannot copy to non-table relation \"{name}\""))),
        };
    }
    match rel.kind {
        RelKind::Table => Ok(()),
        RelKind::View => Err(wrong(format!("cannot copy from view \"{name}\"")).with_hint(TRY_QUERY_VARIANT)),
        RelKind::MatView => {
            let populated = rel.view.as_ref().map(|v| v.populated).unwrap_or(true);
            if populated {
                Ok(())
            } else {
                Err(PgError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot copy from unpopulated materialized view \"{name}\""),
                )
                .with_hint("Use the REFRESH MATERIALIZED VIEW command."))
            }
        }
        RelKind::ForeignTable => {
            Err(wrong(format!("cannot copy from foreign table \"{name}\"")).with_hint(TRY_QUERY_VARIANT))
        }
        RelKind::PartitionedTable => {
            Err(wrong(format!("cannot copy from partitioned table \"{name}\"")).with_hint(TRY_QUERY_VARIANT))
        }
        _ => Err(wrong(format!("cannot copy from non-table relation \"{name}\""))),
    }
}

/// `CopyGetAttnums`: column list -> attnums. No list means all live, non-generated columns.
fn copy_attnums(cols: &[ColSpec<'_>], rel: Option<&str>, names: Option<&[String]>) -> Result<Vec<i16>> {
    let names = match names {
        Some(n) if !n.is_empty() => n,
        _ => {
            return Ok(cols.iter().filter(|c| !c.dropped && !c.generated).map(|c| c.attnum).collect());
        }
    };
    let mut out: Vec<i16> = Vec::with_capacity(names.len());
    for name in names {
        let found = cols.iter().find(|c| !c.dropped && c.name == name.as_str());
        let attnum = match found {
            Some(c) if c.generated => {
                return Err(PgError::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!("column \"{name}\" is a generated column"),
                )
                .with_detail("Generated columns cannot be used in COPY."));
            }
            Some(c) => c.attnum,
            None => {
                let msg = match rel {
                    Some(r) => format!("column \"{name}\" of relation \"{r}\" does not exist"),
                    None => format!("column \"{name}\" does not exist"),
                };
                return Err(PgError::new(SqlState::UNDEFINED_COLUMN, msg));
            }
        };
        if out.contains(&attnum) {
            return Err(PgError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{name}\" specified more than once"),
            ));
        }
        out.push(attnum);
    }
    Ok(out)
}

/// Attnums of FORCE_QUOTE, FORCE_NOT_NULL and FORCE_NULL (empty when not given).
fn resolve_forces(
    cols: &[ColSpec<'_>],
    rel: Option<&str>,
    opts: &ResolvedOptions,
) -> Result<(Vec<i16>, Vec<i16>, Vec<i16>)> {
    let fq = match &opts.force_quote {
        Some(n) => copy_attnums(cols, rel, Some(n.as_slice()))?,
        None => Vec::new(),
    };
    let fnn = match &opts.force_not_null {
        Some(n) => copy_attnums(cols, rel, Some(n.as_slice()))?,
        None => Vec::new(),
    };
    let fnull = match &opts.force_null {
        Some(n) => copy_attnums(cols, rel, Some(n.as_slice()))?,
        None => Vec::new(),
    };
    Ok((fq, fnn, fnull))
}

fn invalid_value(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg)
}

fn syntax_error(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, msg)
}

fn not_supported(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, msg)
}

/// Encoding names PG knows. Only UTF8 is accepted; the others are valid names PGArbor cannot convert.
const PG_ENCODING_NAMES: &[&str] = &[
    "SQLASCII", "EUCJP", "EUCCN", "EUCKR", "EUCTW", "EUCJIS2004", "MULEINTERNAL", "LATIN1", "LATIN2", "LATIN3",
    "LATIN4", "LATIN5", "LATIN6", "LATIN7", "LATIN8", "LATIN9", "LATIN10", "ISO88595", "ISO88596", "ISO88597",
    "ISO88598", "KOI8R", "KOI8U", "WIN866", "WIN874", "WIN1250", "WIN1251", "WIN1252", "WIN1253", "WIN1254",
    "WIN1255", "WIN1256", "WIN1257", "WIN1258", "SJIS", "BIG5", "GBK", "UHC", "GB18030", "JOHAB",
    "SHIFTJIS2004",
];

/// Name folding used for encoding names: case-insensitive, `-` and `_` ignored.
fn norm_encoding(s: &str) -> String {
    s.chars().filter(|c| *c != '-' && *c != '_').map(|c| c.to_ascii_uppercase()).collect()
}

/// `ENCODING` value: UTF8 is the only encoding PGArbor converts; other PG encodings are 0A000.
fn canonical_encoding(name: &str) -> Result<String> {
    let norm = norm_encoding(name);
    if norm == "UTF8" || norm == "UNICODE" {
        return Ok("UTF8".to_string());
    }
    if PG_ENCODING_NAMES.contains(&norm.as_str()) {
        return Err(not_supported(format!("COPY encoding \"{name}\" is not supported by PGArbor yet")));
    }
    Err(invalid_value("argument to option \"encoding\" must be a valid encoding name"))
}

/// `ProcessCopyOptions`: self-consistency of the option list, defaults, then the same checks in the same
/// order as PG 18.6 `copy.c`.
fn resolve_options(defs: &[OptDef], is_from: bool) -> Result<ResolvedOptions> {
    let conflict = || syntax_error("conflicting or redundant options");
    let mut format_seen = false;
    let mut csv = false;
    let mut binary = false;
    let mut freeze = false;
    let mut freeze_seen = false;
    let mut delim: Option<String> = None;
    let mut null_print: Option<String> = None;
    let mut header = false;
    let mut header_seen = false;
    let mut quote: Option<String> = None;
    let mut escape: Option<String> = None;
    let mut force_quote: Option<Vec<String>> = None;
    let mut force_not_null: Option<Vec<String>> = None;
    let mut force_null: Option<Vec<String>> = None;
    let mut encoding: Option<String> = None;
    let mut encoding_seen = false;

    for def in defs {
        match def {
            OptDef::Format(fmt) => {
                if format_seen {
                    return Err(conflict());
                }
                format_seen = true;
                match fmt.as_str() {
                    "text" => {}
                    "csv" => csv = true,
                    "binary" => binary = true,
                    other => return Err(invalid_value(format!("COPY format \"{other}\" not recognized"))),
                }
            }
            OptDef::Freeze(v) => {
                if freeze_seen {
                    return Err(conflict());
                }
                freeze_seen = true;
                freeze = *v;
            }
            OptDef::Delimiter(s) => {
                if delim.is_some() {
                    return Err(conflict());
                }
                delim = Some(s.clone());
            }
            OptDef::Null(s) => {
                if null_print.is_some() {
                    return Err(conflict());
                }
                null_print = Some(s.clone());
            }
            OptDef::Header(v) => {
                if header_seen {
                    return Err(conflict());
                }
                header_seen = true;
                header = *v;
            }
            OptDef::Quote(s) => {
                if quote.is_some() {
                    return Err(conflict());
                }
                quote = Some(s.clone());
            }
            OptDef::Escape(s) => {
                if escape.is_some() {
                    return Err(conflict());
                }
                escape = Some(s.clone());
            }
            OptDef::ForceQuote(cols) => {
                if force_quote.is_some() {
                    return Err(conflict());
                }
                force_quote = Some(cols.clone());
            }
            OptDef::ForceNotNull(cols) => {
                if force_not_null.is_some() {
                    return Err(conflict());
                }
                force_not_null = Some(cols.clone());
            }
            OptDef::ForceNull(cols) => {
                if force_null.is_some() {
                    return Err(conflict());
                }
                force_null = Some(cols.clone());
            }
            OptDef::Encoding(s) => {
                if encoding_seen {
                    return Err(conflict());
                }
                encoding_seen = true;
                encoding = Some(canonical_encoding(s)?);
            }
        }
    }

    // Incompatible with BINARY (checked before the defaults are inserted).
    if binary {
        if delim.is_some() {
            return Err(syntax_error("cannot specify DELIMITER in BINARY mode"));
        }
        if null_print.is_some() {
            return Err(syntax_error("cannot specify NULL in BINARY mode"));
        }
    }

    // Defaults for omitted options.
    let delimiter = delim.unwrap_or_else(|| if csv { ",".to_string() } else { "\t".to_string() });
    let null_string = null_print.clone().unwrap_or_else(|| if csv { String::new() } else { "\\N".to_string() });
    let quote_eff: String = quote.clone().unwrap_or_else(|| "\"".to_string());
    let escape_eff: String = escape.clone().unwrap_or_else(|| quote_eff.clone());

    // Single-byte delimiter; no line terminators; no characters that are unsafe in text mode.
    if delimiter.len() != 1 {
        return Err(not_supported("COPY delimiter must be a single one-byte character"));
    }
    if delimiter.contains('\r') || delimiter.contains('\n') {
        return Err(invalid_value("COPY delimiter cannot be newline or carriage return"));
    }
    if null_string.contains('\r') || null_string.contains('\n') {
        return Err(invalid_value("COPY null representation cannot use newline or carriage return"));
    }
    let delim_char: char = delimiter.chars().next().unwrap_or('\t');
    if !csv && "\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(delim_char) {
        return Err(invalid_value(format!("COPY delimiter cannot be \"{delimiter}\"")));
    }

    // HEADER: BINARY mode rejects a true header; QUOTE and ESCAPE need CSV mode and one byte.
    if binary && header {
        return Err(not_supported("cannot specify HEADER in BINARY mode"));
    }
    if !csv && quote.is_some() {
        return Err(not_supported("COPY QUOTE requires CSV mode"));
    }
    if csv && quote_eff.len() != 1 {
        return Err(not_supported("COPY quote must be a single one-byte character"));
    }
    if csv && delimiter.as_bytes().first() == quote_eff.as_bytes().first() {
        return Err(invalid_value("COPY delimiter and quote must be different"));
    }
    if !csv && escape.is_some() {
        return Err(not_supported("COPY ESCAPE requires CSV mode"));
    }
    if csv && escape_eff.len() != 1 {
        return Err(not_supported("COPY escape must be a single one-byte character"));
    }

    // FORCE_* lists.
    if !csv && force_quote.is_some() {
        return Err(not_supported("COPY FORCE_QUOTE requires CSV mode"));
    }
    if force_quote.is_some() && is_from {
        return Err(not_supported("COPY FORCE_QUOTE cannot be used with COPY FROM"));
    }
    if !csv && force_not_null.is_some() {
        return Err(not_supported("COPY FORCE_NOT_NULL requires CSV mode"));
    }
    if force_not_null.is_some() && !is_from {
        return Err(invalid_value("COPY FORCE_NOT_NULL cannot be used with COPY TO"));
    }
    if !csv && force_null.is_some() {
        return Err(not_supported("COPY FORCE_NULL requires CSV mode"));
    }
    if force_null.is_some() && !is_from {
        return Err(invalid_value("COPY FORCE_NULL cannot be used with COPY TO"));
    }

    // The delimiter and the CSV quote must not appear in the NULL string.
    if null_string.contains(delim_char) {
        return Err(invalid_value("COPY delimiter character must not appear in the NULL specification"));
    }
    if csv && null_string.contains(quote_eff.chars().next().unwrap_or('"')) {
        return Err(invalid_value("CSV quote character must not appear in the NULL specification"));
    }

    if freeze && !is_from {
        return Err(invalid_value("COPY FREEZE cannot be used with COPY TO"));
    }

    let format = if csv {
        CopyFormat::Csv
    } else if binary {
        CopyFormat::Binary
    } else {
        CopyFormat::Text
    };
    Ok(ResolvedOptions {
        format,
        delimiter: delim_char,
        null_string,
        header,
        quote: quote_eff.chars().next().unwrap_or('"'),
        escape: escape_eff.chars().next().unwrap_or('"'),
        encoding,
        freeze,
        force_quote,
        force_not_null,
        force_null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(r: Result<ResolvedOptions>) -> ResolvedOptions {
        match r {
            Ok(v) => v,
            Err(e) => panic!("unexpected error: {} ({})", e.message, e.sqlstate.code()),
        }
    }

    fn err(r: Result<ResolvedOptions>) -> PgError {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    fn col(attnum: i16, name: &'static str, dropped: bool, generated: bool) -> ColSpec<'static> {
        ColSpec { attnum, name, dropped, generated }
    }

    #[test]
    fn text_defaults() {
        let r = ok(resolve_options(&[], true));
        assert_eq!(r.format, CopyFormat::Text);
        assert_eq!(r.delimiter, '\t');
        assert_eq!(r.null_string, "\\N");
        assert!(!r.header);
        assert_eq!(r.quote, '"');
        assert_eq!(r.escape, '"');
        assert_eq!(r.encoding, None);
        assert!(!r.freeze);
    }

    #[test]
    fn csv_defaults_and_header() {
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Header(true)];
        let r = ok(resolve_options(&defs, false));
        assert_eq!(r.format, CopyFormat::Csv);
        assert_eq!(r.delimiter, ',');
        assert_eq!(r.null_string, "");
        assert!(r.header);
        assert_eq!(r.quote, '"');
        assert_eq!(r.escape, '"');
    }

    #[test]
    fn header_is_allowed_in_text_mode() {
        let r = ok(resolve_options(&[OptDef::Header(true)], false));
        assert_eq!(r.format, CopyFormat::Text);
        assert!(r.header);
    }

    #[test]
    fn quote_requires_csv() {
        let e = err(resolve_options(&[OptDef::Quote("'".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY QUOTE requires CSV mode");
    }

    #[test]
    fn escape_requires_csv() {
        let e = err(resolve_options(&[OptDef::Escape("\\".to_string())], false));
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY ESCAPE requires CSV mode");
    }

    #[test]
    fn binary_rejects_delimiter_and_null() {
        let defs = vec![OptDef::Format("binary".to_string()), OptDef::Delimiter(",".to_string())];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "cannot specify DELIMITER in BINARY mode");
        let defs = vec![OptDef::Format("binary".to_string()), OptDef::Null("x".to_string())];
        assert_eq!(err(resolve_options(&defs, true)).message, "cannot specify NULL in BINARY mode");
    }

    #[test]
    fn duplicate_option_conflicts() {
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Format("text".to_string())];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "conflicting or redundant options");
    }

    #[test]
    fn unknown_format() {
        let e = err(resolve_options(&[OptDef::Format("xml".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY format \"xml\" not recognized");
    }

    #[test]
    fn delimiter_rules() {
        let e = err(resolve_options(&[OptDef::Delimiter("ab".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY delimiter must be a single one-byte character");
        let e = err(resolve_options(&[OptDef::Delimiter("a".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY delimiter cannot be \"a\"");
        // Letters are allowed as delimiters in CSV mode.
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Delimiter("a".to_string())];
        assert_eq!(ok(resolve_options(&defs, true)).delimiter, 'a');
    }

    #[test]
    fn csv_delimiter_must_differ_from_quote() {
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Delimiter("|".to_string()), OptDef::Quote("|".to_string())];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY delimiter and quote must be different");
    }

    #[test]
    fn null_must_not_contain_delimiter() {
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Null(",".to_string())];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.message, "COPY delimiter character must not appear in the NULL specification");
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::Null("\"".to_string())];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.message, "CSV quote character must not appear in the NULL specification");
    }

    #[test]
    fn freeze_and_direction_rules() {
        let e = err(resolve_options(&[OptDef::Freeze(true)], false));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY FREEZE cannot be used with COPY TO");
        assert!(ok(resolve_options(&[OptDef::Freeze(true)], true)).freeze);
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::ForceQuote(vec!["a".to_string()])];
        let e = err(resolve_options(&defs, true));
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY FORCE_QUOTE cannot be used with COPY FROM");
        let defs = vec![OptDef::Format("csv".to_string()), OptDef::ForceNull(vec!["a".to_string()])];
        assert_eq!(err(resolve_options(&defs, false)).message, "COPY FORCE_NULL cannot be used with COPY TO");
    }

    #[test]
    fn encodings() {
        let r = ok(resolve_options(&[OptDef::Encoding("utf-8".to_string())], true));
        assert_eq!(r.encoding.as_deref(), Some("UTF8"));
        let e = err(resolve_options(&[OptDef::Encoding("LATIN1".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        let e = err(resolve_options(&[OptDef::Encoding("nonsense".to_string())], true));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn column_list_resolution() {
        let specs = vec![
            col(1, "a", false, false),
            col(2, "dropped", true, false),
            col(3, "g", false, true),
            col(4, "b", false, false),
        ];
        // Default list: live, non-generated columns.
        assert_eq!(copy_attnums(&specs, Some("t"), None).expect("default"), vec![1, 4]);
        // Explicit list keeps the given order.
        let names = vec!["b".to_string(), "a".to_string()];
        assert_eq!(copy_attnums(&specs, Some("t"), Some(names.as_slice())).expect("explicit"), vec![4, 1]);
        // Unknown column.
        let names = vec!["x".to_string()];
        let e = copy_attnums(&specs, Some("t"), Some(names.as_slice())).err().expect("unknown");
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_COLUMN);
        assert_eq!(e.message, "column \"x\" of relation \"t\" does not exist");
        // Generated column.
        let names = vec!["g".to_string()];
        let e = copy_attnums(&specs, Some("t"), Some(names.as_slice())).err().expect("generated");
        assert_eq!(e.sqlstate, SqlState::INVALID_COLUMN_REFERENCE);
        assert_eq!(e.message, "column \"g\" is a generated column");
        assert_eq!(e.detail.as_deref(), Some("Generated columns cannot be used in COPY."));
        // Duplicate column.
        let names = vec!["a".to_string(), "a".to_string()];
        let e = copy_attnums(&specs, None, Some(names.as_slice())).err().expect("duplicate");
        assert_eq!(e.sqlstate, SqlState::DUPLICATE_COLUMN);
        assert_eq!(e.message, "column \"a\" specified more than once");
        // Without a relation name (COPY (query)).
        let names = vec!["zz".to_string()];
        let e = copy_attnums(&specs, None, Some(names.as_slice())).err().expect("query unknown");
        assert_eq!(e.message, "column \"zz\" does not exist");
    }

    #[test]
    fn endpoints_follow_direction() {
        assert_eq!(copy_endpoint(&AstCopyTarget::Stdin, false), CopyEndpoint::Stdout);
        assert_eq!(copy_endpoint(&AstCopyTarget::Stdout, true), CopyEndpoint::Stdin);
        assert_eq!(copy_endpoint(&AstCopyTarget::File { filename: "f".to_string() }, true), CopyEndpoint::File("f".to_string()));
    }
}
