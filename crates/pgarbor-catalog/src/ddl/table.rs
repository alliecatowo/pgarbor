//! CREATE TABLE. Lease CAT-11 (sonnet: many interacting pieces). PG sources:
//! `commands/tablecmds.c` (`DefineRelation`, `MergeAttributes`, `AddRelationNewConstraints` via
//! heap.c), `catalog/heap.c` (`heap_create_with_catalog`, `AddNewAttributeTuples`,
//! `StoreAttrDefault`, `CheckAttributeNamesTypes`), `parser/parse_utilcmd.c` (`transformColumnDefinition`:
//! serial and identity expansion, `generateSerialExtraStmts`), `catalog/pg_type.c` (`TypeCreate`,
//! `makeArrayTypeName`).
//!
//! ## Algorithm of [`CatalogTxn::create_table`]
//! 1. **Pre-checks (no writes):**
//!    - `spec.kind` must be `Table`; `PartitionedTable`, non-empty `inherits`, and
//!      `Persistence::Temp` -> `Err(PgError::unimplemented("CAT-11", "<what>"))` (partitioning,
//!      inheritance and temp schemas are later work).
//!    - `check_creatable_namespace`, `require_schema_create(namespace)`,
//!      `check_new_relation_name(namespace, name, if_not_exists)` (clash with relations -> 42P07;
//!      with types -> 42710; with `if_not_exists` -> NOTICE and `Ok(None)`).
//!    - `common::validate_column_specs`.
//!    - Constraint list sanity (`ConstraintSpec`): at most ONE primary key, else 42P16
//!      `multiple primary keys for table "t" are not allowed`; two constraints with the same
//!      explicit name -> 42710 `check constraint "c" already exists` for CHECKs
//!      (`errors::check_constraint_already_exists`), `constraint "c" for relation "t" already exists`
//!      otherwise; key columns must exist (42703 `column "x" named in key does not exist`,
//!      `errors::key_column_does_not_exist`); duplicate key columns -> 42701 `column "a" appears twice
//!      in unique constraint` / `primary key constraint` (`errors::column_appears_twice`).
//! 2. **OIDs and names:** table OID, rowtype OID, array type OID (all `new_oid`). The array type name
//!    is `naming::choose_array_type_name(snapshot, ns, name, &[])` (stored in
//!    `Relation::array_type_name` only when not canonical).
//! 3. **Columns** (attnum = position + 1; column count <= 1600): collation = explicit or the type's
//!    `TypeInfo::collation` (via `env.type_info`); `ndims` copied.
//!    - `serial`: the column type is `serial.type_oid()`, NOT NULL (+ constraint), default
//!      `nextval('<seq>'::regclass)` where `<seq>` = `naming::choose_relation_name(snapshot, ns,
//!      table, Some(col), "seq", &chosen)` (`t_id_seq`); the sequence is created with
//!      `Sequence{type_oid = column type, owned_by = (table, attnum), identity = false}` (AUTO
//!      dependency on the column) and `SequenceOptions::default()`. The default's `StoredExpr` is
//!      `nextval('t_id_seq'::regclass)` with `depends_on = [ObjectAddress::relation(seq)]`
//!      (regclass literal printed schema-qualified only if the sequence's schema is not visible in
//!      the creating session's search_path - PG prints the literal at deparse time; PGArbor fixes the
//!      text at creation: documented divergence, see contracts/crates/pgarbor-catalog.md).
//!    - `identity`: `Column::identity = Some(Identity{kind, sequence})`, NOT NULL (+ constraint
//!      `t_d_not_null`), no default; sequence `t_d_seq` with `identity = true` and the user's
//!      `SequenceOptions` (`ddl::sequence` validates; identity sequences take their type from the
//!      column unless `AS` given).
//!    - `generated`: `Column::generated = Some(kind)` and the expression stored in `Column::default`
//!      (`ColumnDefault{oid: new_oid, expr}`), as PG stores it in pg_attrdef.
//!    - plain `default`: `ColumnDefault{oid: new_oid, expr: bind_self_deps(expr, table_oid)}`.
//!    - `not_null`: `Column::not_null = true` and a `NotNull` constraint named
//!      `<table>_<col>_not_null` (or the given `CONSTRAINT name NOT NULL`).
//!    The rowtype of the table is implicit: `Relation::{rowtype, array_type}` hold the OIDs and
//!    `CatalogSnapshot::build` derives the pg_type rows.
//! 4. Build and `put` the `Relation` (kind Table, owner, persistence, options, `view = None`,
//!    `replica_identity = Default`).
//! 5. **Sequences** for serial/identity columns are created AFTER the table row exists (their
//!    `owned_by` must reference it) with `create_sequence` (in column order); then
//!    `update_relation` sets `Column::identity.sequence`. Each sequence queues
//!    `FollowUp::CreateSequenceState`.
//! 6. **Constraints** in PG's creation order: all CHECKs (in spec order), then NOT NULL constraints
//!    (columns in attnum order: explicit/implicit/identity/serial, then PRIMARY KEY columns that
//!    lack one - oracle: `create table t(id int primary key)` creates `t_id_not_null` too), then
//!    PRIMARY KEY / UNIQUE (each creates its backing index), then FOREIGN KEYs. Implemented by
//!    `CatalogTxn::add_constraints_to_new_table` (ddl::constraint, lease CAT-12) which this function
//!    calls once.
//! 7. Records `created` for the table, sequences, indexes, constraints (via the callee) and returns
//!    [`CreatedTable`]. No `BuildIndex`/validation follow-ups: the table is empty.
//!
//! ## Oracle-verified details (PG 18.6)
//! - Objects created for `create table t(id serial primary key, a int not null, b text unique,
//!   c int check (c>0), d int generated always as identity, e int generated always as (a*2) stored,
//!   f int references t(id) on delete cascade)`:
//!   relations `t_id_seq` (S), `t_d_seq` (S), `t` (r), `t_pkey` (i), `t_b_key` (i); constraints
//!   `t_c_check` (c, conkey {4}), `t_id_not_null`, `t_a_not_null`, `t_d_not_null` (n), `t_pkey` (p),
//!   `t_b_key` (u), `t_f_fkey` (f); `attnotnull`: id, a, d true; e false (generated column not
//!   NOT NULL); `attidentity` d = 'a'; `attgenerated` e = 's'.
//! - Errors: duplicate column `column "a" specified more than once` (42701); `xmin` as a column
//!   name -> 42701 `column name "xmin" conflicts with a system column name`; unknown referenced
//!   table is the binder's 42P01.
use super::spec::*;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::{TypeEnv, TypeId};
use std::collections::HashSet;

