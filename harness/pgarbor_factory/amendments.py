"""SWARM-09: interface drift reconciler — one canonical amendment per drift, fanned out to
affected leases; never silent per-worker redefinition.

Lease FAC-18.

Amendment files: ``contracts/amendments/<crate>-<NN>-<slug>.md`` (written by architects or by
``open_amendment``; the factory never edits contracts/ outside this API, and this API requires
PGARBOR_ORACLE_REVIEW=1 OR ``--architect`` mode env PGARBOR_ARCHITECT=1). Required header
lines (parsed with ``^(\\w[\\w ]*):\\s*(.*)$`` before the first blank line)::

    Status: PROPOSED | RATIFIED | REJECTED | APPLIED
    Crate: pgarbor-storage
    Affects: pgarbor-core, pgarbor-fault
    Leases: STO-03, CORE-02
    Ticket: AMEND-02-001     (optional)

then markdown sections ``## Problem``, ``## Proposed change``, ``## Affected crates``.

``scan(repo_root)`` -> list[Amendment]; ``drift_from_build(clusters, specs, epoch)`` turns
build ErrorClusters with root_hint "interface_drift" into Amendment proposals (deduplicated by
cluster key; existing PROPOSED amendments mentioning the same item are reused);
``affected_leases`` = union of the cluster's leases and leases owning files of ``Affects``
crates that reference the drifting item (grep the item name in their files).
``suspend`` marks every affected lease in the ledger with a history note
"suspended: <amendment>" and keeps them out of scheduler.plan_wave until the amendment is
RATIFIED (RFC 003 §11 "Shared contract drift").
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class Amendment:
    path: str
    status: str
    crate: str
    affects: list[str] = field(default_factory=list)
    leases: list[str] = field(default_factory=list)
    ticket: str = ""
    title: str = ""


def scan(repo_root: Path) -> list[Amendment]:
    raise NotImplementedError("FAC-18: scan amendments")


def drift_from_build(clusters: list[Any], lease_specs: list[Any], epoch: int) -> list[Amendment]:
    raise NotImplementedError("FAC-18: drift clusters to amendments")


def open_amendment(repo_root: Path, amendment: Amendment, problem: str, proposal: str) -> Path:
    raise NotImplementedError("FAC-18: write amendment file")


def suspended_leases(repo_root: Path) -> dict[str, str]:
    """lease_id -> amendment path for every non-RATIFIED/APPLIED amendment listing it."""
    raise NotImplementedError("FAC-18: suspended leases")
