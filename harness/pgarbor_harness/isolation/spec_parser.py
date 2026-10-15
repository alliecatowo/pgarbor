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
from math import comb
from pathlib import Path
from typing import Any, NamedTuple, Optional

from ..hashing import sha256_bytes, sha256_file


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


# ---------------------------------------------------------------------------------------------
# Lexer (specscanner.l)
# ---------------------------------------------------------------------------------------------

#: flex ``space`` class; note that it excludes "\n", which the scanner counts separately.
_SPACE = " \t\r\f"
_KEYWORDS = frozenset(("notices", "permutation", "session", "setup", "step", "teardown"))
_PUNCT = frozenset("(),*")
_EOF = "EOF"


class _Tok(NamedTuple):
    kind: str     #: keyword name, "ident", "sqlblock", "int", one of "(),*", or "EOF"
    value: str
    line: int


def _is_ident_start(c: str) -> bool:
    # flex: [A-Za-z\200-\377_]; non-ASCII text is UTF-8 in the file, so every non-ASCII
    # character corresponds to bytes in that range.
    return ("a" <= c <= "z") or ("A" <= c <= "Z") or c == "_" or ord(c) >= 128


def _is_ident_cont(c: str) -> bool:
    return _is_ident_start(c) or ("0" <= c <= "9") or c == "$"


def _tokenize(text: str, name: str) -> list[_Tok]:
    toks: list[_Tok] = []
    n = len(text)
    i = 0
    line = 1
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
            continue
        if c in _SPACE:
            i += 1
            continue
        if c == "#":
            # comment: "#" followed by non-newline characters (flex {non_newline}*)
            j = i + 1
            while j < n and text[j] not in "\n\r":
                j += 1
            i = j
            continue
        if _is_ident_start(c):
            j = i + 1
            while j < n and _is_ident_cont(text[j]):
                j += 1
            word = text[i:j]
            toks.append(_Tok(word if word in _KEYWORDS else "ident", word, line))
            i = j
            continue
        if c == '"':
            start_line = line
            buf: list[str] = []
            j = i + 1
            while True:
                if j >= n:
                    raise SpecSyntaxError(f"{name}:{line}: unterminated quoted identifier")
                ch = text[j]
                if ch == '"':
                    if j + 1 < n and text[j + 1] == '"':
                        buf.append('"')
                        j += 2
                        continue
                    j += 1
                    break
                if ch == "\n":
                    raise SpecSyntaxError(
                        f"{name}:{line}: unexpected newline in quoted identifier")
                buf.append(ch)
                j += 1
            toks.append(_Tok("ident", "".join(buf), start_line))
            i = j
            continue
        if c == "{":
            # "{" space* ... space* "}": leading spaces after "{" and trailing spaces before
            # "}" are dropped; newlines are kept (specscanner.l, <sql> state). No nesting.
            j = i + 1
            while j < n and text[j] in _SPACE:
                j += 1
            close = text.find("}", j)
            if close < 0:
                raise SpecSyntaxError(f"{name}:{line}: unterminated sql block")
            body = text[j:close].rstrip(_SPACE)
            toks.append(_Tok("sqlblock", body, line))
            line += text.count("\n", i, close)
            i = close + 1
            continue
        if "0" <= c <= "9":
            j = i + 1
            while j < n and "0" <= text[j] <= "9":
                j += 1
            toks.append(_Tok("int", text[i:j], line))
            i = j
            continue
        if c in _PUNCT:
            toks.append(_Tok(c, c, line))
            i += 1
            continue
        raise SpecSyntaxError(f"{name}:{line}: syntax error: unexpected character {c!r}")
    toks.append(_Tok(_EOF, "", line))
    return toks


# ---------------------------------------------------------------------------------------------
# Parser (specparse.y, recursive descent) and semantic checks (isolationtester.c check_testspec)
# ---------------------------------------------------------------------------------------------


