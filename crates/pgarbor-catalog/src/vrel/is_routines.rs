//! information_schema, part 4 (routines and dependencies): `routines`, `parameters`,
//! `routine_column_usage`, `routine_routine_usage`, `routine_sequence_usage`, `routine_table_usage`,
//! `view_column_usage`, `view_table_usage`, `view_routine_usage`, `check_constraint_routine_usage`.
//! Lease CAT-43. Authoritative semantics: the CREATE VIEW of each name in PG 18.6
//! `information_schema.sql`; conventions and visibility: `is_core.rs`; type columns:
//! `is_columns::describe_type_for_info_schema`.
//!
//! - **routines**: user functions/procedures AND every builtin `pg_proc` row (oracle: 3413 rows,
//!   `WHERE pg_has_role(proowner,'USAGE') OR has_function_privilege(oid,'EXECUTE')`; PUBLIC has
//!   EXECUTE by default). Includes aggregates/window functions (the view has no `prokind` filter; only
//!   `routine_type` is NULL for them: `CASE prokind WHEN 'f' THEN 'FUNCTION' WHEN 'p' THEN 'PROCEDURE'`).
//!   Oracle values for `create function f(x int, y text default 'k') returns int language sql immutable
//!   strict as 'select 1'`: `specific_name` `f_<oid>` (name + `_` + pg_proc OID), `routine_name` `f`,
//!   `routine_type` `FUNCTION`, `data_type` `integer` (NULL for procedures), type-detail columns of the
//!   return type EXCEPT that `numeric_precision`, `character_maximum_length`, ... are NULL and only
//!   `type_udt_catalog/schema/name` (`postgres`/`pg_catalog`/`int4`) are filled, `dtd_identifier` `0`
//!   (NULL for procedures), `routine_body` `SQL` for language sql else `EXTERNAL`, `routine_definition`
//!   = `prosrc` for `sql`/`plpgsql` (NULL for builtin internal functions? oracle: builtin rows have
//!   `routine_definition` non-NULL = prosrc), `external_name` NULL (user SQL) / `prosrc` for non-SQL
//!   non-plpgsql, `external_language` = `upper(lanname)` (`SQL`, `PLPGSQL`, `INTERNAL`, `C`),
//!   `parameter_style` `GENERAL`, `is_deterministic` `YES` iff `provolatile = 'i'`, `sql_data_access`
//!   the constant `MODIFIES`, `is_null_call` `YES`/`NO` from strict (NULL for procedures),
//!   `schema_level_routine` `YES`, `max_dynamic_result_sets` 0, `security_type` `DEFINER`/`INVOKER`,
//!   `as_locator` `NO`, `is_udt_dependent` `NO`, every `result_cast_*` NULL. Port the full CASE
//!   expressions from the view (line ~1546).
//! - **parameters**: one row per declared parameter (`proargnames`/`proargmodes` expanded): `specific_*`,
//!   `ordinal_position`, `parameter_mode` (`IN`, `OUT`, `INOUT`), `is_result` `NO`, `as_locator` `NO`,
//!   `parameter_name` (NULL when unnamed), type columns, `parameter_default` (the default expr text),
//!   `dtd_identifier`. TABLE columns appear as OUT. VARIADIC shows `IN`.
//! - Usage views: `view_table_usage` / `view_column_usage` / `view_routine_usage` read
//!   `Relation::view.query.depends_on` of each view (table/column/function addresses);
//!   `routine_*_usage` and `check_constraint_routine_usage` read `Function::depends_on` and
//!   `Constraint::check.depends_on` for routine addresses (in v1 only SQL-standard `BEGIN ATOMIC`
//!   bodies and check expressions can reference routines; most are empty but MUST be computed, not
//!   assumed empty). `view_*` rows: `view_catalog/schema/name, table_catalog/schema/name[, column_name]`
//!   restricted to objects the current user owns (`pg_has_role(owner, 'USAGE')`).
//!
//! ## Implementation notes (CAT-43)
//! - The `routines` / `parameters` columns follow the PG 18.6 view text, not the wider type
//!   description: `parameters` and `routines` fill only `data_type` (`ARRAY`, `format_type` for
//!   `pg_catalog` types, else `USER-DEFINED`) and the `udt_*` / `type_udt_*` columns; the character,
//!   numeric and datetime detail columns are NULL in those views.
//! - `routine_definition` / `external_name` follow the view: `prosrc` when the owner passes
//!   `pg_has_role(owner,'USAGE')`; `external_name` only for language `c`.
//! - Builtin procs carry no dependency lists in PGArbor, so they contribute no outgoing edges to the
//!   `routine_*_usage` views; they are still resolved as TARGETS (`view_routine_usage`,
//!   `check_constraint_routine_usage`, `routine_routine_usage`).
//! - `has_function_privilege(oid,'EXECUTE')` on a builtin or on a user function without an explicit
//!   ACL is true (the default ACL grants EXECUTE to PUBLIC); with an explicit ACL the `RoleGraph`
//!   privilege mask decides.
//! - `nameconcatoid(name, oid)` clips the NAME part so that the result stays under NAMEDATALEN (64).
//! - All rows are de-duplicated (`SELECT DISTINCT` in the views) and sorted for the usage views.
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::acl::{self, RoleGraph};
use crate::env::CatalogEnv;
use crate::builtin::{self, relid, SystemRel};
use crate::model::common::{acl_mode, Acl};
use crate::model::function::{FuncKind, FuncVolatility, Function};
use crate::snapshot::CatalogSnapshot;
use crate::{ConstraintKind, ObjClass, RelKind};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::{builtin_proc, BuiltinProc, Volatility, PROCS};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names::format_type;
use pgarbor_types::registry::TypeInfo;

