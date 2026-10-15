"""ORACLE-04: the ONLY permitted normalizations (legitimate nondeterminism, documented).

Lease HAR-12.

Rules (RFC 002 §2A): never normalize SQLSTATE, NULLs, type OIDs of built-in types,
transaction status, row multiplicity, or precision. Every normalizer below states WHY the
normalized quantity is not a PostgreSQL behavior that PGArbor must reproduce. Adding a
normalizer is an oracle amendment (contracts/amendments) and requires a selftest mutant that
shows it does not hide a real bug class.

Normalizers mutate a Transcript IN PLACE, must be idempotent (applying twice == once) and
deterministic, and are applied identically to the reference (at record time) and to the
target (at compare time).

Registry (name -> Normalizer):

``user_oids`` (DEFAULT, always applied)
    Why: descriptor table/type identities are allocation-dependent. Canonically renumber
    descriptor OIDs bijectively in first-appearance order. OID-typed row values are data:
    without explicit allocated-object provenance they must remain byte-exact, including
    literal and stored OIDs. See amendment harness-03-verifier-integrity.md.

``timestamps`` (opt-in per step)
    Why: steps that call now()/clock_timestamp()/CURRENT_* legitimately differ per run.
    What: values in columns with type_oid in {1082 date, 1083 time, 1114 timestamp,
    1184 timestamptz, 1266 timetz} -> ``"<timestamp>"`` (NULL stays None).

``uuids`` (opt-in) — gen_random_uuid(): type_oid 2950 values -> ``"<uuid>"``.

``pids`` (opt-in) — backend PIDs: int4 columns named ``pid``, ``pg_backend_pid``,
    ``procpid`` -> ``"<pid>"``; also the integer in notice/error messages matching
    ``process \\d+`` -> ``process <pid>`` (deadlock DETAIL lines).

``xids`` (opt-in) — transaction ids: type_oid in {28 xid, 5069 xid8} and columns named
    ``txid_current``/``pg_current_xact_id`` -> ``"<xid>"``.

``lsns`` (opt-in) — type_oid 3220 pg_lsn -> ``"<lsn>"``.

``explain_numbers`` (opt-in) — EXPLAIN text rows: ``cost=\\d+\\.\\d+\\.\\.\\d+\\.\\d+`` ->
    ``cost=?``, ``rows=\\d+`` -> ``rows=?``, ``width=\\d+`` -> ``width=?``,
    ``actual time=[\\d.]+\\.\\.[\\d.]+`` -> ``actual time=?``, ``Planning Time: [\\d.]+ ms`` ->
    ``Planning Time: ? ms``, ``Execution Time: [\\d.]+ ms`` likewise. Plan *shape* stays.

Step-scoped normalizers apply only to that step's results/notices. ``setup`` gets only the
defaults.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Callable

from ..cases.model import Case
from ..model import ColumnDesc, ErrorInfo, Notice, StatementResult, StepObservation, Transcript

# ---------------------------------------------------------------------------------------------
# Shared constants
# ---------------------------------------------------------------------------------------------

#: First OID PostgreSQL allocates for user objects; everything below is a built-in object.
_FIRST_USER_OID = 16384
#: OID is an unsigned 32-bit integer; larger integers can never be OIDs.
_OID_MAX = 0xFFFFFFFF
#: Canonical user-OID numbering: the k-th distinct descriptor OID becomes _CANON_BASE + k.
#: Canonical descriptor integers occupy [_CANON_BASE, _CANON_BASE + _CANON_SPAN).
_CANON_BASE = 1_000_000_000
_CANON_SPAN = 10**6

_UUID_TYPE = 2950  #: uuid
_INT4_TYPE = 23  #: int4
_LSN_TYPE = 3220  #: pg_lsn
_XID_TYPES = frozenset({28, 5069})  #: xid, xid8
_TIMESTAMP_TYPES = frozenset({1082, 1083, 1114, 1184, 1266})  #: date, time, timestamp, timestamptz, timetz

_PID_COLUMNS = frozenset({"pid", "pg_backend_pid", "procpid"})
_XID_COLUMNS = frozenset({"txid_current", "pg_current_xact_id"})
_EXPLAIN_COLUMN = "QUERY PLAN"

#: ``process <n>`` in deadlock DETAIL / notice text. The leading word keeps its original case
#: (PostgreSQL prints ``Process 123 waits for ...`` at the start of the DETAIL line).
_PROCESS_RE = re.compile(r"\b(process )[0-9]+", re.IGNORECASE)

#: EXPLAIN text substitutions. Only planner *estimates* are masked: the ``rows=`` of an estimate
#: tuple ``(cost=... rows=N width=W)`` is replaced, while the ``rows=`` of an ANALYZE actual
#: tuple ``(actual time=... rows=N loops=L)`` is row multiplicity and is never normalized.
_EXPLAIN_SUBS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"cost=[0-9]+\.[0-9]+\.\.[0-9]+\.[0-9]+ rows=[0-9]+ width=[0-9]+"),
     "cost=? rows=? width=?"),
    (re.compile(r"actual time=[0-9.]+\.\.[0-9.]+"), "actual time=?"),
    (re.compile(r"Planning Time: [0-9.]+ ms"), "Planning Time: ? ms"),
    (re.compile(r"Execution Time: [0-9.]+ ms"), "Execution Time: ? ms"),
)


@dataclass(frozen=True)
class Normalizer:
    name: str
    why: str
    default: bool
    #: For default (transcript-wide) normalizers: fn(transcript). For step normalizers:
    #: fn(step_observation). Exactly one of the two is used, by ``default``.
    apply: Callable[..., None]


# ---------------------------------------------------------------------------------------------
# user_oids (default, transcript-wide)
# ---------------------------------------------------------------------------------------------


def _is_user_oid(v: int) -> bool:
    """True for an integer that PostgreSQL could have allocated as a user OID and that is not
    already in the canonical range (idempotence)."""
    if not (_FIRST_USER_OID <= v <= _OID_MAX):
        return False
    return not (_CANON_BASE <= v < _CANON_BASE + _CANON_SPAN)


class _OidCanonicalizer:
    """Bijective first-appearance numbering of user OIDs for one transcript."""

    __slots__ = ("_numbers",)

    def __init__(self) -> None:
        self._numbers: dict[int, int] = {}

    def _number(self, oid: int) -> int:
        k = self._numbers.get(oid)
        if k is None:
            k = len(self._numbers) + 1
            if k >= _CANON_SPAN:
                raise ValueError(
                    f"transcript has more than {_CANON_SPAN - 1} distinct user OIDs; "
                    "the canonical OID range is exhausted")
            self._numbers[oid] = k
        return k

    def canon_int(self, v: int) -> int:
        if _is_user_oid(v):
            return _CANON_BASE + self._number(v)
        return v



def _canon_result(r: StatementResult, canon: _OidCanonicalizer) -> None:
    cols = r.columns
    # Documented order: all columns' table_oid, then all columns' type_oid.
    for c in cols:
        c.table_oid = canon.canon_int(c.table_oid)
    for c in cols:
        c.type_oid = canon.canon_int(c.type_oid)
    if r.param_type_oids is not None:
        r.param_type_oids = [canon.canon_int(v) for v in r.param_type_oids]


def has_legacy_oid_values(transcript: Transcript) -> bool:
    """Old recordings erased OID data as U<n>; require reviewed PG rerecording."""
    for step in [*transcript.setup, *transcript.steps]:
        for result in step.results:
            for i, col in enumerate(result.columns):
                if col.type_oid == 26:
                    for row in result.rows:
                        if i < len(row) and isinstance(row[i], str) and re.fullmatch(r"U[0-9]+", row[i]):
                            return True
    return False


def _user_oids(t: Transcript) -> None:
    canon = _OidCanonicalizer()
    for s in t.setup:
        for r in s.results:
            _canon_result(r, canon)
    for s in t.steps:
        for r in s.results:
            _canon_result(r, canon)


# ---------------------------------------------------------------------------------------------
# Step-scoped helpers
# ---------------------------------------------------------------------------------------------


def _mask_columns(s: StepObservation, predicate: Callable[[ColumnDesc], bool],
                  placeholder: str) -> None:
    """Replace every non-NULL row value of each matching column with ``placeholder``."""
    for r in s.results:
        idx = [i for i, c in enumerate(r.columns) if predicate(c)]
        if not idx:
            continue
        for row in r.rows:
            for i in idx:
                if i < len(row) and row[i] is not None:
                    row[i] = placeholder


def _redact_pids_in_text(text: str) -> str:
    return _PROCESS_RE.sub(lambda m: m.group(1) + "<pid>", text)


# ---------------------------------------------------------------------------------------------
# Opt-in step normalizers
# ---------------------------------------------------------------------------------------------


def _timestamps(s: StepObservation) -> None:
    _mask_columns(s, lambda c: c.type_oid in _TIMESTAMP_TYPES, "<timestamp>")


def _uuids(s: StepObservation) -> None:
    _mask_columns(s, lambda c: c.type_oid == _UUID_TYPE, "<uuid>")


def _pids(s: StepObservation) -> None:
    _mask_columns(
        s, lambda c: c.type_oid == _INT4_TYPE and c.name in _PID_COLUMNS, "<pid>")
    for n in s.notices:
        _redact_notice(n)
    for r in s.results:
        if r.error is not None:
            _redact_error(r.error)


def _redact_notice(n: Notice) -> None:
    n.message = _redact_pids_in_text(n.message)
    if n.detail is not None:
        n.detail = _redact_pids_in_text(n.detail)


def _redact_error(e: ErrorInfo) -> None:
    e.message = _redact_pids_in_text(e.message)
    if e.detail is not None:
        e.detail = _redact_pids_in_text(e.detail)


def _xids(s: StepObservation) -> None:
    _mask_columns(
        s, lambda c: c.type_oid in _XID_TYPES or c.name in _XID_COLUMNS, "<xid>")


def _lsns(s: StepObservation) -> None:
    _mask_columns(s, lambda c: c.type_oid == _LSN_TYPE, "<lsn>")


def _explain_numbers(s: StepObservation) -> None:
    for r in s.results:
        idx = [i for i, c in enumerate(r.columns) if c.name == _EXPLAIN_COLUMN]
        if not idx:
            continue
        for row in r.rows:
            for i in idx:
                if i < len(row):
                    v = row[i]
                    if isinstance(v, str):
                        for pat, rep in _EXPLAIN_SUBS:
                            v = pat.sub(rep, v)
                        row[i] = v


#: CONTRACT: names are referenced by case files; never rename.
REGISTRY: dict[str, Normalizer] = {
    "user_oids": Normalizer("user_oids", "descriptor object identities are allocation-dependent", True,
                            _user_oids),
    "timestamps": Normalizer("timestamps", "wall clock values differ per run", False,
                             _timestamps),
    "uuids": Normalizer("uuids", "random UUIDs differ per run", False, _uuids),
    "pids": Normalizer("pids", "backend process ids differ per run", False, _pids),
    "xids": Normalizer("xids", "transaction ids are allocation-dependent", False, _xids),
    "lsns": Normalizer("lsns", "WAL positions are implementation-specific", False, _lsns),
    "explain_numbers": Normalizer("explain_numbers", "planner cost/time estimates", False,
                                  _explain_numbers),
}


def apply_case_normalizers(case: Case, transcript: Transcript) -> Transcript:
    """Apply step normalizers (``case.steps[i].normalize`` to ``transcript.steps[i]``), then
    every default normalizer to the whole transcript. Unknown names raise KeyError (the
    validator rejects them earlier). Returns the same object (mutated)."""
    if len(case.steps) != len(transcript.steps):
        raise ValueError(
            f"case {transcript.case_id!r} has {len(case.steps)} steps but its transcript has "
            f"{len(transcript.steps)}")
    # Resolve every name before mutating anything, so an unknown name leaves the transcript
    # untouched. Default names are transcript-wide and are applied below, not per step.
    plan: list[tuple[StepObservation, Normalizer]] = []
    for step_spec, obs in zip(case.steps, transcript.steps):
        for name in step_spec.normalize:
            norm = REGISTRY[name]
            if not norm.default:
                plan.append((obs, norm))
    for obs, norm in plan:
        norm.apply(obs)
    for norm in REGISTRY.values():
        if norm.default:
            norm.apply(transcript)
    return transcript
