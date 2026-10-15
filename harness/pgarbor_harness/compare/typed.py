"""ORACLE-04: exact typed comparison of a target Transcript against the reference Transcript.

Lease HAR-13.

``compare_transcripts(case, reference, target)`` returns ``list[model.Diff]`` in transcript
order (setup first, then steps by index; inside a step: step-level diffs, then results by
index, then notices). Empty list == identical behavior. The reference is already normalized
(recorded that way); the function deep-copies the target and applies
compare.normalize.apply_case_normalizers to the copy (and to a copy of the reference again —
normalizers are idempotent).

What is compared, exactly (anything not listed is NOT compared: elapsed_ms only):

Step level (path ``steps[i]`` / ``setup[i]``):
  * target step missing (transcript shorter) -> ``missing_step``
  * ``timed_out`` differs -> ``timeout``; ``connection_lost`` differs -> ``connection``
  * ``blocked_observed`` differs -> ``blocked``
  * ``txn_status`` differs -> ``txn_status`` (I/T/E matter: a failed transaction block must be
    reported as E exactly like PG)
  * ``len(results)`` differs -> ``result_count`` (then compare pairwise up to the min)
Result level (path ``steps[i].results[k]``):
  * ``kind`` differs -> ``result_kind`` (reference/target = the kinds; note = both sqlstates
    if any side is an error). No further comparison of that result.
  * error results: ``sqlstate`` -> ``sqlstate`` (ALWAYS compared); ``severity`` ->
    ``error_field``; ``message`` -> ``error_message`` (only if step.compare_message);
    ``detail``, ``hint``, ``context`` -> ``error_field`` (only if step.compare_message);
    ``position``, ``internal_position``, ``internal_query``, ``schema_name``, ``table_name``,
    ``column_name``, ``datatype_name``, ``constraint_name`` -> ``error_field`` (always).
  * columns: count -> ``column_count``; per column j (path ``columns[j]``): ``name`` ->
    ``column_name``; ``type_oid`` -> ``column_type``; ``typmod`` -> ``column_typmod``;
    ``format`` -> ``column_format``; ``table_oid``/``column_attnum`` -> ``column_origin``.
  * ``command_tag`` -> ``command_tag`` (exact text: ``INSERT 0 1`` != ``INSERT 1``).
  * ``param_type_oids`` -> ``param_types``.
  * rows (kinds rows/copy_out): order mode = ``resolve_order(case, step_index, result_index)``.
      ordered: equal lists -> ok; same multiset but different order -> ``row_order``;
        otherwise ``row_count`` if lengths differ else ``value`` at the first differing
        ``rows[r][c]`` (reference value, target value).
      multiset: compare ``collections.Counter(tuple(row))``; differences -> ``row_multiset``
        with reference = rows only in reference (sorted by a total key: None < bytes < str,
        then by value), target = rows only in target (same sort), each truncated to 20.
    Value equality is exact: ``None`` != ``""``; ``"1.0"`` != ``"1"``; str != bytes.
Notices (path ``steps[i].notices``): compare as ordered lists of
  (severity, sqlstate, message if compare_message, detail if compare_message, hint if
  compare_message, context if compare_message) -> one ``notice`` diff for the first mismatch
  (or count mismatch).

Order resolution (``resolve_order``): explicit ``ordered``/``multiset`` win. ``auto``:
  * sql step: statements = sqltext.split_statements(sql); if len(statements) == number of
    reference results, use has_top_level_order_by(statements[k].text) for result k;
    otherwise "ordered" if any statement has a top-level ORDER BY else "multiset";
  * execute step: SQL of the most recent ``parse`` with that statement name on the same
    session before this step; exec_params: its sql; wait: the matching async step's sql
    (and that step's ``order`` if explicit);
  * describe/parse/close/reconnect/restart: no rows; irrelevant.

Implementation notes (not in the frozen spec): setup steps have no per-step order or
compare_message, so they are compared "ordered" with messages; an unresolvable ``auto``
(e.g. execute of a statement never parsed in the case) is also compared "ordered" (the
strict direction); extra target steps beyond the reference are reported as ``missing_step``
with a note.
"""

from __future__ import annotations

import collections
import copy
from typing import Any