class _Parser:
    def __init__(self, toks: list[_Tok], name: str) -> None:
        self.toks = toks
        self.pos = 0
        self.name = name
        self.step_session: dict[str, int] = {}   #: step name -> session index

    def peek(self) -> _Tok:
        return self.toks[self.pos]

    def advance(self) -> _Tok:
        t = self.toks[self.pos]
        if t.kind != _EOF:
            self.pos += 1
        return t

    def error(self, t: _Tok) -> SpecSyntaxError:
        near = "end of input" if t.kind == _EOF else f'"{t.value}"'
        return SpecSyntaxError(f"{self.name}:{t.line}: syntax error at or near {near}")

    def expect(self, kind: str) -> _Tok:
        if self.peek().kind != kind:
            raise self.error(self.peek())
        return self.advance()

    def parse(self) -> IsolationSpec:
        setups: list[str] = []
        while self.peek().kind == "setup":
            self.advance()
            setups.append(self.expect("sqlblock").value)
        teardown: Optional[str] = None
        if self.peek().kind == "teardown":
            self.advance()
            teardown = self.expect("sqlblock").value
        sessions: list[SpecSession] = []
        while self.peek().kind == "session":
            sessions.append(self._session(len(sessions)))
        if not sessions:
            raise self.error(self.peek())
        perms: list[list[PermutationStep]] = []
        while self.peek().kind == "permutation":
            perms.append(self._permutation(sessions))
        if self.peek().kind != _EOF:
            raise self.error(self.peek())
        return IsolationSpec(name=self.name, setup=setups, teardown=teardown,
                             sessions=sessions, permutations=perms)

    def _session(self, index: int) -> SpecSession:
        self.expect("session")
        sname = self.expect("ident").value
        setup: Optional[str] = None
        if self.peek().kind == "setup":
            self.advance()
            setup = self.expect("sqlblock").value
        steps: list[SpecStep] = []
        while self.peek().kind == "step":
            self.advance()
            ident = self.expect("ident")
            sql = self.expect("sqlblock").value
            if ident.value in self.step_session:
                raise SpecSyntaxError(
                    f"{self.name}:{ident.line}: duplicate step name: {ident.value}")
            self.step_session[ident.value] = index
            steps.append(SpecStep(name=ident.value, sql=sql, session=sname))
        if not steps:
            raise self.error(self.peek())
        teardown: Optional[str] = None
        if self.peek().kind == "teardown":
            self.advance()
            teardown = self.expect("sqlblock").value
        return SpecSession(name=sname, setup=setup, steps=steps, teardown=teardown)

    def _marker(self) -> Marker:
        if self.peek().kind == "*":
            self.advance()
            return Marker(kind="star")
        step = self.expect("ident").value
        if self.peek().kind == "notices":
            self.advance()
            num = self.expect("int")
            count = int(num.value)
            if count < 1:
                raise SpecSyntaxError(
                    f"{self.name}:{num.line}: notices count must be at least 1")
            return Marker(kind="notices", step=step, count=count)
        return Marker(kind="step", step=step)

    def _permutation(self, sessions: list[SpecSession]) -> list[PermutationStep]:
        perm_line = self.expect("permutation").line
        steps: list[PermutationStep] = []
        lines: list[int] = []
        while self.peek().kind == "ident":
            t = self.advance()
            markers: list[Marker] = []
            if self.peek().kind == "(":
                self.advance()
                markers.append(self._marker())
                while self.peek().kind == ",":
                    self.advance()
                    markers.append(self._marker())
                self.expect(")")
            steps.append(PermutationStep(name=t.value, markers=markers))
            lines.append(t.line)
        if not steps:
            raise self.error(self.peek())
        self._check_permutation(steps, lines, perm_line)
        return steps

    def _check_permutation(self, steps: list[PermutationStep], lines: list[int],
                           perm_line: int) -> None:
        names = [s.name for s in steps]
        for pstep, line in zip(steps, lines):
            if pstep.name not in self.step_session:
                raise SpecSyntaxError(
                    f'{self.name}:{line}: undefined step "{pstep.name}" specified in permutation')
        for pstep in steps:
            for m in pstep.markers:
                if m.kind == "star" or m.step is None:
                    continue
                if m.step not in names:
                    raise SpecSyntaxError(
                        f'{self.name}:{perm_line}: undefined blocking step "{m.step}" '
                        f'referenced in permutation step "{pstep.name}"')
                if self.step_session[m.step] == self.step_session[pstep.name]:
                    raise SpecSyntaxError(
                        f'{self.name}:{perm_line}: permutation step "{pstep.name}" '
                        f"cannot block on its own session")


