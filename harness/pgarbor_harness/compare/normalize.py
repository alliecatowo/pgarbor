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
    Why: OIDs >= 16384 are allocated from a cluster-wide counter in PG (they differ between two
    PG runs) and from a per-branch counter in PGArbor; their numeric value is not observable
    behavior, only their identity is. What: per transcript, collect every occurrence in
    first-appearance order (setup then steps; within a result: columns' table_oid, type_oid,
    then param_type_oids, then row values of columns whose type_oid is 26 ``oid``) of integers
    >= 16384 and replace them with a bijective canonical numbering: the k-th distinct OID
    becomes integer ``1_000_000_000 + k`` in ColumnDesc ints / param_type_oids, and string
    ``"U<k>"`` in oid-typed text values. Values < 16384 are untouched (built-in objects must
    match exactly). Idempotence: 1_000_000_000+k >= 16384 would be renumbered again, so the
    normalizer must skip values already in [1_000_000_000, 1_000_000_000+10**6) and strings
    matching ``^U\\d+$``.

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

from dataclasses import dataclass
from typing import Callable

from ..cases.model import Case
from ..model import StepObservation, Transcript


@dataclass(frozen=True)
class Normalizer:
    name: str
    why: str
    default: bool
    #: For default (transcript-wide) normalizers: fn(transcript). For step normalizers:
    #: fn(step_observation). Exactly one of the two is used, by ``default``.
    apply: Callable[..., None]


def _user_oids(t: Transcript) -> None:
    raise NotImplementedError("HAR-12: user_oids normalizer")


def _timestamps(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: timestamps normalizer")


def _uuids(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: uuids normalizer")


def _pids(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: pids normalizer")


def _xids(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: xids normalizer")


def _lsns(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: lsns normalizer")


def _explain_numbers(s: StepObservation) -> None:
    raise NotImplementedError("HAR-12: explain numbers normalizer")


#: CONTRACT: names are referenced by case files; never rename.
REGISTRY: dict[str, Normalizer] = {
    "user_oids": Normalizer("user_oids", "user OID values are allocation-dependent", True,
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
    raise NotImplementedError("HAR-12: apply normalizers")
