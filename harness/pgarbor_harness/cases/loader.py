"""ORACLE-03: discover, parse, validate and filter the case corpus.

Lease HAR-07.

* Discovery: every ``*.yaml`` under ``paths.cases`` (recursively), sorted by repo-relative
  POSIX path; ``README.md`` and dotfiles ignored; ``*.yml`` is an error (one extension only).
* Parsing: ``yaml.safe_load`` on the exact file bytes (decoded UTF-8, BOM rejected). A file
  must contain exactly one YAML document whose root is a mapping. Duplicate mapping keys must
  be rejected: subclass ``yaml.SafeLoader`` and override ``construct_mapping`` to raise on a
  repeated key (PyYAML silently keeps the last one otherwise).
* Validation: cases.validate.validate(doc, rel_path). Invalid files do NOT abort loading:
  they are returned as ``InvalidCase`` entries so reports count them as INVALID_CASE.
* ``sha256`` = hashing.case_sha256(raw bytes).
* Global rule: case ids are unique across the corpus; a duplicate makes BOTH entries invalid
  ("duplicate id also defined in <path>").

Filters (``select``): family (exact), feature (any of ``feature_ids``), tier, id glob
(fnmatch on id), and ``include_quarantined`` (default False).
"""

from __future__ import annotations

import collections.abc
import fnmatch
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Optional

import yaml

from ..hashing import case_sha256
from .model import Case
from .validate import validate


@dataclass
class InvalidCase:
    path: str
    case_id: Optional[str]
    errors: list[str]


@dataclass
class Corpus:
    cases: list[Case] = field(default_factory=list)
    invalid: list[InvalidCase] = field(default_factory=list)

    def by_id(self) -> dict[str, Case]:
        return {c.id: c for c in self.cases}


# ---------------------------------------------------------------------------------------------
# Strict YAML loading
# ---------------------------------------------------------------------------------------------


class StrictLoader(yaml.SafeLoader):
    """SafeLoader that rejects repeated mapping keys instead of keeping the last one.

    Merge keys (``<<``) are flattened first, so a key that both appears explicitly and is
    supplied by a merge is also treated as a repeat.
    """

    def construct_mapping(self, node, deep=False):  # type: ignore[override]
        if isinstance(node, yaml.MappingNode):
            self.flatten_mapping(node)
            seen: set[Any] = set()
            for key_node, _value_node in node.value:
                key = self.construct_object(key_node, deep=True)
                if not isinstance(key, collections.abc.Hashable):
                    raise yaml.constructor.ConstructorError(
                        "while constructing a mapping", node.start_mark,
                        "found unhashable key", key_node.start_mark)
                if key in seen:
                    raise yaml.constructor.ConstructorError(
                        "while constructing a mapping", node.start_mark,
                        f"found duplicate key {key!r}", key_node.start_mark)
                seen.add(key)
        return super().construct_mapping(node, deep=deep)


StrictLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG,
                             StrictLoader.construct_yaml_map)

_BOM = b"\xef\xbb\xbf"


def _one_line(text: str) -> str:
    return " ".join(text.split())


def _parse_document(raw: bytes) -> tuple[Optional[dict[str, Any]], list[str]]:
    """Decode and parse exact file bytes into the single root mapping, or report why not."""
    if raw.startswith(_BOM):
        return None, ["file starts with a UTF-8 byte order mark; remove it"]
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        return None, [f"file is not valid UTF-8: {exc.reason} at byte {exc.start}"]
    try:
        docs = list(yaml.load_all(text, Loader=StrictLoader))
    except yaml.YAMLError as exc:
        return None, [f"YAML error: {_one_line(str(exc))}"]
    except RecursionError:
        return None, ["YAML error: document is nested too deeply"]
    if len(docs) != 1:
        return None, [f"expected exactly one YAML document, found {len(docs)}"]
    doc = docs[0]
    if not isinstance(doc, dict):
        return None, ["document root must be a mapping"]
    return doc, []


def _declared_id(doc: Optional[dict[str, Any]]) -> Optional[str]:
    if doc is None:
        return None
    value = doc.get("id")
    return value if isinstance(value, str) else None


@dataclass
class _Scan:
    """Result of reading and validating one file, before the cross-file id rule is applied."""

    rel: str
    doc: Optional[dict[str, Any]]
    errors: list[str]
    sha256: str


