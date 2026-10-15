//! `pgarbor dump` (lease CLI-26): pg_dump-like plain SQL schema + data.
//!
//! The catalog query surface is still being implemented by the embedded SQL leases. This command
//! refuses to write a partial dump until those catalog queries can provide a complete inventory.
use pgarbor_common::PgError;
use crate::cli::DumpArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;

/// Run a consistent plain SQL dump. A partial dump is never presented as successful output.
pub fn run(ctx: &mut Ctx, args: &DumpArgs) -> CliResult<Exit> {
    if ctx.json && args.output.is_none() {
        return Err(CliError::usage("--json requires --output for dump"));
    }
    if args.copy {
        return Err(CliError::from(PgError::unimplemented(
            "CLI-26",
            "dump --copy needs COPY streaming (not implemented yet)",
        )));
    }
    let target = crate::paths::resolve_target(args.file.as_deref(), &args.target)?;
    let db = crate::open::open_database(&target.db_path, &crate::open::OpenSpec::read_only())?;
    let branch = crate::paths::bind_branch(&db, &target)?;
    let mut session = db.session(&branch, pgarbor::SessionOptions::default())?;
    session.simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")?;
    let work = build_dump(&mut session, args);
    let _ = session.simple_query("ROLLBACK");
    session.close();
    db.close()?;
    let (text, tables, rows) = work?;
    if let Some(path) = &args.output {
        use std::io::Write;
        let mut tmp = path.as_os_str().to_os_string();
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d|d.as_nanos()).unwrap_or(0);
        tmp.push(format!(".tmp-{}-{nonce}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let published = (|| -> std::io::Result<()> {file.write_all(text.as_bytes())?;file.sync_all()?;std::fs::rename(&tmp,path)?;Ok(())})();
        if let Err(e) = published {let _=std::fs::remove_file(&tmp);return Err(CliError::io("write dump", &e));}
        if ctx.json {ctx.emit_json(&serde_json::json!({"path":path.display().to_string(),"tables":tables,"rows":rows,"bytes":text.len()}))?;}
    } else {use std::io::Write;ctx.out.write_all(text.as_bytes())?;ctx.out.flush()?;}
    Ok(Exit::OK)
}

/// PostgreSQL `quote_ident`: leaves simple lowercase non-keywords bare; otherwise quotes and
/// doubles embedded double quotes.
pub fn quote_ident(s: &str) -> Result<String, CliError> {
    const RESERVED: &[&str] = &[
        "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "both",
        "case", "cast", "check", "collate", "column", "constraint", "create", "current_catalog",
        "current_date", "current_role", "current_time", "current_timestamp", "current_user", "default",
        "deferrable", "desc", "distinct", "do", "else", "end", "except", "false", "fetch", "for",
        "foreign", "from", "grant", "group", "having", "in", "initially", "intersect", "into",
        "lateral", "leading", "limit", "localtime", "localtimestamp", "not", "null", "offset", "on",
        "only", "or", "order", "placing", "primary", "references", "returning", "select",
        "session_user", "some", "symmetric", "system_user", "table", "then", "to", "trailing", "true",
        "union", "unique", "user", "using", "variadic", "when", "where", "window", "with",
    ];
    let mut chars = s.chars();
    let first_ok = chars.next().is_some_and(|c| c == '_' || c.is_ascii_lowercase());
    let body_ok = s.chars().all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit());
    if first_ok && body_ok && !RESERVED.contains(&s) {
        Ok(s.to_owned())
    } else {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            out.push(c);
            if c == '"' { out.push('"'); }
        }
        out.push('"');
        Ok(out)
    }
}

/// PostgreSQL literal quoting under `standard_conforming_strings = on`.
pub fn quote_literal(s: &str) -> Result<String, CliError> {
    let escaped = s.contains('\\');
    let mut out = String::with_capacity(s.len() + if escaped { 3 } else { 2 });
    if escaped { out.push('E'); }
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if escaped => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_quoting() {
        for (input, expected) in [("simple_name", "simple_name"), ("select", "\"select\""),
            ("Mixed", "\"Mixed\""), ("a\"b", "\"a\"\"b\"")] {
            assert_eq!(quote_ident(input).expect("quote identifier"), expected);
        }
    }

    #[test]
    fn literal_quoting() {
        assert_eq!(quote_literal("plain").expect("quote literal"), "'plain'");
        assert_eq!(quote_literal("it's").expect("quote literal"), "'it''s'");
        assert_eq!(quote_literal("a\\b").expect("quote literal"), "E'a\\\\b'");
    }
}


fn text_rows(session: &mut pgarbor::Session, sql: &str) -> CliResult<Vec<Vec<Option<String>>>> {
    let outputs=session.simple_query(sql)?;let mut data=Vec::new();
    for out in outputs {
        if let pgarbor::StatementResult::Rows {columns,rows,..}=out.result {
            for row in rows {
                let encoded=session.encode_row(&columns,&vec![pgarbor::FormatCode::Text;columns.len()],&row)?;
                data.push(encoded.into_iter().map(|value|value.map(String::from_utf8).transpose().map_err(|_|CliError::internal("dump catalog text is not UTF8"))).collect::<CliResult<Vec<_>>>()?);
            }
        }
    }
    Ok(data)
}
fn required(row: &[Option<String>], slot: usize) -> CliResult<&str> {
    row.get(slot).and_then(|v|v.as_deref()).ok_or_else(||CliError::internal("missing dump catalog field"))
}
fn refuse(what: &str) -> CliError {CliError::from(PgError::unimplemented("CLI-26",format!("dump of {what}")))}

