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

import re
from bisect import bisect_right
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .gitutil import GitError, changed_files, git

_MAX_LINES = 1500
_EXCUSES = ("returns empty", "no-op")
_FAKE_RUST = frozenset({
    "Ok(())", "Ok(Vec::new())", "Ok(None)", "Vec::new()", "None",
    "return Ok(())", "return Ok(Vec::new())", "return Ok(None)", "return Vec::new()",
    "return None",
})
_FAKE_PY = frozenset({
    "return []", "return None", "return {}", "return set()", "return dict()", "return list()",
})

_NONNL = re.compile(r"[^\n]")
_RS_MARK = re.compile(r"[ \t]*//[ \t]*CONTRACT\b")
_PY_MARK = re.compile(r"[ \t]*#[ \t]*CONTRACT\b")
_RS_SPECIAL = re.compile(r"[/\"'r]")
_PY_SPECIAL = re.compile(r"[#'\"]")
_RS_ITEM = re.compile(
    r"[ \t]*(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:unsafe|async|default|const|extern\s+\"[^\"]*\")\s+)*"
    r"(?:(fn|struct|enum|trait|type|static|mod|union|const)\s+([A-Za-z_]\w*)|(impl)\b)"
)
_RS_FN = re.compile(r"(?<!\w)fn\s+([A-Za-z_]\w*)")
_RS_PUB = re.compile(
    r"\bpub(?:\([^)]*\))?\s+(?:(?:unsafe|const|async|default)\s+)*"
    r"(?:extern\s+\"[^\"]*\"\s+)?(fn|struct|enum|trait)\s+([A-Za-z_]\w*)"
)
_RS_CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
_RS_STUB = re.compile(r"PgError::unimplemented\s*\(|\bunimplemented!\s*\(|\btodo!\s*\(")
_RS_FORBIDDEN = re.compile(r"\btodo!\s*\(|\bunimplemented!\s*\(|\bpanic!\s*\(")
_RS_INPUT = re.compile(r"parse|from_utf8|decode|read")
_RS_CONTAINER = re.compile(r"(?:unsafe\s+)?impl\b|(?:pub(?:\([^)]*\))?\s+)?(?:trait|mod)\s+\w")
_PY_DEF = re.compile(r"(\s*)(?:async\s+)?(def|class)\s+([A-Za-z_]\w*)")
_PY_NIE = re.compile(r"NotImplementedError\(\s*[\"']([A-Z][A-Z0-9]{1,9}-\d{2,3}):")
_HUNK = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")


@dataclass
class _FnInfo:
    line: int
    body: str
    has_body: bool
    stub: bool
    pass_only: bool
    fake: bool
    doc: str


@dataclass
class _Analysis:
    # key -> (normalized CONTRACT text, 1-based line)
    contract: dict[str, tuple[str, int]] = field(default_factory=dict)
    # key -> (normalized signature header, 1-based line)
    sigs: dict[str, tuple[str, int]] = field(default_factory=dict)
    fns: dict[str, _FnInfo] = field(default_factory=dict)
    test_lines: set[int] = field(default_factory=set)


@dataclass
class _Def:
    kind: str
    name: str
    ind: int
    hdr: int
    hdr_end: int
    header: str
    rest_pc: str
    rest_ps: str
    body_idx: list[int]
    body_end: int
    parent: int


_EMPTY = _Analysis()


# ---------------------------------------------------------------------------------------------
# small helpers

def _norm(s: str) -> str:
    return " ".join(s.split())


def _blank(s: str) -> str:
    return _NONNL.sub(" ", s)


def _put(d: dict[str, Any], key: str, value: Any) -> None:
    """Insert under ``key``; a duplicate key gets a ``#2``, ``#3``... suffix (deterministic)."""
    k = key
    n = 2
    while k in d:
        k = f"{key}#{n}"
        n += 1
    d[k] = value


def _line_starts(text: str) -> list[int]:
    starts = [0]
    pos = text.find("\n")
    while pos != -1:
        starts.append(pos + 1)
        pos = text.find("\n", pos + 1)
    return starts


def _lang(path: str) -> str | None:
    if path.endswith(".rs"):
        return "rust"
    if path.endswith(".py"):
        return "python"
    return None


def _is_test_path(path: str) -> bool:
    parts = path.split("/")
    return any(p in ("tests", "test") for p in parts[:-1])


