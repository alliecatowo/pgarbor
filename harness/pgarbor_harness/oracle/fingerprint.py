"""ORACLE-01 / ORACLE-11: oracle identity fingerprint and verification.

Lease HAR-03.

The fingerprint is a canonical JSON document describing *everything about the reference
server that can change query results*. Its sha256 is stored in every expected file
(model.ReferenceIdentity.fingerprint_sha256) and in every evidence pack. A recording made
under a different fingerprint is stale.

Fingerprint document (keys exactly; values from the live server)::

    {
      "format": "pgarbor-oracle-fingerprint/1",
      "image_digest": config.PG_IMAGE_DIGEST (from podman inspect, NOT from config),
      "server_version": SHOW server_version,
      "server_version_num": SHOW server_version_num (int),
      "database": {"encoding": pg_encoding_to_char(encoding), "locale_provider": datlocprovider,
                   "collate": datcollate, "ctype": datctype, "locale": datlocale}
                  for the CASE TEMPLATE database (config.CASE_TEMPLATE_DB) — read from
                  ``pg_database`` in a session connected to db ``postgres``. NEVER connect
                  to the template: PG refuses ``CREATE DATABASE ... TEMPLATE x`` (55006)
                  while any session is connected to x, which would break running cases,
      "settings": {name: setting} for exactly SETTINGS_KEYS, read in a session on db
                  ``postgres`` that applied config.DIFFERENTIAL_SESSION_GUCS (i.e. as a case
                  session sees them; the template has no ALTER DATABASE SET, so equal),
      "extensions_available": sorted names from pg_available_extensions,
      "builtin_counts": {"pg_type": n, "pg_proc": n, "pg_operator": n, "pg_cast": n}
    }

``fingerprint_sha256`` = hashing.sha256_bytes(model.canonical_json(doc).encode()).

Verification (``verify``) compares a live fingerprint with the pinned expectations in config
(digest, server_version, every DIFFERENTIAL_SESSION_GUCS value, encoding UTF8, provider
``b``, locale ``C.UTF-8``) and returns a list of human-readable violations; the pool refuses
to start when non-empty (HARNESS_ERROR, never silently continue on a drifted oracle).
"""

from __future__ import annotations

import dataclasses
from typing import Any

from .. import config
from .container import OracleContainer, OracleError

FINGERPRINT_FORMAT = "pgarbor-oracle-fingerprint/1"

#: Settings captured in the fingerprint (all can change output text or semantics).
SETTINGS_KEYS: tuple[str, ...] = (
    "TimeZone", "DateStyle", "IntervalStyle", "extra_float_digits", "client_encoding",
    "server_encoding", "lc_messages", "lc_monetary", "lc_numeric", "lc_time",
    "standard_conforming_strings", "bytea_output", "xmlbinary", "xmloption",
    "default_transaction_isolation", "search_path", "max_identifier_length", "integer_datetimes",
    "timezone_abbreviations", "default_text_search_config", "array_nulls",
    "backslash_quote", "escape_string_warning", "transform_null_equals", "jit",
    "enable_hashagg", "enable_seqscan", "enable_indexscan", "plan_cache_mode",
)

#: Expected template database encoding / locale provider / locale (pinned by PG_INITDB_ARGS).
_EXPECTED_ENCODING = "UTF8"
_EXPECTED_LOCALE_PROVIDER = "b"
_EXPECTED_LOCALE = "C.UTF-8"


