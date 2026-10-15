"""SQL text utilities shared by runner, comparator, minimizer, cluster: lexical only, no parser.

Lease HAR-08.

All functions implement PostgreSQL's *lexical* rules (src/backend/parser/scan.l and psql's
psqlscan.l, reference reading only):
  * ``'...'`` strings with ``''`` escapes; ``E'...'`` strings where backslash escapes the next
    char; ``U&'...'``; ``B'..'``/``X'..'`` bit strings;
  * ``"..."`` quoted identifiers with ``""`` escapes;
  * dollar quoting ``$tag$ ... $tag$`` where tag is ``[A-Za-z_\\x80-\\uffff][A-Za-z0-9_\\x80-\\uffff]*``
    or empty; ``$1`` is a parameter, NOT a dollar quote;
  * ``--`` line comments; ``/* ... */`` block comments which NEST in PostgreSQL;
  * statement terminator ``;`` only at top level (outside the above), and NOT inside
    ``BEGIN ATOMIC ... END`` bodies of SQL-standard function bodies (CREATE FUNCTION/PROCEDURE
    ... BEGIN ATOMIC): track ``BEGIN ATOMIC`` / ``CASE``...``END`` nesting the way psqlscan.l
    does (``begin_depth``).
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import NamedTuple


@dataclass(frozen=True)
class SqlStatement:
    """One statement from split_statements: ``text`` excludes the terminating ';' and is
    stripped of leading/trailing whitespace; ``start``/``end`` are character offsets into the
    original script (end exclusive, excluding the ';')."""

    text: str
    start: int
    end: int


# ---------------------------------------------------------------------------
# Lexer. One index-driven scanner shared by every public function. It yields
# coarse tokens; identifiers are consumed whole (so ``a$$b`` is one identifier and
# never opens a dollar quote, and ``somee'x'`` is identifier + string, not E'x').
# ---------------------------------------------------------------------------


class _Tok(NamedTuple):
    kind: str  # ws, comment, str, qident, dollar, param, ident, num, semi, lparen, rparen, op
    start: int
    end: int


_WS = frozenset(" \t\n\r\f\v")
_DIGITS = frozenset("0123456789")
_MAIN_STARTERS = frozenset({"SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE"})


def _is_ident_start(c: str) -> bool:
    # psqlscan/scan.l: [A-Za-z\200-\377_]; every non-ASCII character is a byte >= 0x80 in UTF-8.
    return ("a" <= c <= "z") or ("A" <= c <= "Z") or c == "_" or c >= "\x80"


def _is_ident_cont(c: str) -> bool:
    return _is_ident_start(c) or ("0" <= c <= "9") or c == "$"


def _is_tag_cont(c: str) -> bool:
    # dollar-quote tag continuation: no '$' allowed.
    return _is_ident_start(c) or ("0" <= c <= "9")


def _skip_block_comment(s: str, i: int) -> int:
    """``s[i:i+2] == '/*'``. Return the index just past the matching ``*/`` (nested), or len(s)."""
    n = len(s)
    depth = 0
    j = i
    while j < n:
        if s.startswith("/*", j):
            depth += 1
            j += 2
        elif s.startswith("*/", j):
            depth -= 1
            j += 2
            if depth == 0:
                return j
        else:
            j += 1
    return n


def _skip_quoted(s: str, j: int, quote: str, backslash: bool) -> int:
    """``j`` is the index just after the opening quote. Return the index just past the closing
    quote, or len(s) if unterminated. A doubled quote is an escaped quote; with ``backslash``
    (E-strings) a backslash makes the following character literal."""
    n = len(s)
    while j < n:
        c = s[j]
        if backslash and c == "\\":
            j += 2
        elif c == quote:
            if j + 1 < n and s[j + 1] == quote:
                j += 2
            else:
                return j + 1
        else:
            j += 1
    return n


def _dollar_quote_end(s: str, i: int) -> int | None:
    """``s[i] == '$'``. If a dollar quote opens here, return the index just past its closing
    delimiter (or len(s) if unterminated); otherwise None (``$1``, a lone ``$``, ...)."""
    n = len(s)
    j = i + 1
    if j < n and s[j] == "$":
        delim_end = j + 1
    elif j < n and _is_ident_start(s[j]):
        k = j + 1
        while k < n and _is_tag_cont(s[k]):
            k += 1
        if k >= n or s[k] != "$":
            return None
        delim_end = k + 1
    else:
        return None
    delim = s[i:delim_end]
    close = s.find(delim, delim_end)
    return n if close < 0 else close + len(delim)


def _scan(s: str) -> list[_Tok]:
    n = len(s)
    toks: list[_Tok] = []
    i = 0
    while i < n:
        c = s[i]
        if c in _WS:
            j = i + 1
            while j < n and s[j] in _WS:
                j += 1
            kind = "ws"
        elif c == "-" and s.startswith("--", i):
            j = s.find("\n", i)
            j = n if j < 0 else j  # the newline itself is whitespace
            kind = "comment"
        elif c == "/" and s.startswith("/*", i):
            j = _skip_block_comment(s, i)
            kind = "comment"
        elif c == "'":
            j = _skip_quoted(s, i + 1, "'", False)
            kind = "str"
        elif c == '"':
            j = _skip_quoted(s, i + 1, '"', False)
            kind = "qident"
        elif c == "$":
            end = _dollar_quote_end(s, i)
            if end is not None:
                j = end
                kind = "dollar"
            elif i + 1 < n and s[i + 1] in _DIGITS:
                j = i + 2
                while j < n and s[j] in _DIGITS:
                    j += 1
                kind = "param"
            else:
                j = i + 1
                kind = "op"
        elif _is_ident_start(c):
            j = i + 1
            while j < n and _is_ident_cont(s[j]):
                j += 1
            if j < n and s[j] == "'" and s[i:j] in ("e", "E"):
                j = _skip_quoted(s, j + 1, "'", True)
                kind = "str"
            else:
                kind = "ident"
        elif c in _DIGITS:
            j = i + 1
            while j < n and s[j] in _DIGITS:
                j += 1
            kind = "num"
        elif c == ";":
            j = i + 1
            kind = "semi"
        elif c == "(":
            j = i + 1
            kind = "lparen"
        elif c == ")":
            j = i + 1
            kind = "rparen"
        else:
            j = i + 1
            kind = "op"
        toks.append(_Tok(kind, i, j))
        i = j
    return toks


def _is_create_routine(words: list[str]) -> bool:
    """psqlscan_is_create_routine: CREATE [OR REPLACE] {FUNCTION|PROCEDURE}, lowercased words."""
    if len(words) < 2 or words[0] != "create":
        return False
    if words[1] in ("function", "procedure"):
        return True
    return (
        len(words) >= 4
        and words[1] == "or"
        and words[2] == "replace"
        and words[3] in ("function", "procedure")
    )


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def split_statements(script: str) -> list[SqlStatement]:
    """Split a script at top-level semicolons per the module rules. Empty statements (only
    whitespace/comments) are dropped. A trailing statement without ';' is included.

    >>> [s.text for s in split_statements("select 1; select ';' ; $$a;b$$;")]
    ['select 1', "select ';'", '$$a;b$$']
    >>> [s.text for s in split_statements("/* a /* nested; */ ; */ select 2")]
    ['/* a /* nested; */ ; */ select 2']
    """
    out: list[SqlStatement] = []
    start = -1  # first non-whitespace char of the current statement (comments count)
    last_end = 0  # end of the last non-whitespace token of the current statement
    has_sig = False  # saw a token other than whitespace/comments
    paren = 0
    begin = 0  # psqlscan begin_depth
    words: list[str] = []  # first identifiers of the statement at paren depth 0

    for t in _scan(script):
        k = t.kind
        if k == "ws":
            continue
        if k == "semi" and paren == 0 and begin == 0:
            if has_sig:
                out.append(SqlStatement(script[start:last_end], start, last_end))
            start = -1
            has_sig = False
            paren = 0
            begin = 0
            words = []
            continue
        if start < 0:
            start = t.start
        last_end = t.end
        if k == "comment":
            continue
        has_sig = True
        if k == "lparen":
            paren += 1
        elif k == "rparen":
            if paren > 0:
                paren -= 1
        elif k == "ident" and paren == 0:
            low = script[t.start : t.end].lower()
            if len(words) < 4:
                words.append(low)
            if _is_create_routine(words):
                if low == "begin":
                    begin += 1
                elif low == "case":
                    if begin >= 1:
                        begin += 1
                elif low == "end":
                    if begin > 0:
                        begin -= 1

    if has_sig:
        out.append(SqlStatement(script[start:last_end], start, last_end))
    return out


def strip_comments(sql: str) -> str:
    """Remove comments (replace each by a single space), preserving strings/identifiers.

    >>> strip_comments("select '--x' -- c")
    "select '--x'  "
    """
    parts = []
    for t in _scan(sql):
        parts.append(" " if t.kind == "comment" else sql[t.start : t.end])
    return "".join(parts)


def _top_items(sql: str) -> list[tuple[str, str]]:
    """Significant tokens at parenthesis depth 0 as (kind, text). Identifiers are upper-cased;
    each outermost parenthesized group is collapsed into one ('group', '(') item. Used by the
    keyword and ORDER BY analyses."""
    items: list[tuple[str, str]] = []
    depth = 0
    for t in _scan(sql):
        k = t.kind
        if k in ("ws", "comment"):
            continue
        if k == "lparen":
            if depth == 0:
                items.append(("group", "("))
            depth += 1
            continue
        if k == "rparen":
            if depth > 0:
                depth -= 1
            continue
        if depth > 0:
            continue
        text = sql[t.start : t.end]
        if k == "ident":
            items.append(("ident", text.upper()))
        else:
            items.append((k, text))
    return items


def _main_keyword(items: list[tuple[str, str]]) -> str:
    """Keyword of the main statement. Skips a WITH [RECURSIVE] CTE list (name [(cols)] AS
    [NOT] [MATERIALIZED] (query), comma separated, optional SEARCH/CYCLE clauses)."""
    if not items:
        return ""
    kind, text = items[0]
    if kind == "group":
        return "("
    if kind != "ident":
        return ""
    if text != "WITH":
        return text
    n = len(items)
    i = 1
    if i < n and items[i] == ("ident", "RECURSIVE"):
        i += 1
    while True:
        i += 1  # the CTE name at i-1 is skipped; i is now just after it
        if i < n and items[i][0] == "group" and i + 1 < n and items[i + 1] == ("ident", "AS"):
            i += 1  # column list
        if i >= n or items[i] != ("ident", "AS"):
            return ""
        i += 1
        if i < n and items[i] == ("ident", "NOT"):
            i += 1
        if i < n and items[i] == ("ident", "MATERIALIZED"):
            i += 1
        if i >= n or items[i][0] != "group":
            return ""
        i += 1
        if i < n and items[i] == ("op", ","):
            i += 1
            continue
        break
    while i < n:
        kind, text = items[i]
        if kind == "ident" and text in _MAIN_STARTERS:
            return text
        if kind == "group":
            return "("
        i += 1
    return ""


def has_top_level_order_by(sql: str) -> bool:
    """True if the statement's OUTERMOST query level has an ORDER BY clause.

    Used for ``order: auto``: True -> ordered comparison, False -> multiset. Heuristic but
    conservative: tokens inside parentheses, strings, comments, dollar quotes are ignored;
    window ``OVER (ORDER BY ...)`` and ``WITHIN GROUP (ORDER BY ...)`` and aggregate
    ``array_agg(x ORDER BY y)`` are inside parentheses so they do not count; a top-level
    ``ORDER BY`` that appears after a top-level set operation (UNION/INTERSECT/EXCEPT) applies
    to the whole result and counts. For ``EXPLAIN``/``COPY ... TO STDOUT``/``SHOW``/
    ``VALUES`` without ORDER BY: VALUES and SHOW return False; EXPLAIN returns True (plan text
    order is meaningful). ``FETCH``/``MOVE`` from a cursor return True (cursor order is the
    declared query's order, which the case author must have made deterministic).
    Case-insensitive keywords; any whitespace/comments between ORDER and BY.

    >>> has_top_level_order_by("select a from t order by a")
    True
    >>> has_top_level_order_by("select row_number() over (order by a) from t")
    False
    >>> has_top_level_order_by("select 1 union all select 2 order by 1")
    True
    """
    for st in split_statements(sql):
        items = _top_items(st.text)
        if _main_keyword(items) in ("EXPLAIN", "FETCH", "MOVE"):
            return True
        for k in range(len(items) - 1):
            if items[k] == ("ident", "ORDER") and items[k + 1] == ("ident", "BY"):
                return True
    return False


def first_keyword(sql: str) -> str:
    """Upper-cased first keyword after comments/whitespace (``WITH`` resolved to the keyword of
    the main statement after the CTE list, e.g. ``WITH x AS (...) INSERT`` -> ``INSERT``).
    Empty string for empty input."""
    stmts = split_statements(sql)
    if not stmts:
        return ""
    return _main_keyword(_top_items(stmts[0].text))


_MSG_VARIABLE = re.compile(
    r'"(?:[^"]|"")*"'  # quoted identifier
    r"|'(?:[^']|'')*'"  # string literal
    r"|\$\d+"  # parameter reference
    r"|(?<!\w)\d+(?:\.\d+)?(?!\w)"  # integer / decimal, not part of a word such as int4
)


def _msg_repl(m: re.Match[str]) -> str:
    t = m.group(0)
    if t[0] == '"':
        return '"?"'
    if t[0] == "'":
        return "'?'"
    if t[0] == "$":
        return "$N"
    return "N"


def message_template(message: str) -> str:
    """Normalize a PG error/notice message into a cluster template by replacing
    variable parts: double-quoted identifiers -> ``"?"``, single-quoted literals -> ``'?'``,
    integers/decimals -> ``N``, ``$n`` -> ``$N``, OIDs in ``(oid 1234)`` -> ``N``.
    Never used for comparison, only clustering.

    >>> message_template('relation "foo" does not exist')
    'relation "?" does not exist'
    >>> message_template('value too long for type character varying(5)')
    'value too long for type character varying(N)'
    """
    return _MSG_VARIABLE.sub(_msg_repl, message)
