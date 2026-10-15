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

import threading
import time
from typing import Any, Optional

from .. import config
from ..cases.model import SETUP_SESSION, Case, Step
from ..model import StepObservation, Transcript
from . import session as session_mod
from .session import StepOutput
from .targets import CaseEnv, Target

_LOG_TAILS: dict[str, str] = {}
_LOG_LOCK = threading.Lock()


class _Run:
    """Mutable state of one execute_case call."""

    def __init__(self, case: Case, target: Target, env: CaseEnv, step_timeout_s: float,
                 case_timeout_s: float):
        self.case = case
        self.target = target
        self.env = env
        self.step_timeout_s = step_timeout_s
        self.deadline = time.monotonic() + case_timeout_s
        self.sessions: dict[str, Any] = {}
        self.connected: set[str] = set()
        self.stale: set[str] = set()   # sessions to reconnect lazily
        self.stopped = False

    def get(self, name: str) -> Any:
        s = self.sessions.get(name)
        if s is None:
            s = session_mod.Session(self.env.endpoint, name, self.step_timeout_s)
            self.sessions[name] = s
        if name not in self.connected:
            s.connect()
            self.connected.add(name)
            self.stale.discard(name)
        elif name in self.stale:
            s.reconnect()
            self.stale.discard(name)
        return s

    def close_all(self) -> None:
        for s in self.sessions.values():
            try:
                s.close()
            except Exception:  # noqa: BLE001 - Session.close never raises; be defensive
                pass
        self.connected.clear()


def execute_case(case: Case, target: Target, endpoint_label: str = "target", attempt: int = 1,
                 keep: bool = False, step_timeout_s: float = config.STEP_TIMEOUT_S,
                 case_timeout_s: float = config.CASE_TIMEOUT_S) -> Transcript:  # CONTRACT
    """Run ``case`` on a freshly provisioned env of ``target`` and return its Transcript with
    ``endpoint=endpoint_label`` ("reference" when recording, "target" otherwise)."""
    transcript = Transcript(case_id=case.id, endpoint=endpoint_label)
    env = target.provision(case.id, attempt)
    run = _Run(case, target, env, step_timeout_s, case_timeout_s)
    try:
        try:
            if case.setup:
                _run_setup(run, transcript)
            if not run.stopped:
                for step in case.steps:
                    obs = _run_step(run, step)
                    transcript.steps.append(obs)
                    if run.stopped:
                        break
        finally:
            if run.stopped:
                _stash_log(target, env, case.id)
            run.close_all()
    finally:
        try:
            target.release(env, keep)
        except Exception as e:  # noqa: BLE001 - release failure must not lose the transcript
            msg = f"release failed: {type(e).__name__}: {e}"
            last = (transcript.steps or transcript.setup or [None])[-1]
            if last is not None and last.harness_error is None:
                last.harness_error = msg
    return transcript


def _stash_log(target: Target, env: CaseEnv, case_id: str) -> None:
    try:
        tail = target.server_log_tail(env, 40)
    except Exception:  # noqa: BLE001
        return
    if tail is not None:
        with _LOG_LOCK:
            _LOG_TAILS[case_id] = tail


def server_log_excerpt(target: Target, case_id: str) -> Optional[str]:  # CONTRACT
    """Helper for CRASH reports: last 40 log lines of the most recent env of case_id (or None
    if the target keeps no logs). Executors stash the tail on the Transcript-producing call;
    implement with a module-level dict guarded by a lock: case_id -> last tail."""
    with _LOG_LOCK:
        tail = _LOG_TAILS.get(case_id)
    if not tail:
        return None
    return "\n".join(tail.splitlines()[-40:])


# -- setup -------------------------------------------------------------------------------------
def _run_setup(run: _Run, transcript: Transcript) -> None:
    obs = StepObservation(index=0, session=SETUP_SESSION, kind="sql")
    transcript.setup.append(obs)
    t0 = time.monotonic()
    try:
        sess = run.get(SETUP_SESSION)
        out = sess.run_simple(run.case.setup)
        _copy_output(obs, out)
        _apply_failure(run, obs, out)
    except ConnectionError:
        obs.connection_lost = True
        _on_connection_lost(run)
        if not run.stopped:
            run.stopped = True   # no point running steps without a completed setup
    except Exception as e:  # noqa: BLE001
        obs.harness_error = f"{type(e).__name__}: {e}"
        run.stopped = True
    obs.elapsed_ms = int((time.monotonic() - t0) * 1000)
    # setup must hold no locks/snapshots
    sess = run.sessions.get(SETUP_SESSION)
    if sess is not None:
        try:
            sess.close()
        except Exception:  # noqa: BLE001
            pass
        run.connected.discard(SETUP_SESSION)


