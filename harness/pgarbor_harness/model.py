"""Frozen observation / result data model shared by recorder, runner, comparator, reports.

CONTRACT (harness v1). Fully implemented; workers import, never edit. Any change is an
amendment (contracts/amendments/harness-NN-*.md) because ``tests/expected/**`` is serialized
with these exact shapes.

Layering::

    Transcript                one endpoint's full observation of one Case
      setup:  [StepObservation]   (from case.setup, session "setup")
      steps:  [StepObservation]   (one per case step, same index as case.steps)
        results: [StatementResult]   (one per protocol-level result; simple Query with
                                       3 statements => 3 results)
          columns: [ColumnDesc]
          rows:    [[Value]]          Value = str (text format) | bytes (binary format) | None
          error:   ErrorInfo | None
        notices: [Notice]           (NoticeResponse messages received during the step,
                                     in arrival order)

    ExpectedFile = Transcript recorded from the pinned PG18 + identity, stored at
                   tests/expected/<family>/<case_id>.json (see expected_path()).

    CaseResult = comparison outcome for one case on one target (Outcome + diffs).

JSON encoding rules (byte-exact, see canonical_json):
  * text-format values are JSON strings (exact server bytes decoded as UTF-8; the runner
    connects with client_encoding=UTF8 so decoding never fails for valid servers; if it does,
    the value is stored as bytes and becomes ``{"hex": "..."}``);
  * binary-format values are ``{"hex": "<lowercase hex>"}``;
  * NULL is JSON ``null`` (never the empty string);
  * Enums serialize as their ``.value`` string;
  * keys sorted, 2-space indent, ``ensure_ascii=False``, trailing newline.

Example expected file (abridged)::

    {
      "case_id": "smoke.create-insert-select",
      "case_sha256": "9f2c...",
      "format": "pgarbor-expected/1",
      "reference": {"fingerprint_sha256": "...", "image_digest": "sha256:7493...",
                    "server_version": "18.6 (Debian 18.6-1.pgdg13+2)"},
      "transcript": {
        "case_id": "smoke.create-insert-select", "endpoint": "reference",
        "setup": [],
        "steps": [
          {"blocked_observed": null, "connection_lost": false, "elapsed_ms": 0,
           "harness_error": null, "index": 0, "kind": "sql", "notices": [],
           "results": [{"columns": [], "command_tag": "CREATE TABLE", "error": null,
                        "kind": "command", "param_type_oids": null, "rows": []}],
           "session": "a", "timed_out": false, "txn_status": "I"},
          {"index": 2, "kind": "sql", "session": "a", "txn_status": "I", ...,
           "results": [{"kind": "rows", "command_tag": "SELECT 2", "error": null,
                        "columns": [{"column_attnum": 1, "format": 0, "name": "id",
                                     "table_oid": 16385, "type_oid": 23, "typmod": -1}],
                        "rows": [["1"], ["2"]], "param_type_oids": null}]}
        ]
      }
    }

``elapsed_ms`` is recorded as 0 in expected files (the recorder zeroes it) and is never
compared.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Literal, Optional, Union

# ---------------------------------------------------------------------------------------------
# Outcome taxonomy (ARCHITECTURE §13, RFC 002 §3). CONTRACT
# ---------------------------------------------------------------------------------------------


class Outcome(str, Enum):
    """Per-case classification. Only PASS counts as pass."""

    PASS = "PASS"
    MISMATCH = "MISMATCH"
    CRASH = "CRASH"
    HANG = "HANG"
    CORRUPTION = "CORRUPTION"
    UNEXPECTED_ACCEPTANCE = "UNEXPECTED_ACCEPTANCE"
    UNSUPPORTED = "UNSUPPORTED"
    BLOCKED = "BLOCKED"
    FLAKY = "FLAKY"
    INVALID_CASE = "INVALID_CASE"
    HARNESS_ERROR = "HARNESS_ERROR"

    @property
    def is_pass(self) -> bool:
        return self is Outcome.PASS

    @property
    def is_critical(self) -> bool:
        """Critical outcomes stop the line (RFC 002 §2B, RFC 003 §11)."""
        return self in (Outcome.CORRUPTION, Outcome.CRASH)


#: Report-level status for a corpus item that was never executed in this campaign. Not an
#: Outcome (a case that ran always has an Outcome); reports show it as its own column.
UNRUN = "UNRUN"

#: Precedence when several observations of one case disagree, most severe first. Used by
#: compare.classify for the "override" outcomes; see that module for the full algorithm.
OUTCOME_PRECEDENCE: tuple[Outcome, ...] = (
    Outcome.HARNESS_ERROR,
    Outcome.INVALID_CASE,
    Outcome.BLOCKED,
    Outcome.CORRUPTION,
    Outcome.CRASH,
    Outcome.HANG,
    Outcome.FLAKY,
    Outcome.UNSUPPORTED,
    Outcome.UNEXPECTED_ACCEPTANCE,
    Outcome.MISMATCH,
    Outcome.PASS,
)

#: PostgreSQL transaction status as reported in ReadyForQuery / PQtransactionStatus:
#: "I" idle, "T" in transaction block, "E" failed transaction block,
#: "A" a command is in progress (async step pending), "U" unknown/connection bad.
TxnStatus = Literal["I", "T", "E", "A", "U"]

ResultKind = Literal["rows", "command", "error", "empty", "description", "copy_out"]

Value = Union[str, bytes, None]

# ---------------------------------------------------------------------------------------------
# Canonical JSON. CONTRACT
# ---------------------------------------------------------------------------------------------


def _jsonable(obj: Any) -> Any:
    if obj is None or isinstance(obj, (bool, int, float, str)):
        return obj
    if isinstance(obj, Enum):
        return obj.value
    if isinstance(obj, (bytes, bytearray, memoryview)):
        return {"hex": bytes(obj).hex()}
    if isinstance(obj, Path):
        return obj.as_posix()
    if isinstance(obj, dict):
        return {str(k): _jsonable(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [_jsonable(v) for v in obj]
    to_json = getattr(obj, "to_json", None)
    if callable(to_json):
        return to_json()
    raise TypeError(f"not JSON-serializable: {type(obj).__name__}")


def canonical_json(obj: Any) -> str:
    """Serialize deterministically: sorted keys, indent=2, ensure_ascii=False, trailing "\\n".

    >>> canonical_json({"b": 1, "a": b"\\x01"})
    '{\\n  "a": {\\n    "hex": "01"\\n  },\\n  "b": 1\\n}\\n'
    """
    return json.dumps(_jsonable(obj), sort_keys=True, indent=2, ensure_ascii=False) + "\n"


def write_canonical_json(path: Path, obj: Any) -> None:
    """Atomically write ``canonical_json(obj)`` (temp file in same dir + os.replace)."""
    import os
    import tempfile

    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".tmp-", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
            f.write(canonical_json(obj))
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise


def decode_value(v: Any) -> Value:
    """Inverse of the value encoding: ``{"hex": ..}`` -> bytes, str -> str, null -> None."""
    if v is None or isinstance(v, str):
        return v
    if isinstance(v, dict) and set(v) == {"hex"}:
        return bytes.fromhex(v["hex"])
    raise ValueError(f"bad encoded value: {v!r}")


# ---------------------------------------------------------------------------------------------
# Observation records. CONTRACT
# ---------------------------------------------------------------------------------------------


@dataclass
class ColumnDesc:
    """One RowDescription field (libpq PQfname/PQftype/PQfmod/PQfformat/PQftable/PQftablecol)."""

    name: str
    type_oid: int
    typmod: int = -1
    format: int = 0  #: 0 text, 1 binary
    table_oid: int = 0  #: 0 when not a plain table column
    column_attnum: int = 0  #: 0 when not a plain table column

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name, "type_oid": self.type_oid, "typmod": self.typmod,
            "format": self.format, "table_oid": self.table_oid,
            "column_attnum": self.column_attnum,
        }

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "ColumnDesc":
        return cls(d["name"], int(d["type_oid"]), int(d.get("typmod", -1)),
                   int(d.get("format", 0)), int(d.get("table_oid", 0)),
                   int(d.get("column_attnum", 0)))


@dataclass
class ErrorInfo:
    """ErrorResponse fields (protocol field codes in brackets). Unset fields are None.

    ``severity`` is the non-localized severity [V] (ERROR, FATAL, PANIC).
    ``position`` [P] is a 1-based character index into the statement text as a string
    (kept as str exactly as sent). ``context`` is the [W] "where" field.
    """

    severity: str
    sqlstate: str
    message: str
    detail: Optional[str] = None
    hint: Optional[str] = None
    position: Optional[str] = None
    internal_position: Optional[str] = None
    internal_query: Optional[str] = None
    context: Optional[str] = None
    schema_name: Optional[str] = None
    table_name: Optional[str] = None
    column_name: Optional[str] = None
    datatype_name: Optional[str] = None
    constraint_name: Optional[str] = None

    FIELDS = (
        "severity", "sqlstate", "message", "detail", "hint", "position",
        "internal_position", "internal_query", "context", "schema_name", "table_name",
        "column_name", "datatype_name", "constraint_name",
    )

    def to_json(self) -> dict[str, Any]:
        return {k: getattr(self, k) for k in self.FIELDS}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "ErrorInfo":
        return cls(**{k: d.get(k) for k in cls.FIELDS})  # type: ignore[arg-type]

    @property
    def sqlstate_class(self) -> str:
        return self.sqlstate[:2]


@dataclass
class Notice:
    """NoticeResponse (NOTICE/WARNING/INFO/DEBUG/LOG). Same field semantics as ErrorInfo."""

    severity: str
    sqlstate: str
    message: str
    detail: Optional[str] = None
    hint: Optional[str] = None
    context: Optional[str] = None

    def to_json(self) -> dict[str, Any]:
        return {"severity": self.severity, "sqlstate": self.sqlstate, "message": self.message,
                "detail": self.detail, "hint": self.hint, "context": self.context}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "Notice":
        return cls(d["severity"], d["sqlstate"], d["message"], d.get("detail"), d.get("hint"),
                   d.get("context"))


@dataclass
class StatementResult:
    """One protocol-level result.

    kind:
      * ``rows``        RowDescription + DataRow* + CommandComplete (SELECT, RETURNING, SHOW..)
      * ``command``     CommandComplete without rows (DDL, DML w/o RETURNING, BEGIN, SET..)
      * ``error``       ErrorResponse (``error`` set; ``columns``/``rows`` hold whatever was
                        received before the error, normally empty)
      * ``empty``       EmptyQueryResponse (query string was only whitespace/comments)
      * ``description`` result of a ``describe`` step: ``columns`` (RowDescription or empty
                        for NoData) and ``param_type_oids`` (ParameterDescription)
      * ``copy_out``    ``COPY ... TO STDOUT``: ``rows`` = one single-element row per CopyData
                        message (``[[line]]``, text decoded, or bytes for binary COPY),
                        ``columns`` empty, ``command_tag`` from the final CommandComplete
                        (``COPY 3``). ``COPY FROM STDIN`` is answered by the runner with
                        CopyFail("pgarbor harness: COPY FROM STDIN data not supported"), which
                        yields a deterministic ``error`` result (57014) on every server.

    ``command_tag`` is the exact CommandComplete text (``INSERT 0 3``, ``SELECT 2``).
    """

    kind: ResultKind
    columns: list[ColumnDesc] = field(default_factory=list)
    rows: list[list[Value]] = field(default_factory=list)
    command_tag: Optional[str] = None
    error: Optional[ErrorInfo] = None
    param_type_oids: Optional[list[int]] = None

    def to_json(self) -> dict[str, Any]:
        return {
            "kind": self.kind,
            "columns": [c.to_json() for c in self.columns],
            "rows": [[_jsonable(v) for v in r] for r in self.rows],
            "command_tag": self.command_tag,
            "error": self.error.to_json() if self.error else None,
            "param_type_oids": self.param_type_oids,
        }

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "StatementResult":
        return cls(
            kind=d["kind"],
            columns=[ColumnDesc.from_json(c) for c in d.get("columns", [])],
            rows=[[decode_value(v) for v in r] for r in d.get("rows", [])],
            command_tag=d.get("command_tag"),
            error=ErrorInfo.from_json(d["error"]) if d.get("error") else None,
            param_type_oids=d.get("param_type_oids"),
        )


@dataclass
class StepObservation:
    """What one endpoint did for one case step (or one setup statement batch).

    ``index`` equals the step's index in ``Case.steps`` (or ``Case.setup``).
    ``results`` is empty for ``wait``-less async launches (their results are attached to the
    matching ``wait`` step), for ``reconnect``/``restart`` steps, and on harness errors.
    ``txn_status`` is PQtransactionStatus of the step's session right after the step.
    ``blocked_observed`` only for async steps: True if the step was still running
    ``BLOCK_SETTLE_S`` after launch; otherwise None.
    ``timed_out``: the step did not complete within STEP_TIMEOUT_S (the runner then sent
    CancelRequest and reconnected the session).
    ``connection_lost``: the server closed the connection / socket error (not after an
    explicit FATAL that the case expected).
    ``harness_error``: harness-side exception text; makes the case HARNESS_ERROR.
    """

    index: int
    session: str
    kind: str
    results: list[StatementResult] = field(default_factory=list)
    notices: list[Notice] = field(default_factory=list)
    txn_status: str = "U"
    blocked_observed: Optional[bool] = None
    timed_out: bool = False
    connection_lost: bool = False
    harness_error: Optional[str] = None
    elapsed_ms: int = 0

    def to_json(self) -> dict[str, Any]:
        return {
            "index": self.index, "session": self.session, "kind": self.kind,
            "results": [r.to_json() for r in self.results],
            "notices": [n.to_json() for n in self.notices],
            "txn_status": self.txn_status, "blocked_observed": self.blocked_observed,
            "timed_out": self.timed_out, "connection_lost": self.connection_lost,
            "harness_error": self.harness_error, "elapsed_ms": self.elapsed_ms,
        }

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "StepObservation":
        return cls(
            index=int(d["index"]), session=d["session"], kind=d["kind"],
            results=[StatementResult.from_json(r) for r in d.get("results", [])],
            notices=[Notice.from_json(n) for n in d.get("notices", [])],
            txn_status=d.get("txn_status", "U"), blocked_observed=d.get("blocked_observed"),
            timed_out=bool(d.get("timed_out", False)),
            connection_lost=bool(d.get("connection_lost", False)),
            harness_error=d.get("harness_error"), elapsed_ms=int(d.get("elapsed_ms", 0)),
        )


@dataclass
class Transcript:
    """Full observation of one case on one endpoint. ``endpoint`` is "reference" or "target"."""

    case_id: str
    endpoint: str
    setup: list[StepObservation] = field(default_factory=list)
    steps: list[StepObservation] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {"case_id": self.case_id, "endpoint": self.endpoint,
                "setup": [s.to_json() for s in self.setup],
                "steps": [s.to_json() for s in self.steps]}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "Transcript":
        return cls(d["case_id"], d["endpoint"],
                   [StepObservation.from_json(s) for s in d.get("setup", [])],
                   [StepObservation.from_json(s) for s in d.get("steps", [])])

    def zero_timings(self) -> "Transcript":
        """Set every ``elapsed_ms`` to 0 in place (recorder does this before writing)."""
        for s in [*self.setup, *self.steps]:
            s.elapsed_ms = 0
        return self


@dataclass
class ReferenceIdentity:
    """Which oracle produced an expected file. All three must match the live oracle."""

    image_digest: str
    server_version: str
    fingerprint_sha256: str

    def to_json(self) -> dict[str, Any]:
        return {"image_digest": self.image_digest, "server_version": self.server_version,
                "fingerprint_sha256": self.fingerprint_sha256}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "ReferenceIdentity":
        return cls(d["image_digest"], d["server_version"], d["fingerprint_sha256"])


EXPECTED_FORMAT = "pgarbor-expected/1"


@dataclass
class ExpectedFile:
    """``tests/expected/<family>/<case_id>.json``. Written ONLY by oracle.recorder.

    ``case_sha256`` = hashing.case_sha256(case file bytes); a mismatch with the current case file
    means the expectation is stale -> the case is BLOCKED(reason="reference_stale") until
    re-recorded through the oracle review path.
    """

    case_id: str
    case_sha256: str
    reference: ReferenceIdentity
    transcript: Transcript
    format: str = EXPECTED_FORMAT

    def to_json(self) -> dict[str, Any]:
        return {"format": self.format, "case_id": self.case_id, "case_sha256": self.case_sha256,
                "reference": self.reference.to_json(), "transcript": self.transcript.to_json()}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "ExpectedFile":
        if d.get("format") != EXPECTED_FORMAT:
            raise ValueError(f"unsupported expected-file format {d.get('format')!r}")
        return cls(d["case_id"], d["case_sha256"], ReferenceIdentity.from_json(d["reference"]),
                   Transcript.from_json(d["transcript"]), d["format"])

    @classmethod
    def load(cls, path: Path) -> "ExpectedFile":
        return cls.from_json(json.loads(path.read_text(encoding="utf-8")))

    def save(self, path: Path) -> None:
        write_canonical_json(path, self)


def case_family(case_id: str) -> str:
    """Family = text before the first '.' of the case id (``smoke.x`` -> ``smoke``)."""
    return case_id.split(".", 1)[0]


def expected_path(expected_root: Path, case_id: str) -> Path:
    """CONTRACT path rule: ``<expected_root>/<family>/<case_id>.json``.

    >>> expected_path(Path("tests/expected"), "smoke.create-insert-select").as_posix()
    'tests/expected/smoke/smoke.create-insert-select.json'
    """
    return expected_root / case_family(case_id) / f"{case_id}.json"


# ---------------------------------------------------------------------------------------------
# Comparison results. CONTRACT
# ---------------------------------------------------------------------------------------------


@dataclass
class Diff:
    """One machine-readable difference.

    ``path`` is a JSONPath-like locator into the Transcript, e.g.
    ``steps[3].results[0].rows`` or ``steps[1].results[0].error.sqlstate``.
    ``kind`` is one of compare.typed.DIFF_KINDS (the authoritative list).
    """

    path: str
    kind: str
    reference: Any = None
    target: Any = None
    note: str = ""

    def to_json(self) -> dict[str, Any]:
        return {"path": self.path, "kind": self.kind, "reference": _jsonable(self.reference),
                "target": _jsonable(self.target), "note": self.note}

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "Diff":
        return cls(d["path"], d["kind"], d.get("reference"), d.get("target"), d.get("note", ""))


@dataclass
class CaseResult:
    """Outcome of one case on one target. Serialized into run reports and evidence packs.

    ``reason``: short machine token explaining non-PASS outcomes, e.g. ``first_divergence``,
    ``reference_missing``, ``reference_stale``, ``capability:jsonb``, ``schema:<msg>``,
    ``intent_mismatch``, ``reference_nondeterministic``, ``target_nondeterministic``.
    ``first_divergence``: earliest diff in step order (drives clustering).
    ``signature``: cluster key fields filled by compare.classify (all str):
    ``{"diff_kind", "sqlstate", "ref_sqlstate", "message_template", "step_kind",
       "first_keyword", "unimplemented_lease"}``.
    """

    case_id: str
    family: str
    outcome: Outcome
    reason: str = ""
    first_divergence: Optional[Diff] = None
    diffs: list[Diff] = field(default_factory=list)
    feature_ids: list[str] = field(default_factory=list)
    signature: dict[str, str] = field(default_factory=dict)
    duration_ms: int = 0
    attempts: int = 1
    transcript_path: Optional[str] = None

    def to_json(self) -> dict[str, Any]:
        return {
            "case_id": self.case_id, "family": self.family, "outcome": self.outcome.value,
            "reason": self.reason,
            "first_divergence": self.first_divergence.to_json() if self.first_divergence else None,
            "diffs": [d.to_json() for d in self.diffs], "feature_ids": list(self.feature_ids),
            "signature": dict(self.signature), "duration_ms": self.duration_ms,
            "attempts": self.attempts, "transcript_path": self.transcript_path,
        }

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "CaseResult":
        fd = d.get("first_divergence")
        return cls(
            case_id=d["case_id"], family=d["family"], outcome=Outcome(d["outcome"]),
            reason=d.get("reason", ""), first_divergence=Diff.from_json(fd) if fd else None,
            diffs=[Diff.from_json(x) for x in d.get("diffs", [])],
            feature_ids=list(d.get("feature_ids", [])), signature=dict(d.get("signature", {})),
            duration_ms=int(d.get("duration_ms", 0)), attempts=int(d.get("attempts", 1)),
            transcript_path=d.get("transcript_path"),
        )


# ---------------------------------------------------------------------------------------------
# Endpoints. CONTRACT
# ---------------------------------------------------------------------------------------------


def _conninfo_quote(v: str) -> str:
    if v and all(c not in v for c in " '\\"):
        return v
    return "'" + v.replace("\\", "\\\\").replace("'", "\\'") + "'"


@dataclass(frozen=True)
class Endpoint:
    """A PG-wire endpoint the runner connects sessions to.

    ``gucs`` are sent as the startup ``options`` parameter (``-c name=value`` each, spaces in
    values escaped with backslash as libpq requires).
    """

    host: str
    port: int
    dbname: str
    user: str = "postgres"
    password: Optional[str] = None
    application_name: str = "pgarbor-harness"
    gucs: tuple[tuple[str, str], ...] = ()

    def options_string(self) -> str:
        parts = []
        for k, v in self.gucs:
            parts.append(f"-c {k}=" + v.replace("\\", "\\\\").replace(" ", "\\ "))
        return " ".join(parts)

    def conninfo(self) -> str:
        """libpq keyword/value conninfo, e.g.
        ``host=127.0.0.1 port=54319 dbname=x user=postgres application_name=pgarbor-harness``.
        Always ``sslmode=disable`` and ``gssencmode=disable`` (PGArbor answers 'N' to both).
        """
        kv = [("host", self.host), ("port", str(self.port)), ("dbname", self.dbname),
              ("user", self.user), ("application_name", self.application_name),
              ("sslmode", "disable"), ("gssencmode", "disable")]
        if self.password is not None:
            kv.append(("password", self.password))
        opts = self.options_string()
        if opts:
            kv.append(("options", opts))
        return " ".join(f"{k}={_conninfo_quote(v)}" for k, v in kv)

    def with_db(self, dbname: str) -> "Endpoint":
        return Endpoint(self.host, self.port, dbname, self.user, self.password,
                        self.application_name, self.gucs)
