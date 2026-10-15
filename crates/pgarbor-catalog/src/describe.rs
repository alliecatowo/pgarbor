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
//!
//! Implementation notes (PG 18.6 `objectaddress.c`, verified against the PG source):
//! - `getObjectDescription` prints `function %s` for EVERY pg_proc row (procedures and aggregates
//!   included); only `getObjectTypeDescription` (`pg_identify_object`) distinguishes
//!   `procedure` / `aggregate`. The table above is followed for the type words only.
//! - Role, database, schema, tablespace, extension, language, access method and rule names are
//!   printed UNQUOTED (`NameStr`); collation and relation names use `quote_qualified_identifier`.
//! - Trigger addresses never occur: PGArbor creates no `pg_trigger` rows, so they are `Ok(None)`.
use crate::builtin;
use crate::ddl::function::format_signature;
use crate::env::CatalogEnv;
use crate::model::{ObjClass, ObjectAddress, RelKind};
use pgarbor_common::{Oid, Result};
use pgarbor_types::catalog_data::{self, ProcKind};
use pgarbor_types::names::format_type;
use pgarbor_types::text::format::quote_identifier;

/// PG `getObjectDescription` (module docs). `Ok(None)` if the object does not exist.
// CONTRACT
pub fn describe(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<String>> {
    let snap = env.snapshot();
    match addr.class {
        ObjClass::Relation => {
            if addr.sub_id != 0 {
                match i16::try_from(addr.sub_id) {
                    Ok(attnum) => column_description(env, addr.oid, attnum),
                    Err(_) => Ok(None),
                }
            } else {
                relation_description(env, addr.oid)
            }
        }
        ObjClass::Type => {
            if !type_exists(env, addr.oid) {
                return Ok(None);
            }
            Ok(Some(format!("type {}", format_type(env, addr.oid, -1)?)))
        }
        ObjClass::Namespace => Ok(snap.namespace(addr.oid).map(|n| format!("schema {}", n.name))),
        ObjClass::Constraint => constraint_description(env, addr.oid),
        ObjClass::AttrDefault => match find_attrdef(env, addr.oid) {
            Some((rel, attnum)) => match column_description(env, rel, attnum)? {
                Some(col) => Ok(Some(format!("default value for {col}"))),
                None => Ok(None),
            },
            None => Ok(None),
        },
        ObjClass::Proc => match proc_info(env, addr.oid) {
            Some(info) => {
                let qualified = !env.function_is_visible(addr.oid)?;
                let sig = format_signature(env, info.namespace, &info.name, &info.arg_types, qualified)?;
                Ok(Some(format!("function {sig}")))
            }
            None => Ok(None),
        },
        ObjClass::Role => Ok(snap.role(addr.oid).map(|r| format!("role {}", r.name))),
        ObjClass::Database => Ok(snap.database(addr.oid).map(|d| format!("database {}", d.name))),
        ObjClass::Collation => match builtin::builtin_collation(addr.oid) {
            Some(coll) => {
                let visible = env.search_path().contains(&coll.namespace);
                let name = quote_identifier(coll.name)?;
                if visible {
                    Ok(Some(format!("collation {name}")))
                } else {
                    let ns = snap.namespace(coll.namespace).ok_or_else(|| {
                        crate::errors::cache_lookup_failed("namespace", coll.namespace)
                    })?;
                    Ok(Some(format!("collation {}.{name}", quote_identifier(&ns.name)?)))
                }
            }
            None => Ok(None),
        },
        ObjClass::Operator => match catalog_data::builtin_operator(addr.oid) {
            Some(op) => {
                let left = if op.left == 0 { "NONE".to_string() } else { format_type(env, op.left, -1)? };
                let right = if op.right == 0 { "NONE".to_string() } else { format_type(env, op.right, -1)? };
                // Operators are visible through pg_catalog only (no user operators in PGArbor).
                let prefix = if env.search_path().contains(&builtin::PG_CATALOG_NAMESPACE) { "" } else { "pg_catalog." };
                Ok(Some(format!("operator {prefix}{}({left},{right})", op.name)))
            }
            None => Ok(None),
        },
        ObjClass::Cast => match catalog_data::CASTS.iter().find(|c| c.oid == addr.oid) {
            Some(cast) => Ok(Some(format!(
                "cast from {} to {}",
                format_type(env, cast.source, -1)?,
                format_type(env, cast.target, -1)?
            ))),
            None => Ok(None),
        },
        ObjClass::Language => {
            Ok(builtin::LANGUAGES.iter().find(|l| l.oid == addr.oid).map(|l| format!("language {}", l.name)))
        }
        ObjClass::AccessMethod => Ok(builtin::ACCESS_METHODS
            .iter()
            .find(|a| a.oid == addr.oid)
            .map(|a| format!("access method {}", a.name))),
        ObjClass::Extension => {
            Ok(builtin::EXTENSIONS.iter().find(|e| e.oid == addr.oid).map(|e| format!("extension {}", e.name)))
        }
        ObjClass::Rewrite => {
            // The `_RETURN` rule of a view or materialized view; its OID is the relation's OID.
            match snap.relation(addr.oid) {
                Some(rel) if rel.view.is_some() => match relation_description(env, addr.oid)? {
                    Some(rel_desc) => Ok(Some(format!("rule _RETURN on {rel_desc}"))),
                    None => Ok(None),
                },
                _ => Ok(None),
            }
        }
        // PGArbor has no pg_trigger rows (triggers and FK constraint triggers are not created).
        ObjClass::Trigger => Ok(None),
        ObjClass::Tablespace => {
            Ok(builtin::TABLESPACES.iter().find(|t| t.oid == addr.oid).map(|t| format!("tablespace {}", t.name)))
        }
    }
}

/// Like `describe` but a missing object is XX000 `cache lookup failed for <class> <oid>`.
pub fn describe_existing(env: &CatalogEnv, addr: &ObjectAddress) -> Result<String> {
    describe(env, addr)?.ok_or_else(|| crate::errors::cache_lookup_failed(&format!("{:?}", addr.class).to_lowercase(), addr.oid))
}

/// `pg_identify_object(...).type` word (module docs). `Ok(None)` if the object does not exist.
// CONTRACT
pub fn type_description(env: &CatalogEnv, addr: &ObjectAddress) -> Result<Option<&'static str>> {
    let snap = env.snapshot();
    let word = match addr.class {
        ObjClass::Relation => relation_kind(env, addr.oid)
            .map(|kind| if addr.sub_id != 0 { column_word(kind) } else { kind.description_word() }),
        ObjClass::Type => type_exists(env, addr.oid).then_some("type"),
        ObjClass::Namespace => snap.namespace(addr.oid).map(|_| "schema"),
        ObjClass::Constraint => snap.constraint(addr.oid).and_then(|c| {
            if c.table != 0 {
                Some("table constraint")
            } else if c.domain != 0 {
                Some("domain constraint")
            } else {
                None
            }
        }),
        ObjClass::AttrDefault => find_attrdef(env, addr.oid).map(|_| "default value"),
        ObjClass::Proc => proc_info(env, addr.oid).map(|info| match info.kind {
            ProcKind::Procedure => "procedure",
            ProcKind::Aggregate => "aggregate",
            ProcKind::Function | ProcKind::Window => "function",
        }),
        ObjClass::Role => snap.role(addr.oid).map(|_| "role"),
        ObjClass::Database => snap.database(addr.oid).map(|_| "database"),
        ObjClass::Collation => builtin::builtin_collation(addr.oid).map(|_| "collation"),
        ObjClass::Operator => catalog_data::builtin_operator(addr.oid).map(|_| "operator"),
        ObjClass::Cast => catalog_data::CASTS.iter().find(|c| c.oid == addr.oid).map(|_| "cast"),
        ObjClass::Language => builtin::LANGUAGES.iter().find(|l| l.oid == addr.oid).map(|_| "language"),
        ObjClass::AccessMethod => builtin::ACCESS_METHODS.iter().find(|a| a.oid == addr.oid).map(|_| "access method"),
        ObjClass::Extension => builtin::EXTENSIONS.iter().find(|e| e.oid == addr.oid).map(|_| "extension"),
        ObjClass::Rewrite => snap.relation(addr.oid).filter(|r| r.view.is_some()).map(|_| "rule"),
        ObjClass::Trigger => None,
        ObjClass::Tablespace => builtin::TABLESPACES.iter().find(|t| t.oid == addr.oid).map(|_| "tablespace"),
    };
    Ok(word)
}

