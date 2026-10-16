# pgarbor-sql amendment: table hierarchies

- `parser_hier` rewrites `INHERITS`, `PARTITION BY`, `PARTITION OF ... FOR VALUES` and `ATTACH / DETACH PARTITION`
  into forms sqlparser accepts; `binder::ddl_partition` binds them into `CreateTableSpec.extra`
  (`CreateTableExtra { partition_key, partition_of, on_commit }`).
- `BoundAlterTable.only` records `ALTER TABLE ONLY`; `ON COMMIT` on a permanent table is 42P16.
- `ALTER TABLE` commands PG propagates to children are 0A000 (EXE-33) until recursion exists; inherited columns
  and constraints cannot be dropped / renamed / retyped from a child (42P16).
