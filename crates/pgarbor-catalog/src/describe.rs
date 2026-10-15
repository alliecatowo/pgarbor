//! Object descriptions, PG `catalog/objectaddress.c` `getObjectDescription` (also
//! `pg_describe_object`) and `getObjectTypeDescription` / `pg_identify_object`. Lease CAT-07.
//!
//! Used for DROP errors/notices (`errors::dependent_objects_still_exist`,
//! `errors::drop_cascades_notice`, `errors::required_by`), COMMENT, and `pg_describe_object()`.
//!
//! `describe(env, addr)` — oracle-verified PG 18.6 strings (names qualified with
//! `quote_identifier(schema).` only when the object is NOT visible in the search path; PG prints the
//! schema for e.g. `drop cascades to table zz_scratch.t` because the cascade happened while the schema
//! was being dropped and was not in the search path):
//! | address | description |
//! |---|---|
//! | Relation (table) | `table t` |
//! | Relation (partitioned table) | `table t` |
//! | Relation (view / matview) | `view v` / `materialized view mv` |
//! | Relation (composite type relation) | `composite type c` |
//! | Index / Sequence | `index t_pkey` / `sequence t_id_seq` |
//! | column (Relation + sub_id) | `column b of table t` (`column id of view v`, ...) |
//! | Type (enum/domain/base) | `type mood` (format_type output: `type integer`, `type t[]`) |
//! | Type (table rowtype) | `type t` |
//! | Namespace | `schema zz_scratch` |
//! | Constraint on table | `constraint t_pkey on table t` |
//! | Constraint on domain | `constraint pos_check on type pos`? — NO: PG 18 prints `constraint pos_check` for domain constraints in pg_describe_object (oracle: `constraint pos_check`) |
//! | AttrDefault | `default value for column id of table t` |
//! | Proc | `function f(integer)` / `procedure p(text)` / `aggregate a(integer)` (args via format_type) |
//! | Role | `role alice` |
//! | Database | `database postgres` |
//! | Collation | `collation "C"`? — PG: `collation C` (quote_identifier → `"C"`: prints `collation "C"`) |
//! | Rewrite (view rule) | `rule _RETURN on view v` |
//! | Extension | `extension plpgsql` |
//! | Language | `language sql` |
//! | Trigger | `trigger RI_ConstraintTrigger_a_16474 on table t` |
//! Unknown OIDs: PG returns NULL from pg_describe_object; here → `Ok(None)`.
//!
//! The table's qualification follows the table (e.g. `constraint t_pkey on table s.t` when `s` is not
//! in the path). Columns: `column b of table t`. Default: `default value for column id of table t`.
//!
//! `type_description(addr)` gives `pg_identify_object(...).type` words: `table`, `index`, `sequence`,
//! `view`, `materialized view`, `composite type`, `table column`, `view column`, `type`, `schema`,
//! `table constraint`, `domain constraint`, `default value`, `function`, `procedure`, `aggregate`,
//! `role`, `database`, `collation`, `rule`, `extension`, `language`, `trigger`.
use crate::env::CatalogEnv;
use crate::model::ObjectAddress;
use pgarbor_common::{PgError, Result};

/// PG `getObjectDescription` (module docs). `Ok(None)` if the object does not exist.
// CONTRACT
#[allow(unused_variables)]
pub fn describe(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-07", "object description"))
}

/// Like `describe` but a missing object is XX000 `cache lookup failed for <class> <oid>`.
pub fn describe_existing(env: &CatalogEnv, addr: &ObjectAddress) -> Result<String> {
    describe(env, addr)?.ok_or_else(|| crate::errors::cache_lookup_failed(&format!("{:?}", addr.class).to_lowercase(), addr.oid))
}

/// `pg_identify_object(...).type` word (module docs). `Ok(None)` if the object does not exist.
// CONTRACT
#[allow(unused_variables)]
pub fn type_description(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<&'static str>> {
    Err(PgError::unimplemented("CAT-07", "object type description"))
}