from .. import sqltext
from ..cases.model import Case, Step
from ..model import ColumnDesc, Diff, ErrorInfo, Notice, StatementResult, StepObservation
from ..model import Transcript
from .normalize import apply_case_normalizers

#: CONTRACT: every Diff.kind the comparator can emit.
DIFF_KINDS: tuple[str, ...] = (
    "missing_step", "timeout", "connection", "blocked", "txn_status", "result_count",
    "result_kind", "sqlstate", "error_message", "error_field", "column_count", "column_name",
    "column_type", "column_typmod", "column_format", "column_origin", "command_tag",
    "param_types", "row_order", "row_count", "value", "row_multiset", "notice",
)

_MULTISET_LIMIT = 20
_RENDER_VALUE_LIMIT = 200


def _auto_for_sql(sql: str | None, result_index: int, n_results: int) -> str:
    """Auto order mode from the SQL text of a sql / execute / exec_params step."""
    if sql is None:
        return "ordered"
    statements = sqltext.split_statements(sql)
    if len(statements) == n_results and result_index < len(statements):
        return ("ordered" if sqltext.has_top_level_order_by(statements[result_index].text)
                else "multiset")
    if any(sqltext.has_top_level_order_by(st.text) for st in statements):
        return "ordered"
    return "multiset"


def _prepared_sql(case: Case, step: Step) -> str | None:
    """SQL of the most recent ``parse`` of ``step``'s statement on the same session."""
    name = step.payload.get("statement", "")
    for prev in reversed(case.steps[: step.index]):
        if (prev.kind == "parse" and prev.session == step.session
                and prev.payload.get("name", "") == name):
            return prev.payload.get("sql")
    return None


def resolve_order(case: Case, step_index: int, result_index: int, n_results: int) -> str:
    """Return "ordered" or "multiset" for one result (rules in module docstring)."""
    if step_index < 0 or step_index >= len(case.steps):
        return "ordered"
    step = case.steps[step_index]
    if step.order in ("ordered", "multiset"):
        return step.order
    if step.kind in ("sql", "exec_params"):
        return _auto_for_sql(step.payload.get("sql"), result_index, n_results)
    if step.kind == "execute":
        return _auto_for_sql(_prepared_sql(case, step), result_index, n_results)
    if step.kind == "wait":
        for prev in reversed(case.steps[:step_index]):
            if (prev.is_async and prev.session == step.session
                    and prev.kind in ("sql", "execute", "exec_params")):
                return resolve_order(case, prev.index, result_index, n_results)
        return "ordered"
    return "ordered"


def _row_sort_key(row: Any) -> tuple:
    return tuple((0, b"") if v is None else (1, v) if isinstance(v, bytes) else (2, v)
                 for v in row)


def _compare_rows(path: str, ref_rows: list, tgt_rows: list, order: str) -> list[Diff]:
    ref_t = [tuple(r) for r in ref_rows]
    tgt_t = [tuple(r) for r in tgt_rows]
    if order == "multiset":
        rc = collections.Counter(ref_t)
        tc = collections.Counter(tgt_t)
        if rc == tc:
            return []
        only_ref = sorted((rc - tc).elements(), key=_row_sort_key)[:_MULTISET_LIMIT]
        only_tgt = sorted((tc - rc).elements(), key=_row_sort_key)[:_MULTISET_LIMIT]
        return [Diff(f"{path}.rows", "row_multiset", [list(r) for r in only_ref],
                     [list(r) for r in only_tgt], f"rows {len(ref_t)} vs {len(tgt_t)}")]
    if ref_t == tgt_t:
        return []
    if collections.Counter(ref_t) == collections.Counter(tgt_t):
        return [Diff(f"{path}.rows", "row_order", ref_rows, tgt_rows)]
    if len(ref_t) != len(tgt_t):
        return [Diff(f"{path}.rows", "row_count", len(ref_t), len(tgt_t))]
    for r, (a, b) in enumerate(zip(ref_t, tgt_t)):
        if a == b:
            continue
        for c in range(max(len(a), len(b))):
            if c >= len(a) or c >= len(b):
                return [Diff(f"{path}.rows[{r}][{c}]", "value",
                             a[c] if c < len(a) else "<missing>",
                             b[c] if c < len(b) else "<missing>")]
            if a[c] != b[c]:
                return [Diff(f"{path}.rows[{r}][{c}]", "value", a[c], b[c])]
    return []


