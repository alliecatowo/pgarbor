//! Definition text of catalog objects as PostgreSQL prints it: `pg_get_indexdef`,
//! `pg_get_constraintdef`, `pg_get_viewdef`, function signatures/results/definitions, sequence and
//! default helpers. Lease CAT-26. PG sources: `utils/adt/ruleutils.c` (`pg_get_indexdef_worker`,
//! `pg_get_constraintdef_worker`, `pg_get_functiondef`, `print_function_arguments`,
//! `print_function_rettype`, `pg_get_function_arguments`, `pg_get_function_identity_arguments`,
//! `pg_get_function_result`, `pg_get_serial_sequence` is `utils/adt/misc.c`).
//!
//! These are pure functions of the model (expression TEXT is stored; only structure is formatted
//! here), used by the `sysfuncs::info` SQL functions, `pg_indexes`, `pg_views`,
//! information_schema and `\d` output. All object/column names go through
//! `pgarbor_types::text::format::quote_identifier` (quotes when not `[a-z_][a-z0-9_$]*` or a
//! reserved keyword per `pg_keywords`). Table references in definitions are schema-qualified exactly
//! as PG's `generate_relation_name`/`generate_qualified_relation_name` decide (documented per
//! function).
//!
//! ## `indexdef` (oracle-verified)
//! `CREATE [UNIQUE ]INDEX <name> ON <schema>.<table> USING <am> (<keys>)[ INCLUDE (<cols>)]
//! [ NULLS NOT DISTINCT][ WITH (<opts>)][ WHERE <predicate>]` - the table is ALWAYS schema-qualified
//! (`ON zz_cat.t`), the index name never. Examples:
//! - `CREATE UNIQUE INDEX t_pk2 ON zz_cat.t USING btree (a) WITH (fillfactor='50')` (option VALUES are
//!   single-quoted in indexdef; reloptions array shows `fillfactor=50`)
//! - `CREATE INDEX i1 ON zz_cat.t USING btree (((a + 1)))` (expression keys that are not plain function
//!   calls get an extra pair of parentheses), `... (lower(b))`, `... (a DESC NULLS LAST, b)`,
//!   `... (a) INCLUDE (a)`.
//! Key rendering: plain column -> quoted CURRENT column name (looked up by attnum, so a renamed
//! column prints its new name even though `IndexKey::name` keeps the old one); expression -> the
//! stored text; then ` <opclass>` only when the opclass is not the default for the type, then
//! ` COLLATE "<name>"` only when the collation differs from the column/type default (`COLLATE "C"`),
//! then ` DESC`, then ` NULLS FIRST|LAST` only when different from the default (ASC -> NULLS LAST,
//! DESC -> NULLS FIRST). `pretty` changes nothing here but spacing after commas is `, `.
//! With `column = Some(n)` (1-based) only that key's text is returned (`pg_get_indexdef(oid, n, pretty)`),
//! `column = Some(0)` is the full definition.
//!
//! ## `constraintdef` (oracle-verified for 18.6)
//! - PK: `PRIMARY KEY (id)`; UNIQUE: `UNIQUE (b)` / `UNIQUE NULLS NOT DISTINCT (b)`; with INCLUDE:
//!   `PRIMARY KEY (id) INCLUDE (x)`; backing-index options: ` WITH (fillfactor='70')`.
//! - CHECK: `CHECK ((c > 0))` = `CHECK (` + stored text + `)`; stored text already has its own
//!   parentheses as `pg_get_expr` prints them.
//! - NOT NULL: `NOT NULL a`; domain: `NOT NULL`, `CHECK ((VALUE > 0))`.
//! - FK: `FOREIGN KEY (f) REFERENCES t(id)` + ` MATCH FULL` (simple prints nothing) + ` ON UPDATE
//!   <action>` only if not NO ACTION + ` ON DELETE <action>` (with `SET NULL (a, b)` /
//!   `SET DEFAULT (a)` column lists when `delete_set_columns` is non-empty); the referenced table
//!   name is schema-qualified only when not visible in the search path (`env`).
//! - Trailing modifiers in this order: ` DEFERRABLE`, ` INITIALLY DEFERRED` (only when
//!   deferrable), ` NOT VALID` (when `!validated`), ` NOT ENFORCED` (when `!enforced`).
//! - EXCLUDE is unsupported (0A000 is never reached: such constraints cannot exist).
//!
//! ## Functions
//! - `function_arguments`: `pg_get_function_arguments`: every parameter with mode words:
//!   `a integer, b text DEFAULT 'x'::text, OUT o1 integer, VARIADIC v integer[]`; IN mode is implicit
//!   (no word), `INOUT`, `OUT`, `VARIADIC`; TABLE columns are NOT listed (they appear in the result);
//!   unnamed parameters print only the type; defaults print ` DEFAULT <expr>` (stored text).
//! - `function_identity_arguments`: same but without defaults and without OUT/TABLE params
//!   (`integer, integer[]`; mode words kept for INOUT/VARIADIC).
//! - `function_result`: `integer`, `SETOF integer`, `TABLE(a integer, b text)` (for TABLE params),
//!   `record`; procedures -> None (NULL).
//! - `functiondef`: `pg_get_functiondef` full statement:
//!   `CREATE OR REPLACE FUNCTION zz_cat.f(a integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$select a$function$\n`
//!   with clauses in PG order (RETURNS, LANGUAGE, WINDOW (no), IMMUTABLE|STABLE (VOLATILE omitted),
//!   STRICT, SECURITY DEFINER, LEAKPROOF, PARALLEL SAFE|RESTRICTED (UNSAFE omitted), COST n (when
//!   not default 100/1), ROWS n (SRFs when not 1000), `SET name TO 'value'` lines), body dollar-quoted
//!   with the tag `$function$` (if the body itself contains that tag PG tries `$function$` then appends
//!   a digit suffix tag: `$function1$`...). Procedures (oracle): `CREATE OR REPLACE PROCEDURE
//!   zz_cat.pr(IN x integer)\n LANGUAGE sql\nAS $procedure$select 1$procedure$\n` - no RETURNS, the IN
//!   mode word IS printed for procedure parameters, body tag `$procedure$`. The name is always
//!   schema-qualified. Output ends with a newline; plpgsql/stable example: `CREATE OR REPLACE
//!   FUNCTION zz_cat.g(a integer)\n RETURNS TABLE(x integer, y text)\n LANGUAGE plpgsql\n STABLE\nAS
//!   $function$begin ... end$function$\n`.
//!
//! ## Misc
//! - `serial_sequence(table, column)`: `pg_get_serial_sequence`: the sequence OWNED BY (table, column)
//!   (serial or identity) as `schema.seq` text quoted per `quote_qualified_identifier`; NULL if none.
//! - `viewdef`: `ViewDef::query.sql` verbatim for user views/matviews; system views: the stored
//!   `builtin::system_view_def`. Non-views -> None (`pg_get_viewdef` returns NULL? PG raises
//!   `"t" is not a view` 42809 for tables: return `Err(errors::not_a(name, "view"))`).
use crate::builtin;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{builtin_proc, ProcKind};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names::format_type;
use pgarbor_types::text::format::quote_identifier;
use std::sync::OnceLock;

