"""Tests for HAR-03 (ORACLE-01 / ORACLE-11): oracle fingerprint document and verification.

Pure tests run without the container. Tests marked ``oracle`` need the pinned PG18 container
and are skipped unless PGARBOR_ORACLE=1 (see conftest.py).
"""

from __future__ import annotations

import copy

import pytest

from pgarbor_harness import config
from pgarbor_harness.hashing import sha256_bytes
from pgarbor_harness.model import canonical_json
from pgarbor_harness.oracle import fingerprint as fp
from pgarbor_harness.oracle.container import OracleError


def _good_doc() -> dict:
    settings = {name: "x" for name in fp.SETTINGS_KEYS}
    settings.update(config.DIFFERENTIAL_SESSION_GUCS)
    return {
        "format": fp.FINGERPRINT_FORMAT,
        "image_digest": config.PG_IMAGE_DIGEST,
        "server_version": config.PG_SERVER_VERSION,
        "server_version_num": config.PG_SERVER_VERSION_NUM,
        "database": {
            "encoding": "UTF8",
            "locale_provider": "b",
            "collate": "C.UTF-8",
            "ctype": "C.UTF-8",
            "locale": "C.UTF-8",
        },
        "settings": settings,
        "extensions_available": ["plpgsql"],
        "builtin_counts": {"pg_type": 1, "pg_proc": 2, "pg_operator": 3, "pg_cast": 4},
    }


def test_verify_good_document_is_clean():
    assert fp.verify(_good_doc()) == []


def test_fingerprint_stable_under_key_reordering():
    doc = _good_doc()
    reordered = {k: doc[k] for k in reversed(list(doc))}
    reordered["settings"] = {k: doc["settings"][k] for k in reversed(list(doc["settings"]))}
    assert fp.fingerprint_sha256(doc) == fp.fingerprint_sha256(reordered)


def test_fingerprint_sha256_is_hash_of_canonical_json():
    doc = _good_doc()
    expected = sha256_bytes(canonical_json(doc).encode("utf-8"))
    assert fp.fingerprint_sha256(doc) == expected
    assert len(expected) == 64


def test_fingerprint_sha256_changes_with_content():
    doc = _good_doc()
    changed = copy.deepcopy(doc)
    changed["settings"]["TimeZone"] = "Etc/UTC"
    assert fp.fingerprint_sha256(doc) != fp.fingerprint_sha256(changed)


def test_verify_reports_image_digest_drift():
    doc = _good_doc()
    doc["image_digest"] = "sha256:" + "a" * 64
    assert fp.verify(doc) == [
        f"image_digest: expected {config.PG_IMAGE_DIGEST}, got sha256:{'a' * 64}"
    ]


def test_verify_reports_timezone_drift():
    doc = _good_doc()
    doc["settings"]["TimeZone"] = "Etc/UTC"
    assert fp.verify(doc) == ["settings.TimeZone: expected UTC, got Etc/UTC"]


def test_verify_reports_locale_drift():
    doc = _good_doc()
    doc["database"]["locale"] = "C"
    assert fp.verify(doc) == ["database.locale: expected C.UTF-8, got C"]


def test_verify_reports_encoding_and_provider_drift():
    doc = _good_doc()
    doc["database"]["encoding"] = "SQL_ASCII"
    doc["database"]["locale_provider"] = "c"
    assert fp.verify(doc) == [
        "database.encoding: expected UTF8, got SQL_ASCII",
        "database.locale_provider: expected b, got c",
    ]


def test_verify_reports_server_version_drift():
    doc = _good_doc()
    doc["server_version"] = "18.5 (Debian 18.5-1)"
    doc["server_version_num"] = 180005
    assert fp.verify(doc) == [
        f"server_version: expected {config.PG_SERVER_VERSION}, got 18.5 (Debian 18.5-1)",
        f"server_version_num: expected {config.PG_SERVER_VERSION_NUM}, got 180005",
    ]


def test_verify_missing_setting_reports_none():
    doc = _good_doc()
    del doc["settings"]["DateStyle"]
    assert fp.verify(doc) == ["settings.DateStyle: expected ISO, MDY, got None"]


def test_verify_tolerates_missing_sections():
    violations = fp.verify({})
    assert violations
    assert all(v.split(":", 1)[0] for v in violations)
    assert "image_digest: expected " in violations[0]


def test_verify_live_raises_on_drift(monkeypatch):
    bad = _good_doc()
    bad["settings"]["TimeZone"] = "Etc/UTC"
    monkeypatch.setattr(fp, "collect", lambda container: bad)
    with pytest.raises(OracleError) as info:
        fp.verify_live(object())  # type: ignore[arg-type]
    assert str(info.value) == (
        "oracle fingerprint drift: settings.TimeZone: expected UTC, got Etc/UTC"
    )


def test_verify_live_returns_doc_and_sha_when_clean(monkeypatch):
    good = _good_doc()
    monkeypatch.setattr(fp, "collect", lambda container: good)
    doc, sha = fp.verify_live(object())  # type: ignore[arg-type]
    assert doc is good
    assert sha == fp.fingerprint_sha256(good)


@pytest.mark.oracle
def test_live_oracle_fingerprint_is_clean():
    from pgarbor_harness.oracle.container import OracleContainer

    doc, sha = fp.verify_live(OracleContainer())
    assert fp.verify(doc) == []
    assert doc["format"] == fp.FINGERPRINT_FORMAT
    assert sha == fp.fingerprint_sha256(doc)
    assert set(doc["settings"]) == set(fp.SETTINGS_KEYS)
