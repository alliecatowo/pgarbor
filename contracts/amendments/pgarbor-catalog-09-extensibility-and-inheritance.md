# pgarbor-catalog amendment: extensibility objects, inheritance and partitioning

Extensibility objects (`ExtObject`, `ddl::ext*`):
- user operators, aggregates, operator classes / families (with pg_amop / pg_amproc rows), casts, conversions,
  collations and extensions live in `CatalogData`; `ObjClass` gains `OpClass`, `OpFamily`, `AmOp`, `AmProc`,
  `Conversion` (with `Collation`, `Operator`, `Cast`, `Extension`, which now have objects).
- `CatalogTxn::run_ext(ExtDdl)` dispatches CREATE / ALTER / DROP / COMMENT; `CatalogEnv` resolves operators,
  aggregates, collations and conversions, including user ones, along the search path.
- `depend::derive` records edges for these objects; `DROP ROLE` also reports `target of policy p on table t`.
- ICU-provider and nondeterministic user collations are recorded but not honored by comparison: expressions that
  use them are 0A000 (SQL-14); `CREATE COLLATION ... rules` is 0A000 (CAT-EXT-04).

Inheritance and partitioning:
- `Relation.inherits`, `partition_bound`, `partition_key` (`PartitionKey.expr_types`), `Column.is_local /
  inh_count`, `Constraint.is_local / inh_count`; `CatalogSnapshot::{inheritance_children, inheritance_tree,
  ordered_children, partition_ancestors}`.
- `CreateTableSpec.extra` (`CreateTableExtra`), `FollowUp::ValidatePartitionRows`, `CatalogTxn::take_notices`.
- Constraints and indexes on a partitioned table are 0A000 (CAT-13) until partitioned indexes exist.