// The supported inventory is deliberately checked before publishing any bytes. A database
// containing unsupported objects gets 0A000, never a deceptively successful partial dump.
fn build_dump(s: &mut pgarbor::Session, args: &DumpArgs) -> CliResult<(String,usize,u64)> {
    let relations=text_rows(s,"SELECT c.oid, n.nspname, c.relname, c.relkind, c.relrowsecurity, c.relforcerowsecurity, c.relacl, c.relowner, c.relpersistence, c.reloptions, c.relreplident FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' ORDER BY c.oid")?;
    for r in &relations {
        if !matches!(required(r,3)?,"r"|"i") {return Err(refuse("non-table/index relations"));}
        if required(r,7)?!="10" || required(r,8)?!="p" || r.get(9).is_some_and(Option::is_some) || required(r,10)?!=if required(r,3)?=="i" {"n"} else {"d"} {return Err(refuse("custom ownership, persistence, options or replica identity"));}
        if required(r,4)?=="t" || required(r,5)?=="t" || r.get(6).is_some_and(Option::is_some) {return Err(refuse("row security or custom relation privileges"));}
    }
    for (sql,kind) in [
        ("SELECT t.oid FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND t.typtype IN ('e','d','r','m')","user-defined types"),
        ("SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema')","user-defined routines"),
        ("SELECT objoid FROM pg_description WHERE objoid >= 16384","user object comments"),
    ] {if !text_rows(s,sql)?.is_empty(){return Err(refuse(kind));}}
    let schemas=text_rows(s,"SELECT nspname FROM pg_namespace WHERE nspname NOT IN ('pg_catalog','information_schema','public') AND nspname NOT LIKE 'pg_toast%' AND nspname NOT LIKE 'pg_temp%' ORDER BY nspname")?;
    let mut text=String::from("-- PGArbor plain SQL dump\nSET standard_conforming_strings = on;\n");
    if !args.data_only {for r in schemas {text.push_str(&format!("CREATE SCHEMA {};\n",quote_ident(required(&r,0)?)?));}}
    let mut tables=Vec::new();let mut constraints=Vec::new();let mut indexes=Vec::new();
    for r in &relations {
        if required(r,3)?!="r" {continue;}
        let oid=required(r,0)?.parse::<u32>().map_err(|_|CliError::internal("invalid relation oid"))?;
        let schema=required(r,1)?.to_owned();let name=required(r,2)?.to_owned();
        let qualified=format!("{}.{}",quote_ident(&schema)?,quote_ident(&name)?);
        let constraint_rows=text_rows(s,&format!("SELECT conname, pg_get_constraintdef(oid), condeferrable, condeferred, convalidated, contype FROM pg_constraint WHERE conrelid={oid} ORDER BY oid"))?;
        let columns=text_rows(s,&format!("SELECT a.attname, format_type(a.atttypid,a.atttypmod), a.attnotnull, pg_get_expr(d.adbin,d.adrelid), a.attidentity, a.attgenerated FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid={oid} AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum"))?;
        let mut defs=Vec::new();let mut names=Vec::new();
        for col in columns {
            if !required(&col,4)?.is_empty() || !required(&col,5)?.is_empty(){return Err(refuse("identity/generated columns"));}
            let name=required(&col,0)?.to_owned();let mut def=format!("{} {}",quote_ident(&name)?,required(&col,1)?);
            if let Some(default)=col.get(3).and_then(|v|v.as_deref()){def.push_str(" DEFAULT ");def.push_str(default);}
            if required(&col,2)?=="t" {
                let expected=format!("NOT NULL {}",quote_ident(&name)?);
                let mut named=None;
                for c in &constraint_rows {if required(c,5)?=="n" && required(c,1)?==expected {named=Some(required(c,0)?);break;}}
                if let Some(constraint)=named {def.push_str(&format!(" CONSTRAINT {} NOT NULL",quote_ident(constraint)?));}
                else {def.push_str(" NOT NULL");}
            }
            names.push(name);defs.push(def);
        }
        if !args.data_only {text.push_str(&format!("CREATE TABLE {qualified} ({});\n",defs.join(", ")));}
        // Deferrable/non-enforced constraints have no faithful reload path yet.
        for c in constraint_rows {
            if required(&c,5)?=="n" {continue;}
            if required(&c,2)?=="t" || required(&c,3)?=="t" || required(&c,4)?!="t" {return Err(refuse("deferred/unvalidated constraints"));}
            constraints.push(format!("ALTER TABLE {qualified} ADD CONSTRAINT {} {};\n",quote_ident(required(&c,0)?)?,required(&c,1)?));
        }
        for i in text_rows(s,&format!("SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i LEFT JOIN pg_constraint c ON c.conindid=i.indexrelid WHERE i.indrelid={oid} AND c.oid IS NULL ORDER BY i.indexrelid"))? {indexes.push(format!("{};\n",required(&i,0)?));}
        if names.is_empty() {return Err(refuse("zero-column tables"));}
        tables.push(super::dump_data::TableRef {schema,name,columns:names,identity_always:Vec::new()});
    }
    let mut rows=0;
    if !args.schema_only {let mut data=Vec::new();for table in &tables {rows+=super::dump_data::dump_table_data(s,table,false,&mut data)?;}text.push_str(&String::from_utf8(data).map_err(|_|CliError::internal("dump text is not UTF8"))?);}
    if !args.data_only {for ddl in constraints.into_iter().chain(indexes) {text.push_str(&ddl);}}
    Ok((text,tables.len(),rows))
}