/// A serial/identity sequence to create after the table row exists.
struct SeqPlan {
    /// Index into the column list.
    idx: usize,
    name: String,
    type_oid: Oid,
    options: SequenceOptions,
    identity: Option<IdentityKind>,
}

/// Pure statement-level sanity checks of the constraint list (module docs, step 1).
fn validate_constraint_list(table: &str, columns: &[ColumnSpec], constraints: &[ConstraintSpec]) -> Result<()> {
    let col_exists = |n: &str| columns.iter().any(|c| c.name == n);
    let mut pk_seen = false;
    let mut names: HashSet<&str> = HashSet::new();
    for c in columns {
        if let Some(Some(n)) = &c.not_null {
            names.insert(n.as_str());
        }
    }
    for c in constraints {
        match &c.kind {
            ConstraintSpecKind::PrimaryKey { columns: cols, include } => {
                if pk_seen {
                    return Err(errors::multiple_primary_keys(table));
                }
                pk_seen = true;
                check_key_columns(&col_exists, cols, include, "primary key constraint")?;
            }
            ConstraintSpecKind::Unique { columns: cols, include, .. } => {
                check_key_columns(&col_exists, cols, include, "unique constraint")?;
            }
            ConstraintSpecKind::ForeignKey { columns: cols, delete_set_columns, .. } => {
                for n in cols.iter().chain(delete_set_columns.iter()) {
                    if !col_exists(n) {
                        return Err(PgError::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!("column \"{n}\" referenced in foreign key constraint does not exist"),
                        ));
                    }
                }
            }
            ConstraintSpecKind::NotNull { column } => {
                if !col_exists(column) {
                    return Err(errors::key_column_does_not_exist(column));
                }
            }
            ConstraintSpecKind::Check { .. } => {}
            ConstraintSpecKind::Exclusion => {}
        }
        if let Some(n) = &c.name {
            if !names.insert(n.as_str()) {
                return Err(if matches!(c.kind, ConstraintSpecKind::Check { .. }) {
                    errors::check_constraint_already_exists(n)
                } else {
                    errors::constraint_already_exists(n, table)
                });
            }
        }
    }
    Ok(())
}