// ---------------------------------------------------------------- small text helpers

/// PG `simple_quote_literal`: single quotes, doubling `'` and `\`.
fn simple_quote_literal(out: &mut String, s: &str) {
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
}

/// PG `quote_qualified_identifier`.
fn quote_qualified(nsp: &str, name: &str) -> Result<String> {
    Ok(format!("{}.{}", quote_identifier(nsp)?, quote_identifier(name)?))
}

/// PG `flatten_reloptions` over `name=value` strings.
fn flatten_reloptions(opts: &[String]) -> Result<Option<String>> {
    if opts.is_empty() {
        return Ok(None);
    }
    let mut s = String::new();
    for (i, o) in opts.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let (name, value) = o.split_once('=').unwrap_or((o.as_str(), ""));
        s.push_str(&quote_identifier(name)?);
        s.push('=');
        simple_quote_literal(&mut s, value);
    }
    Ok(Some(s))
}

/// Text-level `looks_like_function`: `name(...)` where the first `(` is closed by the final `)`.
fn looks_like_function(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
        i += 1;
    }
    if i == 0 || i >= b.len() || b[i] != b'(' || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return false;
    }
    let mut depth = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut j = i;
    while j < b.len() {
        let c = b[j];
        if in_sq {
            if c == b'\'' {
                if b.get(j + 1) == Some(&b'\'') {
                    j += 1;
                } else {
                    in_sq = false;
                }
            }
        } else if in_dq {
            if c == b'"' {
                if b.get(j + 1) == Some(&b'"') {
                    j += 1;
                } else {
                    in_dq = false;
                }
            }
        } else {
            match c {
                b'\'' => in_sq = true,
                b'"' => in_dq = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return j == b.len() - 1;
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    false
}

/// Split a comma-separated expression list at top level (outside quotes, parentheses, brackets).
fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    if s.trim().is_empty() {
        return out;
    }
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut start = 0usize;
    let mut j = 0usize;
    while j < b.len() {
        let c = b[j];
        if in_sq {
            if c == b'\'' {
                if b.get(j + 1) == Some(&b'\'') {
                    j += 1;
                } else {
                    in_sq = false;
                }
            }
        } else if in_dq {
            if c == b'"' {
                if b.get(j + 1) == Some(&b'"') {
                    j += 1;
                } else {
                    in_dq = false;
                }
            }
        } else {
            match c {
                b'\'' => in_sq = true,
                b'"' => in_dq = true,
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b',' if depth == 0 => {
                    out.push(s[start..j].trim().to_string());
                    start = j + 1;
                }
                _ => {}
            }
        }
        j += 1;
    }
    out.push(s[start..].trim().to_string());
    out
}

/// C `printf("%g")` of a double (6 significant digits).
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() };
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let sci = format!("{:.5e}", v);
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        let mut m = mant.to_string();
        if m.contains('.') {
            while m.ends_with('0') {
                m.pop();
            }
            if m.ends_with('.') {
                m.pop();
            }
        }
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (5 - exp).max(0) as usize;
        let mut f = format!("{:.*}", decimals, v);
        if f.contains('.') {
            while f.ends_with('0') {
                f.pop();
            }
            if f.ends_with('.') {
                f.pop();
            }
        }
        f
    }
}