def _finding(rule: str, severity: str, file: str, line: int, detail: str) -> dict[str, Any]:
    return {"rule": rule, "severity": severity, "file": file, "line": int(line), "detail": detail}


def _show(repo_root: Path, sha: str, path: str) -> str | None:
    """File text at ``sha`` or None when the path does not exist there."""
    try:
        return git(["show", f"{sha}:{path}"], repo_root)
    except (GitError, UnicodeDecodeError):
        return None


def _diff_lines(repo_root: Path, base_sha: str, head_sha: str, path: str):
    """(added, removed) for one path: added = [(head line, text)], removed = [(base line, text)]."""
    out = git(["diff", "-U0", "--no-renames", "--no-color", "--no-ext-diff",
               base_sha, head_sha, "--", path], repo_root)
    added: list[tuple[int, str]] = []
    removed: list[tuple[int, str]] = []
    in_hunk = False
    bno = 0
    hno = 0
    for line in out.split("\n"):
        if line.startswith("diff --git"):
            in_hunk = False
            continue
        m = _HUNK.match(line)
        if m is not None:
            bno = int(m.group(1))
            hno = int(m.group(2))
            in_hunk = True
            continue
        if not in_hunk or not line:
            continue
        tag = line[0]
        if tag == "+":
            added.append((hno, line[1:]))
            hno += 1
        elif tag == "-":
            removed.append((bno, line[1:]))
            bno += 1
    return added, removed


# ---------------------------------------------------------------------------------------------
# Rust lexing: two equal-length flat views of the file

def _rs_views(text: str) -> tuple[str, str]:
    """(R, B): R has comments blanked (strings kept); B also blanks string/char contents.
    Both keep every newline and every offset, so offsets index all three views alike."""
    rv: list[str] = []
    bv: list[str] = []
    n = len(text)
    i = 0
    while i < n:
        m = _RS_SPECIAL.search(text, i)
        if m is None:
            rv.append(text[i:])
            bv.append(text[i:])
            break
        j = m.start()
        if j > i:
            rv.append(text[i:j])
            bv.append(text[i:j])
            i = j
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            end = text.find("\n", i)
            end = n if end < 0 else end
            blank = _blank(text[i:end])
            rv.append(blank)
            bv.append(blank)
            i = end
            continue
        if c == "/" and nxt == "*":
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            blank = _blank(text[i:end])
            rv.append(blank)
            bv.append(blank)
            i = end
            continue
        if c == "r":
            prev = text[i - 1] if i > 0 else ""
            if not (prev.isalnum() or prev == "_"):
                k = i + 1
                while k < n and text[k] == "#":
                    k += 1
                if k < n and text[k] == '"':
                    closer = '"' + text[i + 1:k]
                    e = text.find(closer, k + 1)
                    if e < 0:
                        body_end = n
                        end = n
                        tail = ""
                    else:
                        body_end = e
                        end = e + len(closer)
                        tail = closer
                    rv.append(text[i:end])
                    bv.append(text[i:k + 1] + _blank(text[k + 1:body_end]) + tail)
                    i = end
                    continue
            rv.append("r")
            bv.append("r")
            i += 1
            continue
        if c == '"':
            k = i + 1
            closed = False
            while k < n:
                ch = text[k]
                if ch == "\\":
                    k += 2
                    continue
                if ch == '"':
                    closed = True
                    break
                k += 1
            if closed:
                end = k + 1
                rv.append(text[i:end])
                bv.append('"' + _blank(text[i + 1:k]) + '"')
            else:
                end = n
                rv.append(text[i:n])
                bv.append('"' + _blank(text[i + 1:n]))
            i = end
            continue
        if c == "'":
            if nxt == "\\":
                e = text.find("'", i + 2)
                if e >= 0 and e - i <= 12:
                    seg = text[i:e + 1]
                    rv.append(seg)
                    bv.append("'" + _blank(seg[1:-1]) + "'")
                    i = e + 1
                    continue
            elif i + 2 < n and text[i + 2] == "'" and nxt != "\n":
                seg = text[i:i + 3]
                rv.append(seg)
                bv.append("'" + _blank(nxt) + "'")
                i += 3
                continue
            # otherwise a lifetime: plain code
        rv.append(c)
        bv.append(c)
        i += 1
    return "".join(rv), "".join(bv)


