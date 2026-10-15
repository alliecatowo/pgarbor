"""Shared pytest configuration (frozen; not leased).

* Adds ``harness/`` to sys.path so ``import pgarbor_harness`` / ``pgarbor_factory`` work
  without installation.
* Tests marked ``@pytest.mark.oracle`` need the pinned PG18 container (podman) and are
  skipped unless ``PGARBOR_ORACLE=1``.
* Each lease writes its tests to ``harness/tests/test_<lease id lowercase, '-'->'_'>.py``.
"""

import os
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


def pytest_collection_modifyitems(config, items):
    if os.environ.get("PGARBOR_ORACLE") == "1":
        return
    skip = pytest.mark.skip(reason="needs the PG18 oracle container (set PGARBOR_ORACLE=1)")
    for item in items:
        if "oracle" in item.keywords:
            item.add_marker(skip)