// ---------------------------------------------------------------- catalog lookups

fn namespace_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    if let Some(n) = env.snapshot().namespace(oid) {
        return Ok(n.name.clone());
    }
    if let Some(n) = builtin::builtin_namespace(oid) {
        return Ok(n.name.to_string());
    }
    Err(errors::cache_lookup_failed("schema", oid))
}

/// PG `generate_qualified_relation_name`: always schema-qualified.
fn qualified_relation_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    match env.snapshot().class_identity(oid) {
        Some((ns, name, _)) => quote_qualified(&namespace_name(env, ns)?, name),
        None => Err(errors::cache_lookup_failed("relation", oid)),
    }
}

/// Current name of column `attnum` of `table` (PG `get_attname`).
fn column_name(env: &CatalogEnv, table: Oid, attnum: i16) -> Result<String> {
    env.snapshot()
        .relation(table)
        .and_then(|r| r.column(attnum))
        .filter(|c| !c.dropped)
        .map(|c| c.name.clone())
        .ok_or_else(|| PgError::internal(format!("cache lookup failed for attribute {attnum} of relation {table}")))
}

/// PG `decompile_column_index_array`.
fn decompile_columns(env: &CatalogEnv, table: Oid, cols: &[i16], with_period: bool, out: &mut String) -> Result<()> {
    for (j, a) in cols.iter().enumerate() {
        let name = quote_identifier(&column_name(env, table, *a)?)?;
        if j == 0 {
            out.push_str(&name);
        } else {
            out.push_str(", ");
            if with_period && j == cols.len() - 1 {
                out.push_str("PERIOD ");
            }
            out.push_str(&name);
        }
    }
    Ok(())
}

struct OpcRow {
    oid: Oid,
    method: Oid,
    name: String,
    namespace: Oid,
    intype: Oid,
    default: bool,
}

fn opclass_rows() -> &'static [OpcRow] {
    static ROWS: OnceLock<Vec<OpcRow>> = OnceLock::new();
    ROWS.get_or_init(|| {
        builtin::OPCLASSES
            .rows
            .iter()
            .filter_map(|r| {
                let get = |i: usize| r.get(i).copied().flatten();
                Some(OpcRow {
                    oid: get(0)?.parse().ok()?,
                    method: get(1)?.parse().ok()?,
                    name: get(2)?.to_string(),
                    namespace: get(3)?.parse().ok()?,
                    intype: get(6)?.parse().ok()?,
                    default: get(7)? == "t",
                })
            })
            .collect()
    })
}

/// PG `GetDefaultOpClass` over the builtin operator classes.
fn default_opclass(env: &CatalogEnv, type_oid: Oid, method: Oid) -> Option<Oid> {
    let mut t = type_oid;
    let mut info = env.type_info(t);
    for _ in 0..8 {
        let next = match &info {
            Some(i) if i.typtype == b'd' && i.basetype != 0 => i.basetype,
            _ => break,
        };
        t = next;
        info = env.type_info(t);
    }
    let rows = opclass_rows();
    let find = |intype: Oid| rows.iter().find(|r| r.method == method && r.default && r.intype == intype).map(|r| r.oid);
    if let Some(o) = find(t) {
        return Some(o);
    }
    // binary-coercible input types
    let coerced = match t {
        1043 => Some(25), // varchar -> text
        650 => Some(869), // cidr -> inet
        _ => None,
    };
    if let Some(c) = coerced {
        if let Some(o) = find(c) {
            return Some(o);
        }
    }
    // polymorphic default classes
    let info = info?;
    let poly: Oid = if info.category == b'A' {
        2277 // anyarray
    } else {
        match info.typtype {
            b'e' => 3500, // anyenum
            b'c' => 2249, // record
            b'r' => 3831, // anyrange
            b'm' => 4537, // anymultirange
            _ => return None,
        }
    };
    find(poly)
}

/// PG `get_opclass_name`: ` <opclass>` only when not the default for `keytype`.
fn append_opclass(env: &CatalogEnv, opclass: Oid, keytype: Oid, buf: &mut String) -> Result<()> {
    let row = opclass_rows()
        .iter()
        .find(|r| r.oid == opclass)
        .ok_or_else(|| errors::cache_lookup_failed("opclass", opclass))?;
    if default_opclass(env, keytype, row.method) == Some(opclass) {
        return Ok(());
    }
    buf.push(' ');
    let visible = row.namespace == builtin::PG_CATALOG_NAMESPACE || env.search_path().contains(&row.namespace);
    if !visible {
        buf.push_str(&quote_identifier(&namespace_name(env, row.namespace)?)?);
        buf.push('.');
    }
    buf.push_str(&quote_identifier(&row.name)?);
    Ok(())
}

