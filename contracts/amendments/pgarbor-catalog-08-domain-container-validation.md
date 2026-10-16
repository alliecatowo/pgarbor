# catalog-08: domain validation lists exclude array containers

Decision: ACCEPTED by central integration (2026-10-15).

`CatalogTxn::domain_columns` keeps its signature. Its validation-list behavior must
follow PostgreSQL 18 `get_rels_with_domain`: an array of the domain or a derived
domain blocks constraint validation with 0A000, naming the element type and
containing column. Array values cannot be evaluated as scalar domain values.
This supersedes the method's old comment including arrays in the returned list.
SET DEFAULT remains allowed. No contract file is edited.

Validation: an API regression exercises SET NOT NULL and ADD CHECK through an
array of a derived domain, SET DEFAULT, and successful validation after DROP TABLE.
