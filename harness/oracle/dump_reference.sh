#!/usr/bin/env bash
# ORACLE-01/02: dump ground-truth catalog data from the pinned PG18 oracle container.
# Usage: harness/oracle/dump_reference.sh [container] [outdir]
set -euo pipefail
C=${1:-pgarbor-oracle}
OUT=${2:-contracts/reference/pg18}
mkdir -p "$OUT"
q(){ podman exec -i "$C" psql -U postgres -X -q -v ON_ERROR_STOP=1 -c "COPY ($2) TO STDOUT WITH (FORMAT csv, HEADER true)" > "$OUT/$1.csv"; echo "$1 $(wc -l < "$OUT/$1.csv")"; }
q pg_type "select t.oid, t.typname, n.nspname, t.typlen, t.typbyval, t.typtype, t.typcategory, t.typispreferred, t.typdelim, t.typrelid, t.typelem, t.typarray, t.typinput::text, t.typoutput::text, t.typreceive::text, t.typsend::text, t.typalign, t.typstorage, t.typbasetype, t.typtypmod, t.typndims, t.typcollation from pg_type t join pg_namespace n on n.oid=t.typnamespace order by t.oid"
q pg_proc "select p.oid, p.proname, n.nspname, p.prokind, p.prorettype, p.proretset, p.provolatile, p.proisstrict, p.pronargs, p.pronargdefaults, p.proargtypes::text, p.provariadic, p.prosrc, p.prolang from pg_proc p join pg_namespace n on n.oid=p.pronamespace order by p.oid"
q pg_operator "select o.oid, o.oprname, o.oprkind, o.oprleft, o.oprright, o.oprresult, o.oprcom, o.oprnegate, o.oprcode::text, o.oprcode::oid as oprcode_oid, o.oprcanmerge, o.oprcanhash from pg_operator o order by o.oid"
q pg_cast "select oid, castsource, casttarget, castfunc, castcontext, castmethod from pg_cast order by oid"
q pg_aggregate "select aggfnoid::oid, aggfnoid::text as aggname, aggkind, aggtransfn::text, aggfinalfn::text, aggtranstype, agginitval from pg_aggregate order by 1"
q pg_namespace "select oid, nspname, nspowner from pg_namespace order by oid"
q pg_am "select oid, amname, amtype from pg_am order by oid"
q pg_opclass "select oc.oid, oc.opcname, am.amname, oc.opcintype, oc.opcdefault from pg_opclass oc join pg_am am on am.oid=oc.opcmethod order by oc.oid"
q pg_collation "select oid, collname, collprovider, collencoding from pg_collation order by oid"
q pg_settings "select name, setting, unit, category, vartype, context, source, min_val, max_val, enumvals::text, boot_val, reset_val from pg_settings order by name"
q catalog_relations "select c.oid, c.relname, n.nspname, c.relkind, c.reltype, c.relnatts from pg_class c join pg_namespace n on n.oid=c.relnamespace where n.nspname in ('pg_catalog','information_schema') order by n.nspname, c.relname"
q catalog_attributes "select n.nspname, c.relname, a.attnum, a.attname, a.atttypid, format_type(a.atttypid,a.atttypmod) as type, a.atttypmod, a.attnotnull from pg_attribute a join pg_class c on c.oid=a.attrelid join pg_namespace n on n.oid=c.relnamespace where n.nspname in ('pg_catalog','information_schema') and a.attnum>0 and not a.attisdropped order by n.nspname, c.relname, a.attnum"
q catalog_view_defs "select schemaname, viewname, definition from pg_views where schemaname in ('pg_catalog','information_schema') order by 1,2"
q pg_roles "select oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication, rolbypassrls from pg_roles order by oid"
q pg_database "select oid, datname, encoding, datlocprovider, datistemplate, datallowconn, datcollate, datctype, datlocale from pg_database order by oid"
q pg_keywords "select word, catcode, barelabel, catdesc from pg_get_keywords() order by word"
q pg_description_builtin "select d.objoid, d.classoid::regclass::text as classname, d.description from pg_description d where d.classoid in ('pg_proc'::regclass,'pg_type'::regclass,'pg_operator'::regclass) order by 2,1"
podman exec -i "$C" psql -U postgres -X -Atc "select version()" > "$OUT/VERSION"