/// Producer for `information_schema.routines`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutines;

impl VirtualRelation for IsRoutines {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        // The executor re-applies the WHERE clause, so the equality filters are not needed here.
        let _ = req;
        routines(ctx)
    }
}

/// Producer for `information_schema.parameters`.
#[derive(Clone, Copy, Debug)]
pub struct IsParameters;

impl VirtualRelation for IsParameters {
    fn oid(&self) -> Oid {
        relid::IS_PARAMETERS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        parameters(ctx)
    }
}

/// Producer for `information_schema.routine_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineColumnUsage;

impl VirtualRelation for IsRoutineColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_COLUMN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        routine_column_usage(ctx)
    }
}

/// Producer for `information_schema.routine_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineRoutineUsage;

impl VirtualRelation for IsRoutineRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_ROUTINE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        routine_routine_usage(ctx)
    }
}

/// Producer for `information_schema.routine_sequence_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineSequenceUsage;

impl VirtualRelation for IsRoutineSequenceUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_SEQUENCE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        routine_sequence_usage(ctx)
    }
}

/// Producer for `information_schema.routine_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutineTableUsage;

impl VirtualRelation for IsRoutineTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_TABLE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        routine_table_usage(ctx)
    }
}

/// Producer for `information_schema.view_column_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewColumnUsage;

impl VirtualRelation for IsViewColumnUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_COLUMN_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        view_column_usage(ctx)
    }
}

/// Producer for `information_schema.view_table_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewTableUsage;

impl VirtualRelation for IsViewTableUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_TABLE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        view_table_usage(ctx)
    }
}

/// Producer for `information_schema.view_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsViewRoutineUsage;

impl VirtualRelation for IsViewRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_VIEW_ROUTINE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        view_routine_usage(ctx)
    }
}

/// Producer for `information_schema.check_constraint_routine_usage`.
#[derive(Clone, Copy, Debug)]
pub struct IsCheckConstraintRoutineUsage;

impl VirtualRelation for IsCheckConstraintRoutineUsage {
    fn oid(&self) -> Oid {
        relid::IS_CHECK_CONSTRAINT_ROUTINE_USAGE
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        check_constraint_routine_usage(ctx)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsRoutines,
    &IsParameters,
    &IsRoutineColumnUsage,
    &IsRoutineRoutineUsage,
    &IsRoutineSequenceUsage,
    &IsRoutineTableUsage,
    &IsViewColumnUsage,
    &IsViewTableUsage,
    &IsViewRoutineUsage,
    &IsCheckConstraintRoutineUsage,
];

// ------------------------------------------------------------------------------------------
// Shared machinery
// ------------------------------------------------------------------------------------------

/// A `pg_proc` row: a compiled-in builtin or a user function (both sides of every routine view).
#[derive(Clone, Copy)]
enum ProcRef<'a> {
    Builtin(&'static BuiltinProc),
    User(&'a Function),
}

/// One declared parameter as the `parameters` view expands it (`proallargtypes` element n).
struct ProcArg<'a> {
    type_oid: Oid,
    /// `proargmodes` byte: b'i' IN, b'o' OUT, b'b' INOUT, b'v' VARIADIC, b't' TABLE.
    mode: u8,
    /// `NULLIF(proargnames[n], '')`.
    name: Option<&'a str>,
    /// `pg_get_function_arg_default(oid, n)`: None for OUT/TABLE parameters and parameters without
    /// a default.
    default: Option<&'a str>,
}

impl<'a> ProcRef<'a> {
    fn oid(self) -> Oid {
        match self {
            ProcRef::Builtin(b) => b.oid,
            ProcRef::User(f) => f.oid,
        }
    }

    fn name(self) -> &'a str {
        match self {
            ProcRef::Builtin(b) => b.name,
            ProcRef::User(f) => f.name.as_str(),
        }
    }

    fn namespace(self) -> Oid {
        match self {
            ProcRef::Builtin(b) => b.namespace,
            ProcRef::User(f) => f.namespace,
        }
    }

    /// `proowner`: builtins belong to the bootstrap superuser.
    fn owner(self) -> Oid {
        match self {
            ProcRef::Builtin(_) => builtin::BOOTSTRAP_SUPERUSER,
            ProcRef::User(f) => f.owner,
        }
    }

    /// `prokind` as the single character (`f`, `a`, `w`, `p`).
    fn prokind(self) -> u8 {
        match self {
            ProcRef::Builtin(b) => b.kind.as_char() as u8,
            ProcRef::User(f) => match f.kind {
                FuncKind::Function => b'f',
                FuncKind::Aggregate => b'a',
                FuncKind::Window => b'w',
                FuncKind::Procedure => b'p',
            },
        }
    }