// ---------------------------------------------------------------- helpers

/// `pg_class` kind of any relation-like OID (table, index, sequence, ...), if it exists.
fn relation_kind(env: &CatalogEnv, oid: Oid) -> Option<RelKind> {
    env.snapshot().class_identity(oid).map(|(_, _, kind)| kind)
}

/// PG `getRelationDescription`: `table t`, `view v`, `index t_pkey`, ... The name is qualified with
/// its schema only when the relation is not visible in the search path (`qualified_class_name`).
fn relation_description(env: &CatalogEnv, oid: Oid) -> Result<Option<String>> {
    let kind = match relation_kind(env, oid) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    let name = env.qualified_class_name(oid)?;
    Ok(Some(format!("{} {}", kind.description_word(), name)))
}

/// PG `getObjectTypeDescription` word for a column of a relation of `kind`.
fn column_word(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table | RelKind::PartitionedTable => "table column",
        RelKind::Index | RelKind::PartitionedIndex => "index column",
        RelKind::Sequence => "sequence column",
        RelKind::View => "view column",
        RelKind::MatView => "materialized view column",
        RelKind::CompositeType => "composite type column",
        RelKind::ForeignTable => "foreign table column",
        RelKind::Toast => "toast table column",
    }
}

/// PG system attribute names (`SelfItemPointerAttributeNumber` etc.).
fn system_column_name(attnum: i16) -> Option<&'static str> {
    match attnum {
        -1 => Some("ctid"),
        -2 => Some("xmin"),
        -3 => Some("cmin"),
        -4 => Some("xmax"),
        -5 => Some("cmax"),
        -7 => Some("tableoid"),
        _ => None,
    }
}