def _compare_error(path: str, ref: ErrorInfo | None, tgt: ErrorInfo | None,
                   compare_message: bool) -> list[Diff]:
    out: list[Diff] = []
    ep = f"{path}.error"

    def g(e: ErrorInfo | None, f: str) -> Any:
        return None if e is None else getattr(e, f)

    if g(ref, "sqlstate") != g(tgt, "sqlstate"):
        out.append(Diff(f"{ep}.sqlstate", "sqlstate", g(ref, "sqlstate"), g(tgt, "sqlstate")))
    if g(ref, "severity") != g(tgt, "severity"):
        out.append(Diff(f"{ep}.severity", "error_field", g(ref, "severity"),
                        g(tgt, "severity")))
    if compare_message and g(ref, "message") != g(tgt, "message"):
        out.append(Diff(f"{ep}.message", "error_message", g(ref, "message"),
                        g(tgt, "message")))
    fields = ["detail", "hint", "context"] if compare_message else []
    fields += ["position", "internal_position", "internal_query", "schema_name",
               "table_name", "column_name", "datatype_name", "constraint_name"]
    for f in fields:
        if g(ref, f) != g(tgt, f):
            out.append(Diff(f"{ep}.{f}", "error_field", g(ref, f), g(tgt, f)))
    return out


def _compare_columns(path: str, ref: list[ColumnDesc], tgt: list[ColumnDesc]) -> list[Diff]:
    out: list[Diff] = []
    cp = f"{path}.columns"
    if len(ref) != len(tgt):
        out.append(Diff(cp, "column_count", len(ref), len(tgt)))
    for j, (a, b) in enumerate(zip(ref, tgt)):
        p = f"{cp}[{j}]"
        if a.name != b.name:
            out.append(Diff(f"{p}.name", "column_name", a.name, b.name))
        if a.type_oid != b.type_oid:
            out.append(Diff(f"{p}.type_oid", "column_type", a.type_oid, b.type_oid))
        if a.typmod != b.typmod:
            out.append(Diff(f"{p}.typmod", "column_typmod", a.typmod, b.typmod))
        if a.format != b.format:
            out.append(Diff(f"{p}.format", "column_format", a.format, b.format))
        if a.table_oid != b.table_oid:
            out.append(Diff(f"{p}.table_oid", "column_origin", a.table_oid, b.table_oid))
        if a.column_attnum != b.column_attnum:
            out.append(Diff(f"{p}.column_attnum", "column_origin", a.column_attnum,
                            b.column_attnum))
    return out


def compare_results(path: str, ref: StatementResult, tgt: StatementResult, order: str,
                    compare_message: bool) -> list[Diff]:
    """Result-level comparison (module docstring)."""
    if ref.kind != tgt.kind:
        note = ""
        if ref.error is not None or tgt.error is not None:
            note = (f"reference sqlstate={ref.error.sqlstate if ref.error else None}, "
                    f"target sqlstate={tgt.error.sqlstate if tgt.error else None}")
        return [Diff(f"{path}.kind", "result_kind", ref.kind, tgt.kind, note)]
    out: list[Diff] = []
    if ref.kind == "error" or ref.error is not None or tgt.error is not None:
        out += _compare_error(path, ref.error, tgt.error, compare_message)
    out += _compare_columns(path, ref.columns, tgt.columns)
    if ref.command_tag != tgt.command_tag:
        out.append(Diff(f"{path}.command_tag", "command_tag", ref.command_tag,
                        tgt.command_tag))
    if ref.param_type_oids != tgt.param_type_oids:
        out.append(Diff(f"{path}.param_type_oids", "param_types", ref.param_type_oids,
                        tgt.param_type_oids))
    if ref.kind in ("rows", "copy_out"):
        out += _compare_rows(path, ref.rows, tgt.rows, order)
    return out


def _notice_key(n: Notice, compare_message: bool) -> tuple:
    if compare_message:
        return (n.severity, n.sqlstate, n.message, n.detail, n.hint, n.context)
    return (n.severity, n.sqlstate)