/// PG `generate_collation_name`.
fn collation_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    let c = builtin::builtin_collation(oid).ok_or_else(|| errors::cache_lookup_failed("collation", oid))?;
    let q = quote_identifier(c.name)?;
    if c.namespace == builtin::PG_CATALOG_NAMESPACE || env.search_path().contains(&c.namespace) {
        Ok(q)
    } else {
        Ok(format!("{}.{}", quote_identifier(&namespace_name(env, c.namespace)?)?, q))
    }
}

// ---------------------------------------------------------------- indexes

fn build_indexdef(env: &CatalogEnv, idx: &Index, colno: usize, pretty: bool) -> Result<String> {
    let snap = env.snapshot();
    let rel = snap.relation(idx.table);
    let attrs_only = colno != 0;
    let nkey = idx.n_key_columns as usize;
    let mut buf = String::new();

    if !attrs_only {
        let tbl = if pretty {
            env.qualified_class_name(idx.table)?
        } else {
            qualified_relation_name(env, idx.table)?
        };
        buf.push_str("CREATE ");
        if idx.unique {
            buf.push_str("UNIQUE ");
        }
        buf.push_str("INDEX ");
        buf.push_str(&quote_identifier(&idx.name)?);
        buf.push_str(" ON ");
        buf.push_str(&tbl);
        buf.push_str(" USING ");
        buf.push_str(&quote_identifier(idx.method.name())?);
        buf.push_str(" (");
    }

    let mut sep = "";
    for (keyno, key) in idx.keys.iter().enumerate() {
        if colno == 0 && keyno == nkey {
            buf.push_str(") INCLUDE (");
            sep = "";
        }
        if colno == 0 {
            buf.push_str(sep);
        }
        sep = ", ";
        let wanted = colno == 0 || colno == keyno + 1;

        let keycoltype: Oid;
        let keycolcollation: Oid;
        match key.column {
            Some(attnum) if attnum != 0 => {
                let col = rel.and_then(|r| r.column(attnum)).filter(|c| !c.dropped);
                let name = match col {
                    Some(c) => c.name.as_str(),
                    None => key.name.as_str(),
                };
                if wanted {
                    buf.push_str(&quote_identifier(name)?);
                }
                match col {
                    Some(c) => {
                        keycoltype = c.type_id.oid;
                        keycolcollation = c.collation;
                    }
                    None => {
                        keycoltype = key.type_id.oid;
                        keycolcollation = key.collation;
                    }
                }
            }
            _ => {
                let e = key
                    .expr
                    .as_ref()
                    .ok_or_else(|| PgError::internal("too few entries in indexprs list"))?;
                if wanted {
                    if looks_like_function(&e.sql) {
                        buf.push_str(&e.sql);
                    } else {
                        buf.push('(');
                        buf.push_str(&e.sql);
                        buf.push(')');
                    }
                }
                keycoltype = key.type_id.oid;
                keycolcollation = env.type_info(keycoltype).map(|i| i.collation).unwrap_or(0);
            }
        }

        if !attrs_only && keyno < nkey && wanted {
            if key.collation != 0 && key.collation != keycolcollation {
                buf.push_str(" COLLATE ");
                buf.push_str(&collation_name(env, key.collation)?);
            }
            append_opclass(env, key.opclass, keycoltype, &mut buf)?;
            if idx.method == IndexMethod::Btree {
                if key.descending {
                    buf.push_str(" DESC");
                    if !key.nulls_first {
                        buf.push_str(" NULLS LAST");
                    }
                } else if key.nulls_first {
                    buf.push_str(" NULLS FIRST");
                }
            }
        }
    }

    if !attrs_only {
        buf.push(')');
        if idx.nulls_not_distinct {
            buf.push_str(" NULLS NOT DISTINCT");
        }
        if let Some(o) = flatten_reloptions(&idx.options)? {
            buf.push_str(" WITH (");
            buf.push_str(&o);
            buf.push(')');
        }
        if let Some(p) = &idx.predicate {
            buf.push_str(" WHERE ");
            buf.push_str(&p.sql);
        }
    }
    Ok(buf)
}

