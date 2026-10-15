"""ORACLE-03: hand-written validator for pgarbor-case/1 (no ``jsonschema`` dependency).

Lease HAR-06.

Two layers, both required:

1. ``schema_errors(doc, schema)``: a JSON-Schema *subset* interpreter sufficient for
   ``contracts/oracle/case.schema.json`` and future harness schemas. Supported keywords:
   ``type`` (string or list; "integer" excludes bool; "number" excludes bool),
   ``properties``, ``required``, ``additionalProperties`` (bool only), ``items`` (schema),
   ``enum``, ``const``, ``pattern`` (``re.search`` semantics, as JSON Schema specifies),
   ``minLength``/``maxLength``, ``minItems``/``maxItems``, ``uniqueItems``, ``minimum``,
   ``oneOf`` (exactly one branch valid), ``not``, ``$ref`` (local ``#/...`` JSON pointers only),
   ``default``/``description``/``title``/``$schema``/``$id``/``$defs`` (ignored annotations).
   Unknown keywords -> raise ValueError (so schema drift is loud). Error strings have the form
   ``"<json path>: <message>"`` with paths like ``$.steps[2].execute.params[0]``. For a failed
   ``oneOf`` on a step, report the errors of the branch whose ``required`` keys are present
   (e.g. a step with key ``sql`` reports sql_step errors) rather than all nine branches.

2. ``semantic_errors(doc, rel_path)``: rules the schema cannot express:
   * ``id`` first segment == ``family``; ``family`` == first directory under tests/cases
     (``rel_path`` like ``tests/cases/smoke/x.yaml``);
   * every step ``session`` is listed in ``sessions`` (default ["a"]);
   * ``sqlstate`` only with ``expect: error``; ``expect_blocked`` only with ``async: true``;
   * ``async`` steps: the session must not already have a pending async step; every async step
     must be followed later by exactly one ``wait`` on the same session; a ``wait`` with no
     pending async step is an error; no other step may use a session while its async step is
     pending (that would deadlock the libpq connection);
   * ``describe``/``execute``/``close`` reference a statement name created by an earlier
     ``parse`` on the same session (and not closed in between; ``reconnect``/``restart``
     forget all prepared statements);
   * ``param_formats`` length is 0, 1 or len(params); binary params must be even-length
     lowercase hex;
   * ``restart`` requires ``required_capabilities`` to contain ``server-restart``;
   * ``normalize`` names must exist in compare.normalize.REGISTRY (import lazily);
   * YAML 1.1 pitfalls: any ``sql`` that is not a str (e.g. YAML parsed ``on`` as True) ->
     error "sql must be a string; use a block scalar (|)".

``validate(doc, rel_path)`` returns schema errors + semantic errors (semantic only checked when
schema errors are empty). Empty list = valid.

Examples::

    validate({"id": "smoke.x", "title": "t", "family": "smoke", "feature_ids": ["select"],
              "steps": [{"sql": "SELECT 1"}]}, "tests/cases/smoke/x.yaml")  -> []
    validate({... "steps": [{"wait": True, "session": "a"}]}, ...)
        -> ["$.steps[0]: wait on session 'a' without a pending async step"]
"""

from __future__ import annotations

import json
from functools import lru_cache
from pathlib import Path
from typing import Any

from ..config import paths


@lru_cache(maxsize=4)
def load_schema(path: str = "") -> dict[str, Any]:
    """Load (and cache) the case JSON Schema; default path = config.paths().case_schema
    (implemented)."""
    p = Path(path) if path else paths().case_schema
    return json.loads(p.read_text(encoding="utf-8"))


def schema_errors(doc: Any, schema: dict[str, Any]) -> list[str]:
    """JSON-Schema-subset validation (see module docstring, layer 1)."""
    raise NotImplementedError("HAR-06: json schema subset validator")


def semantic_errors(doc: dict[str, Any], rel_path: str) -> list[str]:
    """Cross-field rules (see module docstring, layer 2)."""
    raise NotImplementedError("HAR-06: semantic case rules")


def validate(doc: Any, rel_path: str, schema: dict[str, Any] | None = None) -> list[str]:
    """schema_errors then (if none) semantic_errors (implemented)."""
    errs = schema_errors(doc, schema if schema is not None else load_schema())
    if errs:
        return errs
    return semantic_errors(doc, rel_path)