/// `get_attname`: the name of a live column (or index column) of `rel`; None for dropped or
/// nonexistent attributes.
fn column_name(env: &CatalogEnv, rel: Oid, attnum: i16) -> Option<String> {
    let snap = env.snapshot();
    if let Some(r) = snap.relation(rel) {
        if attnum < 0 {
            return if r.kind.has_system_columns() {
                system_column_name(attnum).map(str::to_string)
            } else {
                None
            };
        }
        return r.column(attnum).filter(|c| c.is_visible()).map(|c| c.name.clone());
    }
    if let Some(index) = snap.index(rel) {
        if attnum < 1 {
            return None;
        }
        return index.keys.get(attnum as usize - 1).map(|k| k.name.clone());
    }
    None
}

/// PG `column %s of %s` (`getObjectDescription` for a column address).
fn column_description(env: &CatalogEnv, rel: Oid, attnum: i16) -> Result<Option<String>> {
    let name = match column_name(env, rel, attnum) {
        Some(name) => name,
        None => return Ok(None),
    };
    let relation = match relation_description(env, rel)? {
        Some(relation) => relation,
        None => return Ok(None),
    };
    Ok(Some(format!("column {name} of {relation}")))
}

/// `constraint %s on %s` for table constraints, `constraint %s` for domain constraints.
fn constraint_description(env: &CatalogEnv, oid: Oid) -> Result<Option<String>> {
    let con = match env.snapshot().constraint(oid) {
        Some(con) => con,
        None => return Ok(None),
    };
    if con.table != 0 {
        let relation = relation_description(env, con.table)?
            .ok_or_else(|| crate::errors::cache_lookup_failed("relation", con.table))?;
        Ok(Some(format!("constraint {} on {}", con.name, relation)))
    } else {
        Ok(Some(format!("constraint {}", con.name)))
    }
}

/// `pg_attrdef` OID → (relation, attnum). Only user relations carry defaults.
fn find_attrdef(env: &CatalogEnv, oid: Oid) -> Option<(Oid, i16)> {
    env.snapshot().user_relations().find_map(|rel| {
        rel.columns
            .iter()
            .find(|c| c.default.as_ref().is_some_and(|d| d.oid == oid))
            .map(|c| (rel.oid, c.attnum))
    })
}

