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

from dataclasses import dataclass


@dataclass(frozen=True)
class SqlStatement:
    """One statement from split_statements: ``text`` excludes the terminating ';' and is
    stripped of leading/trailing whitespace; ``start``/``end`` are character offsets into the
    original script (end exclusive, excluding the ';')."""

    text: str
    start: int
    end: int


def split_statements(script: str) -> list[SqlStatement]:
    """Split a script at top-level semicolons per the module rules. Empty statements (only
    whitespace/comments) are dropped. A trailing statement without ';' is included.

    >>> [s.text for s in split_statements("select 1; select ';' ; $$a;b$$;")]
    ['select 1', "select ';'", '$$a;b$$']
    >>> [s.text for s in split_statements("/* a /* nested; */ ; */ select 2")]
    ['/* a /* nested; */ ; */ select 2']
    """
    raise NotImplementedError("HAR-08: statement splitter")


def strip_comments(sql: str) -> str:
    """Remove comments (replace each by a single space), preserving strings/identifiers."""
    raise NotImplementedError("HAR-08: comment stripper")


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
    raise NotImplementedError("HAR-08: ORDER BY detection")


def first_keyword(sql: str) -> str:
    """Upper-cased first keyword after comments/whitespace (``WITH`` resolved to the keyword of
    the main statement after the CTE list, e.g. ``WITH x AS (...) INSERT`` -> ``INSERT``).
    Empty string for empty input."""
    raise NotImplementedError("HAR-08: leading keyword")


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
    raise NotImplementedError("HAR-08: message template")
