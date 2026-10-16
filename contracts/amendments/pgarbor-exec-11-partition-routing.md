# pgarbor-exec amendment: tuple routing and partition constraints

- `StmtState.partitions` (`partition::PartCache`) caches partition descriptions per statement; INSERT into a
  partitioned table routes each row to its leaf (23514 `no partition of relation ... found for row` otherwise).
- `InsertedRow.table` names the relation that stored the row (the leaf for routed inserts) so RETURNING,
  constraints and index maintenance use it.
- `FollowUp::ValidatePartitionRows { table, default_update }` checks existing rows against a new partition
  constraint (ATTACH PARTITION, default partition updates).
- `dml_inherit` executes UPDATE / DELETE on inheritance trees; `CatalogTxn::take_notices` forwards catalog
  notices (e.g. constraint merge) to the session.