fn check_key_columns(
    col_exists: &dyn Fn(&str) -> bool,
    cols: &[String],
    include: &[String],
    what: &str,
) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for n in cols.iter().chain(include.iter()) {
        if !col_exists(n) {
            return Err(errors::key_column_does_not_exist(n));
        }
        if !seen.insert(n.as_str()) {
            return Err(errors::column_appears_twice(n, what));
        }
    }
    Ok(())
}

/// Default text of a serial column as PG prints it: the regclass literal is schema-qualified only
/// when the sequence's schema is not on the search path.
fn regclass_literal(name: &str, schema: &str, visible: bool) -> Result<String> {
    let q = if visible {
        pgarbor_types::text::format::quote_identifier(name)?
    } else {
        format!(
            "{}.{}",
            pgarbor_types::text::format::quote_identifier(schema)?,
            pgarbor_types::text::format::quote_identifier(name)?
        )
    };
    Ok(format!("nextval('{}'::regclass)", q.replace('\'', "''")))
}

impl<'a> CatalogTxn<'a> {
    /// CREATE TABLE (module docs). `Ok(None)` only for `IF NOT EXISTS` on an existing relation.
    // CONTRACT
    pub fn create_table(&mut self, spec: &CreateTableSpec) -> Result<Option<CreatedTable>> {
        // 1. pre-checks
        match spec.kind {
            RelKind::Table => {}
            RelKind::PartitionedTable => return Err(PgError::unimplemented("CAT-11", "partitioned tables")),
            _ => return Err(PgError::internal("CREATE TABLE with a non-table relation kind")),
        }
        if !spec.inherits.is_empty() {
            return Err(PgError::unimplemented("CAT-11", "table inheritance"));
        }
        if spec.persistence == Persistence::Temp {
            return Err(PgError::unimplemented("CAT-11", "temporary tables"));
        }
        self.check_creatable_namespace(spec.namespace, &spec.name)?;
        self.require_schema_create(spec.namespace)?;
        if !self.check_new_relation_name(spec.namespace, &spec.name, spec.if_not_exists)? {
            return Ok(None);
        }
        let env = self.env()?;
        super::common::validate_column_specs(&env, &spec.name, &spec.columns)?;
        validate_constraint_list(&spec.name, &spec.columns, &spec.constraints)?;
        for c in &spec.columns {
            if c.identity.is_some() {
                let t = c.serial.map(|s| s.type_oid()).unwrap_or(c.type_id.oid);
                if !matches!(t, 21 | 23 | 20) {
                    return Err(PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "identity column type must be smallint, integer, or bigint",
                    ));
                }
            }
        }

        // 2. OIDs and names
        let ns = spec.namespace;
        let table_oid = self.new_oid()?;
        let rowtype = self.new_oid()?;
        let array_type = self.new_oid()?;
        let snap = self.snapshot()?;
        let arr_name = naming::choose_array_type_name(&snap, ns, &spec.name, &[])?;
        let array_type_name = if arr_name == naming::array_type_name(&spec.name) { None } else { Some(arr_name) };