    /// `prolang`.
    fn language(self) -> Oid {
        match self {
            ProcRef::Builtin(b) => b.lang,
            ProcRef::User(f) => f.language,
        }
    }

    /// `prorettype`.
    fn ret(self) -> Oid {
        match self {
            ProcRef::Builtin(b) => b.ret,
            ProcRef::User(f) => f.ret,
        }
    }

    fn strict(self) -> bool {
        match self {
            ProcRef::Builtin(b) => b.strict,
            ProcRef::User(f) => f.strict,
        }
    }

    /// `provolatile = 'i'`.
    fn immutable(self) -> bool {
        match self {
            ProcRef::Builtin(b) => b.volatility == Volatility::Immutable,
            ProcRef::User(f) => f.volatility == FuncVolatility::Immutable,
        }
    }

    fn security_definer(self) -> bool {
        match self {
            ProcRef::Builtin(_) => false,
            ProcRef::User(f) => f.security_definer,
        }
    }

    /// `prosrc`. Builtin SQL-language rows with an empty `prosrc` report their name (as the pg_proc
    /// producer does).
    fn src(self) -> &'a str {
        match self {
            ProcRef::Builtin(b) => {
                if b.prosrc.is_empty() {
                    b.name
                } else {
                    b.prosrc
                }
            }
            ProcRef::User(f) => f.body.as_str(),
        }
    }

    /// `proacl`: None means the default ACL (EXECUTE to PUBLIC).
    fn acl(self) -> Option<&'a Acl> {
        match self {
            ProcRef::Builtin(_) => None,
            ProcRef::User(f) => f.acl.as_ref(),
        }
    }

    /// The parameters in `proallargtypes` order (IN, OUT, INOUT, VARIADIC, TABLE as declared).
    fn args(self) -> Vec<ProcArg<'a>> {
        match self {
            ProcRef::Builtin(b) => builtin_args(b),
            ProcRef::User(f) => f
                .args
                .iter()
                .map(|a| {
                    let mode = a.mode.as_char();
                    ProcArg {
                        type_oid: a.type_oid,
                        mode,
                        name: (!a.name.is_empty()).then_some(a.name.as_str()),
                        default: if is_input(mode) {
                            a.default.as_ref().map(|e| e.sql.as_str())
                        } else {
                            None
                        },
                    }
                })
                .collect(),
        }
    }
}

/// Input parameters (`IN`, `INOUT`, `VARIADIC`) are the ones that can carry defaults.
fn is_input(mode: u8) -> bool {
    matches!(mode, b'i' | b'b' | b'v')
}

/// Builtin parameters: `proallargtypes` (or `proargtypes` when all IN), `proargmodes` (NULL = all
/// IN), `proargnames`, and the trailing defaults of the input parameters.
fn builtin_args(b: &'static BuiltinProc) -> Vec<ProcArg<'static>> {
    let types: &'static [Oid] = if b.all_arg_types.is_empty() { b.arg_types } else { b.all_arg_types };
    let defaults: Vec<&'static str> = split_top_level(b.arg_defaults);
    let nargs = b.nargs as usize;
    let first_default = nargs.saturating_sub(b.ndefaults as usize);
    let mut input_idx = 0usize;
    let mut out = Vec::with_capacity(types.len());
    for (i, &type_oid) in types.iter().enumerate() {
        let mode = if b.arg_modes.is_empty() { b'i' } else { b.arg_modes.get(i).copied().unwrap_or(b'i') };
        let name = b.arg_names.get(i).copied().filter(|s| !s.is_empty());
        let default = if is_input(mode) {
            let k = input_idx;
            input_idx += 1;
            if k >= first_default {
                defaults.get(k - first_default).copied()
            } else {
                None
            }
        } else {
            None
        };
        out.push(ProcArg { type_oid, mode, name, default });
    }
    out
}

/// Split a comma-separated default list at top-level commas (commas inside parentheses, brackets,
/// or quotes do not split).
fn split_top_level(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    for (i, &c) in bytes.iter().enumerate() {
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
        } else if in_double {
            if c == b'"' {
                in_double = false;
            }
        } else {
            match c {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b',' if depth == 0 => {
                    out.push(s[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
    }
    out.push(s[start..].trim());
    out
}

/// PG `nameconcatoid(name, oid)`: `name_<oid>`, clipping the NAME part (never the suffix) so that the
/// result is shorter than NAMEDATALEN (64 bytes). The clip keeps whole UTF-8 characters.
fn name_concat_oid(name: &str, oid: Oid) -> String {
    let suffix = format!("_{oid}");
    if name.len() + suffix.len() < 64 {
        return format!("{name}{suffix}");
    }
    let limit = 63 - suffix.len();
    let mut end = limit.min(name.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &name[..end])
}

/// `routine_type` CASE: `f` FUNCTION, `p` PROCEDURE, anything else NULL.
fn routine_type(prokind: u8) -> Option<&'static str> {
    match prokind {
        b'f' => Some("FUNCTION"),
        b'p' => Some("PROCEDURE"),
        _ => None,
    }
}

/// `parameter_mode` CASE over `proargmodes` bytes.
fn parameter_mode(mode: u8) -> Option<&'static str> {
    match mode {
        b'i' | b'v' => Some("IN"),
        b'o' | b't' => Some("OUT"),
        b'b' => Some("INOUT"),
        _ => None,
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "YES"
    } else {
        "NO"
    }
}

/// `information_schema` system relation by OID (column list for the row builder).
fn rel_of(oid: Oid) -> Result<&'static SystemRel> {
    builtin::system_rel(oid).ok_or_else(|| PgError::internal(format!("no system relation with OID {oid}")))
}

/// Every `pg_proc` row in OID order: builtins (OID below 16384) first, then user functions.
fn all_procs(snap: &CatalogSnapshot) -> impl Iterator<Item = ProcRef<'_>> + '_ {
    PROCS.iter().map(ProcRef::Builtin).chain(snap.data().functions.values().map(ProcRef::User))
}

/// The `pg_proc` row for an OID (builtin or user), if any.
fn resolve_proc(snap: &CatalogSnapshot, oid: Oid) -> Option<ProcRef<'_>> {
    if let Some(b) = builtin_proc(oid) {
        return Some(ProcRef::Builtin(b));
    }
    snap.function(oid).map(ProcRef::User)
}

/// `pg_language` row for a language OID (`builtin::LANGUAGES`).
fn lang_of(oid: Oid) -> Option<&'static builtin::BuiltinLanguage> {
    builtin::LANGUAGES.iter().find(|l| l.oid == oid)
}

