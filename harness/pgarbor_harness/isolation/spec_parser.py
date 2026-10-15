"""ORACLE-06: parser for PostgreSQL isolation test specs + schedule manifest.

Lease HAR-19.

Grammar (src/test/isolation/specparse.y + specscanner.l + README; reference reading)::

    spec        := setup* teardown? session+ permutation*
    setup       := "setup" sqlblock          (each block = ONE PQexec submission)
    teardown    := "teardown" sqlblock
    session     := "session" ident setup? step+ teardown?
    step        := "step" ident sqlblock
    permutation := "permutation" pstep+
    pstep       := ident ( "(" marker ("," marker)* ")" )?
    marker      := "*" | ident | ident "notices" INTEGER
    sqlblock    := "{" any chars except "}" "}"   (text between braces; leading/trailing
                   whitespace trimmed like specscanner.l does: it strips leading whitespace
                   and trailing whitespace/newlines of the block)
    ident       := [A-Za-z_][A-Za-z0-9_$]* | '"' ( [^"] | '""' )* '"'   (NO case folding)
    comment     := "#" to end of line (outside sqlblocks only)

Semantics checked: step names unique across the spec; permutation steps must name existing
steps; markers must name existing steps; ``notices`` count >= 1. If there are no permutation
lines, ``all_permutations`` generates every interleaving of session step sequences (order
within a session preserved) in the same order isolationtester's ``run_all_permutations``
does (recursive: at each position try sessions in declaration order).

Schedule: ``isolation_schedule`` lines ``test: <name>`` (one test per line in PG18); spec file
``specs/<name>.spec``, expected ``expected/<name>.out`` plus ``_N.out`` variants (same rule as
regress).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional


@dataclass
class Marker:
    kind: str                 #: "star" | "step" | "notices"
    step: Optional[str] = None
    count: int = 0


@dataclass
class PermutationStep:
    name: str
    markers: list[Marker] = field(default_factory=list)


@dataclass
class SpecStep:
    name: str
    sql: str
    session: str


@dataclass
class SpecSession:
    name: str
    setup: Optional[str]
    steps: list[SpecStep]
    teardown: Optional[str]


@dataclass
class IsolationSpec:
    name: str
    setup: list[str]
    teardown: Optional[str]
    sessions: list[SpecSession]
    permutations: list[list[PermutationStep]]   #: explicit ones; empty -> all_permutations

    def step_by_name(self) -> dict[str, SpecStep]:
        """Implemented helper."""
        return {st.name: st for s in self.sessions for st in s.steps}


class SpecSyntaxError(ValueError):
    """``<file>:<line>: <message>`` like specparse's yyerror."""


def parse_spec(text: str, name: str = "<spec>") -> IsolationSpec:
    """Tokenize + parse per the grammar; raise SpecSyntaxError with line numbers."""
    raise NotImplementedError("HAR-19: isolation spec parser")


def all_permutations(spec: IsolationSpec) -> list[list[PermutationStep]]:
    """Every interleaving (see module docstring); markers empty."""
    raise NotImplementedError("HAR-19: generate permutations")


def parse_schedule(text: str) -> list[str]:
    """``test: name`` lines -> names in order (several names per line allowed, flattened)."""
    raise NotImplementedError("HAR-19: isolation schedule")


def build_manifest(isolation_dir: Path) -> dict[str, Any]:
    """{"format": "pgarbor-isolation-manifest/1", "tests": {name: {"spec": rel, "spec_sha256",
    "expected": [{"path", "sha256"}], "sessions": n, "permutations": n (explicit or generated),
    "parse_error": str|null}}, "order": [names]}"""
    raise NotImplementedError("HAR-19: isolation manifest")
