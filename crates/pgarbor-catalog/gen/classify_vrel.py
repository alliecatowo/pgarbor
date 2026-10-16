#!/usr/bin/env python3
"""Classification of every system relation (pg_catalog / information_schema table or view) by how
pgarbor-catalog serves it. Used ONCE to generate src/vrel/empty.rs and src/vrel/unsupported.rs and
to check that every relation has exactly one producer. Keep in sync with contracts/crates/pgarbor-catalog.md.

    python3 crates/pgarbor-catalog/gen/classify_vrel.py          # prints coverage, rewrites the two files
"""
import re, os, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
SRC = os.path.join(ROOT, "crates", "pgarbor-catalog", "src")
sysrels = open(os.path.join(SRC, "builtin", "system_rels.rs")).read()
RELS = {}  # (schema, name) -> const name
for sch, name, kind, const, oid in re.findall(r"/// `(\w+)\.(\w+)` \(relkind '(\w)'\)\n\s+pub const (\w+): Oid = (\d+);", sysrels):
    if kind in "rv":
        RELS[(sch, name)] = const

# --- producers (file stem in src/vrel) -> relation names
PRODUCERS = {
    "pg_class": ["pg_namespace", "pg_class", "pg_inherits", "pg_sequence", "pg_rewrite"],
    "pg_attribute": ["pg_attribute", "pg_attrdef"],
    "pg_type": ["pg_type", "pg_enum", "pg_range"],
    "pg_constraint": ["pg_index", "pg_constraint"],
    "pg_depend": ["pg_depend", "pg_description", "pg_shdescription", "pg_shdepend"],
    "pg_proc": ["pg_proc", "pg_aggregate", "pg_operator", "pg_cast", "pg_language"],
    "pg_roles": ["pg_authid", "pg_auth_members", "pg_database", "pg_db_role_setting", "pg_tablespace",
                 "pg_roles", "pg_shadow", "pg_group", "pg_user"],
    "pg_ext": ["pg_amop", "pg_amproc", "pg_conversion"],
    "pg_builtin": ["pg_collation", "pg_am", "pg_opclass", "pg_opfamily", "pg_extension",
                   "pg_available_extensions", "pg_available_extension_versions"],
    "pg_views": ["pg_tables", "pg_views", "pg_matviews", "pg_indexes", "pg_sequences"],
    "runtime": ["pg_settings", "pg_stat_activity", "pg_locks", "pg_prepared_statements", "pg_cursors",
                "pg_stat_ssl", "pg_stat_gssapi", "pg_backend_memory_contexts_NOT"],
    "stats": ["pg_stat_all_tables", "pg_stat_user_tables", "pg_stat_sys_tables", "pg_stat_all_indexes",
              "pg_stat_user_indexes", "pg_stat_sys_indexes", "pg_statio_all_tables", "pg_statio_user_tables",
              "pg_statio_sys_tables", "pg_statio_all_indexes", "pg_statio_user_indexes", "pg_statio_sys_indexes",
              "pg_statio_all_sequences", "pg_statio_user_sequences", "pg_statio_sys_sequences",
              "pg_stat_xact_all_tables", "pg_stat_xact_user_tables", "pg_stat_xact_sys_tables"],
}
IS_PRODUCERS = {
    "is_core": ["schemata", "tables", "views", "sequences", "information_schema_catalog_name", "character_sets",
                "collations", "collation_character_set_applicability", "enabled_roles", "applicable_roles",
                "administrable_role_authorizations", "user_defined_types", "attributes"],
    "is_columns": ["columns", "element_types", "domains", "domain_constraints", "column_domain_usage",
                   "column_udt_usage", "domain_udt_usage", "column_column_usage"],
    "is_constraints": ["table_constraints", "key_column_usage", "referential_constraints",
                       "constraint_column_usage", "constraint_table_usage", "check_constraints"],
    "is_routines": ["routines", "parameters", "routine_column_usage", "routine_routine_usage",
                    "routine_sequence_usage", "routine_table_usage", "view_column_usage", "view_table_usage",
                    "view_routine_usage", "check_constraint_routine_usage"],
    "is_privileges": ["table_privileges", "column_privileges", "role_table_grants", "role_column_grants",
                      "usage_privileges", "role_usage_grants", "routine_privileges", "role_routine_grants",
                      "udt_privileges", "role_udt_grants", "data_type_privileges"],
}