/// Relkinds the usage views accept for tables: `r`, `v`, `f`, `p`.
fn is_table_like(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::View | RelKind::ForeignTable | RelKind::PartitionedTable)
}

/// Per-scan context: the env, the role closure of the current user and the current database name.
struct Scan<'a> {
    env: &'a CatalogEnv,
    snap: &'a CatalogSnapshot,
    graph: RoleGraph,
    user: Oid,
    db: &'a str,
}

impl<'a> Scan<'a> {
    fn new(ctx: &ScanContext<'a>) -> Result<Scan<'a>> {
        let env: &'a CatalogEnv = ctx.env;
        let snap: &'a CatalogSnapshot = env.snapshot();
        let db_oid = env.database();
        let db = snap
            .database(db_oid)
            .map(|db_row| db_row.name.as_str())
            .ok_or_else(|| PgError::internal(format!("database {db_oid} is not in the catalog")))?;
        let graph = RoleGraph::build(snap, db_oid)?;
        Ok(Scan { env, snap, graph, user: env.current_user(), db })
    }

    /// `pg_has_role(owner, 'USAGE')`.
    fn owner_usage(&self, owner: Oid) -> Result<bool> {
        self.graph.has_privs_of_role(self.user, owner)
    }

    /// `has_function_privilege(oid, 'EXECUTE')`.
    fn can_execute(&self, p: ProcRef<'a>) -> Result<bool> {
        match p.acl() {
            // The default function ACL grants EXECUTE to PUBLIC.
            None => Ok(true),
            Some(a) => Ok(acl::privilege_mask(&self.graph, self.user, p.owner(), a, acl_mode::EXECUTE, false)? != 0),
        }
    }

    /// Schema name by OID (None when the namespace is missing, which the view's JOIN would drop).
    fn nsp(&self, oid: Oid) -> Option<&'a str> {
        let snap: &'a CatalogSnapshot = self.snap;
        snap.namespace(oid).map(|n| n.name.as_str())
    }

    /// `pg_type` row via the type environment (builtin or user).
    fn type_info(&self, oid: Oid) -> Option<TypeInfo> {
        self.env.type_info(oid)
    }

    /// The `data_type` CASE of `routines` and `parameters`: `ARRAY` for varlena arrays,
    /// `format_type` for `pg_catalog` types, `USER-DEFINED` otherwise (including missing types).
    fn simple_data_type(&self, type_oid: Oid, info: Option<&TypeInfo>) -> Result<String> {
        match info {
            None => Ok("USER-DEFINED".to_string()),
            Some(t) if t.elem != 0 && t.len == -1 => Ok("ARRAY".to_string()),
            Some(t) if self.nsp(t.namespace) == Some("pg_catalog") => format_type(self.env, type_oid, -1),
            Some(_) => Ok("USER-DEFINED".to_string()),
        }
    }
}

/// Render text tuples under the view's column names, sorted and de-duplicated (`SELECT DISTINCT`).
fn text_rows(oid: Oid, columns: &[&str], mut tuples: Vec<Vec<String>>) -> Result<Vec<Row>> {
    let rel = rel_of(oid)?;
    tuples.sort();
    tuples.dedup();
    let mut rows = Vec::with_capacity(tuples.len());
    for t in &tuples {
        let mut b = RowBuilder::new(rel);
        for (c, v) in columns.iter().zip(t.iter()) {
            b.set(*c, d::text(v));
        }
        rows.push(b.finish()?);
    }
    Ok(rows)
}

