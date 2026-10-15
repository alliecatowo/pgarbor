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
"""

from __future__ import annotations

from ..cases.model import Case
from ..model import Diff, StatementResult, StepObservation, Transcript

#: CONTRACT: every Diff.kind the comparator can emit.
DIFF_KINDS: tuple[str, ...] = (
    "missing_step", "timeout", "connection", "blocked", "txn_status", "result_count",
    "result_kind", "sqlstate", "error_message", "error_field", "column_count", "column_name",
    "column_type", "column_typmod", "column_format", "column_origin", "command_tag",
    "param_types", "row_order", "row_count", "value", "row_multiset", "notice",
)


def resolve_order(case: Case, step_index: int, result_index: int, n_results: int) -> str:
    """Return "ordered" or "multiset" for one result (rules in module docstring)."""
    raise NotImplementedError("HAR-13: resolve row order mode")


def compare_results(path: str, ref: StatementResult, tgt: StatementResult, order: str,
                    compare_message: bool) -> list[Diff]:
    """Result-level comparison (module docstring)."""
    raise NotImplementedError("HAR-13: compare one result")


def compare_steps(path: str, case: Case, step_index: int, ref: StepObservation,
                  tgt: StepObservation | None, is_setup: bool = False) -> list[Diff]:
    """Step-level comparison incl. results and notices. ``tgt`` None -> [missing_step]."""
    raise NotImplementedError("HAR-13: compare one step")


def compare_transcripts(case: Case, reference: Transcript, target: Transcript) -> list[Diff]:
    """Full comparison (module docstring)."""
    raise NotImplementedError("HAR-13: compare transcripts")


def render_diff(diffs: list[Diff], limit: int = 50) -> str:
    """Human-readable multi-line rendering for reports, one diff per line:
    ``steps[2].results[0].rows[1][0]: value: reference='2' target='3'`` (values via repr;
    NULL rendered as ``NULL``; truncated to 200 chars each)."""
    raise NotImplementedError("HAR-13: render diffs")
