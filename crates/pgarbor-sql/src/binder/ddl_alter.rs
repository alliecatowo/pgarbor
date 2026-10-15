//! ALTER TABLE / ALTER VIEW / ALTER INDEX sub-commands (SQL-33). PG references: `tablecmds.c`
//! (`AlterTable`, `ATPrepCmd`, `ATExecCmd` and every `ATExec*`), `parse_utilcmd.c`
//! (`transformAlterTableStmt`, `transformColumnDefinition` reuse), `gram.y` (`alter_table_cmd`).
//!
//! `bind_alter_table(b, at)` (sqlparser `AlterTable { name, if_exists, only, operations, .. }`)
//! produces `BoundStatement::AlterTable(Box<BoundAlterTable { relation, keyword, commands }>)`:
//! * Resolve the relation (`env.lookup_class`): missing + `IF EXISTS` -> `BoundStatement::Noop { tag:
//!   "ALTER TABLE", notices: [relation "x" does not exist, skipping] }`; missing otherwise 42P01.
//!   `ALTER TABLE` on a view/index/sequence: 42809 `"v" is not a table` HINT `Use ALTER VIEW to change a
//!   view.`-style (`ATWrongRelkindError`); `keyword` = the word used.
//! * Sub-commands are bound SEQUENTIALLY against an evolving in-memory copy of the relation
//!   (list of `(name, TypeId, dropped)` columns + constraint names) so that `ADD COLUMN b int, ADD
//!   CHECK (b > 0)` binds the CHECK against the new column and `DROP COLUMN a, ADD COLUMN a text`
//!   works. Positions refer to the original statement text.
//! * `ONLY` accepted (no inheritance). `table_type`, `on_cluster`, `location`: 42601.
//!
//! ## Supported operations -> `AlterTableCmd`
//! | sqlparser | command | notes |
//! |---|---|---|
//! | `AddColumn { if_not_exists, column_def, column_position }` | `AddColumn { spec, if_not_exists }` | same ColumnDef binding as CREATE TABLE (`ddl_table::bind_column_def` equivalent, shared helper inside this module's reads); `ColumnSpec.fast_default`/`default_is_volatile` computed here: evaluate the bound default at bind time when it contains no volatile function (`BoundExpr::volatility() != Volatile`) using `io::output` of the folded constant; `FIRST`/`AFTER` positions are MySQL: 42601 |
//! | `DropColumn { column_names, if_exists, drop_behavior }` | one `DropColumn` per name | `CASCADE`/`RESTRICT` |
//! | `RenameColumn { old_column_name, new_column_name }` | `RenameColumn` | |
//! | `AlterColumn { column_name, op }` | by `AlterColumnOperation`: `SetNotNull`->`SetNotNull`; `DropNotNull`->`DropNotNull`; `SetDefault{value}`->`SetDefault{expr: Some(..)}` (bound with `ExprCtx::column_default()`, assignment-coerced); `DropDefault`->`SetDefault{expr: None}`; `SetDataType{data_type, using, had_set}`->`AlterColumnType` (see below); `AddGenerated{generated_as, sequence_options}`->`AddIdentity`; identity `SET GENERATED ALWAYS|BY DEFAULT`->`SetIdentity`; `DROP IDENTITY [IF EXISTS]`->`DropIdentity`; `SET EXPRESSION AS (e)`->`SetExpression`; `DROP EXPRESSION`->`DropExpression`; `SET STATISTICS n`/`SET STORAGE`/`SET COMPRESSION`/`SET (opts)`->`SetColumnAttribute`; others 0A000 |
//! | `AddConstraint { constraint, not_valid }` | `AddConstraint(ConstraintSpec)` via `ddl_constraint::bind_table_constraint` | `ADD COLUMN`-less `ADD PRIMARY KEY (a)`, `ADD CONSTRAINT .. FOREIGN KEY`, `ADD CHECK`, `ADD UNIQUE`; `NOT VALID` |
//! | `DropConstraint { name, if_exists, drop_behavior }` | `DropConstraint` | |
//! | `RenameConstraint { old_name, new_name }` | `RenameConstraint` | |
//! | `ValidateConstraint { name }` | `ValidateConstraint` | |
//! | `SetLogged` / `SetUnlogged` | `SetLogged(bool)` | |
//! | `OwnerTo { new_owner }` | the separate `BoundStatement::Alter(AlterAction::RelationOwner)`; if mixed with other commands bind each part in order (PG allows `ALTER TABLE t OWNER TO x, ADD COLUMN ..`; emit the owner change as `AlterTableCmd`? NOT representable: 0A000 when combined) | |
//! | `RenameTable { table_name }` | `BoundStatement::Alter(AlterAction::RenameRelation)` (only when it is the single operation) | |
//! | `EnableRowLevelSecurity`/`DisableRowLevelSecurity`/`ForceRowLevelSecurity`/`NoForceRowLevelSecurity` | `RowSecurity{..}` | |
//! | `ReplicaIdentity { .. }` | `ReplicaIdentity(..)` | |
//! | `SetOptionsParens { options }` / `SetTblProperties` | `SetOptions { set, reset }` (`RESET (..)` forms) | |
//! | others (`AttachPartition`, `DetachPartition`, triggers, rules, `ClusterBy`, `SwapWith`, Hive/MySQL/ClickHouse/Snowflake operations, `DropPrimaryKey`, `DropForeignKey`, `DropIndex`, `ChangeColumn`, `ModifyColumn`, `Algorithm`, `Lock`, `AutoIncrement`, `AddPartitions`...) | PostgreSQL-valid ones (`ATTACH PARTITION`, `ENABLE TRIGGER`, `INHERIT`, `SET TABLESPACE`, `CLUSTER ON`): 0A000 naming them; non-PG (MySQL/Hive/...): 42601 | |
//!
//! ## `ALTER COLUMN ... TYPE new_type [COLLATE c] [USING expr]`
//! Resolve the type (`types::resolve_data_type`). Bind `USING expr` (if present) with the table scope;
//! otherwise the implicit conversion is an ASSIGNMENT cast column -> new type (42804 `column "a" cannot be
//! cast automatically to type integer` HINT `You might need to specify "USING a::integer".` when no
//! assignment cast exists). `needs_rewrite`: true unless the old->new conversion is binary-coercible with
//! the same representation (`cast::is_binary_coercible` and equal typmod/array rules; `varchar(10)` ->
//! `varchar(20)`/`text` no rewrite; `int4`->`int8` rewrite since `Datum::Int4` != `Int8`). `new_default`:
//! `Some(Some(expr))` if the column default is re-bound to the new type, `None` to leave untouched (42804
//! `default for column "a" cannot be cast automatically to type X` otherwise).
//! `new_ndims` = declared `[]` count.
//!
//! The command list is `commands`; sub-commands that produce a separate `AlterAction` variant make the
//! whole statement `BoundStatement::Alter` (they cannot be mixed: 0A000 `combining this ALTER TABLE
//! action with others is not supported by PGArbor yet`).
//!
//! ## ALTER VIEW / ALTER INDEX
//! [`bind_alter_view`]: `RENAME TO`, `RENAME COLUMN`, `OWNER TO`, `SET SCHEMA`, `SET (opt=..)`/`RESET`, `ALTER COLUMN .. SET
//! DEFAULT` -> as above with `keyword = View`. [`bind_alter_index`]: `RENAME TO`, `SET (..)`/`RESET (..)`,
//! `SET TABLESPACE` (0A000), `ATTACH PARTITION` (0A000), `ALTER COLUMN SET STATISTICS`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{AlterTable, Statement};

use super::context::Binder;
use crate::ir::BoundStatement;

/// ALTER TABLE.
#[allow(unused_variables)]
pub fn bind_alter_table(b: &mut Binder, at: &AlterTable) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-33", "ALTER TABLE binding (ddl_alter::bind_alter_table)"))
}

/// `Statement::AlterView` (module docs).
#[allow(unused_variables)]
pub fn bind_alter_view(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-33", "ALTER VIEW binding (ddl_alter::bind_alter_view)"))
}

/// `Statement::AlterIndex` (module docs).
#[allow(unused_variables)]
pub fn bind_alter_index(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-33", "ALTER INDEX binding (ddl_alter::bind_alter_index)"))
}
