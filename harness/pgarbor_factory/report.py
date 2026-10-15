"""Run reports: markdown + JSON with PASS/FAIL/BLOCKED/UNRUN counts and three denominators.

Lease FAC-13.

``write_reports(pack, clusters, tickets, run_dir)`` writes:

``report.json`` = {"campaign_id", "git_sha", "dirty", "families": [{name, status,
total_cataloged, runnable, passed, counts}], "totals", "critical", "top_clusters": first 20
clusters (key, outcome, size, leases), "tickets": first 50 ticket ids+scores, "halt"}.

``report.md`` (exact section order):
  1. ``# PGArbor campaign <id>`` + line ``git <sha> (dirty!)`` when dirty + reference digest;
  2. a banner when critical.corruption > 0 or critical.crash > 0:
     ``> **STOP THE LINE:** N corruption, M crash failures`` (RFC 002 §2B);
  3. ``## Families`` table: Family | Status | Passed | Runnable | Cataloged | then one column
     per records.COUNT_KEYS except PASS (zeros printed as ``0``, never blank);
     NEVER a single percentage of passed/total without the three denominators (RFC 002 §8);
  4. ``## Totals`` the same columns summed; FAIL is reported as the sum of MISMATCH, CRASH,
     HANG, CORRUPTION, UNEXPECTED_ACCEPTANCE (plus a separate row for UNSUPPORTED, BLOCKED,
     FLAKY, INVALID_CASE, HARNESS_ERROR, UNRUN — none of them is ever folded into PASS);
  5. ``## Top failure clusters`` (20); 6. ``## Next-epoch repair tickets`` (50);
  7. ``## Halt decision``; 8. ``## Provenance`` (toolchain, platform, corpus hashes, seeds).
Markdown table cells escape ``|`` as ``\\|``.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any, Iterable

from pgarbor_harness.model import write_canonical_json

from .records import COUNT_KEYS, EvidencePack, FamilyEvidence

#: Count columns of the family and total tables: every COUNT_KEYS entry except PASS.
_COUNT_COLUMNS: tuple[str, ...] = tuple(k for k in COUNT_KEYS if k != "PASS")

#: Statuses summed into FAIL (report section 4).
_FAIL_KEYS: tuple[str, ...] = (
    "MISMATCH", "CRASH", "HANG", "CORRUPTION", "UNEXPECTED_ACCEPTANCE",
)

#: Statuses reported on their own rows; none is ever counted as passing.
_SEPARATE_KEYS: tuple[str, ...] = (
    "UNSUPPORTED", "BLOCKED", "FLAKY", "INVALID_CASE", "HARNESS_ERROR", "UNRUN",
)

_TOP_CLUSTERS = 20
_TOP_TICKETS = 50
_NONE_TEXT = "_none_"


# ---------------------------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------------------------


def _field(obj: Any, name: str, default: Any = None) -> Any:
    """Read ``name`` from a dict or an object attribute (cluster/ticket payloads may be either)."""
    if isinstance(obj, dict):
        return obj.get(name, default)
    return getattr(obj, name, default)


def _int(value: Any) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def _score(value: Any) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return 0.0


def _cell(value: Any) -> str:
    """Render one markdown table cell: single line, ``|`` escaped as ``\\|``."""
    text = "" if value is None else str(value)
    text = text.replace("\r", " ").replace("\n", " ")
    return text.replace("|", "\\|")


def _row(cells: Iterable[Any]) -> str:
    return "| " + " | ".join(_cell(c) for c in cells) + " |"


def _table(header: list[str], rows: list[list[Any]]) -> list[str]:
    lines = [_row(header), _row(["---"] * len(header))]
    lines.extend(_row(r) for r in rows)
    return lines


def _value(value: Any) -> str:
    """Render a provenance value as plain text; absent values are said to be absent."""
    if value is None:
        return "(absent)"
    if isinstance(value, dict):
        return json.dumps(value, sort_keys=True, ensure_ascii=False)
    if isinstance(value, (list, tuple)):
        return ", ".join(str(v) for v in value) if value else "(none)"
    return str(value)


def _counts_of(family: FamilyEvidence) -> dict[str, int]:
    raw = family.counts or {}
    return {k: _int(raw.get(k, 0)) for k in COUNT_KEYS}


def _critical_counts(pack: EvidencePack) -> tuple[int, int]:
    critical = pack.critical or {}
    return _int(critical.get("corruption", 0)), _int(critical.get("crash", 0))


def _totals(families: list[FamilyEvidence]) -> dict[str, Any]:
    """Sum the family tables. Totals are computed from the families, never from PASS counts."""
    counts = {k: 0 for k in COUNT_KEYS}
    cataloged = runnable = passed = 0
    for family in families:
        cataloged += _int(family.total_cataloged)
        runnable += _int(family.runnable)
        passed += _int(family.passed)
        for key, value in _counts_of(family).items():
            counts[key] += value
    return {
        "total_cataloged": cataloged,
        "runnable": runnable,
        "passed": passed,
        "counts": counts,
        "fail": sum(counts[k] for k in _FAIL_KEYS),
    }


# ---------------------------------------------------------------------------------------------
# Markdown
# ---------------------------------------------------------------------------------------------


def _header_lines(pack: EvidencePack) -> list[str]:
    git = pack.git or {}
    sha = git.get("sha") or "(unknown)"
    dirty = bool(git.get("dirty", False))
    reference = pack.reference or {}
    lines = [f"# PGArbor campaign {pack.campaign_id}", ""]
    lines.append(f"git {sha} (dirty!)" if dirty else f"git {sha}")
    lines.append("")
    lines.append(
        "reference digest {digest}; PostgreSQL server {server}; fingerprint {fp}".format(
            digest=_value(reference.get("image_digest")),
            server=_value(reference.get("server_version")),
            fp=_value(reference.get("fingerprint_sha256")),
        )
    )
    lines.append("")
    return lines


def _banner_lines(pack: EvidencePack) -> list[str]:
    corruption, crash = _critical_counts(pack)
    if corruption > 0 or crash > 0:
        return [f"> **STOP THE LINE:** {corruption} corruption, {crash} crash failures", ""]
    return []


def _families_lines(families: list[FamilyEvidence]) -> list[str]:
    header = ["Family", "Status", "Passed", "Runnable", "Cataloged", *_COUNT_COLUMNS]
    rows: list[list[Any]] = []
    for family in families:
        counts = _counts_of(family)
        rows.append([
            family.name, family.status, _int(family.passed), _int(family.runnable),
            _int(family.total_cataloged), *(counts[k] for k in _COUNT_COLUMNS),
        ])
    lines = ["## Families", ""]
    if rows:
        lines.extend(_table(header, rows))
    else:
        lines.append(_NONE_TEXT)
    lines.append("")
    return lines


def _totals_lines(totals: dict[str, Any]) -> list[str]:
    counts = totals["counts"]
    header = ["Total", "Status", "Passed", "Runnable", "Cataloged", *_COUNT_COLUMNS]
    row = [
        "all families", "-", totals["passed"], totals["runnable"], totals["total_cataloged"],
        *(counts[k] for k in _COUNT_COLUMNS),
    ]
    lines = ["## Totals", ""]
    lines.extend(_table(header, [row]))
    lines.append("")
    outcome_rows: list[list[Any]] = [[
        "FAIL (MISMATCH + CRASH + HANG + CORRUPTION + UNEXPECTED_ACCEPTANCE)", totals["fail"],
    ]]
    outcome_rows.extend([k, counts[k]] for k in _SEPARATE_KEYS)
    lines.extend(_table(["Outcome", "Count"], outcome_rows))
    lines.append("")
    return lines


def _clusters_lines(clusters: list[Any]) -> list[str]:
    lines = ["## Top failure clusters", ""]
    top = clusters[:_TOP_CLUSTERS]
    if not top:
        lines.extend([_NONE_TEXT, ""])
        return lines
    rows = []
    for cluster in top:
        leases = _field(cluster, "lease_ids", None) or _field(cluster, "leases", None) or []
        rows.append([
            _field(cluster, "key", ""), _field(cluster, "outcome", ""),
            _int(_field(cluster, "size", 0)), ", ".join(str(x) for x in leases),
        ])
    lines.extend(_table(["Key", "Outcome", "Size", "Leases"], rows))
    lines.append("")
    return lines


def _tickets_lines(tickets: list[Any]) -> list[str]:
    lines = ["## Next-epoch repair tickets", ""]
    top = tickets[:_TOP_TICKETS]
    if not top:
        lines.extend([_NONE_TEXT, ""])
        return lines
    rows = []
    for ticket in top:
        rows.append([
            _field(ticket, "id", ""), f"{_score(_field(ticket, 'score', 0.0)):g}",
            _field(ticket, "severity", ""), "yes" if _field(ticket, "critical", False) else "no",
            _field(ticket, "title", ""),
        ])
    lines.extend(_table(["ID", "Score", "Severity", "Critical", "Title"], rows))
    lines.append("")
    return lines


def _halt_lines(pack: EvidencePack) -> list[str]:
    lines = ["## Halt decision", ""]
    halt = pack.halt or {}
    if not halt:
        lines.extend(["No halt decision recorded.", ""])
        return lines
    lines.append(f"- **action:** {_value(halt.get('action'))}")
    for key in ("reasons", "blockers", "escalate"):
        items = halt.get(key) or []
        lines.append(f"- **{key}:** " + ("; ".join(str(i) for i in items) if items else "none"))
    lines.append("")
    return lines


def _provenance_block(title: str, mapping: dict[str, Any]) -> list[str]:
    lines = [f"### {title}", ""]
    if not mapping:
        lines.extend([_NONE_TEXT, ""])
        return lines
    for key in sorted(mapping):
        lines.append(f"- **{key}:** {_value(mapping[key])}")
    lines.append("")
    return lines


def _provenance_lines(pack: EvidencePack) -> list[str]:
    lines = ["## Provenance", ""]
    lines.append(f"- **mode:** {_value(pack.mode)}")
    lines.append(f"- **epoch:** {_value(pack.epoch)}")
    lines.append(f"- **branch:** {_value((pack.git or {}).get('branch'))}")
    lines.append(f"- **started_unix:** {_value(pack.started_unix)}")
    lines.append(f"- **finished_unix:** {_value(pack.finished_unix)}")
    lines.append("")
    build = pack.build or {}
    lines.extend(_provenance_block("Build", {k: v for k, v in build.items() if k != "log"}))
    lines.extend(_provenance_block("Toolchain", pack.toolchain or {}))
    lines.extend(_provenance_block("Platform", pack.platform or {}))
    lines.extend(_provenance_block("Reference", pack.reference or {}))
    lines.extend(_provenance_block("Corpus tree hashes", pack.corpus or {}))
    lines.extend(_provenance_block("Resource", pack.resource or {}))
    lines.append("### Seeds")
    lines.append("")
    lines.append(_value([str(s) for s in pack.seeds]) if pack.seeds else _NONE_TEXT)
    lines.append("")
    return lines


def render_markdown(pack: EvidencePack, clusters: list[Any], tickets: list[Any]) -> str:
    families = list(pack.families)
    totals = _totals(families)
    lines: list[str] = []
    lines.extend(_header_lines(pack))
    lines.extend(_banner_lines(pack))
    lines.extend(_families_lines(families))
    lines.extend(_totals_lines(totals))
    lines.extend(_clusters_lines(list(clusters)))
    lines.extend(_tickets_lines(list(tickets)))
    lines.extend(_halt_lines(pack))
    lines.extend(_provenance_lines(pack))
    return "\n".join(lines).rstrip("\n") + "\n"


# ---------------------------------------------------------------------------------------------
# JSON
# ---------------------------------------------------------------------------------------------


def render_json(pack: EvidencePack, clusters: list[Any], tickets: list[Any]) -> dict[str, Any]:
    git = pack.git or {}
    families = list(pack.families)
    corruption, crash = _critical_counts(pack)
    critical = dict(pack.critical or {})
    critical["corruption"] = corruption
    critical["crash"] = crash
    top_clusters = []
    for cluster in list(clusters)[:_TOP_CLUSTERS]:
        leases = _field(cluster, "lease_ids", None) or _field(cluster, "leases", None) or []
        top_clusters.append({
            "key": _field(cluster, "key", ""),
            "outcome": _field(cluster, "outcome", ""),
            "size": _int(_field(cluster, "size", 0)),
            "leases": [str(x) for x in leases],
        })
    top_tickets = [
        {"id": _field(t, "id", ""), "score": _score(_field(t, "score", 0.0))}
        for t in list(tickets)[:_TOP_TICKETS]
    ]
    return {
        "campaign_id": pack.campaign_id,
        "git_sha": git.get("sha"),
        "dirty": bool(git.get("dirty", False)),
        "families": [
            {
                "name": f.name,
                "status": f.status,
                "total_cataloged": _int(f.total_cataloged),
                "runnable": _int(f.runnable),
                "passed": _int(f.passed),
                "counts": _counts_of(f),
            }
            for f in families
        ],
        "totals": _totals(families),
        "critical": critical,
        "top_clusters": top_clusters,
        "tickets": top_tickets,
        "halt": dict(pack.halt or {}),
    }


# ---------------------------------------------------------------------------------------------
# Writing
# ---------------------------------------------------------------------------------------------


def _write_text_atomic(path: Path, text: str) -> None:
    """Write text via a temp file in the same directory, then os.replace."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".tmp-", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def write_reports(pack: EvidencePack, clusters: list[Any], tickets: list[Any],
                  run_dir: Path) -> tuple[Path, Path]:
    """Write report.md and report.json; return their paths."""
    run_dir = Path(run_dir)
    md_path = run_dir / "report.md"
    json_path = run_dir / "report.json"
    _write_text_atomic(md_path, render_markdown(pack, clusters, tickets))
    write_canonical_json(json_path, render_json(pack, clusters, tickets))
    return md_path, json_path