// ------------------------------------------------------------------------------------------
// routines / parameters
// ------------------------------------------------------------------------------------------

fn routines(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let rel = rel_of(relid::IS_ROUTINES)?;
    let mut rows = Vec::new();
    for p in all_procs(sc.snap) {
        let Some(nsp) = sc.nsp(p.namespace()) else { continue };
        let owner_ok = sc.owner_usage(p.owner())?;
        if !owner_ok && !sc.can_execute(p)? {
            continue;
        }
        let Some(lang) = lang_of(p.language()) else { continue };
        let kind = p.prokind();
        let is_proc = kind == b'p';
        // LEFT JOIN pg_type on prorettype, not for procedures.
        let ret_info = if is_proc { None } else { sc.type_info(p.ret()) };

        let mut b = RowBuilder::new(rel);
        b.set("specific_catalog", d::text(sc.db))
            .set("specific_schema", d::text(nsp))
            .set("specific_name", d::text(&name_concat_oid(p.name(), p.oid())))
            .set("routine_catalog", d::text(sc.db))
            .set("routine_schema", d::text(nsp))
            .set("routine_name", d::text(p.name()))
            .set("routine_type", d::opt_text(routine_type(kind)));
        if !is_proc {
            b.set("data_type", d::text(&sc.simple_data_type(p.ret(), ret_info.as_ref())?));
            if let Some(t) = &ret_info {
                if let Some(tns) = sc.nsp(t.namespace) {
                    b.set("type_udt_catalog", d::text(sc.db))
                        .set("type_udt_schema", d::text(tns))
                        .set("type_udt_name", d::text(&t.name));
                }
            }
            b.set("dtd_identifier", d::text("0"));
        }
        b.set("routine_body", d::text(if lang.name == "sql" { "SQL" } else { "EXTERNAL" }))
            .set("routine_definition", d::opt_text(if owner_ok { Some(p.src()) } else { None }))
            .set("external_name", d::opt_text(if lang.name == "c" { Some(p.src()) } else { None }))
            .set("external_language", d::text(&lang.name.to_ascii_uppercase()))
            .set("parameter_style", d::text("GENERAL"))
            .set("is_deterministic", d::text(yes_no(p.immutable())))
            .set("sql_data_access", d::text("MODIFIES"));
        if !is_proc {
            b.set("is_null_call", d::text(yes_no(p.strict())));
        }
        b.set("schema_level_routine", d::text("YES"))
            .set("max_dynamic_result_sets", d::int4(0))
            .set("security_type", d::text(if p.security_definer() { "DEFINER" } else { "INVOKER" }))
            .set("as_locator", d::text("NO"))
            .set("is_udt_dependent", d::text("NO"));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

fn parameters(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let rel = rel_of(relid::IS_PARAMETERS)?;
    let mut rows = Vec::new();
    for p in all_procs(sc.snap) {
        let Some(nsp) = sc.nsp(p.namespace()) else { continue };
        let owner_ok = sc.owner_usage(p.owner())?;
        if !owner_ok && !sc.can_execute(p)? {
            continue;
        }
        let pname = p.name();
        for (i, a) in p.args().into_iter().enumerate() {
            // JOIN pg_type t / pg_namespace nt: parameters whose type is unknown are dropped.
            let Some(info) = sc.type_info(a.type_oid) else { continue };
            let Some(tns) = sc.nsp(info.namespace) else { continue };
            let n = i + 1;
            let mut b = RowBuilder::new(rel);
            b.set("specific_catalog", d::text(sc.db))
                .set("specific_schema", d::text(nsp))
                .set("specific_name", d::text(&name_concat_oid(pname, p.oid())))
                .set("ordinal_position", d::int4(n as i32))
                .set("parameter_mode", d::opt_text(parameter_mode(a.mode)))
                .set("is_result", d::text("NO"))
                .set("as_locator", d::text("NO"))
                .set("parameter_name", d::opt_text(a.name))
                .set("data_type", d::text(&sc.simple_data_type(a.type_oid, Some(&info))?))
                .set("udt_catalog", d::text(sc.db))
                .set("udt_schema", d::text(tns))
                .set("udt_name", d::text(&info.name))
                .set("dtd_identifier", d::text(&n.to_string()))
                .set("parameter_default", d::opt_text(if owner_ok { a.default } else { None }));
            rows.push(b.finish()?);
        }
    }
    Ok(rows)
}

// ------------------------------------------------------------------------------------------
// Usage views
// ------------------------------------------------------------------------------------------

const VIEW_TABLE_USAGE_COLS: &[&str] =
    &["view_catalog", "view_schema", "view_name", "table_catalog", "table_schema", "table_name"];
const VIEW_COLUMN_USAGE_COLS: &[&str] = &[
    "view_catalog",
    "view_schema",
    "view_name",
    "table_catalog",
    "table_schema",
    "table_name",
    "column_name",
];
const VIEW_ROUTINE_USAGE_COLS: &[&str] =
    &["table_catalog", "table_schema", "table_name", "specific_catalog", "specific_schema", "specific_name"];
const ROUTINE_COLUMN_USAGE_COLS: &[&str] = &[
    "specific_catalog",
    "specific_schema",
    "specific_name",
    "routine_catalog",
    "routine_schema",
    "routine_name",
    "table_catalog",
    "table_schema",
    "table_name",
    "column_name",
];
const ROUTINE_ROUTINE_USAGE_COLS: &[&str] =
    &["specific_catalog", "specific_schema", "specific_name", "routine_catalog", "routine_schema", "routine_name"];
const ROUTINE_TABLE_USAGE_COLS: &[&str] = &[
    "specific_catalog",
    "specific_schema",
    "specific_name",
    "routine_catalog",
    "routine_schema",
    "routine_name",
    "table_catalog",
    "table_schema",
    "table_name",
];
const ROUTINE_SEQUENCE_USAGE_COLS: &[&str] = &[
    "specific_catalog",
    "specific_schema",
    "specific_name",
    "routine_catalog",
    "routine_schema",
    "routine_name",
    "sequence_catalog",
    "sequence_schema",
    "sequence_name",
];
const CHECK_CONSTRAINT_ROUTINE_USAGE_COLS: &[&str] = &[
    "constraint_catalog",
    "constraint_schema",
    "constraint_name",
    "specific_catalog",
    "specific_schema",
    "specific_name",
];

/// `view_table_usage`: tables/views a view reads (other than itself), owner-visible.
fn view_table_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for v in sc.snap.data().relations.values() {
        if v.kind != RelKind::View {
            continue;
        }
        let Some(view) = &v.view else { continue };
        let Some(vns) = sc.nsp(v.namespace) else { continue };
        for dep in &view.query.depends_on {
            if dep.class != ObjClass::Relation || dep.oid == v.oid {
                continue;
            }
            let Some(tab) = sc.snap.relation(dep.oid) else { continue };
            if !is_table_like(tab.kind) || !sc.owner_usage(tab.owner)? {
                continue;
            }
            let Some(tns) = sc.nsp(tab.namespace) else { continue };
            t.push(vec![
                sc.db.to_string(),
                vns.to_string(),
                v.name.clone(),
                sc.db.to_string(),
                tns.to_string(),
                tab.name.clone(),
            ]);
        }
    }
    text_rows(relid::IS_VIEW_TABLE_USAGE, VIEW_TABLE_USAGE_COLS, t)
}

/// `view_column_usage`: columns a view reads, owner-visible; dropped columns are skipped.
fn view_column_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for v in sc.snap.data().relations.values() {
        if v.kind != RelKind::View {
            continue;
        }
        let Some(view) = &v.view else { continue };
        let Some(vns) = sc.nsp(v.namespace) else { continue };
        for dep in &view.query.depends_on {
            if dep.class != ObjClass::Relation || dep.oid == v.oid || dep.sub_id <= 0 {
                continue;
            }
            let Some(tab) = sc.snap.relation(dep.oid) else { continue };
            if !is_table_like(tab.kind) || !sc.owner_usage(tab.owner)? {
                continue;
            }
            let Some(col) = i16::try_from(dep.sub_id).ok().and_then(|n| tab.column(n)) else { continue };
            if col.dropped {
                continue;
            }
            let Some(tns) = sc.nsp(tab.namespace) else { continue };
            t.push(vec![
                sc.db.to_string(),
                vns.to_string(),
                v.name.clone(),
                sc.db.to_string(),
                tns.to_string(),
                tab.name.clone(),
                col.name.clone(),
            ]);
        }
    }
    text_rows(relid::IS_VIEW_COLUMN_USAGE, VIEW_COLUMN_USAGE_COLS, t)
}

