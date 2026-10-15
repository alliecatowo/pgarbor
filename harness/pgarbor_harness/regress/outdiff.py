"""ORACLE-05: compare a regress results file with expected outputs exactly like pg_regress.

Lease HAR-18.

pg_regress.c results_differ() semantics (reference reading):
  1. candidate expected files: the resultmap substitute if any, else ``expected/<t>.out``;
     then ``expected/<t>_0.out`` .. ``expected/<t>_9.out`` that exist;
  2. a test PASSES if the results file is identical to ANY candidate, compared byte-exactly
     after ``--strip-trailing-cr`` (a ``\\r`` immediately before ``\\n`` is removed on both
     sides; nothing else is normalized: whitespace, trailing newline, case all matter);
  3. on failure, the reported diff is against the candidate with the FEWEST differing lines
     (count of '+'/'-' lines in a unified diff, ties -> earliest candidate), rendered as a
     unified diff with 3 lines of context (``diff -U3``), headers
     ``diff -U3 <expected path> <results path>`` / ``--- <expected path>\\t<mtime>`` style is
     NOT required: use ``--- <expected>`` and ``+++ <results>`` without timestamps (difflib).

Also used for the differential baseline: comparing a target's results file with the PG
oracle's recorded results file (``tests/expected/regress/<t>.out``) uses the same function
with a single candidate.
"""

from __future__ import annotations

import difflib
from dataclasses import dataclass
from pathlib import Path
from typing import Optional, Sequence


@dataclass
class OutDiff:
    identical: bool
    matched: Optional[Path]        #: candidate that matched (when identical)
    best: Optional[Path]           #: closest candidate (when not identical)
    diff_lines: int                #: +/- line count vs best (0 when identical)
    unified: str                   #: unified diff text vs best ("" when identical)


def strip_trailing_cr(data: bytes) -> bytes:
    """Remove a CR immediately preceding LF (implemented)."""
    return data.replace(b"\r\n", b"\n")


_CONTEXT_LINES = 3


def _decode(data: bytes) -> str:
    return data.decode("utf-8", errors="surrogateescape")


def _split_lines(text: str) -> list[str]:
    """Split on LF only (as pg_regress's line-oriented view does), keeping the LF.

    ``str.splitlines`` is avoided on purpose: it also breaks on CR, VT, FF, U+2028 and
    friends, which would change line counts for outputs that contain them.
    """
    parts = text.split("\n")
    lines = [p + "\n" for p in parts[:-1]]
    if parts[-1]:
        lines.append(parts[-1])
    return lines


def _load_lines(path: Path) -> list[str]:
    """Lines of ``path`` with CR-before-LF removed, as used for diffing."""
    return _split_lines(_decode(strip_trailing_cr(path.read_bytes())))


def _diff_text(
    expected_lines: list[str],
    results_lines: list[str],
    expected_name: str,
    results_name: str,
    context: int,
) -> tuple[int, str]:
    """Render a unified diff and count its changed lines.

    difflib emits the two header lines first; they are skipped by position, not by prefix,
    because a genuine removed line such as ``-- comment`` renders as ``--- comment``.
    """
    changed = 0
    out: list[str] = []
    gen = difflib.unified_diff(
        expected_lines,
        results_lines,
        fromfile=expected_name,
        tofile=results_name,
        n=context,
        lineterm="",
    )
    for index, line in enumerate(gen):
        if index >= 2 and line[:1] in ("+", "-"):
            changed += 1
        out.append(line if line.endswith("\n") else line + "\n")
    return changed, "".join(out)


def compare_output(results: Path, candidates: Sequence[Path]) -> OutDiff:
    """Apply the algorithm above. No candidates -> OutDiff(False, None, None, -1,
    "no expected output") ; missing results file -> same with "no results file"."""
    existing = [Path(c) for c in candidates if Path(c).is_file()]
    if not existing:
        return OutDiff(False, None, None, -1, "no expected output")

    results_path = Path(results)
    if not results_path.is_file():
        return OutDiff(False, None, None, -1, "no results file")

    results_bytes = strip_trailing_cr(results_path.read_bytes())

    # Byte-exact (after CR stripping) identity with any candidate passes; earliest wins.
    for candidate in existing:
        if strip_trailing_cr(candidate.read_bytes()) == results_bytes:
            return OutDiff(True, candidate, None, 0, "")

    results_lines = _split_lines(_decode(results_bytes))
    best: Optional[Path] = None
    best_count = -1
    best_text = ""
    for candidate in existing:
        count, text = _diff_text(
            _load_lines(candidate),
            results_lines,
            str(candidate),
            str(results_path),
            _CONTEXT_LINES,
        )
        # Strict '<' keeps the earliest candidate on ties.
        if best is None or count < best_count:
            best = candidate
            best_count = count
            best_text = text
    return OutDiff(False, None, best, best_count, best_text)


def unified_diff(expected: Path, results: Path, context: int = 3) -> tuple[int, str]:
    """(changed line count, unified diff text) using difflib on lines decoded UTF-8 with
    errors='surrogateescape' and keepends=True.

    Lines are LF-delimited and CR-before-LF is stripped first, so a CRLF-only difference
    produces no changed lines (same normalization as ``compare_output``).
    """
    expected_path = Path(expected)
    results_path = Path(results)
    return _diff_text(
        _load_lines(expected_path),
        _load_lines(results_path),
        str(expected_path),
        str(results_path),
        context,
    )
