#!/usr/bin/env python3
"""SWARM-01 compatibility shim: the implementation moved to harness/pgarbor_factory/dag.py.

Usage (unchanged): python3 harness/factory/dag.py validate contracts/dag/task_graph.v1.json [--leases contracts/leases/*.json]
Exit 0 = valid; 1 = invalid (errors printed as JSON lines).
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from pgarbor_factory.dag import (  # noqa: E402,F401
    REQUIRED, STATES, DagError, find_cycle, main, validate_graph, validate_leases,
)

if __name__ == "__main__":
    sys.exit(main(sys.argv))