/// `view_routine_usage`: routines a view calls, owner-visible.
fn view_routine_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for v in sc.snap.data().relations.values() {
        if v.kind != RelKind::View {
            continue;
        }
        let Some(view) = &v.view else { continue };
        let Some(vns) = sc.nsp(v.namespace) else { continue };
        for dep in &view.query.depends_on {
            if dep.class != ObjClass::Proc {
                continue;
            }
            let Some(p) = resolve_proc(sc.snap, dep.oid) else { continue };
            if !sc.owner_usage(p.owner())? {
                continue;
            }
            let Some(pns) = sc.nsp(p.namespace()) else { continue };
            t.push(vec![
                sc.db.to_string(),
                vns.to_string(),
                v.name.clone(),
                sc.db.to_string(),
                pns.to_string(),
                name_concat_oid(p.name(), p.oid()),
            ]);
        }
    }
    text_rows(relid::IS_VIEW_ROUTINE_USAGE, VIEW_ROUTINE_USAGE_COLS, t)
}

/// `routine_table_usage`: tables a user routine depends on (any column or whole-table reference).
fn routine_table_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for f in sc.snap.data().functions.values() {
        let Some(fns) = sc.nsp(f.namespace) else { continue };
        let spec = name_concat_oid(&f.name, f.oid);
        for dep in &f.depends_on {
            if dep.class != ObjClass::Relation {
                continue;
            }
            let Some(tab) = sc.snap.relation(dep.oid) else { continue };
            if !is_table_like(tab.kind) || !sc.owner_usage(tab.owner)? {
                continue;
            }
            let Some(tns) = sc.nsp(tab.namespace) else { continue };
            t.push(vec![
                sc.db.to_string(),
                fns.to_string(),
                spec.clone(),
                sc.db.to_string(),
                fns.to_string(),
                f.name.clone(),
                sc.db.to_string(),
                tns.to_string(),
                tab.name.clone(),
            ]);
        }
    }
    text_rows(relid::IS_ROUTINE_TABLE_USAGE, ROUTINE_TABLE_USAGE_COLS, t)
}