# --- relations that are correctly EMPTY because PGArbor has no such objects (reason shown in docs)
EMPTY = {
    ("pg_catalog", "pg_default_acl"): "ALTER DEFAULT PRIVILEGES is unsupported (binder 0A000), so no default ACL rows can exist",
    ("pg_catalog", "pg_event_trigger"): "event triggers are unsupported",
    ("pg_catalog", "pg_foreign_data_wrapper"): "foreign data wrappers are unsupported",
    ("pg_catalog", "pg_foreign_server"): "foreign servers are unsupported",
    ("pg_catalog", "pg_foreign_table"): "foreign tables are unsupported",
    ("pg_catalog", "pg_user_mapping"): "user mappings are unsupported",
    ("pg_catalog", "pg_largeobject"): "large objects are unsupported",
    ("pg_catalog", "pg_largeobject_metadata"): "large objects are unsupported",
    ("pg_catalog", "pg_parameter_acl"): "GRANT ON PARAMETER is unsupported",
    ("pg_catalog", "pg_partitioned_table"): "partitioned tables are unsupported in v1 (Relation::partition_key is always None)",
    ("pg_catalog", "pg_policy"): "row level security policies are unsupported",
    ("pg_catalog", "pg_publication"): "logical replication is unsupported",
    ("pg_catalog", "pg_publication_namespace"): "logical replication is unsupported",
    ("pg_catalog", "pg_publication_rel"): "logical replication is unsupported",
    ("pg_catalog", "pg_replication_origin"): "replication is unsupported",
    ("pg_catalog", "pg_seclabel"): "security labels are unsupported",
    ("pg_catalog", "pg_shseclabel"): "security labels are unsupported",
    ("pg_catalog", "pg_statistic"): "no planner statistics are collected (ANALYZE writes none)",
    ("pg_catalog", "pg_statistic_ext"): "extended statistics are unsupported",
    ("pg_catalog", "pg_statistic_ext_data"): "extended statistics are unsupported",
    ("pg_catalog", "pg_subscription"): "logical replication is unsupported",
    ("pg_catalog", "pg_subscription_rel"): "logical replication is unsupported",
    ("pg_catalog", "pg_transform"): "transforms are unsupported",
    ("pg_catalog", "pg_trigger"): "triggers are unsupported in v1 (RI triggers are internal and not materialized)",
    ("pg_catalog", "pg_file_settings"): "there is no configuration file",
    ("pg_catalog", "pg_hba_file_rules"): "there is no pg_hba.conf",
    ("pg_catalog", "pg_ident_file_mappings"): "there is no pg_ident.conf",
    ("pg_catalog", "pg_policies"): "row level security policies are unsupported",
    ("pg_catalog", "pg_prepared_xacts"): "two-phase commit is unsupported",
    ("pg_catalog", "pg_publication_tables"): "logical replication is unsupported",
    ("pg_catalog", "pg_replication_origin_status"): "replication is unsupported",
    ("pg_catalog", "pg_replication_slots"): "replication is unsupported",
    ("pg_catalog", "pg_rules"): "CREATE RULE is unsupported (view _RETURN rules are excluded by the view definition itself)",
    ("pg_catalog", "pg_seclabels"): "security labels are unsupported",
    ("pg_catalog", "pg_stats"): "no planner statistics are collected",
    ("pg_catalog", "pg_stats_ext"): "extended statistics are unsupported",
    ("pg_catalog", "pg_stats_ext_exprs"): "extended statistics are unsupported",
    ("pg_catalog", "pg_user_mappings"): "user mappings are unsupported",
    ("pg_catalog", "pg_stat_replication"): "no replication connections exist",
    ("pg_catalog", "pg_stat_replication_slots"): "replication is unsupported",
    ("pg_catalog", "pg_stat_subscription"): "logical replication is unsupported",
    ("pg_catalog", "pg_stat_subscription_stats"): "logical replication is unsupported",
    ("pg_catalog", "pg_stat_wal_receiver"): "no WAL receiver",
    ("pg_catalog", "pg_stat_progress_analyze"): "no long-running maintenance command reports progress",
    ("pg_catalog", "pg_stat_progress_basebackup"): "no base backups",
    ("pg_catalog", "pg_stat_progress_cluster"): "no long-running maintenance command reports progress",
    ("pg_catalog", "pg_stat_progress_copy"): "COPY progress is not reported",
    ("pg_catalog", "pg_stat_progress_create_index"): "index builds are synchronous and not reported",
    ("pg_catalog", "pg_stat_progress_vacuum"): "no long-running maintenance command reports progress",
    ("pg_catalog", "pg_stat_recovery_prefetch"): "no recovery",
    ("pg_catalog", "pg_stat_user_functions"): "function call statistics are not collected (track_functions = none)",
    ("pg_catalog", "pg_stat_xact_user_functions"): "function call statistics are not collected",
    ("information_schema", "_pg_foreign_data_wrappers"): "foreign data wrappers are unsupported",
    ("information_schema", "_pg_foreign_servers"): "foreign servers are unsupported",
    ("information_schema", "_pg_foreign_table_columns"): "foreign tables are unsupported",
    ("information_schema", "_pg_foreign_tables"): "foreign tables are unsupported",
    ("information_schema", "_pg_user_mappings"): "user mappings are unsupported",
    ("information_schema", "column_options"): "foreign tables are unsupported",
    ("information_schema", "foreign_data_wrapper_options"): "foreign data wrappers are unsupported",
    ("information_schema", "foreign_data_wrappers"): "foreign data wrappers are unsupported",
    ("information_schema", "foreign_server_options"): "foreign servers are unsupported",
    ("information_schema", "foreign_servers"): "foreign servers are unsupported",
    ("information_schema", "foreign_table_options"): "foreign tables are unsupported",
    ("information_schema", "foreign_tables"): "foreign tables are unsupported",
    ("information_schema", "transforms"): "transforms are unsupported",
    ("information_schema", "triggered_update_columns"): "triggers are unsupported in v1",
    ("information_schema", "triggers"): "triggers are unsupported in v1",
    ("information_schema", "user_mapping_options"): "user mappings are unsupported",
    ("information_schema", "user_mappings"): "user mappings are unsupported",
}

