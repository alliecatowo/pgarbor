"""Tests for HAR-18: pg_regress-exact output comparison (ORACLE-05 outdiff)."""

from __future__ import annotations

from pathlib import Path

from pgarbor_harness.regress.outdiff import compare_output, unified_diff


def _write(path: Path, data: bytes) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return path


def test_crlf_only_difference_is_identical(tmp_path: Path) -> None:
    expected = _write(tmp_path / "expected" / "t.out", b"a\nb\n")
    results = _write(tmp_path / "results" / "t.out", b"a\r\nb\r\n")
    out = compare_output(results, [expected])
    assert out.identical is True
    assert out.matched == expected
    assert out.best is None
    assert out.diff_lines == 0
    assert out.unified == ""


def test_crlf_in_expected_only_is_identical(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"x\r\ny\r\n")
    results = _write(tmp_path / "r.out", b"x\ny\n")
    assert compare_output(results, [expected]).identical is True


def test_lone_cr_is_not_stripped(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"a\rb\n")
    results = _write(tmp_path / "r.out", b"ab\n")
    assert compare_output(results, [expected]).identical is False


def test_trailing_whitespace_is_not_identical(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"select 1;\n")
    results = _write(tmp_path / "r.out", b"select 1; \n")
    out = compare_output(results, [expected])
    assert out.identical is False
    assert out.best == expected
    assert out.diff_lines == 2


def test_missing_trailing_newline_is_not_identical(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"a\nb\n")
    results = _write(tmp_path / "r.out", b"a\nb")
    assert compare_output(results, [expected]).identical is False


def test_case_matters(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"SELECT\n")
    results = _write(tmp_path / "r.out", b"select\n")
    assert compare_output(results, [expected]).identical is False


def test_matches_any_candidate(tmp_path: Path) -> None:
    base = _write(tmp_path / "t.out", b"variant base\n")
    v1 = _write(tmp_path / "t_1.out", b"variant one\n")
    results = _write(tmp_path / "r.out", b"variant one\n")
    out = compare_output(results, [base, v1])
    assert out.identical is True
    assert out.matched == v1


def test_best_candidate_is_fewest_changed_lines(tmp_path: Path) -> None:
    # far: 4 changed lines against results; near: 2 changed lines (the trailing space).
    far = _write(tmp_path / "t.out", b"1\n2\nX\n4\n6\n")
    near = _write(tmp_path / "t_1.out", b"1\n2\n3\n4\n5\n")
    results = _write(tmp_path / "r.out", b"1\n2\n3\n4\n5 \n")
    out = compare_output(results, [far, near])
    assert out.identical is False
    assert out.best == near
    assert out.diff_lines == 2
    assert out.unified.startswith(f"--- {near}\n+++ {results}\n")


def test_tie_goes_to_earliest_candidate(tmp_path: Path) -> None:
    first = _write(tmp_path / "t.out", b"aaa\n")
    second = _write(tmp_path / "t_1.out", b"bbb\n")
    results = _write(tmp_path / "r.out", b"ccc\n")
    out = compare_output(results, [first, second])
    assert out.identical is False
    assert out.best == first
    assert out.diff_lines == 2


def test_no_candidates(tmp_path: Path) -> None:
    results = _write(tmp_path / "r.out", b"x\n")
    out = compare_output(results, [])
    assert out.identical is False
    assert out.best is None and out.matched is None
    assert out.diff_lines == -1
    assert out.unified == "no expected output"


def test_nonexistent_candidates_count_as_no_candidates(tmp_path: Path) -> None:
    results = _write(tmp_path / "r.out", b"x\n")
    out = compare_output(results, [tmp_path / "missing.out"])
    assert out.diff_lines == -1
    assert out.unified == "no expected output"


def test_missing_results_file(tmp_path: Path) -> None:
    expected = _write(tmp_path / "t.out", b"x\n")
    out = compare_output(tmp_path / "missing.out", [expected])
    assert out.identical is False
    assert out.diff_lines == -1
    assert out.unified == "no results file"


def test_unified_diff_counts_changes_not_headers(tmp_path: Path) -> None:
    # A removed line that starts with "--" renders as "---..." in the diff body and must
    # still be counted; the real header lines must not be.
    expected = _write(tmp_path / "e.out", b"-- old comment\nkeep\n")
    results = _write(tmp_path / "r.out", b"-- new comment\nkeep\n")
    count, text = unified_diff(expected, results)
    assert count == 2
    assert text.splitlines()[0] == f"--- {expected}"
    assert text.splitlines()[1] == f"+++ {results}"


def test_unified_diff_identical_is_empty(tmp_path: Path) -> None:
    expected = _write(tmp_path / "e.out", b"a\n")
    results = _write(tmp_path / "r.out", b"a\r\n")
    assert unified_diff(expected, results) == (0, "")


def test_unified_diff_context_lines(tmp_path: Path) -> None:
    body = b"".join(f"line{i}\n".encode() for i in range(10))
    changed = body.replace(b"line5\n", b"LINE5\n")
    expected = _write(tmp_path / "e.out", body)
    results = _write(tmp_path / "r.out", changed)
    count, text = unified_diff(expected, results, context=3)
    assert count == 2
    assert " line2\n" in text and " line8\n" in text
    assert " line1\n" not in text and " line9\n" not in text


def test_non_utf8_bytes_compare_exactly(tmp_path: Path) -> None:
    expected = _write(tmp_path / "e.out", b"caf\xff\n")
    same = _write(tmp_path / "s.out", b"caf\xff\r\n")
    other = _write(tmp_path / "o.out", b"caf\xfe\n")
    assert compare_output(same, [expected]).identical is True
    out = compare_output(other, [expected])
    assert out.identical is False
    assert out.diff_lines == 2