/// `routine_column_usage`: columns a user routine depends on; dropped columns are skipped.
fn routine_column_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for f in sc.snap.data().functions.values() {
        let Some(fns) = sc.nsp(f.namespace) else { continue };
        let spec = name_concat_oid(&f.name, f.oid);
        for dep in &f.depends_on {
            if dep.class != ObjClass::Relation || dep.sub_id <= 0 {
                continue;
            }
            let Some(tab) = sc.snap.relation(dep.oid) else { continue };
            if !is_table_like(tab.kind) || !sc.owner_usage(tab.owner)? {
                continue;
            }
            let Some(col) = i16::try_from(dep.sub_id).ok().and_then(|n| tab.column(n)) else { continue };
            if col.dropped {
                continue;
            }
            let Some(tns) = sc.nsp(tab.namespace) else { continue };
            t.push(vec![
                sc.db.to_string(),
                fns.to_string(),
                spec.clone(),
                sc.db.to_string(),
                fns.to_string(),
                f.name.clone(),
                sc.db.to_string(),
                tns.to_string(),
                tab.name.clone(),
                col.name.clone(),
            ]);
        }
    }
    text_rows(relid::IS_ROUTINE_COLUMN_USAGE, ROUTINE_COLUMN_USAGE_COLS, t)
}

/// `routine_sequence_usage`: sequences a user routine depends on, owner-visible.
fn routine_sequence_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for f in sc.snap.data().functions.values() {
        let Some(fns) = sc.nsp(f.namespace) else { continue };
        let spec = name_concat_oid(&f.name, f.oid);
        for dep in &f.depends_on {
            if dep.class != ObjClass::Relation {
                continue;
            }
            let Some(seq) = sc.snap.sequence(dep.oid) else { continue };
            if !sc.owner_usage(seq.owner)? {
                continue;
            }
            let Some(sns) = sc.nsp(seq.namespace) else { continue };
            t.push(vec![
                sc.db.to_string(),
                fns.to_string(),
                spec.clone(),
                sc.db.to_string(),
                fns.to_string(),
                f.name.clone(),
                sc.db.to_string(),
                sns.to_string(),
                seq.name.clone(),
            ]);
        }
    }
    text_rows(relid::IS_ROUTINE_SEQUENCE_USAGE, ROUTINE_SEQUENCE_USAGE_COLS, t)
}

/// `routine_routine_usage`: routines a user function (prokind f/p) calls; the callee must be a
/// function or procedure (prokind f/p) owned by a visible role.
fn routine_routine_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for f in sc.snap.data().functions.values() {
        if !matches!(f.kind, FuncKind::Function | FuncKind::Procedure) {
            continue;
        }
        let Some(fns) = sc.nsp(f.namespace) else { continue };
        let spec = name_concat_oid(&f.name, f.oid);
        for dep in &f.depends_on {
            if dep.class != ObjClass::Proc {
                continue;
            }
            let Some(p1) = resolve_proc(sc.snap, dep.oid) else { continue };
            if !matches!(p1.prokind(), b'f' | b'p') || !sc.owner_usage(p1.owner())? {
                continue;
            }
            let Some(p1ns) = sc.nsp(p1.namespace()) else { continue };
            t.push(vec![
                sc.db.to_string(),
                fns.to_string(),
                spec.clone(),
                sc.db.to_string(),
                p1ns.to_string(),
                name_concat_oid(p1.name(), p1.oid()),
            ]);
        }
    }
    text_rows(relid::IS_ROUTINE_ROUTINE_USAGE, ROUTINE_ROUTINE_USAGE_COLS, t)
}

