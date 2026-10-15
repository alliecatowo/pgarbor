"""SWARM-07: static contract review pass (no compilation, no execution).

Lease FAC-17.

``review_patch(repo_root, spec, base_sha, head_sha)`` inspects the lease's diff and the
resulting files and returns findings ``{"rule", "severity": "reject"|"warn", "file",
"line", "detail"}``. Rules:

  reject:
  * ``contract_item_changed``: any line between a ``// CONTRACT`` (Rust) / ``CONTRACT`` marker
    comment and the end of the following item (next blank line at the same indentation) that
    differs from base_sha — frozen signatures must not change (compare the item text with
    whitespace collapsed);
  * ``signature_changed``: a ``pub fn``/``pub struct``/``pub enum``/``pub trait`` (Rust) or
    top-level ``def``/``class`` / method ``def`` (Python) present at base is missing or has a
    different signature line at head (normalize whitespace; Python: compare the ``def ...:``
    header including annotations);
  * ``forbidden_placeholder``: added lines containing ``todo!(``, ``unimplemented!(``,
    ``panic!(`` (outside ``#[cfg(test)]``), ``unwrap()`` on lines mentioning ``parse``/
    ``from_utf8``/``decode``/``read`` (user/file input), Python NotImplementedError raised
    whose id is not this lease's, ``pass`` as the only statement of a function body that was a
    stub at base;
  * ``fake_success``: a function whose base body was a stub marker and whose new body is
    only ``Ok(())`` / ``Ok(Vec::new())`` / ``Ok(None)`` / ``return []`` / ``return None`` /
    ``return {}`` (single-statement) — flagged reject unless the docstring/doc comment says
    the empty result is the specified behavior (contains "returns empty"/"no-op" text);
  * ``test_weakening``: removed ``assert``/``#[test]`` lines, added ``#[ignore]``,
    ``@pytest.mark.skip`` or ``xfail``;
  warn:
  * ``stub_remaining``: lease markers still present (count);
  * ``large_file``: file grew beyond 1500 lines.
The review is advisory except for reject rules, which make assemble.check_patch reject.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any


def review_patch(repo_root: Path, spec: Any, base_sha: str, head_sha: str) -> list[dict[str, Any]]:
    raise NotImplementedError("FAC-17: static patch review")


def contract_items(text: str, lang: str) -> dict[str, str]:
    """Map a stable item key (``fn name`` / ``struct Name`` / ``def name`` / ``class Name``)
    to its normalized CONTRACT-marked text for ``lang`` in ("rust", "python")."""
    raise NotImplementedError("FAC-17: extract contract items")
