"""SWARM-09: interface drift reconciler — one canonical amendment per drift, fanned out to
affected leases; never silent per-worker redefinition.

Lease FAC-18.

Amendment files: ``contracts/amendments/<crate>-<NN>-<slug>.md`` (written by architects or by
``open_amendment``; the factory never edits contracts/ outside this API, and this API requires
PGARBOR_ORACLE_REVIEW=1 OR ``--architect`` mode env PGARBOR_ARCHITECT=1). Required header
lines (parsed with ``^(\\w[\\w ]*):\\s*(.*)$`` before the first blank line)::

    Status: PROPOSED | RATIFIED | REJECTED | APPLIED
    Crate: pgarbor-storage
    Affects: pgarbor-core, pgarbor-fault
    Leases: STO-03, CORE-02
    Ticket: AMEND-02-001     (optional)

then markdown sections ``## Problem``, ``## Proposed change``, ``## Affected crates``.

``scan(repo_root)`` -> list[Amendment]; ``drift_from_build(clusters, specs, epoch)`` turns
build ErrorClusters with root_hint "interface_drift" into Amendment proposals (deduplicated by
cluster key; existing PROPOSED amendments mentioning the same item are reused);
``affected_leases`` = union of the cluster's leases and leases owning files of ``Affects``
crates that reference the drifting item (grep the item name in their files).
``suspend`` marks every affected lease in the ledger with a history note
"suspended: <amendment>" and keeps them out of scheduler.plan_wave until the amendment is
RATIFIED (RFC 003 §11 "Shared contract drift").
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

from pgarbor_harness.config import Paths

#: Directory (repo-relative) holding every amendment file.
AMENDMENT_DIR = "contracts/amendments"
#: Environment gates for ``open_amendment``. Either one set to exactly "1" permits writing.
ARCHITECT_ENV = "PGARBOR_ARCHITECT"
ORACLE_REVIEW_ENV = "PGARBOR_ORACLE_REVIEW"
#: Statuses whose leases are NOT suspended.
CLOSED_STATUSES = frozenset({"RATIFIED", "APPLIED"})

_HEADER_RE = re.compile(r"^(\w[\w ]*):\s*(.*)$")
_LEASE_ID_RE = re.compile(r"^[A-Z][A-Z0-9]{1,9}-\d{2,3}$")
_LEASE_RANGE_RE = re.compile(
    r"^([A-Z][A-Z0-9]{1,9})-(\d{2,3})\.\.(?:([A-Z][A-Z0-9]{1,9})-)?(\d{2,3})$"
)
_FILENAME_RE = re.compile(r"^(.+?)-(\d{2,3})-")
_CRATE_RE = re.compile(r"^[a-z][a-z0-9_-]*$")
_PAREN_RE = re.compile(r"\([^)]*\)")
_LIST_SPLIT_RE = re.compile(r"[,;]")
_BACKTICK_RE = re.compile(r"`([^`]+)`")
_IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
_NON_ITEM_WORDS = frozenset({
    "bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128", "usize",
    "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "Self", "self", "super",
    "crate", "Option", "Result", "Vec", "Box", "mut", "ref",
})


class AmendmentError(RuntimeError):
    """Invalid amendment input (never raised for read-only scans of file data)."""


@dataclass
class Amendment:
    path: str
    status: str
    crate: str
    affects: list[str] = field(default_factory=list)
    leases: list[str] = field(default_factory=list)
    ticket: str = ""
    title: str = ""


# ---------------------------------------------------------------------------------------------
# Parsing helpers
# ---------------------------------------------------------------------------------------------


def _get(obj: Any, name: str, default: Any = None) -> Any:
    """Attribute or key access, so clusters/specs may be dataclasses or loaded JSON dicts."""
    if isinstance(obj, dict):
        return obj.get(name, default)
    return getattr(obj, name, default)


def _expand_leases(value: str) -> list[str]:
    """``"HAR-01..HAR-03, FAC-09 (campaign); pgarbor-cli leases"`` -> explicit lease ids.

    Parenthetical annotations are dropped, ranges are expanded (zero-padding kept), and
    anything that is not a lease id (e.g. prose) is ignored. Order preserved, no duplicates.
    """
    out: list[str] = []
    for raw in _LIST_SPLIT_RE.split(value):
        token = _PAREN_RE.sub("", raw).strip()
        if not token:
            continue
        m = _LEASE_RANGE_RE.match(token)
        if m:
            prefix, lo, prefix2, hi = m.group(1), m.group(2), m.group(3), m.group(4)
            if prefix2 is not None and prefix2 != prefix:
                continue
            width = len(lo)
            for n in range(int(lo), int(hi) + 1):
                lease_id = f"{prefix}-{n:0{width}d}"
                if lease_id not in out:
                    out.append(lease_id)
            continue
        if _LEASE_ID_RE.match(token) and token not in out:
            out.append(token)
    return out


def _split_names(value: str) -> list[str]:
    """Comma/semicolon separated names (crates or paths); ``none`` and blanks dropped."""
    out: list[str] = []
    for raw in _LIST_SPLIT_RE.split(value):
        token = _PAREN_RE.sub("", raw).strip()
        if token and token.lower() != "none" and token not in out:
            out.append(token)
    return out


def _header_block(text: str) -> tuple[dict[str, str], str]:
    """Header ``Key: value`` lines (first occurrence wins) and the first ``# `` title.

    The block runs from the top of the file to the first ``## `` section. Blank lines and
    markdown headings are skipped, so a status written as prose after the title (as in the
    pgarbor-* amendments) is still found.
    """
    headers: dict[str, str] = {}
    title = ""
    for line in text.splitlines():
        if line.startswith("## "):
            break
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("#"):
            if not title and line.startswith("# "):
                title = line[2:].strip()
            continue
        m = _HEADER_RE.match(stripped)
        if m:
            headers.setdefault(m.group(1).strip().lower(), m.group(2).strip())
    return headers, title


def _first_word(value: str) -> str:
    words = _PAREN_RE.sub("", value).strip().split()
    return words[0] if words else ""


def _parse_amendment(path: Path, rel: str) -> Amendment:
    text = path.read_text(encoding="utf-8", errors="replace")
    headers, title = _header_block(text)
    status_m = re.match(r"[A-Za-z]+", headers.get("status", ""))
    status = status_m.group(0).upper() if status_m else ""
    file_m = _FILENAME_RE.match(path.name)
    crate = _first_word(headers.get("crate", "")) or (file_m.group(1) if file_m else "")
    return Amendment(
        path=rel,
        status=status,
        crate=crate,
        affects=_split_names(headers.get("affects", "")),
        leases=_expand_leases(headers.get("leases", "")),
        ticket=_first_word(headers.get("ticket", "")),
        title=title,
    )


def _word_re(item: str) -> "re.Pattern[str]":
    return re.compile(r"\b" + re.escape(item) + r"\b")


def _def_re(item: str) -> "re.Pattern[str]":
    return re.compile(
        r"\b(?:struct|enum|trait|type|fn|const|static|union|mod)\s+" + re.escape(item) + r"\b"
    )


def _expand_pattern(root: Path, pattern: str) -> list[Path]:
    """Files matching one lease ``files`` entry (plain path or glob), repo-relative only."""
    if not pattern or pattern.startswith("/") or ".." in Path(pattern).parts:
        return []
    if any(c in pattern for c in "*?["):
        return [p for p in root.glob(pattern) if p.is_file()]
    candidate = root / pattern
    return [candidate] if candidate.is_file() else []


def _lease_file_texts(root: Path, specs: list[Any]) -> dict[str, list[str]]:
    texts: dict[str, list[str]] = {}
    for spec in specs:
        lease_id = str(_get(spec, "lease_id", ""))
        bucket = texts.setdefault(lease_id, [])
        for pattern in _get(spec, "files", []) or []:
            for path in _expand_pattern(root, str(pattern)):
                try:
                    bucket.append(path.read_text(encoding="utf-8", errors="replace"))
                except OSError:
                    continue
    return texts


def _drift_item(cluster: Any) -> str:
    """The drifting item name: leaf identifier of the first backticked rustc item.

    Prefers qualified or capitalised paths (``pgarbor_core::Widget``, ``Widget``) over bare
    lowercase identifiers; primitives such as ``u32`` are never the drifting item.
    """
    fallback = ""
    for example in _get(cluster, "examples", []) or []:
        message = str(_get(example, "message", "") or "")
        for m in _BACKTICK_RE.finditer(message):
            token = m.group(1).strip()
            idents = [i for i in _IDENT_RE.findall(token) if i not in _NON_ITEM_WORDS]
            if not idents:
                continue
            leaf = idents[-1]
            if "::" in token or leaf[0].isupper():
                return leaf
            if not fallback:
                fallback = leaf
    return fallback


def _defining_crate(item: str, specs: list[Any], texts: dict[str, list[str]]) -> str:
    """Crate of the first lease (by id) whose files define ``item``; "" when none does."""
    pattern = _def_re(item)
    for spec in sorted(specs, key=lambda s: str(_get(s, "lease_id", ""))):
        lease_id = str(_get(spec, "lease_id", ""))
        if any(pattern.search(t) for t in texts.get(lease_id, [])):
            return str(_get(spec, "crate", ""))
    return ""


def _merged(am: Amendment, leases: set[str], affects: list[str]) -> Amendment:
    return replace(
        am,
        leases=sorted(set(am.leases) | leases),
        affects=sorted(set(am.affects) | set(affects)),
    )


def _reusable(existing: list[Amendment], root: Path, item: str) -> Amendment | None:
    """First existing PROPOSED amendment whose file mentions ``item``."""
    pattern = _word_re(item)
    for am in existing:
        if am.status != "PROPOSED":
            continue
        try:
            text = (root / am.path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if pattern.search(text):
            return am
    return None


# ---------------------------------------------------------------------------------------------
# Public API (CONTRACT: signatures frozen by contracts/crates/harness.md)
# ---------------------------------------------------------------------------------------------


def scan(repo_root: Path) -> list[Amendment]:
    """Every ``contracts/amendments/*.md`` under ``repo_root``, sorted by file name."""
    directory = Path(repo_root) / AMENDMENT_DIR
    if not directory.is_dir():
        return []
    out: list[Amendment] = []
    for path in sorted(directory.glob("*.md")):
        if path.is_file():
            out.append(_parse_amendment(path, f"{AMENDMENT_DIR}/{path.name}"))
    return out


def drift_from_build(clusters: list[Any], lease_specs: list[Any], epoch: int) -> list[Amendment]:
    """Interface-drift clusters -> amendment proposals (one per drifting item).

    Only clusters with ``root_hint == "interface_drift"`` count. Clusters are deduplicated by
    cluster key and by drifting item, so one drift yields one Amendment. An existing PROPOSED
    amendment whose file mentions the item is reused (returned with the new leases/crates
    merged in). Proposals are not written; pass them to ``open_amendment``.
    ``affected_leases`` = the cluster's leases plus leases of the involved crates whose files
    reference the item. Repo files are read from ``Paths().root``.
    """
    root = Paths().root
    specs = list(lease_specs)
    crate_of = {str(_get(s, "lease_id", "")): str(_get(s, "crate", "")) for s in specs}
    texts = _lease_file_texts(root, specs)
    existing = scan(root)

    results: list[Amendment] = []
    by_item: dict[str, int] = {}
    seen_keys: set[str] = set()
    new_count = 0
    for cluster in clusters:
        if _get(cluster, "root_hint", "") != "interface_drift":
            continue
        key = str(_get(cluster, "key", ""))
        if key in seen_keys:
            continue
        seen_keys.add(key)

        item = _drift_item(cluster)
        cluster_leases = [str(x) for x in (_get(cluster, "leases", []) or [])]
        involved = {str(_get(ex, "crate", "")) for ex in (_get(cluster, "examples", []) or [])}
        involved |= {crate_of[lid] for lid in cluster_leases if crate_of.get(lid)}
        involved.discard("")
        affects = sorted(involved)

        leases = set(cluster_leases)
        if item:
            pattern = _word_re(item)
            leases |= {
                lid
                for lid, bodies in texts.items()
                if crate_of.get(lid, "") in involved and any(pattern.search(b) for b in bodies)
            }

        if item and item in by_item:
            idx = by_item[item]
            results[idx] = _merged(results[idx], leases, affects)
            continue

        reused = _reusable(existing, root, item) if item else None
        if reused is not None:
            am = _merged(reused, leases, affects)
        else:
            new_count += 1
            code = str(_get(cluster, "code", ""))
            label = item or key
            am = Amendment(
                path="",
                status="PROPOSED",
                crate=_defining_crate(item, specs, texts) if item else "",
                affects=affects,
                leases=sorted(leases),
                ticket=f"AMEND-{epoch:02d}-{new_count:03d}",
                title=f"Interface drift: {label} ({code})" if code else f"Interface drift: {label}",
            )
        results.append(am)
        if item:
            by_item[item] = len(results) - 1
    return results


def _render(crate: str, affects: list[str], leases: list[str], ticket: str, title: str,
            problem: str, proposal: str) -> str:
    lines = [
        "Status: PROPOSED",
        f"Crate: {crate}",
        f"Affects: {', '.join(affects) if affects else 'none'}",
        f"Leases: {', '.join(leases) if leases else 'none'}",
    ]
    if ticket:
        lines.append(f"Ticket: {ticket}")
    heading = title.strip().lstrip("#").strip() or f"{crate} amendment"
    body = [
        "",
        f"# {heading}",
        "",
        "## Problem",
        "",
        problem.strip(),
        "",
        "## Proposed change",
        "",
        proposal.strip(),
        "",
        "## Affected crates",
        "",
    ]
    body += [f"- {name}" for name in affects] or ["None."]
    return "\n".join(lines + body) + "\n"


def _slugify(title: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    return slug[:60].strip("-")


def _next_number(directory: Path, crate: str) -> int:
    pattern = re.compile(r"^" + re.escape(crate) + r"-(\d{2,3})-")
    best = 0
    if directory.is_dir():
        for entry in directory.iterdir():
            m = pattern.match(entry.name)
            if m:
                best = max(best, int(m.group(1)))
    return best + 1


def open_amendment(repo_root: Path, amendment: Amendment, problem: str, proposal: str) -> Path:
    """Write a new PROPOSED amendment file and return its path.

    Refuses (PermissionError) unless PGARBOR_ARCHITECT=1 or PGARBOR_ORACLE_REVIEW=1. Files are
    created exclusively, so an existing amendment is never overwritten. The number is the
    next free ``<crate>-NN`` in ``contracts/amendments``.
    """
    if os.environ.get(ARCHITECT_ENV) != "1" and os.environ.get(ORACLE_REVIEW_ENV) != "1":
        raise PermissionError(
            "open_amendment refused: set PGARBOR_ARCHITECT=1 (architect) or "
            "PGARBOR_ORACLE_REVIEW=1 (oracle review) to write contracts/amendments"
        )
    crate = amendment.crate.strip()
    if not _CRATE_RE.match(crate):
        raise AmendmentError(f"invalid amendment crate {amendment.crate!r}")
    directory = Path(repo_root) / AMENDMENT_DIR
    directory.mkdir(parents=True, exist_ok=True)
    body = _render(
        crate=crate,
        affects=list(amendment.affects),
        leases=list(amendment.leases),
        ticket=amendment.ticket.strip(),
        title=amendment.title,
        problem=problem,
        proposal=proposal,
    )
    slug = _slugify(amendment.title) or "amendment"
    number = _next_number(directory, crate)
    while True:
        path = directory / f"{crate}-{number:02d}-{slug}.md"
        try:
            with open(path, "x", encoding="utf-8", newline="\n") as fh:
                fh.write(body)
            return path
        except FileExistsError:
            number += 1


def suspended_leases(repo_root: Path) -> dict[str, str]:
    """lease_id -> amendment path for every non-RATIFIED/APPLIED amendment listing it."""
    out: dict[str, str] = {}
    for am in scan(repo_root):
        if am.status in CLOSED_STATUSES:
            continue
        for lease_id in am.leases:
            out.setdefault(lease_id, am.path)
    return out