        // 3. columns
        let mut columns: Vec<Column> = Vec::with_capacity(spec.columns.len());
        let mut plans: Vec<SeqPlan> = Vec::new();
        let mut chosen: Vec<String> = vec![spec.name.clone()];
        let mut notnull_cols: Vec<(usize, Option<String>)> = Vec::new();
        for (i, c) in spec.columns.iter().enumerate() {
            let attnum = (i + 1) as i16;
            let type_id = match c.serial {
                Some(s) => TypeId::of(s.type_oid()),
                None => c.type_id,
            };
            let collation = match c.collation {
                Some(o) => o,
                None => match env.type_info(type_id.oid) {
                    Some(ti) => ti.collation,
                    None => return Err(errors::cache_lookup_failed("type", type_id.oid)),
                },
            };
            let mut col = Column::new(attnum, c.name.clone(), type_id, collation);
            col.ndims = c.ndims;
            col.storage = c.storage;
            col.compression = c.compression;
            let mut needs_nn = false;
            let mut nn_name: Option<String> = None;
            if let Some(n) = &c.not_null {
                needs_nn = true;
                nn_name = n.clone();
            }
            if c.serial.is_some() {
                needs_nn = true;
                let name = naming::choose_relation_name(&snap, ns, &spec.name, Some(&c.name), "seq", &chosen)?;
                chosen.push(name.clone());
                plans.push(SeqPlan {
                    idx: i,
                    name,
                    type_oid: type_id.oid,
                    options: SequenceOptions::default(),
                    identity: None,
                });
            } else if let Some(id) = &c.identity {
                needs_nn = true;
                let name = naming::choose_identity_sequence_name(
                    &snap,
                    ns,
                    &spec.name,
                    Some(&c.name),
                    id.sequence_name.as_deref(),
                    &chosen,
                )?;
                chosen.push(name.clone());
                let t = id.options.type_oid.unwrap_or(type_id.oid);
                plans.push(SeqPlan {
                    idx: i,
                    name,
                    type_oid: t,
                    options: id.options.clone(),
                    identity: Some(id.kind),
                });
            } else if let Some(g) = &c.generated {
                col.generated = Some(g.kind);
                col.default = Some(ColumnDefault {
                    oid: self.new_oid()?,
                    expr: CatalogTxn::bind_self_deps(&g.expr, table_oid),
                });
            } else if let Some(d) = &c.default {
                col.default = Some(ColumnDefault {
                    oid: self.new_oid()?,
                    expr: CatalogTxn::bind_self_deps(d, table_oid),
                });
            }
            if needs_nn {
                notnull_cols.push((i, nn_name));
            }
            columns.push(col);
        }

        // 4. the relation row. NOT NULL flags are set by the constraint creator (and re-asserted below).
        self.put(CatalogObject::Relation(Relation {
            oid: table_oid,
            name: spec.name.clone(),
            namespace: ns,
            kind: RelKind::Table,
            owner: spec.owner,
            persistence: spec.persistence,
            columns,
            rowtype,
            array_type,
            array_type_name,
            options: spec.options.clone(),
            acl: None,
            replica_identity: ReplicaIdentity::Default,
            row_security: false,
            force_row_security: false,
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        }))?;
        self.record_created(ObjectAddress::relation(table_oid));

        // 5. sequences
        let mut sequences: Vec<Oid> = Vec::with_capacity(plans.len());
        for p in &plans {
            let attnum = (p.idx + 1) as i16;
            let seq = self.create_column_sequence(
                ns,
                &p.name,
                spec.owner,
                spec.persistence,
                p.type_oid,
                &p.options,
                (table_oid, attnum),
                p.identity.is_some(),
            )?;
            sequences.push(seq);
            let idx = p.idx;
            if let Some(kind) = p.identity {
                self.update_relation(table_oid, |r| {
                    if let Some(c) = r.columns.get_mut(idx) {
                        c.identity = Some(Identity { kind, sequence: seq });
                    }
                    Ok(())
                })?;
            } else {
                let env2 = self.env()?;
                let schema = match env2.snapshot().namespace(ns) {
                    Some(n) => n.name.clone(),
                    None => return Err(errors::cache_lookup_failed("schema", ns)),
                };
                let visible = env2.search_path().contains(&ns);
                let sql = regclass_literal(&p.name, &schema, visible)?;
                let expr = StoredExpr::new(sql).with_deps(vec![ObjectAddress::relation(seq)]);
                let def_oid = self.new_oid()?;
                self.update_relation(table_oid, |r| {
                    if let Some(c) = r.columns.get_mut(idx) {
                        c.default = Some(ColumnDefault { oid: def_oid, expr });
                    }
                    Ok(())
                })?;
            }
        }