/// Whether a type OID names a builtin type, a user type or a relation rowtype / array type.
fn type_exists(env: &CatalogEnv, oid: Oid) -> bool {
    let snap = env.snapshot();
    snap.type_obj(oid).is_some() || snap.user_type(oid).is_some() || catalog_data::builtin_type(oid).is_some()
}

/// The pieces of a pg_proc row needed for a signature.
struct ProcInfo {
    namespace: Oid,
    name: String,
    arg_types: Vec<Oid>,
    kind: ProcKind,
}

/// A user function, or a builtin proc (aggregates included: their argument types come from the
/// proc row with the same OID).
fn proc_info(env: &CatalogEnv, oid: Oid) -> Option<ProcInfo> {
    if let Some(func) = env.snapshot().function(oid) {
        return Some(ProcInfo {
            namespace: func.namespace,
            name: func.name.clone(),
            arg_types: func.in_arg_types(),
            kind: func.kind.to_types(),
        });
    }
    catalog_data::builtin_proc(oid).map(|p| ProcInfo {
        namespace: p.namespace,
        name: p.name.to_string(),
        arg_types: p.arg_types.to_vec(),
        kind: p.kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, ConstraintSpec, ConstraintSpecKind, CreateTableSpec};
    use crate::model::{Persistence, StoredExpr};
    use crate::testing;
    use pgarbor_common::{MemKv, PgError};
    use pgarbor_types::TypeId;

    /// Description that must exist.
    fn expect_desc(env: &CatalogEnv, addr: &ObjectAddress) -> Result<String> {
        describe(env, addr)?.ok_or_else(|| PgError::internal(format!("no description for {addr}")))
    }

    /// `CREATE TABLE <namespace>.t (id integer NOT NULL, b integer DEFAULT 1, PRIMARY KEY (id))`.
    fn make_t(kv: &mut MemKv, env: &CatalogEnv, namespace: Oid) -> Result<(Oid, CatalogEnv)> {
        let mut id = ColumnSpec::new("id", TypeId::new(23, -1));
        id.not_null = Some(None);
        let mut b = ColumnSpec::new("b", TypeId::new(23, -1));
        b.default = Some(StoredExpr::new("1"));
        let spec = CreateTableSpec {
            namespace,
            name: "t".to_string(),
            owner: builtin::BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::default(),
            columns: vec![id, b],
            constraints: vec![ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
                columns: vec!["id".to_string()],
                include: Vec::new(),
            })],
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let mut created = None;
        let (_res, next) = testing::run(kv, env, |t| {
            created = t.create_table(&spec)?;
            Ok(())
        })?;
        let table = created
            .map(|c| c.table)
            .ok_or_else(|| PgError::internal("create_table returned no table"))?;
        Ok((table, next))
    }

    #[test]
    fn unknown_oids_are_none() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let missing: Oid = 999_999;
        let addrs = [
            ObjectAddress::relation(missing),
            ObjectAddress::column(missing, 1),
            ObjectAddress::type_(missing),
            ObjectAddress::namespace(missing),
            ObjectAddress::constraint(missing),
            ObjectAddress::proc(missing),
            ObjectAddress::new(ObjClass::AttrDefault, missing),
            ObjectAddress::new(ObjClass::Role, missing),
            ObjectAddress::new(ObjClass::Database, missing),
            ObjectAddress::new(ObjClass::Collation, missing),
            ObjectAddress::new(ObjClass::Language, missing),
            ObjectAddress::new(ObjClass::Rewrite, missing),
            ObjectAddress::new(ObjClass::Trigger, missing),
        ];
        for addr in addrs {
            assert_eq!(describe(&env, &addr)?, None, "{addr}");
            assert_eq!(type_description(&env, &addr)?, None, "{addr}");
        }
        Ok(())
    }

    #[test]
    fn builtin_objects() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        assert_eq!(expect_desc(&env, &ObjectAddress::namespace(builtin::PG_CATALOG_NAMESPACE))?, "schema pg_catalog");
        assert_eq!(expect_desc(&env, &ObjectAddress::new(ObjClass::Language, builtin::LANG_SQL))?, "language sql");
        assert_eq!(expect_desc(&env, &ObjectAddress::type_(23))?, "type integer");
        assert_eq!(expect_desc(&env, &ObjectAddress::relation(1259))?, "table pg_class");
        assert_eq!(expect_desc(&env, &ObjectAddress::proc(31))?, "function byteaout(bytea)");
        assert_eq!(expect_desc(&env, &ObjectAddress::new(ObjClass::Role, builtin::BOOTSTRAP_SUPERUSER))?, "role postgres");
        assert_eq!(
            expect_desc(&env, &ObjectAddress::new(ObjClass::Database, builtin::POSTGRES_DATABASE))?,
            "database postgres"
        );
        assert_eq!(expect_desc(&env, &ObjectAddress::new(ObjClass::Collation, builtin::C_COLLATION))?, "collation \"C\"");
        assert_eq!(type_description(&env, &ObjectAddress::proc(31))?, Some("function"));
        assert_eq!(type_description(&env, &ObjectAddress::type_(23))?, Some("type"));
        assert_eq!(type_description(&env, &ObjectAddress::namespace(builtin::PG_CATALOG_NAMESPACE))?, Some("schema"));
        Ok(())
    }

    #[test]
    fn table_objects() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (table, env) = make_t(&mut kv, &env, builtin::PUBLIC_NAMESPACE)?;
        let snap = env.snapshot();
        let rel = snap.relation(table).ok_or_else(|| PgError::internal("table missing from snapshot"))?;
        let pk = snap.primary_key(table).ok_or_else(|| PgError::internal("primary key missing"))?;
        let b_default = rel
            .column(2)
            .and_then(|c| c.default.as_ref())
            .map(|d| d.oid)
            .ok_or_else(|| PgError::internal("default of b missing"))?;

        assert_eq!(expect_desc(&env, &ObjectAddress::relation(table))?, "table t");
        assert_eq!(expect_desc(&env, &ObjectAddress::column(table, 2))?, "column b of table t");
        assert_eq!(type_description(&env, &ObjectAddress::column(table, 2))?, Some("table column"));
        assert_eq!(expect_desc(&env, &ObjectAddress::new(ObjClass::AttrDefault, b_default))?, "default value for column b of table t");
        assert_eq!(type_description(&env, &ObjectAddress::new(ObjClass::AttrDefault, b_default))?, Some("default value"));
        assert_eq!(expect_desc(&env, &ObjectAddress::constraint(pk.oid))?, "constraint t_pkey on table t");
        assert_eq!(type_description(&env, &ObjectAddress::constraint(pk.oid))?, Some("table constraint"));
        assert_eq!(expect_desc(&env, &ObjectAddress::relation(pk.index))?, "index t_pkey");
        assert_eq!(type_description(&env, &ObjectAddress::relation(pk.index))?, Some("index"));
        assert_eq!(expect_desc(&env, &ObjectAddress::type_(rel.rowtype))?, "type t");
        assert_eq!(expect_desc(&env, &ObjectAddress::type_(rel.array_type))?, "type t[]");
        Ok(())
    }

    #[test]
    fn invisible_schema_is_qualified() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut schema = None;
        let (_res, env) = testing::run(&mut kv, &env, |t| {
            schema = t.create_schema("zz_scratch", builtin::BOOTSTRAP_SUPERUSER, false)?;
            Ok(())
        })?;
        let ns = schema.ok_or_else(|| PgError::internal("create_schema returned no schema"))?;
        let (table, env) = make_t(&mut kv, &env, ns)?;

        assert_eq!(expect_desc(&env, &ObjectAddress::namespace(ns))?, "schema zz_scratch");
        assert_eq!(expect_desc(&env, &ObjectAddress::relation(table))?, "table zz_scratch.t");
        assert_eq!(expect_desc(&env, &ObjectAddress::column(table, 1))?, "column id of table zz_scratch.t");
        Ok(())
    }
}
