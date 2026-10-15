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
import re
from functools import lru_cache
from pathlib import Path
from typing import Any

from ..config import paths
from .model import STEP_KINDS


@lru_cache(maxsize=4)
def load_schema(path: str = "") -> dict[str, Any]:
    """Load (and cache) the case JSON Schema; default path = config.paths().case_schema
    (implemented)."""
    p = Path(path) if path else paths().case_schema
    return json.loads(p.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------------------------
# Layer 1: JSON Schema subset
# ---------------------------------------------------------------------------------------------

_SCHEMA_KEYWORDS = frozenset({
    "type", "properties", "required", "additionalProperties", "items", "enum", "const",
    "pattern", "minLength", "maxLength", "minItems", "maxItems", "uniqueItems", "minimum",
    "oneOf", "not", "$ref",
    "default", "description", "title", "$schema", "$id", "$defs",
})
_TYPE_NAMES = frozenset({"object", "array", "string", "integer", "number", "boolean", "null"})
_BINARY_HEX_RE = re.compile(r"(?:[0-9a-f]{2})*")
_MAX_REF_DEPTH = 64


def _check_schema(schema: Any, where: str) -> None:
    """Walk a schema tree and raise ValueError on any keyword outside the supported subset."""
    if isinstance(schema, bool):
        return
    if not isinstance(schema, dict):
        raise ValueError(f"{where}: schema must be an object or a boolean")
    for kw, val in schema.items():
        if kw not in _SCHEMA_KEYWORDS:
            raise ValueError(f"{where}: unsupported schema keyword {kw!r}")
        here = f"{where}/{kw}"
        if kw in ("properties", "$defs"):
            if not isinstance(val, dict):
                raise ValueError(f"{here}: must be an object")
            for name, sub in val.items():
                _check_schema(sub, f"{here}/{name}")
        elif kw in ("items", "not"):
            _check_schema(val, here)
        elif kw == "oneOf":
            if not isinstance(val, list) or not val:
                raise ValueError(f"{here}: must be a non-empty array")
            for i, sub in enumerate(val):
                _check_schema(sub, f"{here}/{i}")
        elif kw == "additionalProperties":
            if not isinstance(val, bool):
                raise ValueError(f"{here}: only boolean additionalProperties is supported")
        elif kw == "$ref":
            if not isinstance(val, str) or not val.startswith("#"):
                raise ValueError(f"{here}: only local '#/...' references are supported")
        elif kw == "type":
            names = [val] if isinstance(val, str) else val
            if not isinstance(names, list) or not names or any(
                    not isinstance(n, str) or n not in _TYPE_NAMES for n in names):
                raise ValueError(f"{here}: unsupported type {val!r}")
        elif kw == "required":
            if not isinstance(val, list) or any(not isinstance(n, str) for n in val):
                raise ValueError(f"{here}: must be an array of property names")


def _resolve_ref(ref: str, root: Any) -> Any:
    """Resolve a local JSON pointer ``#/a/b`` against the root schema (RFC 6901 unescaping)."""
    if not ref.startswith("#/"):
        raise ValueError(f"unsupported $ref {ref!r}")
    node: Any = root
    for raw in ref[2:].split("/"):
        tok = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(node, dict) and tok in node:
            node = node[tok]
        elif isinstance(node, list) and tok.isdigit() and int(tok) < len(node):
            node = node[int(tok)]
        else:
            raise ValueError(f"unresolvable $ref {ref!r}")
    return node


def _json_type(x: Any) -> str:
    if x is None:
        return "null"
    if isinstance(x, bool):
        return "boolean"
    if isinstance(x, int):
        return "integer"
    if isinstance(x, float):
        return "number"
    if isinstance(x, str):
        return "string"
    if isinstance(x, list):
        return "array"
    if isinstance(x, dict):
        return "object"
    return type(x).__name__


def _matches_type(x: Any, t: str) -> bool:
    if t == "null":
        return x is None
    if t == "boolean":
        return isinstance(x, bool)
    if t == "string":
        return isinstance(x, str)
    if t == "array":
        return isinstance(x, list)
    if t == "object":
        return isinstance(x, dict)
    if t == "number":
        return isinstance(x, (int, float)) and not isinstance(x, bool)
    if t == "integer":
        if isinstance(x, bool):
            return False
        if isinstance(x, int):
            return True
        return isinstance(x, float) and x.is_integer()
    raise ValueError(f"unknown type {t!r}")


def _json_equal(a: Any, b: Any) -> bool:
    """JSON equality: booleans never equal numbers (Python's True == 1 must not leak in)."""
    if isinstance(a, bool) or isinstance(b, bool):
        return isinstance(a, bool) and isinstance(b, bool) and a == b
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return a == b
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(_json_equal(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(_json_equal(a[k], b[k]) for k in a)
    return type(a) is type(b) and a == b


def _child(path: str, key: Any) -> str:
    return f"{path}.{key}"


def _item(path: str, index: int) -> str:
    return f"{path}[{index}]"


def _required_of(branch: Any, root: Any) -> set[str]:
    """The ``required`` keys of a (possibly $ref-indirected) branch schema."""
    depth = 0
    while isinstance(branch, dict) and "$ref" in branch:
        depth += 1
        if depth > _MAX_REF_DEPTH:
            raise ValueError("$ref chain too deep")
        branch = _resolve_ref(branch["$ref"], root)
    if isinstance(branch, dict):
        return set(branch.get("required", []))
    return set()


def _one_of(inst: Any, branches: list[Any], path: str, root: Any) -> list[str]:
    results = [_collect(inst, branch, path, root) for branch in branches]
    valid = [i for i, errs in enumerate(results) if not errs]
    if len(valid) == 1:
        return []
    if len(valid) > 1:
        return [f"{path}: matches {len(valid)} oneOf branches; exactly one is allowed"]
    if isinstance(inst, dict):
        keys = set(inst)
        candidates = [i for i, b in enumerate(branches) if _required_of(b, root) <= keys]
        if candidates:
            out: list[str] = []
            for i in candidates:
                out.extend(results[i])
            return out
        return [f"{path}: matches none of the allowed forms (keys: {sorted(map(str, keys))})"]
    return [f"{path}: matches none of the allowed forms"]


def _collect(inst: Any, schema: Any, path: str, root: Any) -> list[str]:
    """Return every error of ``inst`` against ``schema`` (empty list = valid)."""
    if schema is True:
        return []
    if schema is False:
        return [f"{path}: no value is allowed here"]

    errs: list[str] = []
    if "$ref" in schema:
        errs.extend(_collect(inst, _resolve_ref(schema["$ref"], root), path, root))

    if "type" in schema:
        types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_matches_type(inst, t) for t in types):
            if path.endswith(".sql") and "string" in types:
                errs.append(f"{path}: sql must be a string; use a block scalar (|)")
            else:
                errs.append(f"{path}: expected {' or '.join(types)}, got {_json_type(inst)}")
            return errs

    if "enum" in schema and not any(_json_equal(inst, e) for e in schema["enum"]):
        errs.append(f"{path}: must be one of {json.dumps(schema['enum'])}")
    if "const" in schema and not _json_equal(inst, schema["const"]):
        errs.append(f"{path}: must be {json.dumps(schema['const'])}")

    if isinstance(inst, dict):
        props = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in inst:
                errs.append(f"{path}: missing required property {name!r}")
        for name, sub in props.items():
            if name in inst:
                errs.extend(_collect(inst[name], sub, _child(path, name), root))
        if schema.get("additionalProperties") is False:
            for key in inst:
                if key not in props:
                    errs.append(f"{_child(path, key)}: unexpected property {key!r}")

    if isinstance(inst, list):
        if "minItems" in schema and len(inst) < schema["minItems"]:
            errs.append(f"{path}: must have at least {schema['minItems']} items")
        if "maxItems" in schema and len(inst) > schema["maxItems"]:
            errs.append(f"{path}: must have at most {schema['maxItems']} items")
        if schema.get("uniqueItems"):
            dup = None
            for i in range(len(inst)):
                for j in range(i):
                    if _json_equal(inst[i], inst[j]):
                        dup = (i, j)
                        break
                if dup:
                    break
            if dup:
                errs.append(f"{_item(path, dup[0])}: duplicates item {dup[1]}; items must be unique")
        if "items" in schema:
            for i, el in enumerate(inst):
                errs.extend(_collect(el, schema["items"], _item(path, i), root))

    if isinstance(inst, str):
        if "minLength" in schema and len(inst) < schema["minLength"]:
            errs.append(f"{path}: must be at least {schema['minLength']} characters")
        if "maxLength" in schema and len(inst) > schema["maxLength"]:
            errs.append(f"{path}: must be at most {schema['maxLength']} characters")
        if "pattern" in schema and re.search(schema["pattern"], inst) is None:
            errs.append(f"{path}: does not match pattern {schema['pattern']!r}")

    if isinstance(inst, (int, float)) and not isinstance(inst, bool):
        if "minimum" in schema and inst < schema["minimum"]:
            errs.append(f"{path}: must be >= {schema['minimum']}")

    if "not" in schema and not _collect(inst, schema["not"], path, root):
        excluded = schema["not"]
        if isinstance(excluded, dict) and "const" in excluded:
            errs.append(f"{path}: must not be {json.dumps(excluded['const'])}")
        else:
            errs.append(f"{path}: must not match the excluded schema")

    if "oneOf" in schema:
        errs.extend(_one_of(inst, schema["oneOf"], path, root))

    return errs


def schema_errors(doc: Any, schema: dict[str, Any]) -> list[str]:
    """JSON-Schema-subset validation (see module docstring, layer 1)."""
    _check_schema(schema, "$schema")
    return _collect(doc, schema, "$", schema)


# ---------------------------------------------------------------------------------------------
# Layer 2: semantic rules
# ---------------------------------------------------------------------------------------------

def _family_dir(rel_path: str) -> str | None:
    """First directory under ``tests/cases`` in ``rel_path`` (None if absent)."""
    parts = Path(rel_path.replace("\\", "/")).parts
    for i in range(len(parts) - 3):
        if parts[i] == "tests" and parts[i + 1] == "cases":
            return parts[i + 2]
    return None


def _step_kind(step: dict[str, Any]) -> str | None:
    for kind in STEP_KINDS:
        if kind in step:
            return kind
    return None


def semantic_errors(doc: dict[str, Any], rel_path: str) -> list[str]:
    """Cross-field rules (see module docstring, layer 2)."""
    # Lazy: keeps the normalizer registry out of the import path of plain schema checks.
    from ..compare.normalize import REGISTRY

    if not isinstance(doc, dict):
        return ["$: document must be a mapping"]

    errs: list[str] = []
    family = doc.get("family")
    case_id = doc.get("id")
    if isinstance(case_id, str) and isinstance(family, str):
        first = case_id.split(".", 1)[0]
        if first != family:
            errs.append(f"$.id: first segment {first!r} must equal family {family!r}")
    dir_family = _family_dir(rel_path)
    if dir_family is None:
        errs.append(f"$: {rel_path!r} must live under tests/cases/<family>/")
    elif isinstance(family, str) and family != dir_family:
        errs.append(f"$.family: {family!r} must equal its directory {dir_family!r}")

    sessions_raw = doc.get("sessions", ["a"])
    sessions = sessions_raw if isinstance(sessions_raw, list) else ["a"]
    declared = set(sessions)
    default_session = sessions[0] if sessions else "a"
    caps = doc.get("required_capabilities", [])

    steps = doc.get("steps", [])
    if not isinstance(steps, list):
        return errs

    pending: dict[str, int] = {}                  # session -> index of its pending async step
    prepared: dict[str, set[str]] = {}            # session -> live prepared statement names

    for i, step in enumerate(steps):
        here = f"$.steps[{i}]"
        if not isinstance(step, dict):
            continue
        kind = _step_kind(step)
        if kind is None:
            continue

        # Step-local rules, independent of session state.
        if "sqlstate" in step and step.get("expect", "any") != "error":
            errs.append(f"{here}.sqlstate: sqlstate requires expect: error")
        is_async = step.get("async", False) is True
        if "expect_blocked" in step and not is_async:
            errs.append(f"{here}.expect_blocked: expect_blocked requires async: true")
        for j, name in enumerate(step.get("normalize", [])):
            if name not in REGISTRY:
                errs.append(f"{here}.normalize[{j}]: unknown normalizer {name!r}")

        sql_value: Any = None
        sql_path: str | None = None
        if kind == "sql":
            sql_value, sql_path = step["sql"], f"{here}.sql"
        elif kind in ("parse", "exec_params"):
            sql_value, sql_path = step[kind]["sql"], f"{here}.{kind}.sql"
        if sql_path is not None and not isinstance(sql_value, str):
            errs.append(f"{sql_path}: sql must be a string; use a block scalar (|)")

        if kind in ("execute", "exec_params"):
            body = step[kind]
            params = body.get("params", [])
            formats = body.get("param_formats", [])
            if len(formats) not in (0, 1, len(params)):
                errs.append(
                    f"{here}.{kind}.param_formats: length {len(formats)} must be 0, 1 or "
                    f"{len(params)} (the number of params)")
            else:
                for j, value in enumerate(params):
                    if len(formats) == 0:
                        fmt = "text"
                    elif len(formats) == 1:
                        fmt = formats[0]
                    else:
                        fmt = formats[j]
                    if fmt == "binary" and isinstance(value, str) and \
                            _BINARY_HEX_RE.fullmatch(value) is None:
                        errs.append(f"{here}.{kind}.params[{j}]: binary parameter must be "
                                    "even-length lowercase hex")

        if kind == "restart":
            for sess in sorted(pending):
                errs.append(f"{here}: restart while the async step at "
                            f"$.steps[{pending[sess]}] is pending on session {sess!r}")
            if "server-restart" not in caps:
                errs.append(f"{here}: restart requires 'server-restart' in required_capabilities")
            prepared.clear()
            continue

        session = step.get("session", default_session)
        if session not in declared:
            errs.append(f"{here}.session: session {session!r} is not listed in sessions")

        if kind == "wait":
            if session in pending:
                del pending[session]
            else:
                errs.append(f"{here}: wait on session {session!r} without a pending async step")
            continue

        if session in pending:
            errs.append(f"{here}: session {session!r} is busy with the async step at "
                        f"$.steps[{pending[session]}]; only wait may run on it")
            continue

        names = prepared.setdefault(session, set())
        if kind == "parse":
            names.add(step["parse"].get("name", ""))
        elif kind == "exec_params":
            names.add("")                          # an unnamed Parse replaces the unnamed slot
        elif kind in ("describe", "execute", "close"):
            name = step[kind]["statement"]
            if name not in names:
                errs.append(f"{here}.{kind}.statement: unknown prepared statement {name!r} "
                            f"on session {session!r}")
            if kind == "close":
                names.discard(name)
        elif kind == "reconnect":
            names.clear()

        if is_async and kind in ("sql", "execute", "exec_params"):
            pending[session] = i

    for sess, index in sorted(pending.items(), key=lambda kv: kv[1]):
        errs.append(f"$.steps[{index}]: async step on session {sess!r} is never waited for; "
                    "add a wait step on that session")
    return errs


def validate(doc: Any, rel_path: str, schema: dict[str, Any] | None = None) -> list[str]:
    """schema_errors then (if none) semantic_errors (implemented)."""
    errs = schema_errors(doc, schema if schema is not None else load_schema())
    if errs:
        return errs
    return semantic_errors(doc, rel_path)