        // 6. constraints in PG order: CHECKs, NOT NULLs, PRIMARY KEY / UNIQUE, FOREIGN KEYs
        let mut specs: Vec<ConstraintSpec> = Vec::new();
        for c in &spec.constraints {
            if matches!(c.kind, ConstraintSpecKind::Check { .. }) {
                specs.push(c.clone());
            }
        }
        let explicit_nn: HashSet<&str> = spec
            .constraints
            .iter()
            .filter_map(|c| match &c.kind {
                ConstraintSpecKind::NotNull { column } => Some(column.as_str()),
                _ => None,
            })
            .collect();
        let mut nn_columns: Vec<usize> = Vec::new();
        for (i, name) in &notnull_cols {
            if explicit_nn.contains(spec.columns[*i].name.as_str()) {
                continue;
            }
            nn_columns.push(*i);
            let mut cs = ConstraintSpec::new(ConstraintSpecKind::NotNull { column: spec.columns[*i].name.clone() });
            cs.name = name.clone();
            specs.push(cs);
        }
        for c in &spec.constraints {
            if let ConstraintSpecKind::NotNull { column } = &c.kind {
                if let Some(i) = spec.columns.iter().position(|x| &x.name == column) {
                    nn_columns.push(i);
                }
                specs.push(c.clone());
            }
        }
        for c in &spec.constraints {
            if matches!(c.kind, ConstraintSpecKind::PrimaryKey { .. } | ConstraintSpecKind::Unique { .. }) {
                specs.push(c.clone());
            }
        }
        for c in &spec.constraints {
            if matches!(c.kind, ConstraintSpecKind::ForeignKey { .. } | ConstraintSpecKind::Exclusion) {
                specs.push(c.clone());
            }
        }
        let constraints = self.add_constraints_to_new_table(table_oid, &specs)?;

        // Re-assert Column::not_null for the folded NOT NULL columns.
        let need_fix = match self.data().relations.get(&table_oid) {
            Some(r) => nn_columns.iter().any(|i| r.columns.get(*i).map(|c| !c.not_null).unwrap_or(false)),
            None => return Err(errors::cache_lookup_failed("relation", table_oid)),
        };
        if need_fix {
            self.update_relation(table_oid, |r| {
                for i in &nn_columns {
                    if let Some(c) = r.columns.get_mut(*i) {
                        c.not_null = true;
                    }
                }
                Ok(())
            })?;
        }