def parse_spec(text: str, name: str = "<spec>") -> IsolationSpec:
    """Tokenize + parse per the grammar; raise SpecSyntaxError with line numbers."""
    toks = _tokenize(text, name)
    return _Parser(toks, name).parse()


def _count_all_permutations(spec: IsolationSpec) -> int:
    """Number of interleavings = total! / prod(len_i!), computed without generating them."""
    result = 1
    running = 0
    for length in (len(s.steps) for s in spec.sessions):
        running += length
        result *= comb(running, length)
    return result


def all_permutations(spec: IsolationSpec) -> list[list[PermutationStep]]:
    """Every interleaving (see module docstring); markers empty."""
    seqs = [[st.name for st in s.steps] for s in spec.sessions]
    total = sum(len(s) for s in seqs)
    piles = [0] * len(seqs)
    out: list[list[PermutationStep]] = []
    chosen: list[str] = []

    def recurse() -> None:
        if len(chosen) == total:
            out.append([PermutationStep(name=n, markers=[]) for n in chosen])
            return
        # isolationtester: try each session in declaration order that still has steps
        for i, seq in enumerate(seqs):
            if piles[i] < len(seq):
                chosen.append(seq[piles[i]])
                piles[i] += 1
                recurse()
                piles[i] -= 1
                chosen.pop()

    recurse()
    return out


def parse_schedule(text: str) -> list[str]:
    """``test: name`` lines -> names in order (several names per line allowed, flattened)."""
    names: list[str] = []
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if not line.startswith("test:"):
            raise SpecSyntaxError(f"isolation_schedule:{lineno}: expected 'test: <name>'")
        names.extend(line[len("test:"):].split())
    return names


#: Format tag of the manifest document returned by build_manifest.
ISOLATION_MANIFEST_FORMAT = "pgarbor-isolation-manifest/1"


def _expected_paths(isolation_dir: Path, name: str) -> list[Path]:
    """Base expected file first, then the ``_N`` variants that exist (N in 0..9)."""
    exp_dir = isolation_dir / "expected"
    out: list[Path] = []
    base = exp_dir / f"{name}.out"
    if base.is_file():
        out.append(base)
    for n in range(10):
        variant = exp_dir / f"{name}_{n}.out"
        if variant.is_file():
            out.append(variant)
    return out


def build_manifest(isolation_dir: Path) -> dict[str, Any]:
    """{"format": "pgarbor-isolation-manifest/1", "tests": {name: {"spec": rel, "spec_sha256",
    "expected": [{"path", "sha256"}], "sessions": n, "permutations": n (explicit or generated),
    "parse_error": str|null}}, "order": [names]}"""
    schedule_path = isolation_dir / "isolation_schedule"
    order = parse_schedule(schedule_path.read_text(encoding="utf-8"))
    tests: dict[str, Any] = {}
    for name in order:
        spec_rel = f"specs/{name}.spec"
        spec_path = isolation_dir / spec_rel
        entry: dict[str, Any] = {
            "spec": spec_rel,
            "spec_sha256": None,
            "expected": [],
            "sessions": 0,
            "permutations": 0,
            "parse_error": None,
        }
        if not spec_path.is_file():
            entry["parse_error"] = f"spec file missing: {spec_rel}"
        else:
            raw = spec_path.read_bytes()
            entry["spec_sha256"] = sha256_bytes(raw)
            try:
                spec = parse_spec(raw.decode("utf-8"), name=spec_rel)
            except ValueError as exc:   # SpecSyntaxError and UnicodeDecodeError
                entry["parse_error"] = str(exc)
            else:
                entry["sessions"] = len(spec.sessions)
                entry["permutations"] = (len(spec.permutations) if spec.permutations
                                         else _count_all_permutations(spec))
        for path in _expected_paths(isolation_dir, name):
            entry["expected"].append({
                "path": path.relative_to(isolation_dir).as_posix(),
                "sha256": sha256_file(path),
            })
        tests[name] = entry
    return {"format": ISOLATION_MANIFEST_FORMAT, "tests": tests, "order": order}