def _compare_notices(path: str, ref: list[Notice], tgt: list[Notice],
                     compare_message: bool) -> list[Diff]:
    rk = [_notice_key(n, compare_message) for n in ref]
    tk = [_notice_key(n, compare_message) for n in tgt]
    if rk == tk:
        return []
    i = 0
    while i < len(rk) and i < len(tk) and rk[i] == tk[i]:
        i += 1
    a = list(rk[i]) if i < len(rk) else None
    b = list(tk[i]) if i < len(tk) else None
    note = f"notice[{i}]"
    if len(rk) != len(tk):
        note += f"; count {len(rk)} vs {len(tk)}"
    return [Diff(f"{path}.notices", "notice", a, b, note)]


def compare_steps(path: str, case: Case, step_index: int, ref: StepObservation,
                  tgt: StepObservation | None, is_setup: bool = False) -> list[Diff]:
    """Step-level comparison incl. results and notices. ``tgt`` None -> [missing_step]."""
    if tgt is None:
        return [Diff(path, "missing_step", ref.kind, None,
                     "target transcript has no such step")]
    out: list[Diff] = []
    if ref.timed_out != tgt.timed_out:
        out.append(Diff(f"{path}.timed_out", "timeout", ref.timed_out, tgt.timed_out))
    if ref.connection_lost != tgt.connection_lost:
        out.append(Diff(f"{path}.connection_lost", "connection", ref.connection_lost,
                        tgt.connection_lost))
    if ref.blocked_observed != tgt.blocked_observed:
        out.append(Diff(f"{path}.blocked_observed", "blocked", ref.blocked_observed,
                        tgt.blocked_observed))
    if ref.txn_status != tgt.txn_status:
        out.append(Diff(f"{path}.txn_status", "txn_status", ref.txn_status, tgt.txn_status))
    if len(ref.results) != len(tgt.results):
        out.append(Diff(f"{path}.results", "result_count", len(ref.results),
                        len(tgt.results)))
    compare_message = True
    if not is_setup and 0 <= step_index < len(case.steps):
        compare_message = case.steps[step_index].compare_message
    n = len(ref.results)
    for k, (rr, tr) in enumerate(zip(ref.results, tgt.results)):
        order = "ordered" if is_setup else resolve_order(case, step_index, k, n)
        out += compare_results(f"{path}.results[{k}]", rr, tr, order, compare_message)
    out += _compare_notices(path, ref.notices, tgt.notices, compare_message)
    return out


def compare_transcripts(case: Case, reference: Transcript, target: Transcript) -> list[Diff]:
    """Full comparison (module docstring)."""
    ref = apply_case_normalizers(case, copy.deepcopy(reference))
    tgt = apply_case_normalizers(case, copy.deepcopy(target))
    out: list[Diff] = []
    for prefix, rs, ts, is_setup in (("setup", ref.setup, tgt.setup, True),
                                     ("steps", ref.steps, tgt.steps, False)):
        for i, r in enumerate(rs):
            t = ts[i] if i < len(ts) else None
            out += compare_steps(f"{prefix}[{i}]", case, i, r, t, is_setup)
        for i in range(len(rs), len(ts)):
            out.append(Diff(f"{prefix}[{i}]", "missing_step", None, ts[i].kind,
                            "target has an extra step not present in the reference"))
    return out


def _render_value(v: Any) -> str:
    s = "NULL" if v is None else repr(v)
    if len(s) > _RENDER_VALUE_LIMIT:
        s = s[: _RENDER_VALUE_LIMIT - 3] + "..."
    return s


def render_diff(diffs: list[Diff], limit: int = 50) -> str:
    """Human-readable multi-line rendering for reports, one diff per line:
    ``steps[2].results[0].rows[1][0]: value: reference='2' target='3'`` (values via repr;
    NULL rendered as ``NULL``; truncated to 200 chars each)."""
    lines = []
    for d in diffs[: max(limit, 0)]:
        line = (f"{d.path}: {d.kind}: reference={_render_value(d.reference)} "
                f"target={_render_value(d.target)}")
        if d.note:
            line += f" ({d.note})"
        lines.append(line)
    if len(diffs) > limit:
        lines.append(f"... {len(diffs) - max(limit, 0)} more diffs")
    return "\n".join(lines)