# -- steps -------------------------------------------------------------------------------------
def _copy_output(obs: StepObservation, out: StepOutput) -> None:
    obs.results = list(out.results)
    obs.notices = list(out.notices)
    obs.txn_status = out.txn_status
    obs.timed_out = out.timed_out
    obs.connection_lost = out.connection_lost
    obs.elapsed_ms = out.elapsed_ms


def _on_connection_lost(run: _Run) -> None:
    """Crash (stop) vs server-initiated disconnect (continue)."""
    try:
        alive = bool(run.target.is_alive(run.env))
    except Exception:  # noqa: BLE001
        alive = False
    panicked = False
    if alive:
        try:
            panicked = "panicked at" in (run.target.server_log_tail(run.env, 40) or "")
        except Exception:  # noqa: BLE001
            panicked = False
    if not alive or panicked:
        run.stopped = True


def _apply_failure(run: _Run, obs: StepObservation, out: StepOutput) -> None:
    if out.timed_out:
        run.stopped = True
    elif out.connection_lost:
        _on_connection_lost(run)
        if not run.stopped and obs.session:
            run.stale.add(obs.session)


def _run_step(run: _Run, step: Step) -> StepObservation:
    obs = StepObservation(index=step.index, session=step.session, kind=step.kind)
    t0 = time.monotonic()
    if t0 > run.deadline:
        obs.timed_out = True
        run.stopped = True
        return obs
    try:
        if step.kind == "restart":
            _do_restart(run, step, obs)
        elif step.kind == "reconnect":
            _do_reconnect(run, step, obs)
        elif step.kind == "wait":
            sess = run.get(step.session)
            out = sess.finish_pending()
            _copy_output(obs, out)
            _apply_failure(run, obs, out)
        elif step.is_async:
            _do_async(run, step, obs)
        else:
            sess = run.get(step.session)
            out = _dispatch_sync(sess, step)
            _copy_output(obs, out)
            _apply_failure(run, obs, out)
    except ConnectionError:
        obs.connection_lost = True
        _on_connection_lost(run)
        if not run.stopped:
            run.stale.add(step.session)
    except Exception as e:  # noqa: BLE001 - programming errors become harness_error
        obs.harness_error = f"{type(e).__name__}: {e}"
        run.stopped = True
    if not run.stopped and time.monotonic() > run.deadline:
        # whole-case budget exhausted: treat the current step as timed out
        obs.timed_out = True
        run.stopped = True
    obs.elapsed_ms = int((time.monotonic() - t0) * 1000)
    return obs


def _dispatch_sync(sess: Any, step: Step) -> StepOutput:
    p = step.payload
    k = step.kind
    if k == "sql":
        return sess.run_simple(p["sql"])
    if k == "parse":
        return sess.run_prepare(p.get("name", ""), p["sql"], p.get("param_types", []))
    if k == "describe":
        return sess.run_describe(p["statement"])
    if k == "execute":
        return sess.run_execute(p["statement"], p.get("params", []),
                                p.get("param_formats", []), p.get("result_format", "text"))
    if k == "exec_params":
        return sess.run_exec_params(p["sql"], p.get("params", []), p.get("param_types", []),
                                    p.get("param_formats", []), p.get("result_format", "text"))
    if k == "close":
        return sess.run_close(p["statement"])
    raise ValueError(f"unknown step kind {k!r}")


def _do_async(run: _Run, step: Step, obs: StepObservation) -> None:
    sess = run.get(step.session)
    sess.start(step.kind, step.payload)
    done = sess.poll_done(config.BLOCK_SETTLE_S)
    obs.results = []
    obs.txn_status = "A"
    obs.blocked_observed = not done


def _do_reconnect(run: _Run, step: Step, obs: StepObservation) -> None:
    sess = run.sessions.get(step.session)
    if sess is None:
        sess = session_mod.Session(run.env.endpoint, step.session, run.step_timeout_s)
        run.sessions[step.session] = sess
    sess.reconnect()
    run.connected.add(step.session)
    run.stale.discard(step.session)
    obs.results = []
    obs.txn_status = sess.txn_status


def _do_restart(run: _Run, step: Step, obs: StepObservation) -> None:
    obs.session = ""
    for name, sess in run.sessions.items():
        if name in run.connected and getattr(sess, "pending", False):
            try:
                sess.finish_pending()
            except Exception:  # noqa: BLE001
                pass
            obs.harness_error = (f"restart with pending async step on session {name!r} "
                                 "(results discarded)")
    run.target.restart(run.env, step.payload.get("mode", "clean"))
    lost = False
    for name, sess in run.sessions.items():
        if name in run.connected:
            try:
                sess.reconnect()
                run.stale.discard(name)
            except ConnectionError:
                lost = True
                run.stale.add(name)
    if lost:
        obs.connection_lost = True
        _on_connection_lost(run)
    obs.results = []
    obs.txn_status = "I"
    if obs.harness_error:
        run.stopped = True
