"""Frozen harness configuration: pinned oracle identity, GUC profiles, repository paths.

CONTRACT (harness v1). Everything in this module is implemented and frozen; workers import it,
never edit it. Changing a value here (e.g. the PG image digest) is an ORACLE migration that goes
through ``contracts/amendments/`` and re-records ``tests/expected/**``.

Source of truth: ``contracts/ARCHITECTURE.md`` section 0 rule 6.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------------------------
# Pinned reference (ARCHITECTURE §0.6). CONTRACT
# ---------------------------------------------------------------------------------------------

#: Exact image reference. Never use a tag; the digest *is* the oracle identity.
PG_IMAGE = (
    "docker.io/library/postgres@sha256:"
    "74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336"
)
PG_IMAGE_DIGEST = "sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336"
#: ``SHOW server_version`` must return exactly this (checked by oracle.fingerprint).
PG_SERVER_VERSION = "18.6 (Debian 18.6-1.pgdg13+2)"
PG_SERVER_VERSION_NUM = 180006
#: Passed via POSTGRES_INITDB_ARGS when the container data dir is first created.
PG_INITDB_ARGS = "--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8"
#: Server-level GUCs passed as ``-c name=value`` postgres arguments (container CMD).
#: DateStyle/IntervalStyle/extra_float_digits are PG defaults but are pinned explicitly so a
#: future image default change cannot silently drift the oracle.
PG_SERVER_GUCS: dict[str, str] = {
    "TimeZone": "UTC",
    "DateStyle": "ISO, MDY",
    "IntervalStyle": "postgres",
    "extra_float_digits": "1",
    # Determinism: never let autovacuum/analyze change plans or stats mid-case.
    "autovacuum": "off",
    # Keep error output predictable for comparison.
    "lc_messages": "C.UTF-8",
}
#: Superuser used for all oracle work (trust auth inside the container network).
PG_SUPERUSER = "postgres"
PG_SUPERUSER_PASSWORD = "oracle"

#: Name of the container the harness owns. The developer container ``pgarbor-oracle`` used by
#: architects for ad-hoc fact checks is NEVER touched by the harness.
HARNESS_CONTAINER_NAME = "pgarbor-harness-pg18"
#: Host port (loopback only) the harness container publishes 5432 on.
HARNESS_CONTAINER_PORT = 54319
#: Template database every differential case database is cloned from.
CASE_TEMPLATE_DB = "pgarbor_case_template"
#: Prefix of per-case databases (``pgarbor_case_<12 hex>``); anything with this prefix is
#: garbage-collectable by oracle.pool.
CASE_DB_PREFIX = "pgarbor_case_"

# ---------------------------------------------------------------------------------------------
# GUC profiles. CONTRACT
# ---------------------------------------------------------------------------------------------

#: Session GUCs the differential runner sets (as startup ``options``) on BOTH reference and
#: target for every session of every case. Identical to the server pins so that a target that
#: ignores server-level config still runs under the same settings.
DIFFERENTIAL_SESSION_GUCS: dict[str, str] = {
    "TimeZone": "UTC",
    "DateStyle": "ISO, MDY",
    "IntervalStyle": "postgres",
    "extra_float_digits": "1",
    "client_encoding": "UTF8",
    "lc_messages": "C.UTF-8",
}

#: pg_regress profile (src/test/regress/pg_regress.c initialize_environment / create_database).
#: This is a SEPARATE profile from the differential corpus: official expected/*.out files were
#: produced under these settings, so the regress runner must reproduce them exactly.
REGRESS_ENV: dict[str, str] = {
    "PGTZ": "America/Los_Angeles",
    "PGDATESTYLE": "Postgres, MDY",
    "LC_MESSAGES": "C",
    "LANG": "C",
    "PGAPPNAME": "pg_regress",
}
#: psql flags pg_regress_main.c uses for every test file.
REGRESS_PSQL_FLAGS: tuple[str, ...] = (
    "-X", "-a", "-q", "-v", "HIDE_TABLEAM=on", "-v", "HIDE_TOAST_COMPRESSION=on",
)
#: Statements pg_regress.c create_database() issues (with %s = database name) after
#: ``CREATE DATABASE "%s" TEMPLATE=template0``.
REGRESS_DB_SETTINGS: tuple[str, ...] = (
    "ALTER DATABASE \"{db}\" SET lc_messages TO 'C'",
    "ALTER DATABASE \"{db}\" SET lc_monetary TO 'C'",
    "ALTER DATABASE \"{db}\" SET lc_numeric TO 'C'",
    "ALTER DATABASE \"{db}\" SET lc_time TO 'C'",
    "ALTER DATABASE \"{db}\" SET bytea_output TO 'hex'",
    "ALTER DATABASE \"{db}\" SET timezone_abbreviations TO 'Default'",
)

# ---------------------------------------------------------------------------------------------
# Timeouts (seconds). CONTRACT
# ---------------------------------------------------------------------------------------------

STEP_TIMEOUT_S = 10.0          #: a single step exceeding this on the target => HANG
CASE_TIMEOUT_S = 60.0          #: whole case wall clock budget
BLOCK_SETTLE_S = 0.5           #: grace before declaring an async step "observed blocked"
CONNECT_TIMEOUT_S = 10.0
CONTAINER_READY_TIMEOUT_S = 90.0
REGRESS_TEST_TIMEOUT_S = 300.0  #: per regress .sql file

# ---------------------------------------------------------------------------------------------
# Repository layout. CONTRACT
# ---------------------------------------------------------------------------------------------


def _find_repo_root() -> Path:
    env = os.environ.get("PGARBOR_REPO")
    if env:
        return Path(env).resolve()
    here = Path(__file__).resolve()
    for p in [here, *here.parents]:
        if (p / "contracts" / "ARCHITECTURE.md").is_file():
            return p
    return Path.cwd().resolve()


@dataclass(frozen=True)
class Paths:
    """All repository-relative locations the harness and factory read or write.

    Protected (never writable by feature leases; see pgarbor_factory.guard):
    ``contracts/**``, ``harness/oracle/**``, ``tests/expected/**``.
    """

    root: Path = field(default_factory=_find_repo_root)

    @property
    def contracts(self) -> Path:
        return self.root / "contracts"

    @property
    def case_schema(self) -> Path:
        return self.root / "contracts" / "oracle" / "case.schema.json"

    @property
    def evidence_schema(self) -> Path:
        return self.root / "contracts" / "oracle" / "evidence.schema.json"

    @property
    def dag(self) -> Path:
        return self.root / "contracts" / "dag" / "task_graph.v1.json"

    @property
    def leases_dir(self) -> Path:
        return self.root / "contracts" / "leases"

    @property
    def amendments_dir(self) -> Path:
        return self.root / "contracts" / "amendments"

    @property
    def cases(self) -> Path:
        return self.root / "tests" / "cases"

    @property
    def expected(self) -> Path:
        return self.root / "tests" / "expected"

    @property
    def runs(self) -> Path:
        return self.root / "runs"

    @property
    def state(self) -> Path:
        return self.root / "runs" / "state"

    @property
    def worktrees(self) -> Path:
        # Per the user's global rules: worktrees live under <repo>/.claude/worktrees/ only.
        return self.root / ".claude" / "worktrees"

    @property
    def pg_source(self) -> Path:
        env = os.environ.get("PGARBOR_PG_SOURCE")
        return Path(env) if env else Path.home() / "develop" / "reference" / "postgres-18"

    @property
    def pg_regress_dir(self) -> Path:
        return self.pg_source / "src" / "test" / "regress"

    @property
    def pg_isolation_dir(self) -> Path:
        return self.pg_source / "src" / "test" / "isolation"


#: Paths no feature (Rust) lease may ever modify, and no harness lease either, except through
#: the oracle review path (pgarbor_factory.guard). Glob syntax: fnmatch over repo-relative POSIX
#: paths where ``**`` matches any depth.
PROTECTED_GLOBS: tuple[str, ...] = (
    "contracts/**",
    "harness/oracle/**",
    "tests/expected/**",
)

#: The verifier itself: hashed into the trusted oracle manifest (ORACLE-11) and protected from
#: non-harness leases (SWARM-14). Harness leases may only touch their own leased files here.
VERIFIER_GLOBS: tuple[str, ...] = (
    "harness/pyproject.toml",
    "harness/pgarbor_harness/**",
    "harness/pgarbor_factory/**",
    "harness/factory/dag.py",
    "harness/factory/test_dag.py",
    "tests/cases/**",
)

#: Lease id prefixes owned by the harness (Python) side.
HARNESS_LEASE_PREFIXES: tuple[str, ...] = ("HAR-", "FAC-")


def paths() -> Paths:
    """Return the Paths for the current repository (env ``PGARBOR_REPO`` overrides discovery)."""
    return Paths()