def _match_brace(bv: str, i: int) -> int:
    """Index of the ``}`` matching the ``{`` at ``i`` (len-1 when unbalanced)."""
    depth = 0
    for k in range(i, len(bv)):
        c = bv[k]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return k
    return len(bv) - 1


def _sig_end(bv: str, p: int) -> int:
    """Index of the first ``{`` or ``;`` at bracket depth 0 from ``p`` (len when none)."""
    depth = 0
    n = len(bv)
    for k in range(p, n):
        c = bv[k]
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        elif depth <= 0 and c in "{;":
            return k
    return n


def _rs_item_end(bv: str, p: int) -> int:
    """Exclusive end of the item starting at ``p``: after its ``;`` or its closing ``}``."""
    depth = 0
    n = len(bv)
    i = p
    while i < n:
        c = bv[i]
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        elif c == ";" and depth <= 0:
            return i + 1
        elif c == "{" and depth <= 0:
            return _match_brace(bv, i) + 1
        i += 1
    return n


def _rs_doc(o: list[str], line: int) -> str:
    """Lowercased doc/attribute/comment lines directly above 1-based ``line``."""
    parts: list[str] = []
    k = line - 2
    while k >= 0 and o[k].lstrip().startswith(("///", "#[", "//")):
        parts.append(o[k])
        k -= 1
    return " ".join(reversed(parts)).lower()


def _analyze_rust(text: str) -> _Analysis:
    rv, bv = _rs_views(text)
    o = text.split("\n")
    starts = _line_starts(text)

    # top-level containers (impl/trait/mod headers at column 0); any other column-0 item resets
    top_offs: list[int] = []
    top_labels: list[str] = []
    for ln, line in enumerate(o):
        if line[:1].isalpha():
            head = line.split("{", 1)[0]
            label = _norm(head) if _RS_CONTAINER.match(head.lstrip()) else ""
            top_offs.append(starts[ln])
            top_labels.append(label)

    def container(pos: int) -> str:
        idx = bisect_right(top_offs, pos) - 1
        return top_labels[idx] if idx >= 0 else ""

    # every fn: (decl offset, body open, body close or -1, name)
    fns_raw: list[tuple[int, int, int, str]] = []
    for m in _RS_FN.finditer(bv):
        k = _sig_end(bv, m.end())
        opn = -1
        cls = -1
        if k < len(bv) and bv[k] == "{":
            opn = k
            cls = _match_brace(bv, k)
        fns_raw.append((m.start(), opn, cls, m.group(1)))

    fn_map: dict[str, _FnInfo] = {}
    for decl, opn, cls, name in fns_raw:
        line = bisect_right(starts, decl)
        key = f"{container(decl)}|fn {name}"
        if opn >= 0:
            body_stub = bool(_RS_STUB.search(bv[opn + 1:cls]))
            core = _norm(rv[opn + 1:cls]).rstrip("; ")
            info = _FnInfo(line, core, True, body_stub, False, core in _FAKE_RUST,
                           _rs_doc(o, line))
        else:
            info = _FnInfo(line, "", False, False, False, False, "")
        _put(fn_map, key, info)

    # CONTRACT-marked items: header through the end of the item, fn bodies cut out
    contract: dict[str, tuple[str, int]] = {}
    for ln, line in enumerate(o):
        if _RS_MARK.match(line) is None:
            continue
        j = ln + 1
        while j < len(o) and (not o[j].strip()
                              or o[j].lstrip().startswith(("#[", "#![", "//"))):
            j += 1
        if j >= len(o):
            break
        start = starts[j] + (len(o[j]) - len(o[j].lstrip()))
        mm = _RS_ITEM.match(bv, start)
        if mm is None:
            continue
        s = mm.start()
        end = _rs_item_end(bv, s)
        cuts: list[tuple[int, int]] = []
        last = -1
        for decl, opn, cls, _name in fns_raw:
            if s <= decl < end and opn >= 0 and decl >= last and opn < end:
                cuts.append((opn + 1, cls))
                last = cls
        pieces: list[str] = []
        pos = s
        for a, b in cuts:
            pieces.append(rv[pos:a])
            pieces.append(" ... ")
            pos = b
        pieces.append(rv[pos:end])
        if mm.group(3) == "impl":
            key = "impl " + _norm(rv[s:_sig_end(bv, s)])
        else:
            key = f"{mm.group(1)} {mm.group(2)}"
        _put(contract, key, (_norm("".join(pieces)), j + 1))

    # public signatures (any depth), header only
    sigs: dict[str, tuple[str, int]] = {}
    for m in _RS_PUB.finditer(bv):
        s = m.start()
        e = _sig_end(bv, m.end())
        key = f"{container(s)}|{m.group(1)} {m.group(2)}"
        _put(sigs, key, (_norm(rv[s:e]), bisect_right(starts, s)))

    # #[cfg(test)] item line ranges
    test_lines: set[int] = set()
    for m in _RS_CFG_TEST.finditer(bv):
        q = m.end()
        while q < len(bv) and bv[q].isspace():
            q += 1
        e = _rs_item_end(bv, q)
        first = bisect_right(starts, m.start())
        last_line = bisect_right(starts, max(e - 1, m.start()))
        test_lines.update(range(first, last_line + 1))

    return _Analysis(contract, sigs, fn_map, test_lines)