/// `pg_get_indexdef` (module docs). `Ok(None)` for an unknown OID.
// CONTRACT
pub fn indexdef(env: &CatalogEnv, index: Oid, column: Option<usize>, pretty: bool) -> Result<Option<String>> {
    match env.snapshot().index(index) {
        Some(idx) => build_indexdef(env, idx, column.unwrap_or(0), pretty).map(Some),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------- constraints

/// `pg_get_constraintdef`. `Ok(None)` for an unknown OID.
// CONTRACT
#[allow(unused_variables)]
pub fn constraintdef(env: &CatalogEnv, constraint: Oid, pretty: bool) -> Result<Option<String>> {
    let snap = env.snapshot();
    let Some(c) = snap.constraint(constraint) else {
        return Ok(None);
    };
    let mut buf = String::new();
    match c.kind {
        ConstraintKind::ForeignKey => {
            let fk = c
                .fk
                .as_ref()
                .ok_or_else(|| errors::cache_lookup_failed("foreign key constraint", c.oid))?;
            buf.push_str("FOREIGN KEY (");
            decompile_columns(env, c.table, &c.columns, c.period, &mut buf)?;
            buf.push_str(") REFERENCES ");
            buf.push_str(&env.qualified_class_name(fk.ref_table)?);
            buf.push('(');
            decompile_columns(env, fk.ref_table, &fk.ref_columns, c.period, &mut buf)?;
            buf.push(')');
            match fk.match_type {
                FkMatch::Full => buf.push_str(" MATCH FULL"),
                FkMatch::Partial => buf.push_str(" MATCH PARTIAL"),
                FkMatch::Simple => {}
            }
            if fk.on_update != FkAction::NoAction {
                buf.push_str(" ON UPDATE ");
                buf.push_str(fk.on_update.sql());
            }
            if fk.on_delete != FkAction::NoAction {
                buf.push_str(" ON DELETE ");
                buf.push_str(fk.on_delete.sql());
            }
            if !fk.delete_set_columns.is_empty() {
                buf.push_str(" (");
                decompile_columns(env, c.table, &fk.delete_set_columns, false, &mut buf)?;
                buf.push(')');
            }
        }
        ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
            buf.push_str(if c.kind == ConstraintKind::PrimaryKey { "PRIMARY KEY " } else { "UNIQUE " });
            let idx = snap.index(c.index).ok_or_else(|| errors::cache_lookup_failed("index", c.index))?;
            if c.kind == ConstraintKind::Unique && idx.nulls_not_distinct {
                buf.push_str("NULLS NOT DISTINCT ");
            }
            buf.push('(');
            decompile_columns(env, c.table, &c.columns, false, &mut buf)?;
            if c.period {
                buf.push_str(" WITHOUT OVERLAPS");
            }
            buf.push(')');
            let keyatts = c.columns.len();
            if idx.keys.len() > keyatts {
                buf.push_str(" INCLUDE (");
                for (j, k) in idx.keys.iter().enumerate().skip(keyatts) {
                    let attnum = k.column.unwrap_or(0);
                    if j > keyatts {
                        buf.push_str(", ");
                    }
                    buf.push_str(&quote_identifier(&column_name(env, c.table, attnum)?)?);
                }
                buf.push(')');
            }
            // pg_get_constraintdef (fullCommand = false) never prints the index options/tablespace.
        }
        ConstraintKind::Check => {
            let e = c
                .check
                .as_ref()
                .ok_or_else(|| errors::cache_lookup_failed("check constraint", c.oid))?;
            buf.push_str("CHECK (");
            buf.push_str(&e.sql);
            buf.push(')');
            if c.no_inherit {
                buf.push_str(" NO INHERIT");
            }
        }
        ConstraintKind::NotNull => {
            if c.table != 0 {
                let attnum = c
                    .columns
                    .first()
                    .copied()
                    .ok_or_else(|| errors::cache_lookup_failed("not-null constraint", c.oid))?;
                buf.push_str("NOT NULL ");
                buf.push_str(&quote_identifier(&column_name(env, c.table, attnum)?)?);
                if c.no_inherit {
                    buf.push_str(" NO INHERIT");
                }
            } else if c.domain != 0 {
                buf.push_str("NOT NULL");
            }
        }
        ConstraintKind::Trigger => buf.push_str("TRIGGER"),
        ConstraintKind::Exclusion => {
            // Exclusion constraints cannot be created in PGArbor v1 (no operator list in the model).
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "exclusion constraints are not supported".to_string(),
            ));
        }
    }
    if c.deferrable {
        buf.push_str(" DEFERRABLE");
    }
    if c.initially_deferred {
        buf.push_str(" INITIALLY DEFERRED");
    }
    if !c.enforced {
        buf.push_str(" NOT ENFORCED");
    } else if !c.validated {
        buf.push_str(" NOT VALID");
    }
    Ok(Some(buf))
}

/// `pg_get_viewdef(oid)`.
// CONTRACT
#[allow(unused_variables)]
pub fn viewdef(env: &CatalogEnv, rel: Oid, pretty: bool) -> Result<Option<String>> {
    let snap = env.snapshot();
    if let Some(r) = snap.relation(rel) {
        if let Some(v) = &r.view {
            return Ok(Some(v.query.sql.clone()));
        }
        if let Some(def) = builtin::system_view_def(rel) {
            return Ok(Some(def.to_string()));
        }
        return Err(errors::not_a(&r.name, "view"));
    }
    match snap.class_identity(rel) {
        Some((_, name, _)) => Err(errors::not_a(name, "view")),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------- functions

struct ArgView {
    name: String,
    ty: Oid,
    mode: ArgMode,
    default: Option<String>,
}

struct ProcView {
    kind: FuncKind,
    args: Vec<ArgView>,
    ret: Oid,
    retset: bool,
}

fn proc_view(env: &CatalogEnv, func: Oid) -> Option<ProcView> {
    if let Some(f) = env.snapshot().function(func) {
        return Some(ProcView {
            kind: f.kind,
            args: f
                .args
                .iter()
                .map(|a| ArgView {
                    name: a.name.clone(),
                    ty: a.type_oid,
                    mode: a.mode,
                    default: a.default.as_ref().map(|d| d.sql.clone()),
                })
                .collect(),
            ret: f.ret,
            retset: f.retset,
        });
    }
    let p = builtin_proc(func)?;
    let types = if p.all_arg_types.is_empty() { p.arg_types } else { p.all_arg_types };
    let mut args: Vec<ArgView> = types
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mode = if p.arg_modes.is_empty() {
                ArgMode::In
            } else {
                match p.arg_modes.get(i).copied().unwrap_or(b'i') {
                    b'o' => ArgMode::Out,
                    b'b' => ArgMode::InOut,
                    b'v' => ArgMode::Variadic,
                    b't' => ArgMode::Table,
                    _ => ArgMode::In,
                }
            };
            ArgView { name: p.arg_names.get(i).copied().unwrap_or("").to_string(), ty: *t, mode, default: None }
        })
        .collect();
    let defaults = split_top_level(p.arg_defaults);
    if !defaults.is_empty() {
        let input_idx: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a.mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|(i, _)| i)
            .collect();
        let skip = input_idx.len().saturating_sub(defaults.len());
        for (slot, d) in input_idx.iter().skip(skip).zip(defaults) {
            args[*slot].default = Some(d);
        }
    }
    let kind = match p.kind {
        ProcKind::Function => FuncKind::Function,
        ProcKind::Aggregate => FuncKind::Aggregate,
        ProcKind::Window => FuncKind::Window,
        ProcKind::Procedure => FuncKind::Procedure,
    };
    Some(ProcView { kind, args, ret: p.ret, retset: p.retset })
}

