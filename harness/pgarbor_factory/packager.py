"""SWARM-05: agent input contract packager — a bounded, versioned context packet per lease.

Lease FAC-16.

``build_packet(repo_root, lease_id)`` returns (and ``write_packet`` stores at
``runs/packets/<LEASE-ID>.md``) a markdown document with EXACTLY these sections, in order:

  1. ``# Lease <ID> — <summary>``; lines: crate, model tier, contract_version, base SHA
     (``git rev-parse HEAD``), task ids with their DAG titles/deliverables;
  2. ``## Rules`` — fixed text: only edit the listed files; never edit Cargo.toml / lib.rs /
     mod.rs / __init__.py / anything under contracts/, harness/oracle/, tests/expected/;
     replace every ``PgError::unimplemented("<ID>", ...)`` / ``NotImplementedError("<ID>: ...")``
     in your files with real behavior; do not add todo!/unimplemented!/panic!/fake results;
     do not run cargo/pytest/podman (cathedral mode); file contract problems as
     ``contracts/amendments/<crate>-<NN>-<slug>.md`` requests in your final message instead
     of changing signatures; commit on branch ``lease/<ID>`` and submit;
  3. ``## Your files`` — each leased file path with its CURRENT full content in a fenced
     block (language from extension);
  4. ``## Spec`` — the lease ``spec`` and ``acceptance`` list verbatim;
  5. ``## Read for context`` — for each ``reads`` entry: if it is a file under 400 lines,
     inline it; else list the path with its public signatures extracted (Rust: lines starting
     with ``pub ``/``pub(crate) `` + preceding ``///`` docs; Python: ``def``/``class`` lines +
     docstrings' first paragraph);
  6. ``## Contract`` — ``contracts/crates/<crate>.md`` inlined (harness leases:
     contracts/crates/harness.md).
Size bound: ``max_chars`` (default 120_000); when exceeded, section 5 entries are reduced to
signatures, then dropped from the end (listed as "omitted: <path>"). Deterministic output.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

from pgarbor_harness.config import PROTECTED_GLOBS, VERIFIER_GLOBS, Paths

from .gitutil import head_sha
from .records import LeaseSpec

#: Reads at or above this many lines are never inlined (signatures only).
INLINE_MAX_LINES = 400

_LANG = {".rs": "rust", ".py": "python", ".md": "markdown", ".json": "json", ".toml": "toml"}

# Rust: a ``pub`` item line (fields such as ``pub x: T,`` are deliberately excluded).
_RUST_PUB_RE = re.compile(
    r"^\s*pub(?:\([^)]*\))?\s+(?:(?:const|async|unsafe|extern|default)\s+)*"
    r"(?:fn|struct|enum|trait|type|const|static|mod|union)\b"
)
# Python: a def/class line, public names only (must start with a letter).
_PY_DEF_RE = re.compile(r"^(\s*)(async\s+def|def|class)\s+([A-Za-z][A-Za-z0-9_]*)\b")


# ---------------------------------------------------------------------------------------------
# Lookup helpers
# ---------------------------------------------------------------------------------------------


def _find_lease(repo_root: Path, lease_id: str) -> LeaseSpec:
    """Locate a lease in contracts/leases/*.json (sorted by file name)."""
    for path in sorted(Paths(root=repo_root).leases_dir.glob("*.json")):
        doc = json.loads(path.read_text(encoding="utf-8"))
        crate = str(doc.get("crate", ""))
        version = str(doc.get("contract_version", "v1"))
        for entry in doc.get("leases", []):
            if isinstance(entry, dict) and entry.get("lease_id") == lease_id:
                return LeaseSpec.from_json(crate, version, entry)
    raise ValueError(f"unknown lease: {lease_id}")


def _dag_tasks(repo_root: Path) -> dict[str, dict]:
    path = Paths(root=repo_root).dag
    if not path.is_file():
        return {}
    graph = json.loads(path.read_text(encoding="utf-8"))
    return {
        t["id"]: t for t in graph.get("tasks", []) if isinstance(t, dict) and "id" in t
    }


def _read_text(path: Path) -> str | None:
    if not path.is_file():
        return None
    return path.read_text(encoding="utf-8", errors="replace")


def _lang(rel: str) -> str:
    return _LANG.get(Path(rel).suffix, "")


def _fence(text: str, lang: str) -> str:
    """Fenced block whose fence is longer than any backtick run inside ``text``."""
    longest = run = 0
    for ch in text:
        run = run + 1 if ch == "`" else 0
        longest = max(longest, run)
    fence = "`" * max(3, longest + 1)
    body = text if (text == "" or text.endswith("\n")) else text + "\n"
    return f"{fence}{lang}\n{body}{fence}"


def _join(sections: list[str]) -> str:
    return "\n\n".join(s.rstrip("\n") for s in sections) + "\n"


# ---------------------------------------------------------------------------------------------
# Sections
# ---------------------------------------------------------------------------------------------


def _header_section(repo_root: Path, lease: LeaseSpec) -> str:
    tasks = _dag_tasks(repo_root)
    title = f"# Lease {lease.lease_id}"
    if lease.summary:
        title += f" — {lease.summary}"
    task_lines: list[str] = []
    for tid in lease.task_ids:
        t = tasks.get(tid)
        if t is None:
            task_lines.append(f"  - {tid}: (not found in the task DAG)")
        else:
            task_lines.append(
                f"  - {tid}: {t.get('title', '')}. Deliverable: {t.get('deliverable', '')}"
            )
    if not task_lines:
        task_lines = ["  - (none)"]
    lines = [
        title,
        "",
        f"- Crate: {lease.crate}",
        f"- Model tier: {lease.model_tier}",
        f"- Contract version: {lease.contract_version}",
        f"- Base SHA: {head_sha(repo_root)}",
        "- Tasks:",
        *task_lines,
    ]
    return "\n".join(lines)


def _rules_section(lease: LeaseSpec) -> str:
    lid = lease.lease_id
    protected = ", ".join(f"`{g}`" for g in PROTECTED_GLOBS)
    verifier = ", ".join(f"`{g}`" for g in VERIFIER_GLOBS)
    lines = [
        "## Rules",
        "",
        '1. Only edit the files listed under "Your files". Every other path is read-only for you.',
        "2. Never edit `Cargo.toml`, `lib.rs`, `mod.rs`, `__init__.py`, `__main__.py`, or "
        "anything under `contracts/`, `harness/oracle/` or `tests/expected/`.",
        f"3. Protected paths (never writable by feature leases): {protected}.",
        f"4. Verifier paths (only HAR-/FAC- leases may touch them, and only their own files): "
        f"{verifier}.",
        f'5. Replace every `PgError::unimplemented("{lid}", ...)` (Rust) or '
        f'`NotImplementedError("{lid}: ...")` (Python) in your files with real, '
        "PostgreSQL-faithful behavior.",
        "6. Do not add `todo!`, `unimplemented!`, `panic!`, fake results or silent no-ops. "
        "Never panic on user or file data.",
        "7. Do not run cargo, pytest or podman (cathedral mode). Write carefully and re-read "
        "your code for compile and type errors.",
        "8. If a contract is wrong or insufficient, file a request as "
        "`contracts/amendments/<crate>-<NN>-<slug>.md` in your final message instead of "
        "changing signatures.",
        f"9. Commit on branch `lease/{lid}` and submit.",
    ]
    return "\n".join(lines)


def _files_section(repo_root: Path, lease: LeaseSpec) -> str:
    parts = ["## Your files"]
    for rel in lease.files:
        parts.append(f"### `{rel}`")
        text = _read_text(repo_root / rel)
        if text is None:
            parts.append("_(file does not exist yet)_")
        else:
            parts.append(_fence(text, _lang(rel)))
    return "\n\n".join(parts)


def _spec_section(lease: LeaseSpec) -> str:
    acceptance = [f"- {a}" for a in lease.acceptance] or ["- (none)"]
    return "\n".join(
        [
            "## Spec",
            "",
            lease.spec.strip() or "(none)",
            "",
            "### Acceptance",
            "",
            *acceptance,
        ]
    )


def _contract_section(repo_root: Path, lease: LeaseSpec) -> str:
    rel = f"contracts/crates/{lease.crate}.md"
    text = _read_text(repo_root / rel)
    body = "_(missing)_" if text is None else _fence(text, "markdown")
    return f"## Contract\n\nSource: `{rel}`\n\n{body}"


def _read_entry(repo_root: Path, rel: str, signatures: bool) -> str:
    header = f"### `{rel}`"
    path = repo_root / rel
    text = _read_text(path)
    if text is None:
        return f"{header}\n\n_(missing)_"
    n_lines = len(text.splitlines())
    if not signatures and n_lines < INLINE_MAX_LINES:
        return f"{header}\n\n{_fence(text, _lang(rel))}"
    if path.suffix in (".rs", ".py"):
        sigs = extract_signatures(path)
        return (
            f"{header}\n\n_{n_lines} lines; public signatures only._\n\n"
            f"{_fence(sigs, _lang(rel))}"
        )
    return f"{header}\n\n_{n_lines} lines; not inlined. Read it directly._"


def _reads_body(repo_root: Path, reads: list[str], signatures: bool, keep: int) -> str:
    if not reads:
        return "(none)"
    parts = [_read_entry(repo_root, rel, signatures) for rel in reads[:keep]]
    parts += [f"- omitted: {rel}" for rel in reads[keep:]]
    return "\n\n".join(parts)


# ---------------------------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------------------------


def build_packet(repo_root: Path, lease_id: str, max_chars: int = 120_000) -> str:
    """Context packet for one lease (sections and bounds: see module docstring).

    Degradation when over ``max_chars``: first every section 5 entry becomes its signatures,
    then the entries are dropped from the end (each listed as ``omitted: <path>``). Raises
    ValueError when the packet cannot fit even with every read omitted.
    """
    root = Path(repo_root)
    lease = _find_lease(root, lease_id)
    header = _header_section(root, lease)
    rules = _rules_section(lease)
    files = _files_section(root, lease)
    spec = _spec_section(lease)
    contract = _contract_section(root, lease)

    n = len(lease.reads)
    variants = [(False, n), (True, n)] + [(True, k) for k in range(n - 1, -1, -1)]
    for signatures, keep in variants:
        reads = "## Read for context\n\n" + _reads_body(root, lease.reads, signatures, keep)
        packet = _join([header, rules, files, spec, reads, contract])
        if len(packet) <= max_chars:
            return packet
    raise ValueError(
        f"packet for {lease_id} exceeds max_chars={max_chars} even with all reads omitted"
    )


def extract_signatures(path: Path) -> str:
    """Public-signature summary of a .rs or .py file (rules in section 5)."""
    path = Path(path)
    suffix = path.suffix
    if suffix not in (".rs", ".py"):
        raise ValueError(f"no signature extraction for {path.name}")
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    out = _rust_signatures(lines) if suffix == ".rs" else _python_signatures(lines)
    return "\n".join(out).strip("\n") or "(no public signatures found)"


def _rust_signatures(lines: list[str]) -> list[str]:
    out: list[str] = []
    docs: list[str] = []
    for idx, raw in enumerate(lines):
        s = raw.strip()
        if s.startswith("///"):
            body = s[3:]
            docs.append(body[1:] if body.startswith(" ") else body)
            continue
        if s.startswith("#["):
            continue  # attributes keep the pending doc block attached
        if _RUST_PUB_RE.match(raw):
            out.extend(f"/// {d}".rstrip() for d in docs)
            out.append(_rust_sig(lines, idx))
            out.append("")
        docs = []
    return out


def _rust_sig(lines: list[str], start: int) -> str:
    """The item header from ``start`` up to its first ``{`` or ``;`` (at most 15 lines)."""
    parts: list[str] = []
    for j in range(start, min(len(lines), start + 15)):
        ln = lines[j].strip()
        cuts = [k for k in (ln.find("{"), ln.find(";")) if k >= 0]
        if cuts:
            parts.append(ln[: min(cuts)].rstrip())
            break
        parts.append(ln)
    return " ".join(p for p in parts if p)


def _python_signatures(lines: list[str]) -> list[str]:
    out: list[str] = []
    in_class = False
    n = len(lines)
    i = 0
    while i < n:
        raw = lines[i]
        stripped = raw.strip()
        indent = len(raw) - len(raw.lstrip())
        if not stripped:
            i += 1
            continue
        if indent == 0 and not stripped.startswith(("@", "#")):
            in_class = stripped.startswith("class ")
        m = _PY_DEF_RE.match(raw)
        if not m or not (indent == 0 or (in_class and indent == 4)):
            i += 1
            continue
        # Signature: from the def line up to the line ending in ':' (at most 20 lines).
        j = i
        sig_parts: list[str] = []
        while j < n and j < i + 20:
            part = lines[j].strip()
            sig_parts.append(part)
            if part.endswith(":"):
                break
            j += 1
        last = j
        out.append(" ".join(sig_parts))
        # Docstring: first paragraph only, if it is the first statement of the body.
        k = last + 1
        while k < n and not lines[k].strip():
            k += 1
        para = ""
        if k < n:
            s = lines[k].strip()
            quote = next((q for q in ('"""', "'''") if s.startswith(q)), None)
            if quote is not None:
                body = s[3:]
                if quote in body:
                    block = [body[: body.index(quote)]]
                    last = k
                else:
                    block = [body]
                    k2 = k + 1
                    while k2 < n:
                        t = lines[k2]
                        if quote in t:
                            block.append(t[: t.index(quote)])
                            break
                        block.append(t)
                        k2 += 1
                    last = min(k2, n - 1)
                para = _first_paragraph(block)
        if para:
            out.append(f'    """{para}"""')
        out.append("")
        i = last + 1
    return out


def _first_paragraph(block: list[str]) -> str:
    para: list[str] = []
    for line in block:
        t = line.strip()
        if not t:
            if para:
                break
            continue
        para.append(t)
    return " ".join(para)


def write_packet(repo_root: Path, lease_id: str) -> Path:
    """Build the packet and store it at ``runs/packets/<LEASE-ID>.md`` (atomic replace)."""
    root = Path(repo_root)
    text = build_packet(root, lease_id)
    out = Paths(root=root).runs / "packets" / f"{lease_id}.md"
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(out.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, out)
    return out
