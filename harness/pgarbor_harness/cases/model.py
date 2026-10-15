"""Typed in-memory form of a differential case (pgarbor-case/1).

CONTRACT (harness v1). Implemented; workers import, never edit. The YAML/JSON shape is defined
by ``contracts/oracle/case.schema.json``; ``cases.loader`` turns a validated dict into these
objects via ``Case.from_dict`` (implemented here so every consumer agrees on defaults).

Defaults applied by from_dict:
  * ``sessions`` -> ``["a"]``; a step without ``session`` uses ``sessions[0]``
    (``restart`` steps have session ``""``)
  * ``expect`` -> ``"any"``, ``order`` -> ``"auto"``, ``compare_message`` -> True,
    ``normalize`` -> [], ``async`` -> False
  * parse ``name`` -> ``""`` (unnamed statement), ``param_types`` -> []
  * ``params`` -> [], ``param_formats`` -> [] (all text), ``result_format`` -> ``"text"``
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any, Literal, Optional

CASE_FORMAT = "pgarbor-case/1"

StepKind = Literal["sql", "parse", "describe", "execute", "exec_params", "close", "wait",
                   "reconnect", "restart"]
STEP_KINDS: tuple[str, ...] = ("sql", "parse", "describe", "execute", "exec_params", "close",
                               "wait", "reconnect", "restart")
#: Step kinds that may carry ``async: true``.
ASYNC_KINDS: tuple[str, ...] = ("sql", "execute", "exec_params")
#: Step kinds whose results are compared row-wise (``order`` applies).
ROW_KINDS: tuple[str, ...] = ("sql", "execute", "exec_params", "wait")

Expect = Literal["any", "ok", "rows", "error", "notice"]
OrderMode = Literal["auto", "ordered", "multiset"]

SETUP_SESSION = "setup"


@dataclass
class Step:
    """One case step. Exactly one action is set, named by ``kind``.

    Action payloads (``payload``), by kind:
      * sql:         {"sql": str}
      * parse:       {"name": str, "sql": str, "param_types": [int]}
      * describe:    {"statement": str}
      * execute:     {"statement": str, "params": [str|None], "param_formats": ["text"|"binary"],
                      "result_format": "text"|"binary"}
      * exec_params: {"sql": str, "params": [...], "param_types": [int],
                      "param_formats": [...], "result_format": ...}
      * close:       {"statement": str}
      * wait / reconnect: {}
      * restart:     {"mode": "clean"|"kill"}
    """

    index: int
    kind: str
    session: str
    payload: dict[str, Any]
    is_async: bool = False
    expect_blocked: Optional[bool] = None
    expect: str = "any"
    sqlstate: Optional[str] = None
    order: str = "auto"
    compare_message: bool = True
    normalize: list[str] = field(default_factory=list)
    comment: str = ""

    @property
    def sql(self) -> Optional[str]:
        """SQL text for sql/parse/exec_params steps, else None."""
        if self.kind in ("sql", "parse", "exec_params"):
            return self.payload.get("sql")
        return None

    @classmethod
    def from_dict(cls, index: int, d: dict[str, Any], default_session: str) -> "Step":
        kinds = [k for k in STEP_KINDS if k in d]
        if len(kinds) != 1:
            raise ValueError(f"step {index}: exactly one action key required, got {kinds}")
        kind = kinds[0]
        raw = d[kind]
        if kind == "sql":
            payload: dict[str, Any] = {"sql": raw}
        elif kind == "parse":
            payload = {"name": raw.get("name", ""), "sql": raw["sql"],
                       "param_types": list(raw.get("param_types", []))}
        elif kind in ("describe", "close"):
            payload = {"statement": raw["statement"]}
        elif kind == "execute":
            payload = {"statement": raw["statement"], "params": list(raw.get("params", [])),
                       "param_formats": list(raw.get("param_formats", [])),
                       "result_format": raw.get("result_format", "text")}
        elif kind == "exec_params":
            payload = {"sql": raw["sql"], "params": list(raw.get("params", [])),
                       "param_types": list(raw.get("param_types", [])),
                       "param_formats": list(raw.get("param_formats", [])),
                       "result_format": raw.get("result_format", "text")}
        elif kind == "restart":
            payload = {"mode": raw}
        else:
            payload = {}
        session = "" if kind == "restart" else d.get("session", default_session)
        return cls(
            index=index, kind=kind, session=session, payload=payload,
            is_async=bool(d.get("async", False)), expect_blocked=d.get("expect_blocked"),
            expect=d.get("expect", "any"), sqlstate=d.get("sqlstate"),
            order=d.get("order", "auto"), compare_message=bool(d.get("compare_message", True)),
            normalize=list(d.get("normalize", [])), comment=d.get("comment", ""),
        )


_CLUSTER_GLOBAL_RE = re.compile(
    r"\b(CREATE|ALTER|DROP)\s+(ROLE|USER|GROUP|DATABASE|TABLESPACE|(TRUSTED\s+)?(PROCEDURAL\s+)?LANGUAGE)\b"
    r"|\b(DROP|REASSIGN)\s+OWNED\b|\bALTER\s+DEFAULT\s+PRIVILEGES\b|\bSET\s+(SESSION\s+)?ROLE\b"
    r"|\bSET\s+SESSION\s+AUTHORIZATION\b|\bGRANT\b|\bREVOKE\b|\bOR\s+REPLACE\s+(TRUSTED\s+)?(PROCEDURAL\s+)?LANGUAGE\b",
    re.IGNORECASE)


@dataclass
class Case:
    """A validated case. ``path`` is repo-relative POSIX (``tests/cases/smoke/x.yaml``).
    ``sha256`` is hashing.case_sha256 of the file bytes (filled by the loader)."""

    id: str
    title: str
    family: str
    feature_ids: list[str]
    steps: list[Step]
    sessions: list[str] = field(default_factory=lambda: ["a"])
    required_capabilities: list[str] = field(default_factory=list)
    tier: str = "core"
    provenance: dict[str, str] = field(default_factory=dict)
    seed: Optional[int] = None
    setup: Optional[str] = None
    quarantine: Optional[dict[str, str]] = None
    notes: str = ""
    path: str = ""
    sha256: str = ""

    @property
    def needs_restart(self) -> bool:
        return any(s.kind == "restart" for s in self.steps)

    @property
    def touches_cluster_globals(self) -> bool:
        """True when the case creates/alters/drops cluster-wide objects (roles, databases,
        tablespaces, languages) or rewrites role-owned state. Per-case databases do not isolate
        these, so such cases run serially after the parallel pool (like restart cases)."""
        texts = [self.setup or ""] + [str(s.payload.get("sql", "")) for s in self.steps]
        return any(_CLUSTER_GLOBAL_RE.search(t) for t in texts)

    @property
    def runs_serially(self) -> bool:
        return self.needs_restart or self.touches_cluster_globals

    @property
    def is_multi_session(self) -> bool:
        return len({s.session for s in self.steps if s.session}) > 1

    @classmethod
    def from_dict(cls, d: dict[str, Any], path: str = "", sha256: str = "") -> "Case":
        """Build from an ALREADY VALIDATED dict (cases.validate). Applies defaults only."""
        sessions = list(d.get("sessions", ["a"]))
        steps = [Step.from_dict(i, s, sessions[0]) for i, s in enumerate(d["steps"])]
        return cls(
            id=d["id"], title=d["title"], family=d["family"],
            feature_ids=list(d["feature_ids"]), steps=steps, sessions=sessions,
            required_capabilities=list(d.get("required_capabilities", [])),
            tier=d.get("tier", "core"), provenance=dict(d.get("provenance", {})),
            seed=d.get("seed"), setup=d.get("setup"), quarantine=d.get("quarantine"),
            notes=d.get("notes", ""), path=path, sha256=sha256,
        )