# ---------------------------------------------------------------------------------------------
# Python lexing and structure

def _py_views(text: str) -> tuple[str, str]:
    """(PC, PS): PC has comments blanked (strings kept); PS also blanks string contents.
    Same length and newlines as the input."""
    pc: list[str] = []
    ps: list[str] = []
    n = len(text)
    i = 0
    while i < n:
        m = _PY_SPECIAL.search(text, i)
        if m is None:
            seg = text[i:]
            pc.append(seg)
            ps.append(seg)
            break
        j = m.start()
        if j > i:
            seg = text[i:j]
            pc.append(seg)
            ps.append(seg)
            i = j
        c = text[i]
        if c == "#":
            end = text.find("\n", i)
            end = n if end < 0 else end
            blank = _blank(text[i:end])
            pc.append(blank)
            ps.append(blank)
            i = end
            continue
        q = text[i:i + 3] if text[i:i + 3] in ('"""', "'''") else c
        k = i + len(q)
        if len(q) == 3:
            e = text.find(q, k)
            if e < 0:
                body_end = n
                end = n
                tail = ""
            else:
                body_end = e
                end = e + 3
                tail = q
        else:
            e = k
            while e < n and text[e] != c and text[e] != "\n":
                e += 2 if text[e] == "\\" else 1
            if e < n and text[e] == c:
                body_end = e
                end = e + 1
                tail = c
            else:
                body_end = min(e, n)
                end = body_end
                tail = ""
        opening = text[i:k]
        pc.append(opening + text[k:body_end] + tail)
        ps.append(opening + _blank(text[k:body_end]) + tail)
        i = end
    return "".join(pc), "".join(ps)


def _py_header_end(psl: list[str], i: int) -> tuple[int, int]:
    """(line, colon position) of the colon that ends a def/class header starting at ``i``."""
    depth = 0
    for k in range(i, len(psl)):
        for pos, ch in enumerate(psl[k]):
            if ch in "([{":
                depth += 1
            elif ch in ")]}":
                depth -= 1
            elif ch == ":" and depth <= 0:
                return k, pos
    return i, len(psl[i])


def _py_body(d: _Def, pcl: list[str], psl: list[str]) -> tuple[str, str, bool, bool, bool, bool]:
    """(doc lowercased, body normalized, has_body, stub, pass_only, fake)."""
    parts_pc: list[str] = []
    parts_ps: list[str] = []
    if d.rest_ps.strip():
        parts_pc.append(d.rest_pc)
        parts_ps.append(d.rest_ps)
    doc = ""
    remain = list(d.body_idx)
    if remain:
        f = remain[0]
        s = psl[f].strip()
        if s[:1] in ("'", '"'):
            q = s[:3] if s[:3] in ('"""', "'''") else s[:1]
            end_doc = f
            if len(q) == 3 and psl[f].count(q) < 2:
                for j in range(f + 1, len(psl)):
                    if q in psl[j]:
                        end_doc = j
                        break
            doc = _norm(" ".join(pcl[x] for x in range(f, end_doc + 1))).lower()
            remain = [x for x in remain if x > end_doc]
    for x in remain:
        parts_pc.append(pcl[x])
        parts_ps.append(psl[x])
    body = _norm(" ".join(parts_pc))
    body_ps = _norm(" ".join(parts_ps))
    stub = "NotImplementedError" in body_ps or body in ("pass", "...")
    fake = body in _FAKE_PY and "NotImplementedError" not in body_ps
    return doc, body, bool(body), stub, body == "pass", fake


