# pgarbor-catalog amendment: policies and rules in the relation model

- `Relation.policies: Vec<Policy>` and `Relation.rules: Vec<Rule>` (serde default); OIDs counted by max_oid/oid_in_use.
- `ddl::policy` (create/alter/rename/drop policy); `vrel::pg_policy` producers for pg_policy, pg_policies, pg_rules.
- `CatalogEnv::{with_row_security, row_security}` carries the `row_security` GUC.