def _scan(path: Path, rel: str) -> _Scan:
    name = path.name
    if name.endswith(".yml"):
        return _Scan(rel, None, ["unsupported extension .yml; case files use .yaml"], "")
    if not name.endswith(".yaml"):
        return _Scan(rel, None, ["not a .yaml case file"], "")
    try:
        raw = path.read_bytes()
    except OSError as exc:
        return _Scan(rel, None, [f"cannot read file: {_one_line(str(exc))}"], "")
    sha = case_sha256(raw)
    doc, errors = _parse_document(raw)
    if doc is None:
        return _Scan(rel, None, errors, sha)
    return _Scan(rel, doc, errors + validate(doc, rel), sha)


def _to_entry(scan: _Scan) -> Case | InvalidCase:
    if scan.errors or scan.doc is None:
        return InvalidCase(path=scan.rel, case_id=_declared_id(scan.doc),
                           errors=list(scan.errors))
    try:
        return Case.from_dict(scan.doc, path=scan.rel, sha256=scan.sha256)
    except (KeyError, TypeError, ValueError, AttributeError, IndexError) as exc:
        return InvalidCase(path=scan.rel, case_id=_declared_id(scan.doc),
                           errors=[f"cannot build case: {type(exc).__name__}: {exc}"])


def _rel(path: Path, repo_root: Path) -> str:
    return path.resolve().relative_to(repo_root.resolve()).as_posix()


# ---------------------------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------------------------


def load_file(path: Path, repo_root: Path) -> Case | InvalidCase:
    """Parse + validate one file (rules above)."""
    rel = _rel(path, repo_root)
    return _to_entry(_scan(path, rel))


def _discover(cases_root: Path) -> list[Path]:
    """Case files under cases_root: ``.yaml``/``.yml`` only, skipping README.md and dotfiles
    (any path component starting with a dot)."""
    out: list[Path] = []
    for p in cases_root.rglob("*"):
        rel_parts = p.relative_to(cases_root).parts
        if any(part.startswith(".") for part in rel_parts):
            continue
        if p.name == "README.md" or not p.is_file():
            continue
        if p.name.endswith((".yaml", ".yml")):
            out.append(p)
    return out


def load_corpus(cases_root: Path, repo_root: Path) -> Corpus:
    """Load every case under cases_root, enforce id uniqueness, return Corpus."""
    root = cases_root.resolve()
    repo = repo_root.resolve()
    entries = sorted(((p, p.relative_to(root).as_posix()) for p in _discover(root)),
                     key=lambda t: t[1])
    scans = [_scan(p, p.relative_to(repo).as_posix()) for p, _ in entries]

    # Global rule: an id declared by more than one file makes every declaring file invalid.
    # Every file whose root mapping carries a string id counts as declaring it, even if it is
    # otherwise invalid, so ids stay unique as a corpus-wide namespace.
    declared: dict[str, list[str]] = {}
    for scan in scans:
        cid = _declared_id(scan.doc)
        if cid is not None:
            declared.setdefault(cid, []).append(scan.rel)
    for scan in scans:
        cid = _declared_id(scan.doc)
        if cid is None:
            continue
        for other in declared[cid]:
            if other != scan.rel:
                scan.errors.append(f"duplicate id also defined in {other}")

    corpus = Corpus()
    for scan in scans:
        entry = _to_entry(scan)
        if isinstance(entry, Case):
            corpus.cases.append(entry)
        else:
            corpus.invalid.append(entry)
    return corpus


def select(corpus: Corpus, families: Iterable[str] = (), features: Iterable[str] = (),
           tiers: Iterable[str] = (), id_glob: Optional[str] = None,
           include_quarantined: bool = False) -> list[Case]:
    """Filter cases (empty filter = no restriction); result sorted by id."""
    fams = frozenset(families)
    feats = frozenset(features)
    tier_set = frozenset(tiers)
    out: list[Case] = []
    for c in corpus.cases:
        if fams and c.family not in fams:
            continue
        if feats and feats.isdisjoint(c.feature_ids):
            continue
        if tier_set and c.tier not in tier_set:
            continue
        if id_glob and not fnmatch.fnmatchcase(c.id, id_glob):
            continue
        if not include_quarantined and c.quarantine is not None:
            continue
        out.append(c)
    out.sort(key=lambda c: c.id)
    return out
