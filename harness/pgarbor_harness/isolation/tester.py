"""ORACLE-06: isolationtester-compatible runner (output diffable against expected/*.out).

Lease HAR-20 (sonnet: concurrent sessions, wait detection, exact output formatting).

Re-implements src/test/isolation/isolationtester.c behavior (reference reading) in Python on
top of runner.session.Session (async start/poll/finish):

Per spec: print ``Parsed test spec with N sessions\\n``; per permutation:
``\\nstarting permutation: s1 s2 ...\\n``; run main setup blocks on the control connection,
per-session setups, then steps, then per-session teardowns, then main teardown (setup/teardown
result sets are printed with print_result_set; errors in setup abort the test: ``setup failed:
<msg>`` -> HARNESS-level failure of this spec).

Step output lines (exact):
  * ``step <name>: <sql>`` followed by `` <waiting ...>`` when the step is detected waiting,
    newline;
  * on later completion of a waiting step: ``step <name>: <... completed>\\n``;
  * result sets via ``print_result_set`` (PQprint with header, align, fieldSep "|"): see
    example below — column header centered/right/left exactly as PQprint does (libpq
    fe-print.c; numeric-looking columns are right aligned), separator line of '-' with '+'
    between columns, then rows, then ``(N row)``/``(N rows)`` and a blank line;
  * errors (isolationtester.c ~line 1023): ``f"{severity}:  {message_primary}\\n"`` — ONLY
    the severity and primary message (no DETAIL/HINT/CONTEXT); only when either field is
    missing, the whole PQresultErrorMessage text + newline;
  * NOTIFY messages received are printed after the step's results as
    ``NOTIFY "<channel>" with payload "<payload>" from <session name or pid>``;
  * notices: ``<session>: NOTICE:  <message>\\n`` printed when received.

Example (expected/read-write-unique.out)::

    step r2: SELECT * FROM test;
     i
    --
    42
    (1 row)

    step w2: INSERT INTO test VALUES (42); <waiting ...>
    step c1: COMMIT;
    step w2: <... completed>
    ERROR:  could not serialize access due to read/write dependencies among transactions

Wait detection (isolationtester.c line ~222): after launching a step, if not complete, ask the
control connection ``SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{<all other
session pids>}')`` with the step session's backend pid (``SELECT pg_backend_pid()`` at
session start). True -> waiting. Markers (``*``, ``<step>``, ``<step> notices N``) delay the
reported completion as described in the isolation README.

Targets without ``pg_isolation_test_session_is_blocked`` (detected once: the control query
fails with 42883 undefined_function): fall back to HEURISTIC detection — a step still running
after ``config.BLOCK_SETTLE_S`` is considered waiting. Under heuristic detection the spec's
Outcome is NEVER PASS: identical output -> BLOCKED(reason="heuristic_wait_detection");
output differing only in lines containing ``<waiting ...>`` or ``<... completed>`` -> FLAKY;
other differences -> MISMATCH.

Per-spec outcome with real detection: identical to any expected variant (regress.outdiff
rules) -> PASS; else MISMATCH (CRASH/HANG as in regress.runner). Step timeout: 2 x
config.STEP_TIMEOUT_S like isolationtester's 2 * PG_TEST_TIMEOUT_DEFAULT -> cancel -> HANG.

Each spec runs in a fresh database (target.provision) — isolationtester itself runs all
specs in one ``isolation_regression`` database; fresh databases are stricter and equivalent
because every spec's teardown drops its objects.
"""

from __future__ import annotations

import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from .. import config
from ..model import Endpoint, ErrorInfo, Notice, Outcome, StatementResult
from ..regress import outdiff
from ..runner.session import Session
from .spec_parser import (IsolationSpec, PermutationStep, SpecSyntaxError, all_permutations,
                          parse_schedule, parse_spec)


@dataclass
class IsolationResult:
    test: str
    outcome: Outcome
    reason: str
    output_path: Optional[str]
    diff_lines: int = 0
    wait_detection: str = "function"   #: "function" | "heuristic"
    elapsed_ms: int = 0


class IsolationAbort(RuntimeError):
    """The spec run stopped early (isolationtester would ``exit(1)``).

    ``kind``: "setup" (setup failed / connection failed / lock-wait query failed / bad
    permutation: harness-level), "hang" (step timeout), "conn_lost" (server connection
    dropped). ``output`` is everything printed before the abort, ``mode`` the wait detection
    mode in force.
    """

    def __init__(self, kind: str, message: str, output: str = "", mode: str = "function"):
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.output = output
        self.mode = mode