# --- relations whose data exists in PG but PGArbor cannot serve yet: scan raises 0A000 (CAT-37)
UNSUPPORTED = {
    ("pg_catalog", "pg_init_privs"): "initial privileges of builtin objects are not materialized",
    ("pg_catalog", "pg_ts_config"): "text search is unsupported",
    ("pg_catalog", "pg_ts_config_map"): "text search is unsupported",
    ("pg_catalog", "pg_ts_dict"): "text search is unsupported",
    ("pg_catalog", "pg_ts_parser"): "text search is unsupported",
    ("pg_catalog", "pg_ts_template"): "text search is unsupported",
    ("pg_catalog", "pg_aios"): "asynchronous I/O is not implemented",
    ("pg_catalog", "pg_backend_memory_contexts"): "memory contexts are not tracked",
    ("pg_catalog", "pg_config"): "build configuration is not exposed",
    ("pg_catalog", "pg_shmem_allocations"): "shared memory is not used",
    ("pg_catalog", "pg_shmem_allocations_numa"): "shared memory is not used",
    ("pg_catalog", "pg_stat_archiver"): "server-wide statistics are not collected",
    ("pg_catalog", "pg_stat_bgwriter"): "server-wide statistics are not collected",
    ("pg_catalog", "pg_stat_checkpointer"): "server-wide statistics are not collected",
    ("pg_catalog", "pg_stat_database"): "per-database statistics are not collected",
    ("pg_catalog", "pg_stat_database_conflicts"): "per-database statistics are not collected",
    ("pg_catalog", "pg_stat_io"): "I/O statistics are not collected",
    ("pg_catalog", "pg_stat_slru"): "SLRU statistics are not collected",
    ("pg_catalog", "pg_stat_wal"): "WAL statistics are not collected",
    ("pg_catalog", "pg_timezone_abbrevs"): "timezone abbreviation table not wired (pgarbor-types timezone data)",
    ("pg_catalog", "pg_timezone_names"): "timezone name table not wired (pgarbor-types timezone data)",
    ("pg_catalog", "pg_wait_events"): "wait event catalog is not materialized",
    ("information_schema", "sql_features"): "static SQL feature table not materialized",
    ("information_schema", "sql_implementation_info"): "static table not materialized",
    ("information_schema", "sql_parts"): "static table not materialized",
    ("information_schema", "sql_sizing"): "static table not materialized",
}

assigned = {}
for stem, names in PRODUCERS.items():
    for n in names:
        if n.endswith("_NOT"): continue
        assigned[("pg_catalog", n)] = stem
for stem, names in IS_PRODUCERS.items():
    for n in names:
        assigned[("information_schema", n)] = stem
for k in EMPTY: assigned[k] = "empty" if k not in assigned else sys.exit("dup " + str(k))
for k in UNSUPPORTED:
    if k in assigned: sys.exit("dup " + str(k))
    assigned[k] = "unsupported"
missing = [k for k in RELS if k not in assigned]
extra = [k for k in assigned if k not in RELS]
print("relations:", len(RELS), "assigned:", len(assigned))
print("missing:", missing)
print("extra:", extra)
if missing or extra:
    sys.exit(1)
import collections
print(collections.Counter(assigned.values()))

def gen(title, doc, rels_name, reasons_name, struct):
    lines = ["// @generated by crates/pgarbor-catalog/gen/classify_vrel.py - edit the script, not this list.", ""]
    lines.append(f"/// {title}")
    lines.append(f"pub static {rels_name}: &[{struct}] = &[")
    for (sch, name), why in sorted(doc.items(), key=lambda kv: RELS[kv[0]]):
        lines.append(f"    {struct}(relid::{RELS[(sch, name)]}),")
    lines.append("];")
    lines.append("")
    lines.append(f"/// (oid, qualified name, reason) for every entry of [`{rels_name}`].")
    lines.append(f"pub static {reasons_name}: &[(Oid, &str, &str)] = &[")
    for (sch, name), why in sorted(doc.items(), key=lambda kv: RELS[kv[0]]):
        lines.append(f'    (relid::{RELS[(sch, name)]}, "{sch}.{name}", "{why}"),')
    lines.append("];")
    return "\n".join(lines) + "\n"

open(os.path.join(SRC, "vrel", "empty_list.inc"), "w").write(gen("Correctly-empty system relations.", EMPTY, "EMPTY_RELATIONS", "EMPTY_REASONS", "EmptyRelation"))
open(os.path.join(SRC, "vrel", "unsupported_list.inc"), "w").write(gen("System relations PGArbor cannot serve yet.", UNSUPPORTED, "UNSUPPORTED_RELATIONS", "UNSUPPORTED_REASONS", "UnsupportedRelation"))
