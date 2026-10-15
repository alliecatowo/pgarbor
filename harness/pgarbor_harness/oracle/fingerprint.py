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

from typing import Any

from .container import OracleContainer

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


def collect(container: OracleContainer) -> dict[str, Any]:
    """Build the fingerprint document from the live container (see module docstring).
    Uses container.status() for the digest and a psycopg connection (lazy import) to
    ``container.endpoint("postgres")`` with DIFFERENTIAL_SESSION_GUCS for the rest."""
    raise NotImplementedError("HAR-03: collect fingerprint")


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
    raise NotImplementedError("HAR-03: verify fingerprint")


def verify_live(container: OracleContainer) -> tuple[dict[str, Any], str]:
    """collect + verify; raise container.OracleError("oracle fingerprint drift: ...") on any
    violation; return (doc, sha256)."""
    raise NotImplementedError("HAR-03: verify live oracle")