# ---------------------------------------------------------------------------------------------
# PQprint
# ---------------------------------------------------------------------------------------------

_NUMERIC_CHARS = frozenset("0123456789.Ee -")


def _blen(s: str) -> int:
    return len(s.encode("utf-8", "surrogateescape"))


def _ljust_b(s: str, width: int) -> str:
    n = _blen(s)
    return s + " " * (width - n) if width > n else s


def _rjust_b(s: str, width: int) -> str:
    n = _blen(s)
    return " " * (width - n) + s if width > n else s


def _text_of(v: Any) -> str:
    if v is None:
        return ""
    if isinstance(v, (bytes, bytearray)):
        return bytes(v).decode("utf-8", "replace")
    return str(v)


def print_result_set(res: StatementResult) -> str:
    """Render a rows result exactly like libpq PQprint(header=1, align=1, fieldSep="|").

    Follows fe-print.c with po = {header, align, fieldSep "|"} (not standard, not expanded):
    column widths are byte lengths, numeric-looking columns (only chars of "0123456789.Ee -",
    last char a digit, not starting with E/e) are right aligned, others left aligned; header
    names use the same alignment as their column (PQprint does not center). Empty/NULL values
    do not influence width or numeric detection. A result with no columns prints nothing.
    """
    ncols = len(res.columns)
    if ncols == 0:
        return ""
    names = [c.name for c in res.columns]
    field_max = [_blen(n) for n in names]
    not_num = [False] * ncols
    cells: list[list[str]] = []
    for row in res.rows:
        out_row: list[str] = []
        for j in range(ncols):
            v = _text_of(row[j]) if j < len(row) else ""
            plen = _blen(v)
            if plen < 1:
                out_row.append("")          # skipit: no width / numeric effect
                continue
            if not not_num[j]:
                ch = "0"
                for ch in v:
                    if ch not in _NUMERIC_CHARS:
                        not_num[j] = True
                        break
                if v[0] in "Ee" or not ("0" <= ch <= "9"):
                    not_num[j] = True
            if plen > field_max[j]:
                field_max[j] = plen
            out_row.append(v)
        cells.append(out_row)

    parts: list[str] = []
    border = "+".join("-" * m for m in field_max)
    hdr = "|".join(_ljust_b(n, field_max[j]) if not_num[j] else _rjust_b(n, field_max[j])
                   for j, n in enumerate(names))
    parts.append(hdr + "\n" + border + "\n")
    for out_row in cells:
        line = "|".join(
            _ljust_b(p, field_max[j]) if not_num[j] else _rjust_b(p, field_max[j])
            for j, p in enumerate(out_row))
        parts.append(line + "\n")
    n = len(res.rows)
    parts.append(f"({n} row{'' if n == 1 else 's'})\n\n")
    return "".join(parts)


# ---------------------------------------------------------------------------------------------
# Message formatting
# ---------------------------------------------------------------------------------------------


def _extra_lines(detail: Optional[str], hint: Optional[str]) -> str:
    s = ""
    if detail:
        s += f"DETAIL:  {detail}\n"
    if hint:
        s += f"HINT:  {hint}\n"
    return s


def _notice_text(n: Notice) -> str:
    """libpq default-verbosity rendering of a notice (context only shown for errors)."""
    return f"{n.severity}:  {n.message}\n" + _extra_lines(n.detail, n.hint)


def _error_message(e: ErrorInfo) -> str:
    """PQresultErrorMessage-like text (default verbosity)."""
    s = f"{e.severity}:  {e.message}\n" + _extra_lines(e.detail, e.hint)
    if e.context:
        s += f"CONTEXT:  {e.context}\n"
    return s


def _error_line(e: ErrorInfo) -> str:
    """isolationtester.c step error line: severity + primary message only."""
    if e.severity and e.message:
        return f"{e.severity}:  {e.message}\n"
    return _error_message(e) + "\n"


# ---------------------------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------------------------

_WAIT_SQL_HEAD = "SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{"


@dataclass
class _Blocker:
    kind: str                 #: "once" | "step" | "notices"
    session: int = -1         #: index of the blocking step's session
    step: str = ""
    num: int = 0
    target: int = 0


