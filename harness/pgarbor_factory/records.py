"""Frozen record shapes of the factory: lease ledger, submissions, evidence packs, repair
tickets, plus a locked JSON state store.

CONTRACT (harness v1). Implemented; workers import, never edit. Every file under ``runs/``
the factory writes uses these shapes (canonical JSON from pgarbor_harness.model).

Files:
  runs/state/leases.json              LeaseLedger          (pgarbor-lease-ledger/1)
  runs/submissions/<LEASE-ID>.json    Submission           (pgarbor-submission/1)
  runs/<campaign_id>/evidence.json    EvidencePack         (pgarbor-evidence/1)
  runs/<campaign_id>/repair_dag.json  {"tasks": [RepairTicket...]} (task_graph-compatible)
"""

from __future__ import annotations

import fcntl
import json
import os
import time
from contextlib import contextmanager
from dataclasses import asdict, dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Iterator, Optional

from pgarbor_harness.model import canonical_json, write_canonical_json

# ---------------------------------------------------------------------------------------------
# Locked JSON store. CONTRACT
# ---------------------------------------------------------------------------------------------


@contextmanager
def locked_json(path: Path, default: Any) -> Iterator[dict[str, Any]]:
    """Exclusive read-modify-write of a JSON document.

    ``with locked_json(p, {"leases": {}}) as doc: doc["x"] = 1`` — takes ``fcntl.flock`` on
    ``<path>.lock`` (created if needed), loads the document (or a deep copy of ``default``
    when missing), yields it, and on normal exit writes it back atomically with canonical
    JSON. On exception nothing is written. Safe across processes on one machine.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = path.with_name(path.name + ".lock")
    with open(lock_path, "a+") as lf:
        fcntl.flock(lf.fileno(), fcntl.LOCK_EX)
        try:
            if path.exists():
                doc = json.loads(path.read_text(encoding="utf-8"))
            else:
                doc = json.loads(json.dumps(default))
            yield doc
            write_canonical_json(path, doc)
        finally:
            fcntl.flock(lf.fileno(), fcntl.LOCK_UN)


def read_json(path: Path, default: Any = None) -> Any:
    """Unlocked read (callers that only read a snapshot)."""
    if not path.exists():
        return default
    return json.loads(path.read_text(encoding="utf-8"))


def now_unix() -> int:
    return int(time.time())


# ---------------------------------------------------------------------------------------------
# Lease ledger. CONTRACT
# ---------------------------------------------------------------------------------------------


class LeaseState(str, Enum):
    """RFC 003 §2 implementation state machine (+ REJECTED for assembly refusals).

    Allowed transitions (anything else is an error in pgarbor_factory.leases.transition):
      SPEC_READY -> SPECULATING (lease granted)
      SPECULATING -> PATCH_SUBMITTED | SPEC_READY (expired/released)
      PATCH_SUBMITTED -> ASSEMBLED | REJECTED
      REJECTED -> SPEC_READY (re-dispatch, attempts+1)
      ASSEMBLED -> CAMPAIGN_PASS | CAMPAIGN_FAIL
      CAMPAIGN_FAIL -> REPAIR_QUEUED
      REPAIR_QUEUED -> SPEC_READY
      CAMPAIGN_PASS -> VERIFIED  (only by the trusted campaign; never by a worker)
    """

    SPEC_READY = "SPEC_READY"
    SPECULATING = "SPECULATING"
    PATCH_SUBMITTED = "PATCH_SUBMITTED"
    REJECTED = "REJECTED"
    ASSEMBLED = "ASSEMBLED"
    CAMPAIGN_FAIL = "CAMPAIGN_FAIL"
    CAMPAIGN_PASS = "CAMPAIGN_PASS"
    REPAIR_QUEUED = "REPAIR_QUEUED"
    VERIFIED = "VERIFIED"


LEASE_TRANSITIONS: dict[LeaseState, tuple[LeaseState, ...]] = {
    LeaseState.SPEC_READY: (LeaseState.SPECULATING,),
    LeaseState.SPECULATING: (LeaseState.PATCH_SUBMITTED, LeaseState.SPEC_READY),
    LeaseState.PATCH_SUBMITTED: (LeaseState.ASSEMBLED, LeaseState.REJECTED),
    LeaseState.REJECTED: (LeaseState.SPEC_READY,),
    LeaseState.ASSEMBLED: (LeaseState.CAMPAIGN_PASS, LeaseState.CAMPAIGN_FAIL),
    LeaseState.CAMPAIGN_FAIL: (LeaseState.REPAIR_QUEUED,),
    LeaseState.REPAIR_QUEUED: (LeaseState.SPEC_READY,),
    LeaseState.CAMPAIGN_PASS: (LeaseState.VERIFIED,),
    LeaseState.VERIFIED: (),
}

LEDGER_FORMAT = "pgarbor-lease-ledger/1"


@dataclass
class Transition:
    at_unix: int
    from_state: str
    to_state: str
    note: str = ""


@dataclass
class LeaseEntry:
    """Ledger row for one lease (keyed by lease_id in LeaseLedger.leases)."""

    lease_id: str
    crate: str
    state: LeaseState = LeaseState.SPEC_READY
    holder: Optional[str] = None
    expires_unix: Optional[int] = None
    attempts: int = 0
    base_sha: Optional[str] = None
    head_sha: Optional[str] = None
    branch: Optional[str] = None
    history: list[Transition] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        d = asdict(self)
        d["state"] = self.state.value
        return d

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "LeaseEntry":
        return cls(
            lease_id=d["lease_id"], crate=d["crate"], state=LeaseState(d["state"]),
            holder=d.get("holder"), expires_unix=d.get("expires_unix"),
            attempts=int(d.get("attempts", 0)), base_sha=d.get("base_sha"),
            head_sha=d.get("head_sha"), branch=d.get("branch"),
            history=[Transition(**h) for h in d.get("history", [])],
        )


def empty_ledger() -> dict[str, Any]:
    """Default document for locked_json(runs/state/leases.json, ...)."""
    return {"format": LEDGER_FORMAT, "leases": {}}


# ---------------------------------------------------------------------------------------------
# Lease files (contracts/leases/<crate>.json). CONTRACT (read-only for the factory)
# ---------------------------------------------------------------------------------------------


@dataclass
class LeaseSpec:
    lease_id: str
    crate: str
    task_ids: list[str]
    files: list[str]
    model_tier: str
    summary: str
    spec: str
    reads: list[str] = field(default_factory=list)
    depends_on_leases: list[str] = field(default_factory=list)
    acceptance: list[str] = field(default_factory=list)
    contract_version: str = "v1"

    @classmethod
    def from_json(cls, crate: str, contract_version: str, d: dict[str, Any]) -> "LeaseSpec":
        return cls(
            lease_id=d["lease_id"], crate=crate, task_ids=list(d.get("task_ids", [])),
            files=list(d.get("files", [])), model_tier=d.get("model_tier", "haiku"),
            summary=d.get("summary", ""), spec=d.get("spec", ""),
            reads=list(d.get("reads", [])),
            depends_on_leases=list(d.get("depends_on_leases", [])),
            acceptance=list(d.get("acceptance", [])), contract_version=contract_version,
        )


# ---------------------------------------------------------------------------------------------
# Submissions (SWARM-06). CONTRACT
# ---------------------------------------------------------------------------------------------

SUBMISSION_FORMAT = "pgarbor-submission/1"


@dataclass
class Submission:
    """What a worker hands back. ``branch`` is the git branch holding the work
    (``lease/<LEASE-ID>``), ``head_sha`` its tip, ``base_sha`` the frozen contract SHA the
    worker started from. Claims are advisory; only the campaign decides VERIFIED."""

    lease_id: str
    worker: str
    base_sha: str
    branch: str
    head_sha: str
    files_changed: list[str] = field(default_factory=list)
    assumptions: list[str] = field(default_factory=list)
    implemented_behaviors: list[str] = field(default_factory=list)
    tests_added: list[str] = field(default_factory=list)
    unresolved: list[str] = field(default_factory=list)
    submitted_unix: int = 0
    format: str = SUBMISSION_FORMAT

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "Submission":
        if d.get("format") != SUBMISSION_FORMAT:
            raise ValueError(f"bad submission format {d.get('format')!r}")
        return cls(**d)


# ---------------------------------------------------------------------------------------------
# Evidence pack (RFC 002 §3, RFC 003 §6.8; SWARM-13). CONTRACT
# ---------------------------------------------------------------------------------------------

EVIDENCE_FORMAT = "pgarbor-evidence/1"

#: Status columns every family count table has, in report order. "UNRUN" = cataloged but not
#: executed in this campaign.
COUNT_KEYS: tuple[str, ...] = (
    "PASS", "MISMATCH", "CRASH", "HANG", "CORRUPTION", "UNEXPECTED_ACCEPTANCE", "UNSUPPORTED",
    "BLOCKED", "FLAKY", "INVALID_CASE", "HARNESS_ERROR", "UNRUN",
)


def empty_counts() -> dict[str, int]:
    return {k: 0 for k in COUNT_KEYS}


@dataclass
class FamilyEvidence:
    """One verification family's result inside a campaign.

    ``status``: "RAN" | "SKIPPED_PRECONDITION" (e.g. build failed, so SQL families did not
    run — all their items count as UNRUN) | "BLOCKED" (family-level blocker, e.g. regress
    could not create its database) | "HARNESS_ERROR".
    Three denominators (RFC 002 §8): ``total_cataloged`` (all items known), ``runnable``
    (executed and not BLOCKED/INVALID_CASE), ``passed``.
    """

    name: str
    status: str
    duration_s: float = 0.0
    counts: dict[str, int] = field(default_factory=empty_counts)
    total_cataloged: int = 0
    runnable: int = 0
    passed: int = 0
    artifacts: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


@dataclass
class EvidencePack:
    """``runs/<campaign_id>/evidence.json``. Immutable once written (the campaign writes it
    once at the end; a rerun creates a new campaign_id)."""

    campaign_id: str
    epoch: int
    mode: str                                   #: "cathedral" | "seams" | "conventional"
    git: dict[str, Any]                         #: {"sha", "branch", "dirty": bool}
    toolchain: dict[str, str]                   #: {"rustc": `rustc -vV`, "cargo": `cargo -V`}
    platform: dict[str, str]                    #: {"os", "kernel", "arch", "python"}
    reference: dict[str, str]                   #: {"image_digest","server_version","fingerprint_sha256"}
    corpus: dict[str, Optional[str]]            #: tree hashes: cases, expected, verifier,
                                                #: protected, regress_manifest, isolation_manifest
    seeds: list[int]
    started_unix: int
    finished_unix: int
    build: dict[str, Any]                       #: {"ok", "duration_s", "error_clusters", "log"}
    families: list[FamilyEvidence]
    totals: dict[str, int]                      #: sum of family counts (COUNT_KEYS)
    critical: dict[str, int]                    #: {"corruption": n, "crash": n}
    halt: dict[str, Any] = field(default_factory=dict)   #: halt.HaltDecision.to_json()
    resource: dict[str, Any] = field(default_factory=dict)  #: {"cpu_s", "wall_s", "max_rss_mb"}
    format: str = EVIDENCE_FORMAT

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> "EvidencePack":
        if d.get("format") != EVIDENCE_FORMAT:
            raise ValueError(f"bad evidence format {d.get('format')!r}")
        d = dict(d)
        d["families"] = [FamilyEvidence(**f) for f in d["families"]]
        return cls(**d)

    def save(self, path: Path) -> None:
        write_canonical_json(path, self.to_json())


# ---------------------------------------------------------------------------------------------
# Repair tickets (RFC 003 §7; SWARM-12). CONTRACT
# ---------------------------------------------------------------------------------------------

#: Severity weights for priority scoring (critical storage first).
SEVERITY_WEIGHT: dict[str, float] = {
    "CORRUPTION": 1000.0, "CRASH": 300.0, "HANG": 100.0, "BUILD": 200.0,
    "UNEXPECTED_ACCEPTANCE": 30.0, "MISMATCH": 20.0, "UNSUPPORTED": 10.0, "FLAKY": 5.0,
}


@dataclass
class RepairTicket:
    """A next-epoch DAG node. Field names are a superset of contracts/dag task nodes so
    pgarbor_factory.dag.validate_graph accepts {"tasks": [ticket.to_json(), ...]}.

    ``score = severity_weight * expected_unblocked * dependency_fanout * confidence / cost``
    (RFC 003 §7), except CORRUPTION/CRASH tickets of storage-owned files are always ordered
    first regardless of score (``critical`` True)."""

    id: str
    title: str
    root_cause: str
    severity: str
    expected_unblocked: int
    dependency_fanout: int
    confidence: float
    cost: float
    score: float
    critical: bool
    repro: list[str]                          #: case ids / regress tests / seeds
    expected_behavior: str
    actual_behavior: str
    owned_files: list[str]
    leases: list[str]
    epic: str = "repair"
    integration_dependencies: list[str] = field(default_factory=list)
    contract_dependencies: list[str] = field(default_factory=list)
    ownership_domain: str = ""
    owned_path_glob: str = ""
    deliverable: str = ""
    deferred_verification_family: str = ""
    state: str = "PLANNED"
    contract_version: str = "v1"
    acceptance: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


def dumps(obj: Any) -> str:
    """canonical_json re-export for factory modules."""
    return canonical_json(obj)


def env_flag(name: str) -> bool:
    """True when env var is "1"/"true"/"yes" (case-insensitive)."""
    return os.environ.get(name, "").strip().lower() in ("1", "true", "yes")