/// `check_constraint_routine_usage`: routines called from CHECK constraints (table and domain
/// checks), owner-visible.
fn check_constraint_routine_usage(ctx: &ScanContext) -> Result<Vec<Row>> {
    let sc = Scan::new(ctx)?;
    let mut t: Vec<Vec<String>> = Vec::new();
    for c in sc.snap.data().constraints.values() {
        if c.kind != ConstraintKind::Check {
            continue;
        }
        let Some(check) = &c.check else { continue };
        let Some(cns) = sc.nsp(c.namespace) else { continue };
        for dep in &check.depends_on {
            if dep.class != ObjClass::Proc {
                continue;
            }
            let Some(p) = resolve_proc(sc.snap, dep.oid) else { continue };
            if !sc.owner_usage(p.owner())? {
                continue;
            }
            let Some(pns) = sc.nsp(p.namespace()) else { continue };
            t.push(vec![
                sc.db.to_string(),
                cns.to_string(),
                c.name.clone(),
                sc.db.to_string(),
                pns.to_string(),
                name_concat_oid(p.name(), p.oid()),
            ]);
        }
    }
    text_rows(relid::IS_CHECK_CONSTRAINT_ROUTINE_USAGE, CHECK_CONSTRAINT_ROUTINE_USAGE_COLS, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::{Datum, IoConfig};

    fn col(rel: Oid, name: &str) -> usize {
        rel_of(rel).unwrap().columns.iter().position(|c| c.name == name).unwrap()
    }

    fn text(d: &Datum) -> Option<&str> {
        match d {
            Datum::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[test]
    fn name_concat_matches_nameconcatoid() {
        assert_eq!(name_concat_oid("f", 42), "f_42");
        assert_eq!(name_concat_oid("int4in", 42), "int4in_42");
        let long = "a".repeat(80);
        let s = name_concat_oid(&long, 16384);
        assert_eq!(s.len(), 63);
        assert!(s.ends_with("_16384"));
        assert_eq!(&s[..57], &"a".repeat(57));
    }

    #[test]
    fn name_concat_keeps_utf8_characters_whole() {
        // 'é' is two bytes; clipping must not split it.
        let name = format!("{}é", "a".repeat(56));
        let s = name_concat_oid(&name, 16384);
        assert!(s.len() <= 63);
        assert!(s.ends_with("_16384"));
        assert!(s.is_char_boundary(s.len() - 6));
    }

    #[test]
    fn split_defaults_respects_brackets_and_quotes() {
        assert_eq!(split_top_level("0, 0, 0.0"), vec!["0", "0", "0.0"]);
        assert_eq!(split_top_level("'{}'::jsonb, false"), vec!["'{}'::jsonb", "false"]);
        assert_eq!(split_top_level("ARRAY[1,2], 'a,b'"), vec!["ARRAY[1,2]", "'a,b'"]);
        assert_eq!(split_top_level("f(1,2), 3"), vec!["f(1,2)", "3"]);
        assert!(split_top_level("").is_empty());
    }

    #[test]
    fn modes_map_like_the_view() {
        assert_eq!(parameter_mode(b'i'), Some("IN"));
        assert_eq!(parameter_mode(b'v'), Some("IN"));
        assert_eq!(parameter_mode(b'o'), Some("OUT"));
        assert_eq!(parameter_mode(b't'), Some("OUT"));
        assert_eq!(parameter_mode(b'b'), Some("INOUT"));
        assert_eq!(routine_type(b'f'), Some("FUNCTION"));
        assert_eq!(routine_type(b'p'), Some("PROCEDURE"));
        assert_eq!(routine_type(b'a'), None);
    }

    #[test]
    fn builtin_int4in_routine_row() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = IsRoutines.scan(&ctx, &ScanRequest::all())?;
        // The superuser sees every builtin pg_proc row (oracle: 3413).
        assert_eq!(rows.len(), PROCS.len());
        let name_col = col(relid::IS_ROUTINES, "routine_name");
        let row = rows
            .iter()
            .find(|r| text(&r[name_col]) == Some("int4in") && text(&r[col(relid::IS_ROUTINES, "specific_name")]) == Some("int4in_42"))
            .expect("int4in row");
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "routine_type")]), Some("FUNCTION"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "data_type")]), Some("integer"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "routine_body")]), Some("EXTERNAL"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "external_language")]), Some("INTERNAL"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "is_deterministic")]), Some("YES"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "is_null_call")]), Some("YES"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "security_type")]), Some("INVOKER"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "sql_data_access")]), Some("MODIFIES"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "routine_definition")]), Some("int4in"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "dtd_identifier")]), Some("0"));
        assert_eq!(text(&row[col(relid::IS_ROUTINES, "type_udt_name")]), Some("int4"));
        Ok(())
    }

    #[test]
    fn builtin_parameters_and_bootstrap_usage_views() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let params = IsParameters.scan(&ctx, &ScanRequest::all())?;
        let spec = col(relid::IS_PARAMETERS, "specific_name");
        let ord = col(relid::IS_PARAMETERS, "ordinal_position");
        let p1 = params
            .iter()
            .find(|r| text(&r[spec]) == Some("int4in_42") && r[ord] == Datum::Int4(1))
            .expect("int4in parameter 1");
        assert_eq!(text(&p1[col(relid::IS_PARAMETERS, "parameter_mode")]), Some("IN"));
        assert_eq!(text(&p1[col(relid::IS_PARAMETERS, "data_type")]), Some("cstring"));
        assert_eq!(p1[col(relid::IS_PARAMETERS, "parameter_name")], Datum::Null);
        assert_eq!(p1[col(relid::IS_PARAMETERS, "parameter_default")], Datum::Null);

        // No user views, functions or constraints in a bootstrapped catalog.
        assert!(IsViewTableUsage.scan(&ctx, &ScanRequest::all())?.is_empty());
        assert!(IsViewColumnUsage.scan(&ctx, &ScanRequest::all())?.is_empty());
        assert!(IsRoutineTableUsage.scan(&ctx, &ScanRequest::all())?.is_empty());
        assert!(IsCheckConstraintRoutineUsage.scan(&ctx, &ScanRequest::all())?.is_empty());
        Ok(())
    }
}