/// PG `print_function_arguments`; returns the number of arguments printed.
fn print_function_arguments(
    env: &CatalogEnv,
    pv: &ProcView,
    print_table_args: bool,
    print_defaults: bool,
    buf: &mut String,
) -> Result<usize> {
    let mut printed = 0usize;
    for a in &pv.args {
        let (modename, isinput) = match a.mode {
            ArgMode::In => (if pv.kind == FuncKind::Procedure { "IN " } else { "" }, true),
            ArgMode::InOut => ("INOUT ", true),
            ArgMode::Out => ("OUT ", false),
            ArgMode::Variadic => ("VARIADIC ", true),
            ArgMode::Table => ("", false),
        };
        if print_table_args != (a.mode == ArgMode::Table) {
            continue;
        }
        if printed > 0 {
            buf.push_str(", ");
        }
        buf.push_str(modename);
        if !a.name.is_empty() {
            buf.push_str(&quote_identifier(&a.name)?);
            buf.push(' ');
        }
        buf.push_str(&format_type(env, a.ty, -1)?);
        if print_defaults && isinput {
            if let Some(d) = &a.default {
                buf.push_str(" DEFAULT ");
                buf.push_str(d);
            }
        }
        printed += 1;
    }
    Ok(printed)
}

/// PG `print_function_rettype`.
fn print_function_rettype(env: &CatalogEnv, pv: &ProcView) -> Result<String> {
    let mut r = String::new();
    let mut ntab = 0;
    if pv.retset {
        r.push_str("TABLE(");
        ntab = print_function_arguments(env, pv, true, false, &mut r)?;
        if ntab > 0 {
            r.push(')');
        } else {
            r.clear();
        }
    }
    if ntab == 0 {
        if pv.retset {
            r.push_str("SETOF ");
        }
        r.push_str(&format_type(env, pv.ret, -1)?);
    }
    Ok(r)
}

/// `pg_get_function_arguments` for a user or builtin function OID.
// CONTRACT
pub fn function_arguments(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    let Some(pv) = proc_view(env, func) else {
        return Ok(None);
    };
    let mut buf = String::new();
    print_function_arguments(env, &pv, false, true, &mut buf)?;
    Ok(Some(buf))
}

/// `pg_get_function_identity_arguments`.
// CONTRACT
pub fn function_identity_arguments(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    let Some(pv) = proc_view(env, func) else {
        return Ok(None);
    };
    let mut buf = String::new();
    print_function_arguments(env, &pv, false, false, &mut buf)?;
    Ok(Some(buf))
}

/// `pg_get_function_result` (None for procedures and unknown OIDs).
// CONTRACT
pub fn function_result(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    let Some(pv) = proc_view(env, func) else {
        return Ok(None);
    };
    if pv.kind == FuncKind::Procedure {
        return Ok(None);
    }
    print_function_rettype(env, &pv).map(Some)
}

/// GUCs flagged `GUC_LIST_QUOTE` (their proconfig value is a list of identifiers/strings).
fn guc_is_list_quote(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "search_path",
        "datestyle",
        "temp_tablespaces",
        "local_preload_libraries",
        "session_preload_libraries",
        "shared_preload_libraries",
        "synchronous_standby_names",
        "debug_io_direct",
        "unix_socket_directories",
        "listen_addresses",
        "oauth_validator_libraries",
    ];
    NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// PG `SplitGUCList` (comma separated, optional double-quoted elements, whitespace trimmed).
