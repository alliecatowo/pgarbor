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

from pathlib import Path


def build_packet(repo_root: Path, lease_id: str, max_chars: int = 120_000) -> str:
    raise NotImplementedError("FAC-16: build lease context packet")


def extract_signatures(path: Path) -> str:
    """Public-signature summary of a .rs or .py file (rules in section 5)."""
    raise NotImplementedError("FAC-16: signature extraction")


def write_packet(repo_root: Path, lease_id: str) -> Path:
    raise NotImplementedError("FAC-16: write packet")