def _py_class_text(d: _Def, defs: list[_Def], pcl: list[str]) -> str:
    """Class header and class-level lines; bodies of nested defs are excluded."""
    excluded: set[int] = set()
    for nd in defs:
        if d.hdr < nd.hdr < d.body_end and nd.kind == "def":
            excluded.update(range(nd.hdr_end + 1, nd.body_end))
    return _norm(" ".join(pcl[x] for x in range(d.hdr, d.body_end) if x not in excluded))


def _analyze_py(text: str) -> _Analysis:
    pc_text, ps_text = _py_views(text)
    o = text.split("\n")
    pcl = pc_text.split("\n")
    psl = ps_text.split("\n")
    nlines = len(psl)

    defs: list[_Def] = []
    stack: list[int] = []
    for i in range(nlines):
        m = _PY_DEF.match(psl[i])
        if m is None:
            continue
        ind = len(m.group(1))
        kind = "class" if m.group(2) == "class" else "def"
        name = m.group(3)
        hk, pos = _py_header_end(psl, i)
        parts = [pcl[x] for x in range(i, hk)] + [pcl[hk][:pos + 1]]
        header = _norm(" ".join(parts))
        body_idx: list[int] = []
        x = hk + 1
        while x < nlines:
            line = psl[x]
            if line.strip():
                if len(line) - len(line.lstrip()) <= ind:
                    break
                body_idx.append(x)
            x += 1
        while stack and defs[stack[-1]].ind >= ind:
            stack.pop()
        parent = stack[-1] if stack else -1
        defs.append(_Def(kind, name, ind, i, hk, header, pcl[hk][pos + 1:], psl[hk][pos + 1:],
                         body_idx, x, parent))
        stack.append(len(defs) - 1)

    by_hdr = {d.hdr: idx for idx, d in enumerate(defs)}
    contract: dict[str, tuple[str, int]] = {}
    sigs: dict[str, tuple[str, int]] = {}
    fn_map: dict[str, _FnInfo] = {}
    for d in defs:
        pref = f"{defs[d.parent].name}." if d.parent >= 0 else ""
        if d.parent < 0 or defs[d.parent].kind == "class":
            _put(sigs, f"{d.kind} {pref}{d.name}", (d.header, d.hdr + 1))
        if d.kind == "def":
            doc, body, has_body, stub, pass_only, fake = _py_body(d, pcl, psl)
            _put(fn_map, pref + d.name,
                 _FnInfo(d.hdr + 1, body, has_body, stub, pass_only, fake, doc))

    for i, line in enumerate(o):
        if _PY_MARK.match(line) is None:
            continue
        j = i + 1
        while j < len(o) and (not o[j].strip() or o[j].lstrip().startswith(("#", "@"))):
            j += 1
        idx = by_hdr.get(j)
        if idx is None:
            continue
        d = defs[idx]
        if d.kind == "def":
            _put(contract, f"def {d.name}", (d.header, j + 1))
        else:
            _put(contract, f"class {d.name}", (_py_class_text(d, defs, pcl), j + 1))

    return _Analysis(contract, sigs, fn_map, set())


def _analyze(text: str, lang: str) -> _Analysis:
    if lang == "rust":
        return _analyze_rust(text)
    if lang == "python":
        return _analyze_py(text)
    raise ValueError(f"unknown lang {lang!r}")


# ---------------------------------------------------------------------------------------------
# checks

