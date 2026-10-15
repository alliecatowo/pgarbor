"""ORACLE-03: execute one Case against one provisioned endpoint -> model.Transcript.

Lease HAR-10 (sonnet: multi-session interleaving, async/blocked detection, crash handling).

Algorithm (``execute_case``)
----------------------------
1. ``env = target.provision(case.id, attempt)``; always ``target.release(env, keep)`` in a
   ``finally``.
2. Sessions: a ``runner.session.Session`` per name in ``case.sessions`` plus ``"setup"``;
   connected lazily on first use with ``step_timeout_s``.
3. Setup: if ``case.setup`` -> ``setup`` session ``run_simple(case.setup)`` ->
   ``StepObservation(index=0, session="setup", kind="sql")`` into ``Transcript.setup``; then
   close the setup session (so it holds no locks/snapshots).
4. Steps in order; for step i produce exactly one ``StepObservation(index=i, ...)``:
   * ``sql``/``parse``/``describe``/``execute``/``exec_params``/``close``: call the matching
     ``Session.run_*``; copy StepOutput into the observation.
   * async (``is_async``): ``Session.start(kind, payload)``; then
     ``done = session.poll_done(config.BLOCK_SETTLE_S)``; observation has ``results=[]``,
     ``txn_status="A"`` and ``blocked_observed = not done``. Results are collected later.
   * ``wait``: ``session.finish_pending()``; the observation carries those results/notices.
   * ``reconnect``: ``session.reconnect()``; empty results; txn_status after reconnect ("I").
   * ``restart``: every session with a pending async step is finished first (results are
     DISCARDED and the restart observation gets ``harness_error`` if any was pending — the
     validator forbids it, so this is defensive); ``target.restart(env, mode)``; all sessions
     ``reconnect()``; observation kind "restart", session "".
5. ``txn_status`` = the step session's ``txn_status`` right after the step.
6. Failure handling:
   * StepOutput.timed_out -> record, then stop executing further steps (remaining steps are
     NOT observed: the transcript is shorter; comparator reports ``timeout`` at that step).
   * StepOutput.connection_lost -> if ``target.is_alive(env)`` is False or the server log tail
     contains ``panicked at`` -> stop (CRASH will be classified); else (server-initiated
     disconnect, e.g. FATAL from ``pg_terminate_backend``) mark the session to reconnect
     lazily and continue.
   * any Python exception from the harness -> ``harness_error=f"{type(e).__name__}: {e}"``
     on that step, stop.
   * whole-case wall clock > ``config.CASE_TIMEOUT_S`` -> treat the current step as timed out.
7. Close all sessions (``Session.close`` never raises).

``elapsed_ms`` is measured with ``time.monotonic()`` per step. Deterministic: no randomness;
``case.seed`` is not used by the executor (generators use it).
"""

from __future__ import annotations

from typing import Optional

from .. import config
from ..cases.model import Case
from ..model import Transcript
from .targets import Target


def execute_case(case: Case, target: Target, endpoint_label: str = "target", attempt: int = 1,
                 keep: bool = False, step_timeout_s: float = config.STEP_TIMEOUT_S,
                 case_timeout_s: float = config.CASE_TIMEOUT_S) -> Transcript:
    """Run ``case`` on a freshly provisioned env of ``target`` and return its Transcript with
    ``endpoint=endpoint_label`` ("reference" when recording, "target" otherwise)."""
    raise NotImplementedError("HAR-10: case executor")


def server_log_excerpt(target: Target, case_id: str) -> Optional[str]:
    """Helper for CRASH reports: last 40 log lines of the most recent env of case_id (or None
    if the target keeps no logs). Executors stash the tail on the Transcript-producing call;
    implement with a module-level dict guarded by a lock: case_id -> last tail."""
    raise NotImplementedError("HAR-10: server log excerpt")
