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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Optional

from .model import Case


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


def load_file(path: Path, repo_root: Path) -> Case | InvalidCase:
    """Parse + validate one file (rules above)."""
    raise NotImplementedError("HAR-07: load one case file")


def load_corpus(cases_root: Path, repo_root: Path) -> Corpus:
    """Load every case under cases_root, enforce id uniqueness, return Corpus."""
    raise NotImplementedError("HAR-07: load corpus")


def select(corpus: Corpus, families: Iterable[str] = (), features: Iterable[str] = (),
           tiers: Iterable[str] = (), id_glob: Optional[str] = None,
           include_quarantined: bool = False) -> list[Case]:
    """Filter cases (empty filter = no restriction); result sorted by id."""
    raise NotImplementedError("HAR-07: corpus filters")