def _check_source(findings: list[dict[str, Any]], path: str, lang: str,
                  base_text: str | None, head_text: str | None,
                  added: list[tuple[int, str]], lease_id: str) -> None:
    base = _analyze(base_text, lang) if base_text else _EMPTY
    head = _analyze(head_text, lang) if head_text else _EMPTY

    # frozen CONTRACT items
    for key, (txt, line) in base.contract.items():
        hit = head.contract.get(key)
        if hit is None:
            findings.append(_finding("contract_item_changed", "reject", path, line,
                                     f"CONTRACT item {key} removed or no longer marked"))
        elif hit[0] != txt:
            findings.append(_finding("contract_item_changed", "reject", path, hit[1],
                                     f"CONTRACT item {key} changed"))

    # public signatures
    for key, (hdr, line) in base.sigs.items():
        hit = head.sigs.get(key)
        if hit is None:
            findings.append(_finding("signature_changed", "reject", path, line,
                                     f"public item {key} removed or renamed"))
        elif hit[0] != hdr:
            findings.append(_finding("signature_changed", "reject", path, hit[1],
                                     f"signature of {key} changed: '{hdr}' -> '{hit[0]}'"))

    # stub bodies replaced by fake success or a bare pass
    for key, bf in base.fns.items():
        if not bf.stub:
            continue
        hf = head.fns.get(key)
        if hf is None or not hf.has_body or hf.body == bf.body:
            continue
        if hf.pass_only:
            findings.append(_finding("forbidden_placeholder", "reject", path, hf.line,
                                     f"function {key} replaces its stub with a bare pass"))
        elif hf.fake and not any(e in hf.doc for e in _EXCUSES):
            findings.append(_finding("fake_success", "reject", path, hf.line,
                                     f"function {key} replaces its stub with an empty result"
                                     f" ({hf.body})"))

    # placeholders and unwrap-on-input in added lines
    test_file = _is_test_path(path)
    for ln, text in added:
        if lang == "rust":
            if test_file or ln in head.test_lines:
                continue
            if _RS_FORBIDDEN.search(text):
                findings.append(_finding("forbidden_placeholder", "reject", path, ln,
                                         "todo!/unimplemented!/panic! outside #[cfg(test)]"))
            if "unwrap()" in text and _RS_INPUT.search(text):
                findings.append(_finding("forbidden_placeholder", "reject", path, ln,
                                         "unwrap() on parse/decode/read of input"))
        elif "NotImplementedError(" in text:
            mm = _PY_NIE.search(text)
            if mm is None or mm.group(1) != lease_id:
                findings.append(_finding("forbidden_placeholder", "reject", path, ln,
                                         f"NotImplementedError not carrying {lease_id}"))


def review_patch(repo_root: Path, spec: Any, base_sha: str, head_sha: str) -> list[dict[str, Any]]:
    findings: list[dict[str, Any]] = []
    lease_id = spec.lease_id
    for _status, path in changed_files(base_sha, head_sha, repo_root):
        base_text = _show(repo_root, base_sha, path)
        head_text = _show(repo_root, head_sha, path)
        added, removed = _diff_lines(repo_root, base_sha, head_sha, path)

        for ln, text in removed:
            if "assert" in text or "#[test]" in text:
                findings.append(_finding("test_weakening", "reject", path, ln,
                                         f"removed assertion or test attribute: {text.strip()}"))
        for ln, text in added:
            if "#[ignore" in text or "@pytest.mark.skip" in text or "xfail" in text:
                findings.append(_finding("test_weakening", "reject", path, ln,
                                         f"added ignore/skip/xfail marker: {text.strip()}"))

        if head_text is not None:
            head_lines = len(head_text.splitlines())
            base_lines = len(base_text.splitlines()) if base_text is not None else 0
            if head_lines > _MAX_LINES and head_lines > base_lines:
                findings.append(_finding("large_file", "warn", path, _MAX_LINES + 1,
                                         f"file grew to {head_lines} lines"))

        lang = _lang(path)
        if lang is not None and (base_text or head_text):
            _check_source(findings, path, lang, base_text, head_text, added, lease_id)

    # lease markers still standing at head
    marker_res = [
        re.compile(r"PgError::unimplemented\(\s*\"" + re.escape(lease_id) + r"\""),
        re.compile(r"NotImplementedError\(\s*[\"']" + re.escape(lease_id) + r":"),
    ]
    for path in spec.files:
        text = _show(repo_root, head_sha, path)
        if not text:
            continue
        hits = [m for rx in marker_res for m in rx.finditer(text)]
        if hits:
            first = min(m.start() for m in hits)
            line = text.count("\n", 0, first) + 1
            findings.append(_finding("stub_remaining", "warn", path, line,
                                     f"{len(hits)} {lease_id} stub marker(s) remain"))
    return findings


def contract_items(text: str, lang: str) -> dict[str, str]:
    """Map a stable item key (``fn name`` / ``struct Name`` / ``def name`` / ``class Name``)
    to its normalized CONTRACT-marked text for ``lang`` in ("rust", "python")."""
    return {key: txt for key, (txt, _line) in _analyze(text, lang).contract.items()}
