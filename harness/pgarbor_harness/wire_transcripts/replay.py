"""ORACLE-08: replay a recorded client transcript against a target and compare server messages.

Lease HAR-25.

Inputs: a recorded transcript (proxy.py format) from ``tests/expected/wire/<name>.jsonl``
(recorded from the PG oracle through the oracle review path).

Replay per connection ``cN`` (connections replayed sequentially in first-seen order; a
transcript with overlapping connections replays them concurrently, one thread each, keeping
per-connection order):
  * open a TCP socket to the target; send each frontend message (messages.encode) in order;
  * after sending a message that the recorded stream shows was followed by backend messages
    before the next frontend message, read backend messages until the same COUNT of messages
    has been received or a ReadyForQuery is seen when the recording also ended that run with
    ReadyForQuery (timeout config.STEP_TIMEOUT_S -> HANG);
  * the CancelRequest connection: rewrite pid/secret with the values of the replayed
    connection's BackendKeyData.

Comparison of backend messages (recorded PG vs target), in order, with these documented
normalizations ONLY:
  * BackendKeyData: pid and secret ignored (random per connection);
  * ParameterStatus: compared exactly for ``server_encoding``, ``client_encoding``,
    ``DateStyle``, ``IntervalStyle``, ``TimeZone``, ``integer_datetimes``,
    ``standard_conforming_strings``, ``is_superuser``, ``session_authorization``,
    ``application_name``, ``default_transaction_read_only``, ``in_hot_standby``,
    ``scram_iterations``, ``search_path``; ``server_version`` compared exactly too (clients
    parse it; PGArbor must report a PG18-compatible version string — the expected value is
    the oracle's ``18.6 (...)``; a difference is a MISMATCH that the wire lease owner must
    resolve by amendment, not a normalization). The ORDER of ParameterStatus messages within
    the startup burst is NOT compared (PG sends them in GUC-table order; clients must not
    depend on it) — compare as a set keyed by name;
  * RowDescription: table OID and type OIDs >= 16384 canonicalized like
    compare.normalize ``user_oids`` (bijective per transcript);
  * ErrorResponse/NoticeResponse: fields ``F`` (file), ``L`` (line), ``R`` (routine) ignored
    (source locations of the C implementation); all other fields exact.
Everything else (message types, order, CommandComplete tags, DataRow bytes, ReadyForQuery
status) must be identical.

Outcome: PASS / MISMATCH (first differing message index + both decoded messages) / CRASH
(connection reset before the recorded end) / HANG.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from ..model import Outcome


@dataclass
class WireReplayResult:
    name: str
    outcome: Outcome
    reason: str = ""
    connection: Optional[str] = None
    message_index: Optional[int] = None
    expected: Optional[dict[str, Any]] = None
    actual: Optional[dict[str, Any]] = None
    notes: list[str] = field(default_factory=list)


def load_transcript(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """(header, records) from a JSONL transcript; validates the header format."""
    raise NotImplementedError("HAR-25: load wire transcript")


def normalize_backend(msg: dict[str, Any], oid_map: dict[int, int]) -> dict[str, Any]:
    """Apply the documented normalizations to one decoded backend message (returns a copy)."""
    raise NotImplementedError("HAR-25: normalize backend message")


def replay(path: Path, host: str, port: int) -> WireReplayResult:
    """Replay one transcript against host:port and compare."""
    raise NotImplementedError("HAR-25: replay transcript")
