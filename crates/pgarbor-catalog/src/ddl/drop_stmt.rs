//! Statement-level DROP: name lookup, kind checks, permission checks, IF EXISTS notices, then one
//! `drop_objects` call for all targets. Lease CAT-25. PG sources: `commands/dropcmds.c`
//! (`RemoveObjects`, `owningrel_does_not_exist_skipping`, `does_not_exist_skipping`),
//! `commands/tablecmds.c` (`RemoveRelations`, `RangeVarCallbackForDropRelation`),
//! `commands/typecmds.c` (`RemoveTypeById`), `commands/schemacmds.c`, `parser/parse_func.c`
//! (`LookupFuncWithArgs`).
//!
//! ## What gets resolved here
//! The binder passes NAMES (not OIDs) because the error and NOTICE wording depends on lookup results:
//! [`DropTarget`] = optional schema, name, and for routines the optional argument type list.
//! Lookup uses the statement's `CatalogEnv` (search_path-aware, `lookup_class` / `lookup_type` /
//! `function_candidates`). Duplicate targets are collapsed (`drop table t, t` succeeds).
//!
//! ## Messages (oracle-verified PG 18.6; `kind_word` = table|view|materialized view|index|sequence|
//! type|schema|foreign table)
//! - missing object: 42P01 `table "nosuch" does not exist` (tables, views, matviews, sequences, foreign
//!   tables: 42P01); `index "nosuch" does not exist` and `type "nosuch" does not exist` (also for
//!   DROP DOMAIN): 42704 (`errors::kind_does_not_exist`). With a qualifier the name prints qualified
//!   (`table "s.nosuch" does not exist`). Missing SCHEMA of a qualified name: error 3F000 `schema
//!   "zz_nos" does not exist`; with IF EXISTS NOTICE `schema "zz_nos" does not exist, skipping`.
//!   IF EXISTS on a missing object: NOTICE `table "nosuch" does not exist, skipping`
//!   (`errors::kind_does_not_exist_skipping`).
//! - wrong kind (`wrong_object_type_for_drop(name, expected_word, actual)`): `DROP TABLE s` on a sequence
//!   -> 42809 `"s" is not a table` HINT `Use DROP SEQUENCE to remove a sequence.`; DROP VIEW on a table ->
//!   `"t" is not a view` HINT `Use DROP TABLE to remove a table.`; composite types: `DROP TABLE c` ->
//!   HINT `Use DROP TYPE to remove a type.`; DROP INDEX on a table -> `"t" is not an index`.
//!   `DROP TABLE` also accepts partitioned tables; PG's DROP TABLE refuses views etc. as above.
//! - DROP TYPE on a table/matview rowtype -> 2BP01-class via the INTERNAL rule:
//!   `cannot drop type t because table t requires it` HINT `You can drop table t instead.`; on an
//!   array type -> `cannot drop type t[] because type t requires it`. DROP TYPE on a composite type
//!   (a `CompositeType` relation) drops the relation. DROP DOMAIN on a non-domain -> 42809
//!   `e is not a domain` (`errors::not_a_domain`, unquoted).
//! - system objects: relations in pg_catalog/information_schema -> 42501 `permission denied: "pg_class"
//!   is a system catalog`; schemas `pg_catalog`, `pg_toast`, `information_schema` -> 2BP01
//!   `cannot drop schema pg_catalog because it is required by the database system`
//!   (`errors::required_by_system`, from the pinned check in `plan_drop`).
//! - ownership: the current user must own the object (relations: the table owner; types, functions:
//!   their owner; DROP SCHEMA: the schema owner) or be a member of its owner or a superuser, else
//!   42501 `must be owner of table t` (`errors::must_be_owner`), checked per target BEFORE planning
//!   (PG `RangeVarCallbackForDropRelation`). The schema's owner has NO right to drop other roles'
//!   objects inside it.
//! - routines (`drop function/procedure/routine/aggregate`):
//!   `args = Some(types)`: exact match of IN-argument identity, none -> 42883 `function f(integer) does not
//!   exist` (`errors::function_does_not_exist(sig)`; IF EXISTS NOTICE `function nosuch(pg_catalog.int4,text)
//!   does not exist, skipping` - NOTE the internal `pg_catalog.int4` type names in the NOTICE, plain names
//!   in the error; `errors::function_does_not_exist_skipping`); kind mismatch -> 42809 `p(integer) is not a
//!   function` / `f(integer) is not a procedure` (`errors::wrong_routine_kind`; ROUTINE accepts both).
//!   `args = None` (`drop function f`): all candidates named f visible in the search path (or the
//!   qualified schema); none -> 42883 `could not find a function named "f"` (`errors::no_function_named`;
//!   procedure variant says `procedure`; IF EXISTS NOTICE `function nosuch() does not exist, skipping`,
//!   `procedure nosuch() does not exist, skipping`); more than one -> 42725 `function name "f" is not
//!   unique` HINT `Specify the argument list to select the function unambiguously.`
//!   (`errors::function_name_not_unique`). Builtin (pg_catalog) functions: system-catalog permission denied.
//! - DROP ROLE: `drop_role` per name (with `errors::role_does_not_exist` /
//!   `role_does_not_exist_skipping`), executed after resolving all names.
//!
//! After resolution: `drop_objects(&targets, behavior)` once for all targets (one CASCADE notice
//! for the whole statement). Multiple targets with IF EXISTS: every missing one gets its own NOTICE
//! in argument order, then the existing ones are dropped.
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

/// What a DROP statement removes.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropKind {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    ForeignTable,
    /// DROP TYPE (enum, domain, composite, base).
    Type,
    Domain,
    Schema,
    Function,
    Procedure,
    /// DROP ROUTINE: function or procedure.
    Routine,
    Aggregate,
    Role,
}

/// One name in a DROP list.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub schema: Option<String>,
    pub name: String,
    /// Routines only: `Some(IN-argument types)` or `None` when the argument list was omitted.
    pub args: Option<Vec<Oid>>,
}

impl DropTarget {
    pub fn simple(name: impl Into<String>) -> DropTarget {
        DropTarget { schema: None, name: name.into(), args: None }
    }
}

impl<'a> CatalogTxn<'a> {
    /// Execute a DROP statement (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_by_names(
        &mut self,
        kind: DropKind,
        targets: &[DropTarget],
        if_exists: bool,
        behavior: DropBehavior,
    ) -> Result<()> {
        Err(PgError::unimplemented("CAT-25", "DROP statement"))
    }
}