@dataclass
class _Active:
    step_name: str
    sql: str
    session: int
    blockers: list[_Blocker] = field(default_factory=list)
    launched_at: float = 0.0


class _Run:
    def __init__(self, spec: IsolationSpec, endpoint: Endpoint):
        self.spec = spec
        self.endpoint = endpoint
        self.out: list[str] = []
        self.mode = "function"
        self.max_wait = float(config.STEP_TIMEOUT_S)
        self.ctrl: Optional[Session] = None
        self.sess: list[Session] = []         # sess[i] belongs to spec.sessions[i]
        self.pid_by_session: list[str] = []
        self.active: list[Optional[_Active]] = []
        self.total_notices: list[int] = []
        self.step_info = {st.name: (st.sql, i) for i, s in enumerate(spec.sessions)
                          for st in s.steps}
        self.wait_sql = ""

    # -- helpers ---------------------------------------------------------------------------
    def emit(self, s: str) -> None:
        self.out.append(s)

    def abort(self, kind: str, msg: str) -> IsolationAbort:
        return IsolationAbort(kind, msg, "".join(self.out), self.mode)

    def _check(self, name: str, o: Any) -> None:
        if o.connection_lost:
            raise self.abort("conn_lost", f"connection lost on {name}")
        if o.timed_out:
            raise self.abort("hang", f"step on {name} timed out")

    def _backend_pid(self, s: Session, label: str) -> str:
        o = s.run_simple("SELECT pg_backend_pid()")
        self._check(label, o)
        if not o.results or o.results[-1].kind != "rows" or not o.results[-1].rows:
            raise self.abort("setup", f"could not get backend pid of {label}")
        return _text_of(o.results[-1].rows[0][0])

    # -- connections -----------------------------------------------------------------------
    def connect_all(self) -> None:
        try:
            self.ctrl = Session(self.endpoint, "control connection", self.max_wait)
            self.ctrl.connect()
            for s in self.spec.sessions:
                x = Session(self.endpoint, s.name, self.max_wait)
                self.sess.append(x)
                x.connect()
        except Exception as e:  # connection failure
            raise self.abort("setup", f"Connection failed: {e}")
        n = len(self.spec.sessions)
        self.active = [None] * n
        self.total_notices = [0] * n
        self._backend_pid(self.ctrl, "control connection")
        self.pid_by_session = [self._backend_pid(x, self.spec.sessions[i].name)
                               for i, x in enumerate(self.sess)]
        if not self.pid_by_session:
            return
        self.wait_sql = _WAIT_SQL_HEAD + ",".join(self.pid_by_session) + "}')"
        # Probe once for pg_isolation_test_session_is_blocked.
        o = self.ctrl.run_exec_params(self.wait_sql, [self.pid_by_session[0]], [], [], "text")
        self._check("control connection", o)
        r = o.results[-1] if o.results else None
        if r is not None and r.kind == "error" and r.error is not None:
            if r.error.sqlstate == "42883":
                self.mode = "heuristic"
            else:
                raise self.abort("setup",
                                 "prepare of lock wait query failed: " + _error_message(r.error))
        elif r is None or r.kind != "rows":
            raise self.abort("setup", "prepare of lock wait query failed")

    def close_all(self) -> None:
        for s in [self.ctrl, *self.sess]:
            if s is not None:
                try:
                    s.close()
                except Exception:
                    pass

    # -- blocked check ---------------------------------------------------------------------
    def _is_blocked(self, act: _Active) -> bool:
        if self.mode == "heuristic":
            return time.monotonic() - act.launched_at >= config.BLOCK_SETTLE_S
        assert self.ctrl is not None
        o = self.ctrl.run_exec_params(self.wait_sql, [self.pid_by_session[act.session]],
                                      [], [], "text")
        self._check("control connection", o)
        r = o.results[-1] if o.results else None
        if r is None or r.kind != "rows" or len(r.rows) != 1:
            msg = _error_message(r.error) if r is not None and r.error else ""
            raise self.abort("setup", "lock wait query failed: " + msg)
        return _text_of(r.rows[0][0]).startswith("t")

    # -- blockers --------------------------------------------------------------------------
    def _has_blocker(self, act: _Active) -> bool:
        for b in act.blockers:
            if b.kind == "step":
                a = self.active[b.session]
                if a is not None and a.step_name == b.step:
                    return True
            elif b.kind == "notices":
                if self.total_notices[b.session] < b.target:
                    return True
        return False

    def _link(self, steps: list[PermutationStep]) -> list[_Active]:
        names = {p.name for p in steps}
        acts: list[_Active] = []
        for p in steps:
            if p.name not in self.step_info:
                raise self.abort("setup", f'undefined step "{p.name}" specified in permutation')
            sql, sidx = self.step_info[p.name]
            a = _Active(p.name, sql, sidx)
            for m in p.markers:
                if m.kind == "star":
                    a.blockers.append(_Blocker("once"))
                    continue
                bname = m.step or ""
                if bname not in names:
                    raise self.abort("setup", f'undefined blocking step "{bname}" referenced '
                                              f'in permutation step "{p.name}"')
                bsess = self.step_info[bname][1]
                if bsess == sidx:
                    raise self.abort("setup",
                                     f'permutation step "{p.name}" cannot block on its own '
                                     f'session')
                if m.kind == "step":
                    a.blockers.append(_Blocker("step", bsess, bname))
                else:
                    a.blockers.append(_Blocker("notices", bsess, bname, m.count))
            acts.append(a)
        return acts

    # -- setup / teardown ------------------------------------------------------------------
    def _run_block(self, s: Session, label: str, sql: str, what: str, fatal: bool,
                   show_notices: bool) -> None:
        o = s.run_simple(sql)
        self._check(label, o)
        if show_notices:
            for n in o.notices:
                self.emit(f"{label}: " + _notice_text(n))
        r = o.results[-1] if o.results else None   # PQexec keeps only the last result
        if r is not None and r.kind == "rows":
            self.emit(print_result_set(r))
        elif r is not None and r.kind == "command":
            pass
        elif fatal:
            msg = _error_message(r.error) if r is not None and r.error else ""
            raise self.abort("setup", f"{what} failed: {msg}")

    # -- completion ------------------------------------------------------------------------
    def try_complete(self, act: _Active, nonblock: bool, retry: bool) -> bool:
        """Port of try_complete_step. True = NOT completed."""
        if not retry and any(b.kind == "once" for b in act.blockers):
            self.emit(f"step {act.step_name}: {act.sql} <waiting ...>\n")
            return True
        sess = self.sess[act.session]
        start = time.monotonic()
        canceled = False
        while not sess.poll_done(0.01):
            if nonblock and self._is_blocked(act):
                if sess.poll_done(0):
                    break
                if not retry:
                    self.emit(f"step {act.step_name}: {act.sql} <waiting ...>\n")
                return True
            td = time.monotonic() - start
            if td > self.max_wait and not canceled:
                sess.cancel()
                self.emit(f"isolationtester: canceling step {act.step_name} after "
                          f"{int(td)} seconds\n")
                canceled = True
            if td > 2 * self.max_wait:
                raise self.abort("hang", f"step {act.step_name} timed out after {int(td)} "
                                         f"seconds")
        if self._has_blocker(act):
            if not retry:
                self.emit(f"step {act.step_name}: {act.sql} <waiting ...>\n")
            return True
        o = sess.finish_pending()
        self.total_notices[act.session] += len(o.notices)
        sname = self.spec.sessions[act.session].name
        for n in o.notices:
            self.emit(f"{sname}: " + _notice_text(n))
        if retry:
            self.emit(f"step {act.step_name}: <... completed>\n")
        else:
            self.emit(f"step {act.step_name}: {act.sql}\n")
        for r in o.results:
            if r.kind in ("command", "empty"):
                continue
            if r.kind == "rows":
                self.emit(print_result_set(r))
            elif r.kind == "error" and r.error is not None:
                self.emit(_error_line(r.error))
            else:
                status = "PGRES_COPY_OUT" if r.kind == "copy_out" else "PGRES_BAD_RESPONSE"
                self.emit(f"unexpected result status: {status}\n")
        take = getattr(sess, "take_notifies", None)   # optional hook: [(pid, channel, payload)]
        if callable(take):
            for pid, chan, payload in take():
                sender = f"PID {pid}"
                for i, p in enumerate(self.pid_by_session):
                    if str(pid) == p:
                        sender = self.spec.sessions[i].name
                        break
                self.emit(f'{sname}: NOTIFY "{chan}" with payload "{payload}" from {sender}\n')
        self.active[act.session] = None
        self._check(sname, o)
        return False

    def try_complete_steps(self, waiting: list[_Active], nonblock: bool) -> None:
        """Port of try_complete_steps (always STEP_RETRY); mutates ``waiting``."""
        while True:
            old = len(waiting)
            have_blocker = False
            notices_before = sum(self.total_notices)
            w = 0
            while w < len(waiting):
                if self.try_complete(waiting[w], nonblock, True):
                    if waiting[w].blockers:
                        have_blocker = True
                    w += 1
                else:
                    del waiting[w]
            progressed = len(waiting) < old or sum(self.total_notices) != notices_before
            if not (have_blocker and progressed):
                return

    # -- permutation -----------------------------------------------------------------------
    def run_permutation(self, steps: list[PermutationStep]) -> None:
        assert self.ctrl is not None
        acts = self._link(steps)
        self.emit("\nstarting permutation:" + "".join(f" {a.step_name}" for a in acts) + "\n")
        for sql in self.spec.setup:
            self._run_block(self.ctrl, "control connection", sql, "setup", True, False)
        for i, s in enumerate(self.spec.sessions):
            if s.setup:
                self._run_block(self.sess[i], s.name, s.setup,
                                f"setup of session {s.name}", True, True)
        waiting: list[_Active] = []
        for act in acts:
            t0 = time.monotonic()
            while True:
                old = self.active[act.session]
                if old is None:
                    break
                if not self.try_complete(old, False, True) and old in waiting:
                    waiting.remove(old)
                self.try_complete_steps(waiting, True)
                cur = self.active[act.session]
                if cur is not None and time.monotonic() - t0 > 2 * self.max_wait:
                    raise self.abort("hang", f"step {cur.step_name} timed out after "
                                             f"{int(time.monotonic() - t0)} seconds")
            self.sess[act.session].start("sql", {"sql": act.sql})
            act.launched_at = time.monotonic()
            self.active[act.session] = act
            for b in act.blockers:
                if b.kind == "notices":
                    b.target = b.num + self.total_notices[b.session]
            mustwait = self.try_complete(act, True, False)
            self.try_complete_steps(waiting, True)
            if mustwait:
                waiting.append(act)
        self.try_complete_steps(waiting, False)
        if waiting:
            raise self.abort("setup", "failed to complete permutation due to mutually-blocking "
                                      "steps")
        for i, s in enumerate(self.spec.sessions):
            if s.teardown:
                self._run_block(self.sess[i], s.name, s.teardown,
                                f"teardown of session {s.name}", False, True)
        if self.spec.teardown:
            self._run_block(self.ctrl, "control connection", self.spec.teardown, "teardown",
                            False, False)