def _read_fingerprint(cur: Any, image_digest: str) -> dict[str, Any]:
    """Run every catalog/settings query on one open cursor (private helper)."""
    cur.execute("SHOW server_version")
    server_version = str(cur.fetchone()[0])
    cur.execute("SHOW server_version_num")
    server_version_num = int(cur.fetchone()[0])

    cur.execute(
        "SELECT pg_encoding_to_char(encoding), datlocprovider::text, datcollate, datctype, "
        "datlocale FROM pg_database WHERE datname = %s",
        (config.CASE_TEMPLATE_DB,),
    )
    row = cur.fetchone()
    if row is None:
        raise OracleError(
            f"oracle fingerprint: case template database {config.CASE_TEMPLATE_DB!r} does not exist"
        )
    database = {
        "encoding": row[0],
        "locale_provider": row[1],
        "collate": row[2],
        "ctype": row[3],
        "locale": row[4],
    }

    cur.execute(
        "SELECT name, setting FROM pg_settings WHERE name = ANY(%s)",
        (list(SETTINGS_KEYS),),
    )
    found: dict[str, str] = {str(name): str(setting) for name, setting in cur.fetchall()}
    settings: dict[str, str] = {}
    for key in SETTINGS_KEYS:
        value = found.get(key)
        if value is None:
            # pg_settings.name matching is case-sensitive; current_setting() is not and
            # raises for unknown names, which is the desired failure.
            cur.execute("SELECT current_setting(%s)", (key,))
            value = str(cur.fetchone()[0])
        settings[key] = value

    cur.execute("SELECT name FROM pg_available_extensions")
    extensions = sorted(str(r[0]) for r in cur.fetchall())

    cur.execute(
        "SELECT (SELECT count(*) FROM pg_type), (SELECT count(*) FROM pg_proc), "
        "(SELECT count(*) FROM pg_operator), (SELECT count(*) FROM pg_cast)"
    )
    n_type, n_proc, n_operator, n_cast = cur.fetchone()

    return {
        "format": FINGERPRINT_FORMAT,
        "image_digest": image_digest,
        "server_version": server_version,
        "server_version_num": server_version_num,
        "database": database,
        "settings": settings,
        "extensions_available": extensions,
        "builtin_counts": {
            "pg_type": int(n_type),
            "pg_proc": int(n_proc),
            "pg_operator": int(n_operator),
            "pg_cast": int(n_cast),
        },
    }


def collect(container: OracleContainer) -> dict[str, Any]:
    """Build the fingerprint document from the live container (see module docstring).
    Uses container.status() for the digest and a psycopg connection (lazy import) to
    ``container.endpoint("postgres")`` with DIFFERENTIAL_SESSION_GUCS for the rest."""
    st = container.status()
    if not st.exists or not st.image_digest:
        raise OracleError(
            f"oracle fingerprint: container {container.spec.name!r} has no image digest "
            f"(exists={st.exists}, running={st.running})"
        )

    try:
        import psycopg
    except ImportError as exc:
        raise OracleError(f"oracle fingerprint: psycopg is not importable: {exc}") from exc

    ep = dataclasses.replace(
        container.endpoint("postgres"),
        gucs=tuple(config.DIFFERENTIAL_SESSION_GUCS.items()),
    )
    try:
        with psycopg.connect(
            ep.conninfo(), autocommit=True, connect_timeout=max(1, int(config.CONNECT_TIMEOUT_S))
        ) as conn:
            with conn.cursor() as cur:
                return _read_fingerprint(cur, st.image_digest)
    except OracleError:
        raise
    except psycopg.Error as exc:
        raise OracleError(f"oracle fingerprint: query failed: {exc}") from exc


def fingerprint_sha256(doc: dict[str, Any]) -> str:
    """sha256 of canonical_json(doc) (implemented)."""
    from ..hashing import sha256_bytes
    from ..model import canonical_json

    return sha256_bytes(canonical_json(doc).encode("utf-8"))


def verify(doc: dict[str, Any]) -> list[str]:
    """Return violations of the pinned expectations; [] means the oracle is trustworthy.

    Example violation strings:
      ``"image_digest: expected sha256:7493..., got sha256:aaaa..."``
      ``"settings.TimeZone: expected UTC, got Etc/UTC"`` (exact string compare; note PG
      reports the value as set, so ``UTC`` stays ``UTC``)."""
    violations: list[str] = []

    def check(path: str, expected: Any, got: Any) -> None:
        if expected != got:
            violations.append(f"{path}: expected {expected}, got {got}")

    check("image_digest", config.PG_IMAGE_DIGEST, doc.get("image_digest"))
    check("server_version", config.PG_SERVER_VERSION, doc.get("server_version"))
    check("server_version_num", config.PG_SERVER_VERSION_NUM, doc.get("server_version_num"))

    database = doc.get("database") or {}
    check("database.encoding", _EXPECTED_ENCODING, database.get("encoding"))
    check("database.locale_provider", _EXPECTED_LOCALE_PROVIDER, database.get("locale_provider"))
    check("database.locale", _EXPECTED_LOCALE, database.get("locale"))

    settings = doc.get("settings") or {}
    for name, value in config.DIFFERENTIAL_SESSION_GUCS.items():
        check(f"settings.{name}", value, settings.get(name))

    return violations


def verify_live(container: OracleContainer) -> tuple[dict[str, Any], str]:
    """collect + verify; raise container.OracleError("oracle fingerprint drift: ...") on any
    violation; return (doc, sha256)."""
    doc = collect(container)
    violations = verify(doc)
    if violations:
        raise OracleError("oracle fingerprint drift: " + "; ".join(violations))
    return doc, fingerprint_sha256(doc)
