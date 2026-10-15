"""Stub-marker inventory: the factory's UNIMPLEMENTED count (ARCHITECTURE §0.1).

Lease FAC-03.

Markers (exact syntactic forms the skeletons use):
  * Rust: ``PgError::unimplemented("<LEASE-ID>", "<what>")`` — regex
    ``PgError::unimplemented\\(\\s*"([A-Z][A-Z0-9]{1,9}-\\d{2,3})"\\s*,\\s*"((?:[^"\\\\]|\\\\.)*)"``
    (may span lines: run over the whole file text, compute line numbers from match.start()).
  * Python: ``NotImplementedError("<LEASE-ID>: <what>")`` — regex
    ``NotImplementedError\\(\\s*"([A-Z][A-Z0-9]{1,9}-\\d{2,3}):\\s*([^"]*)"``.
Scanned trees: ``crates/**/*.rs`` and ``harness/**/*.py`` (skip ``target/``, ``.venv/``,
``__pycache__``, ``.claude/``). Comments are NOT excluded (a commented-out marker still counts;
skeleton files never contain those).

Also counts forbidden placeholders that ARCHITECTURE §0.1 bans (reported separately, never
part of UNIMPLEMENTED): Rust ``todo!(``, ``unimplemented!(``, ``panic!(`` outside
``#[cfg(test)]`` modules (balanced module bodies, strings/comments skipped), Python NotImplementedError raised WITHOUT a ``"<LEASE-ID>: "`` message prefix, ``pass  # TODO``.

``inventory(repo_root)`` -> document (canonical JSON at runs/<campaign>/inventory.json)::

    {"format": "pgarbor-inventory/1", "git_sha": ..., "total": n,
     "by_lease": {"STO-03": 4}, "by_crate": {"pgarbor-storage": 12, "harness": 80},
     "by_file": {"crates/x/src/a.rs": 3},
     "markers": [{"file", "line", "lease_id", "what"}],   # sorted by file, line
     "forbidden": [{"file", "line", "kind", "text"}]}

``crate`` for a file: ``crates/<name>/...`` -> ``<name>``; ``harness/...`` -> ``harness``.
"""

from __future__ import annotations

import os
import re
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from pgarbor_factory.gitutil import GitError, head_sha

FORMAT = "pgarbor-inventory/1"

_RUST_MARKER_RE = re.compile(
    r'PgError::unimplemented\(\s*"([A-Z][A-Z0-9]{1,9}-\d{2,3})"\s*,\s*"((?:[^"\\]|\\.)*)"'
)
_PY_MARKER_RE = re.compile(r'NotImplementedError\(\s*"([A-Z][A-Z0-9]{1,9}-\d{2,3}):\s*([^"]*)"')
_RUST_FORBIDDEN_RE = re.compile(r"(?<![A-Za-z0-9_])(todo|unimplemented|panic)!\(")
_PY_BARE_RAISE_RE = re.compile(r"raise\s+(NotImplementedError)\b")
_PY_PASS_TODO_RE = re.compile(r"\s*pass\s*#\s*TODO\b")
_UNESCAPE_RE = re.compile(r'\\([\\"])')
_SKIP_DIRS = frozenset({"target", ".venv", "__pycache__", ".claude", ".git"})
_SCAN_ROOTS = (("crates", ".rs"), ("harness", ".py"))


@dataclass(frozen=True)
class Marker:
    file: str
    line: int
    lease_id: str
    what: str


def _line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def _line_text(text: str, pos: int) -> str:
    start = text.rfind("\n", 0, pos) + 1
    end = text.find("\n", pos)
    if end == -1:
        end = len(text)
    return text[start:end].strip()


def _finding(text: str, rel: str, pos: int, kind: str) -> dict[str, Any]:
    return {"file": rel, "line": _line_of(text, pos), "kind": kind, "text": _line_text(text, pos)}