def run_spec(spec: IsolationSpec, endpoint: Endpoint) -> tuple[str, str]:
    """Run all permutations; return (output text, wait_detection mode).

    Raises IsolationAbort (carrying the partial output and mode) when isolationtester would
    have exited early.
    """
    run = _Run(spec, endpoint)
    try:
        run.emit(f"Parsed test spec with {len(spec.sessions)} sessions\n")
        run.connect_all()
        perms = spec.permutations or all_permutations(spec)
        for p in perms:
            run.run_permutation(p)
    except IsolationAbort as e:
        e.output = "".join(run.out)
        e.mode = run.mode
        raise
    finally:
        run.close_all()
    return "".join(run.out), run.mode


# ---------------------------------------------------------------------------------------------
# Suite
# ---------------------------------------------------------------------------------------------

_WAIT_MARKERS = ("<waiting ...>", "<... completed>")


def _without_wait_markers(text: str) -> list[str]:
    """Output lines with wait markers removed: ``<waiting ...>`` suffixes are stripped and
    ``<... completed>`` lines are dropped, so a wait-only difference compares equal."""
    out: list[str] = []
    for ln in text.splitlines():
        if _WAIT_MARKERS[1] in ln:
            continue
        out.append(ln.replace(" " + _WAIT_MARKERS[0], "").replace(_WAIT_MARKERS[0], ""))
    return out


