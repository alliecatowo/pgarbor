# pgarbor-sql amendment: extensibility statements

- `ext_stmt::ExtStmt` and `parser_ext` parse CREATE / ALTER / DROP OPERATOR, AGGREGATE, OPERATOR CLASS / FAMILY,
  CAST, CONVERSION, COLLATION, EXTENSION and the matching COMMENT ON forms (sqlparser cannot).
- `BoundStatement::Ext(Box<ExtDdl>)` carries the folded inputs (`pgarbor_catalog::ddl::ext_spec`); its tag comes
  from `ExtDdl::tag`.
- Operator names `=>` and names containing `/*` or `--` are syntax errors, as in the PG grammar.
- Token rewrite for operator DDL (`merge_operator_names`) joins symbol runs sqlparser splits (`===`, `!==`).