def _without_rust_test_modules(text: str) -> str:
    """Blank cfg(test) modules, preserving offsets and production items after them.

    Skip strings/comments while balancing module braces: an example containing "}" must
    not move the test/production boundary. Unknown/unclosed modules remain inventoried.
    """
    chars = list(text)
    module = re.compile(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+\w+\s*\{")
    token = re.compile(
        r'//[^\n]*|/\*|b?r(?P<hashes>\#*)"|b?"(?:\\.|[^"\\])*"'
        r"|'(?:\\.|[^'\\\n])'|[{}]", re.DOTALL)
    for match in module.finditer(text):
        depth, pos, end = 1, match.end(), None
        while depth:
            found = token.search(text, pos)
            if found is None:
                break
            value, pos = found.group(), found.end()
            if value == "/*":
                nesting = 1
                while nesting:
                    comment = re.search(r"/\*|\*/", text[pos:])
                    if comment is None:
                        pos = len(text)
                        break
                    pos += comment.end()
                    nesting += 1 if comment.group() == "/*" else -1
            elif found.group("hashes") is not None:
                closing = '"' + found.group("hashes")
                close = text.find(closing, pos)
                pos = len(text) if close < 0 else close + len(closing)
            elif value == "{":
                depth += 1
            elif value == "}":
                depth -= 1
                if depth == 0:
                    end = pos
        if end is not None:
            for i in range(match.start(), end):
                if chars[i] != "\n":
                    chars[i] = " "
    return "".join(chars)


def _scan_rust(text: str, rel: str) -> tuple[list[Marker], list[dict[str, Any]]]:
    # Constructor examples in cfg(test) modules are test data, not production stubs.
    production = _without_rust_test_modules(text)
    markers = [
        Marker(rel, _line_of(production, m.start()), m.group(1), _UNESCAPE_RE.sub(r"\1", m.group(2)))
        for m in _RUST_MARKER_RE.finditer(production)
    ]
    forbidden: list[dict[str, Any]] = []
    for lineno, raw in enumerate(production.split("\n"), 1):
        line = raw.rstrip("\r")
        for fm in _RUST_FORBIDDEN_RE.finditer(line):
            forbidden.append(
                {"file": rel, "line": lineno, "kind": f"rust-{fm.group(1)}", "text": line.strip()}
            )
    return markers, forbidden


def _scan_python(text: str, rel: str) -> tuple[list[Marker], list[dict[str, Any]]]:
    markers: list[Marker] = []
    marker_starts: set[int] = set()
    for m in _PY_MARKER_RE.finditer(text):
        marker_starts.add(m.start())
        markers.append(Marker(rel, _line_of(text, m.start()), m.group(1), m.group(2)))
    forbidden: list[dict[str, Any]] = []
    for m in _PY_BARE_RAISE_RE.finditer(text):
        pos = m.start(1)
        if pos not in marker_starts:
            forbidden.append(_finding(text, rel, pos, "python-not-implemented"))
    for lineno, line in enumerate(text.split("\n"), 1):
        if _PY_PASS_TODO_RE.match(line):
            forbidden.append(
                {"file": rel, "line": lineno, "kind": "python-pass-todo", "text": line.strip()}
            )
    return markers, forbidden


def scan_file(path: Path, rel: str) -> tuple[list[Marker], list[dict[str, Any]]]:
    """(markers, forbidden findings) for one file."""
    text = path.read_text(encoding="utf-8", errors="replace")
    if path.suffix == ".rs":
        return _scan_rust(text, rel)
    if path.suffix == ".py":
        return _scan_python(text, rel)
    raise ValueError(f"not a Rust or Python source file: {rel}")


def _source_files(repo_root: Path):
    for top, suffix in _SCAN_ROOTS:
        base = repo_root / top
        if not base.is_dir():
            continue
        for dirpath, dirnames, filenames in os.walk(base):
            dirnames[:] = sorted(d for d in dirnames if d not in _SKIP_DIRS)
            for fn in sorted(filenames):
                if fn.endswith(suffix):
                    yield Path(dirpath) / fn


def _collect(repo_root: Path) -> tuple[list[Marker], list[dict[str, Any]]]:
    markers: list[Marker] = []
    forbidden: list[dict[str, Any]] = []
    for path in _source_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        m, f = scan_file(path, rel)
        markers.extend(m)
        forbidden.extend(f)
    markers.sort(key=lambda mk: (mk.file, mk.line))
    forbidden.sort(key=lambda d: (d["file"], d["line"]))
    return markers, forbidden


def scan_markers(repo_root: Path) -> list[Marker]:
    """All markers in the scanned trees, sorted by (file, line)."""
    markers, _ = _collect(Path(repo_root))
    return markers


def _crate_of(rel: str) -> str:
    parts = rel.split("/")
    if parts[0] == "crates" and len(parts) >= 2:
        return parts[1]
    if parts[0] == "harness":
        return "harness"
    return parts[0]


def _git_sha(repo_root: Path) -> str | None:
    try:
        return head_sha(repo_root)
    except (GitError, OSError):
        return None


def inventory(repo_root: Path) -> dict[str, Any]:
    """Full inventory document (module docstring)."""
    root = Path(repo_root)
    markers, forbidden = _collect(root)
    by_lease: Counter[str] = Counter()
    by_crate: Counter[str] = Counter()
    by_file: Counter[str] = Counter()
    for mk in markers:
        by_lease[mk.lease_id] += 1
        by_crate[_crate_of(mk.file)] += 1
        by_file[mk.file] += 1
    return {
        "format": FORMAT,
        "git_sha": _git_sha(root),
        "total": len(markers),
        "by_lease": dict(sorted(by_lease.items())),
        "by_crate": dict(sorted(by_crate.items())),
        "by_file": dict(sorted(by_file.items())),
        "markers": [
            {"file": mk.file, "line": mk.line, "lease_id": mk.lease_id, "what": mk.what}
            for mk in markers
        ],
        "forbidden": forbidden,
    }


def delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    """{"total_before", "total_after", "closed_by_lease": {id: n}, "new_markers": [...]} —
    new markers appearing in a lease's files after a patch are reported (a worker may not add
    stubs for behavior it was asked to implement unless the file's lease spec allows it)."""
    for name, doc in (("before", before), ("after", after)):
        if doc.get("format") != FORMAT:
            raise ValueError(f"{name} is not a {FORMAT} document: {doc.get('format')!r}")
    before_m: list[dict[str, Any]] = before.get("markers", [])
    after_m: list[dict[str, Any]] = after.get("markers", [])

    def key(d: dict[str, Any]) -> tuple[str, str, str]:
        # Line numbers shift when a patch edits a file, so identity is (file, lease, what).
        return (d["file"], d["lease_id"], d["what"])

    # Multiset difference, per occurrence: before-markers with no remaining match are closed.
    after_left = Counter(key(d) for d in after_m)
    closed: Counter[str] = Counter()
    for d in before_m:
        k = key(d)
        if after_left[k] > 0:
            after_left[k] -= 1
        else:
            closed[d["lease_id"]] += 1

    before_left = Counter(key(d) for d in before_m)
    new_markers: list[dict[str, Any]] = []
    for d in after_m:
        k = key(d)
        if before_left[k] > 0:
            before_left[k] -= 1
        else:
            new_markers.append(
                {"file": d["file"], "line": d["line"], "lease_id": d["lease_id"], "what": d["what"]}
            )
    return {
        "total_before": before.get("total", len(before_m)),
        "total_after": after.get("total", len(after_m)),
        "closed_by_lease": dict(sorted(closed.items())),
        "new_markers": new_markers,
    }