fn split_guc_list(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    loop {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut cur = String::new();
        if chars[i] == '"' {
            i += 1;
            loop {
                if i >= n {
                    return None;
                }
                if chars[i] == '"' {
                    if i + 1 < n && chars[i + 1] == '"' {
                        cur.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                cur.push(chars[i]);
                i += 1;
            }
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
        } else {
            while i < n && chars[i] != ',' {
                cur.push(chars[i]);
                i += 1;
            }
            cur = cur.trim_end().to_string();
        }
        out.push(cur);
        if i >= n {
            break;
        }
        if chars[i] != ',' {
            return None;
        }
        i += 1;
        if i >= n {
            return None;
        }
    }
    Some(out)
}

/// `pg_get_functiondef` (user functions; builtin -> 42809 `"x" is an aggregate function`-style
/// errors are `internal`: builtin functions have no source: `Err(errors::cache_lookup_failed)`).
// CONTRACT
pub fn functiondef(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    let snap = env.snapshot();
    let Some(f) = snap.function(func) else {
        if builtin_proc(func).is_some() {
            return Err(errors::cache_lookup_failed("function", func));
        }
        return Ok(None);
    };
    if f.kind == FuncKind::Aggregate {
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is an aggregate function", f.name)));
    }
    let pv = proc_view(env, func).ok_or_else(|| errors::cache_lookup_failed("function", func))?;
    let isfunction = f.kind != FuncKind::Procedure;
    let mut buf = String::new();

    buf.push_str("CREATE OR REPLACE ");
    buf.push_str(if isfunction { "FUNCTION " } else { "PROCEDURE " });
    buf.push_str(&quote_qualified(&namespace_name(env, f.namespace)?, &f.name)?);
    buf.push('(');
    print_function_arguments(env, &pv, false, true, &mut buf)?;
    buf.push_str(")\n");
    if isfunction {
        buf.push_str(" RETURNS ");
        buf.push_str(&print_function_rettype(env, &pv)?);
        buf.push('\n');
    }

    let lang = builtin::LANGUAGES
        .iter()
        .find(|l| l.oid == f.language)
        .ok_or_else(|| errors::cache_lookup_failed("language", f.language))?;
    buf.push_str(" LANGUAGE ");
    buf.push_str(&quote_identifier(lang.name)?);
    buf.push('\n');

    let oldlen = buf.len();
    if f.kind == FuncKind::Window {
        buf.push_str(" WINDOW");
    }
    match f.volatility {
        FuncVolatility::Immutable => buf.push_str(" IMMUTABLE"),
        FuncVolatility::Stable => buf.push_str(" STABLE"),
        FuncVolatility::Volatile => {}
    }
    match f.parallel {
        b's' => buf.push_str(" PARALLEL SAFE"),
        b'r' => buf.push_str(" PARALLEL RESTRICTED"),
        _ => {}
    }
    if f.strict {
        buf.push_str(" STRICT");
    }
    if f.security_definer {
        buf.push_str(" SECURITY DEFINER");
    }
    if f.leakproof {
        buf.push_str(" LEAKPROOF");
    }
    let default_cost: f32 = if f.language == builtin::LANG_INTERNAL || f.language == builtin::LANG_C { 1.0 } else { 100.0 };
    if f.cost != default_cost {
        buf.push_str(" COST ");
        buf.push_str(&fmt_g(f.cost as f64));
    }
    if f.retset && f.rows > 0.0 && f.rows != 1000.0 {
        buf.push_str(" ROWS ");
        buf.push_str(&fmt_g(f.rows as f64));
    }
    if buf.len() != oldlen {
        buf.push('\n');
    }

    for item in &f.config {
        let Some((name, value)) = item.split_once('=') else {
            continue;
        };
        buf.push_str(" SET ");
        buf.push_str(&quote_identifier(name)?);
        buf.push_str(" TO ");
        if guc_is_list_quote(name) {
            let list = split_guc_list(value)
                .ok_or_else(|| PgError::internal("invalid list syntax in proconfig item"))?;
            for (i, el) in list.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                simple_quote_literal(&mut buf, el);
            }
        } else {
            simple_quote_literal(&mut buf, value);
        }
        buf.push('\n');
    }

    match (&f.sql_body, f.language == builtin::LANG_SQL) {
        (Some(b), true) => buf.push_str(&b.sql),
        _ => {
            buf.push_str("AS ");
            let mut dq = String::from(if isfunction { "$function" } else { "$procedure" });
            while f.body.contains(dq.as_str()) {
                dq.push('x');
            }
            dq.push('$');
            buf.push_str(&dq);
            buf.push_str(&f.body);
            buf.push_str(&dq);
        }
    }
    buf.push('\n');
    Ok(Some(buf))
}

// ---------------------------------------------------------------- misc

/// `pg_get_serial_sequence`: the sequence owned by `table.column`.
// CONTRACT
pub fn serial_sequence(env: &CatalogEnv, table: Oid, column: &str) -> Result<Option<String>> {
    let snap = env.snapshot();
    let rel = snap.relation(table).ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
    let col = rel
        .column_by_name(column)
        .ok_or_else(|| errors::column_of_relation_does_not_exist(column, &rel.name))?;
    match snap.sequences_owned_by(table, col.attnum).first() {
        Some(seq) => Ok(Some(quote_qualified(&namespace_name(env, seq.namespace)?, &seq.name)?)),
        None => Ok(None),
    }
}

/// Index access method name (`btree`) and key/option text helpers are shared with the virtual
/// `pg_indexes` view: the `indexdef` text for an `Index` value that is not (yet) in the snapshot
/// (used by `diff.rs` and tests).
// CONTRACT
pub fn indexdef_for(env: &CatalogEnv, index: &Index) -> Result<String> {
    build_indexdef(env, index, 0, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_detection() {
        assert!(looks_like_function("lower(b)"));
        assert!(looks_like_function("pg_catalog.lower(b)"));
        assert!(looks_like_function("coalesce(a, ')')"));
        assert!(!looks_like_function("(a + 1)"));
        assert!(!looks_like_function("lower(a) || lower(b)"));
        assert!(!looks_like_function("lower(a)::text"));
        assert!(!looks_like_function("a"));
    }

    #[test]
    fn printf_g() {
        assert_eq!(fmt_g(100.0), "100");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1000000.0), "1e+06");
        assert_eq!(fmt_g(123456.0), "123456");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(0.00001), "1e-05");
        assert_eq!(fmt_g(2.5), "2.5");
    }

    #[test]
    fn reloptions_and_literals() {
        let o = vec!["fillfactor=50".to_string(), "deduplicate_items=off".to_string()];
        assert_eq!(flatten_reloptions(&o).unwrap().unwrap(), "fillfactor='50', deduplicate_items='off'");
        assert!(flatten_reloptions(&[]).unwrap().is_none());
        let mut s = String::new();
        simple_quote_literal(&mut s, "a'b\\c");
        assert_eq!(s, "'a''b\\\\c'");
    }

    #[test]
    fn splitting() {
        assert_eq!(split_top_level("1, 'a,b', f(1,2)"), vec!["1", "'a,b'", "f(1,2)"]);
        assert!(split_top_level("").is_empty());
        assert_eq!(split_guc_list("public, \"My Schema\"").unwrap(), vec!["public", "My Schema"]);
        assert_eq!(split_guc_list("a").unwrap(), vec!["a"]);
        assert!(split_guc_list("\"a").is_none());
    }

    #[test]
    fn default_opclasses() {
        let env = CatalogEnv::with_default_path(crate::snapshot::CatalogSnapshot::builtin_only().unwrap());
        // int4_ops (1978) is the default btree class for integer; text_pattern_ops is not for text.
        assert_eq!(default_opclass(&env, 23, 403), Some(1978));
        assert_eq!(default_opclass(&env, 1043, 403), default_opclass(&env, 25, 403));
        let mut s = String::new();
        append_opclass(&env, 1978, 23, &mut s).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn function_definition_text() {
        let mut data = crate::data::CatalogData::default();
        data.insert(CatalogObject::Namespace(Namespace { oid: 50000, name: "zz_cat".into(), owner: 10, acl: None }));
        data.insert(CatalogObject::Function(Function {
            oid: 50001,
            name: "f".into(),
            namespace: 50000,
            owner: 10,
            kind: FuncKind::Function,
            language: builtin::LANG_SQL,
            args: vec![FuncArg { name: "a".into(), type_oid: 23, mode: ArgMode::In, default: None }],
            ret: 23,
            retset: false,
            volatility: FuncVolatility::Volatile,
            strict: false,
            security_definer: false,
            leakproof: false,
            parallel: b'u',
            cost: 100.0,
            rows: 0.0,
            body: "select a".into(),
            sql_body: None,
            config: Vec::new(),
            acl: None,
            depends_on: Vec::new(),
        }));
        data.insert(CatalogObject::Function(Function {
            oid: 50002,
            name: "pr".into(),
            namespace: 50000,
            owner: 10,
            kind: FuncKind::Procedure,
            language: builtin::LANG_SQL,
            args: vec![FuncArg { name: "x".into(), type_oid: 23, mode: ArgMode::In, default: None }],
            ret: 2278,
            retset: false,
            volatility: FuncVolatility::Volatile,
            strict: false,
            security_definer: false,
            leakproof: false,
            parallel: b'u',
            cost: 100.0,
            rows: 0.0,
            body: "select 1".into(),
            sql_body: None,
            config: Vec::new(),
            acl: None,
            depends_on: Vec::new(),
        }));
        let snap = crate::snapshot::CatalogSnapshot::build(crate::persist::CatalogVersion::ZERO, data).unwrap();
        let env = CatalogEnv::with_default_path(snap);
        assert_eq!(
            functiondef(&env, 50001).unwrap().unwrap(),
            "CREATE OR REPLACE FUNCTION zz_cat.f(a integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$select a$function$\n"
        );
        assert_eq!(
            functiondef(&env, 50002).unwrap().unwrap(),
            "CREATE OR REPLACE PROCEDURE zz_cat.pr(IN x integer)\n LANGUAGE sql\nAS $procedure$select 1$procedure$\n"
        );
        assert_eq!(function_result(&env, 50002).unwrap(), None);
        assert_eq!(function_result(&env, 50001).unwrap().as_deref(), Some("integer"));
        assert_eq!(function_arguments(&env, 50001).unwrap().as_deref(), Some("a integer"));
        assert_eq!(functiondef(&env, 99999999).unwrap(), None);
    }
}