        // 7. result
        let indexes: Vec<Oid> =
            self.data().indexes.values().filter(|ix| ix.table == table_oid).map(|ix| ix.oid).collect();
        Ok(Some(CreatedTable { table: table_oid, sequences, indexes, constraints }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn spec(name: &str, columns: Vec<ColumnSpec>, constraints: Vec<ConstraintSpec>) -> CreateTableSpec {
        CreateTableSpec {
            namespace: crate::builtin::PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns,
            constraints,
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    fn int_col(name: &str) -> ColumnSpec {
        ColumnSpec::new(name, TypeId::INT4)
    }

    fn pk(cols: &[&str]) -> ConstraintSpec {
        ConstraintSpec::new(ConstraintSpecKind::PrimaryKey {
            columns: cols.iter().map(|s| s.to_string()).collect(),
            include: Vec::new(),
        })
    }

    #[test]
    fn constraint_list_two_primary_keys() {
        let e = validate_constraint_list("t", &[int_col("a"), int_col("b")], &[pk(&["a"]), pk(&["b"])]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_TABLE_DEFINITION);
        assert_eq!(e.message, "multiple primary keys for table \"t\" are not allowed");
    }

    #[test]
    fn constraint_list_unknown_and_duplicate_key_columns() {
        let e = validate_constraint_list("t", &[int_col("a")], &[pk(&["x"])]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_COLUMN);
        assert_eq!(e.message, "column \"x\" named in key does not exist");
        let e = validate_constraint_list("t", &[int_col("a")], &[pk(&["a", "a"])]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DUPLICATE_COLUMN);
        assert_eq!(e.message, "column \"a\" appears twice in primary key constraint");
    }

    #[test]
    fn constraint_list_duplicate_check_name() {
        let chk = |n: &str| {
            let mut c = ConstraintSpec::new(ConstraintSpecKind::Check {
                expr: StoredExpr::new("(a > 0)"),
                columns: vec!["a".to_string()],
                no_inherit: false,
            });
            c.name = Some(n.to_string());
            c
        };
        let e = validate_constraint_list("t", &[int_col("a")], &[chk("c"), chk("c")]).unwrap_err();
        assert_eq!(e.message, "check constraint \"c\" already exists");
    }

    #[test]
    fn regclass_text() {
        assert_eq!(regclass_literal("t_id_seq", "public", true).unwrap(), "nextval('t_id_seq'::regclass)");
        assert_eq!(regclass_literal("t_id_seq", "s", false).unwrap(), "nextval('s.t_id_seq'::regclass)");
        assert_eq!(regclass_literal("T_seq", "public", true).unwrap(), "nextval('\"T_seq\"'::regclass)");
    }

    #[test]
    fn unsupported_forms() {
        let (mut kv, env) = testing::bootstrapped().unwrap();
        let mut s = spec("t", vec![int_col("a")], vec![]);
        s.persistence = Persistence::Temp;
        let r = testing::run(&mut kv, &env, |t| t.create_table(&s).map(|_| ()));
        assert_eq!(r.err().map(|e| e.sqlstate), Some(SqlState::FEATURE_NOT_SUPPORTED));
        let mut s = spec("t", vec![int_col("a")], vec![]);
        s.kind = RelKind::PartitionedTable;
        let r = testing::run(&mut kv, &env, |t| t.create_table(&s).map(|_| ()));
        assert_eq!(r.err().map(|e| e.sqlstate), Some(SqlState::FEATURE_NOT_SUPPORTED));
    }

    #[test]
    fn create_simple_and_serial() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut id = int_col("id");
        id.serial = Some(SerialKind::Int);
        let s = spec("t", vec![id, int_col("a")], vec![pk(&["id"])]);
        let mut created = None;
        let (res, env) = testing::run(&mut kv, &env, |t| {
            created = t.create_table(&s)?;
            Ok(())
        })?;
        let created = created.expect("table created");
        assert_eq!(created.sequences.len(), 1);
        assert_eq!(created.indexes.len(), 1);
        let rel = res.snapshot.data().relations.get(&created.table).expect("relation");
        assert_eq!(rel.columns.len(), 2);
        assert!(rel.columns[0].not_null);
        let d = rel.columns[0].default.as_ref().expect("serial default");
        assert_eq!(d.expr.sql, "nextval('t_id_seq'::regclass)");
        assert_eq!(d.expr.depends_on, vec![ObjectAddress::relation(created.sequences[0])]);
        let seq = res.snapshot.data().sequences.get(&created.sequences[0]).expect("sequence");
        assert_eq!(seq.name, "t_id_seq");
        assert_eq!(seq.owned_by, Some((created.table, 1)));
        assert!(res.outcome.follow_ups.contains(&FollowUp::CreateSequenceState { sequence: created.sequences[0] }));

        // existing relation
        let r = testing::run(&mut kv, &env, |t| t.create_table(&s).map(|_| ()));
        assert_eq!(r.err().map(|e| e.sqlstate), Some(SqlState::DUPLICATE_TABLE));
        // IF NOT EXISTS
        let mut s2 = s.clone();
        s2.if_not_exists = true;
        let mut out = Some(CreatedTable::default());
        let (res2, _env) = testing::run(&mut kv, &env, |t| {
            out = t.create_table(&s2)?;
            Ok(())
        })?;
        assert!(out.is_none());
        assert_eq!(res2.outcome.notices.len(), 1);
        Ok(())
    }
}