def _only_wait_diffs(candidates: list[Path], actual: str) -> bool:
    """True when some candidate equals the actual output once wait markers are ignored."""
    act = outdiff.strip_trailing_cr(actual.encode("utf-8", "surrogateescape")).decode(
        "utf-8", "surrogateescape")
    want = _without_wait_markers(act)
    for c in candidates:
        try:
            exp = outdiff.strip_trailing_cr(c.read_bytes()).decode("utf-8", "surrogateescape")
        except OSError:
            continue
        if _without_wait_markers(exp) == want:
            return True
    return False


def _candidates(isolation_dir: Path, name: str) -> list[Path]:
    base = isolation_dir / "expected" / f"{name}.out"
    cands = [base] if base.exists() else []
    for n in range(10):
        p = isolation_dir / "expected" / f"{name}_{n}.out"
        if p.exists():
            cands.append(p)
    return cands


def _run_one(name: str, isolation_dir: Path, target: Any, out_dir: Path) -> IsolationResult:
    t0 = time.monotonic()

    def done(outcome: Outcome, reason: str, path: Optional[Path] = None, diff: int = 0,
             mode: str = "function") -> IsolationResult:
        return IsolationResult(name, outcome, reason, str(path) if path else None, diff, mode,
                               int((time.monotonic() - t0) * 1000))

    spec_path = isolation_dir / "specs" / f"{name}.spec"
    try:
        spec = parse_spec(spec_path.read_text(encoding="utf-8"), str(spec_path))
    except (SpecSyntaxError, OSError, UnicodeDecodeError) as e:
        return done(Outcome.HARNESS_ERROR, f"spec parse error: {e}")
    try:
        env = target.provision(name)
    except PermissionError as e:
        return done(Outcome.BLOCKED, f"environment_permission: {e}")
    except Exception as e:
        return done(Outcome.CRASH, f"target_start: {e}")
    text = ""
    mode = "function"
    outcome: Optional[Outcome] = None
    reason = ""
    try:
        try:
            text, mode = run_spec(spec, env.endpoint)
        except IsolationAbort as e:
            text, mode = e.output, e.mode
            if e.kind == "hang":
                outcome, reason = Outcome.HANG, e.message
            elif e.kind == "conn_lost":
                alive = True
                try:
                    alive = bool(target.is_alive(env))
                except Exception:
                    pass
                outcome = Outcome.MISMATCH if alive else Outcome.CRASH
                reason = e.message
            else:
                outcome, reason = Outcome.HARNESS_ERROR, e.message
        except Exception as e:  # unexpected harness/driver failure
            outcome, reason = Outcome.HARNESS_ERROR, f"{type(e).__name__}: {e}"
    finally:
        try:
            target.release(env)
        except Exception:
            pass
    res_path = out_dir / "results" / f"{name}.out"
    res_path.parent.mkdir(parents=True, exist_ok=True)
    res_path.write_text(text, encoding="utf-8", newline="\n", errors="surrogateescape")
    if outcome is not None:
        return done(outcome, reason, res_path, 0, mode)
    cands = _candidates(isolation_dir, name)
    if not cands:
        return done(Outcome.HARNESS_ERROR, "no expected output", res_path, 0, mode)
    d = outdiff.compare_output(res_path, cands)
    if d.identical:
        if mode == "heuristic":
            return done(Outcome.BLOCKED, "heuristic_wait_detection", res_path, 0, mode)
        return done(Outcome.PASS, "", res_path, 0, mode)
    if mode == "heuristic" and _only_wait_diffs(cands, text):
        return done(Outcome.FLAKY, "heuristic_wait_detection: only wait lines differ", res_path,
                    d.diff_lines, mode)
    return done(Outcome.MISMATCH, "output differs from expected", res_path, d.diff_lines, mode)


def run_suite(isolation_dir: Path, target: object, out_dir: Path,
              only: Optional[list[str]] = None, jobs: int = 4) -> list[IsolationResult]:
    """Parse the schedule, run each spec in a fresh env of ``target`` (runner.targets.Target),
    write ``out_dir/results/<test>.out``, compare with expected variants (regress.outdiff),
    classify as in the module docstring. Unparseable spec -> HARNESS_ERROR."""
    sched = isolation_dir / "isolation_schedule"
    names = parse_schedule(sched.read_text(encoding="utf-8"))
    if only is not None:
        wanted = set(only)
        unknown = sorted(wanted - set(names))
        if unknown:
            raise ValueError("unknown isolation spec(s): " + ", ".join(unknown))
        names = [n for n in names if n in wanted]
    if not names:
        raise ValueError("isolation selection contains no specs")
    out_dir.mkdir(parents=True, exist_ok=True)
    if jobs <= 1 or len(names) <= 1:
        return [_run_one(n, isolation_dir, target, out_dir) for n in names]
    with ThreadPoolExecutor(max_workers=jobs) as ex:
        return list(ex.map(lambda n: _run_one(n, isolation_dir, target, out_dir), names))
