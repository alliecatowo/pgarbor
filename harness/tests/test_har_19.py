"""Tests for HAR-19: isolation spec parser, permutation generation, schedule and manifest."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from pgarbor_harness.config import paths
from pgarbor_harness.isolation.spec_parser import (
    Marker,
    SpecSyntaxError,
    all_permutations,
    build_manifest,
    parse_schedule,
    parse_spec,
)

ISO_DIR = paths().pg_isolation_dir
SPECS_DIR = ISO_DIR / "specs"

SIMPLE = """
setup { CREATE TABLE t (i int); }
teardown { DROP TABLE t; }

session "s1"
setup { BEGIN; }
step s1a { SELECT 1 }
step s1b {
  SELECT 2;
}
teardown { COMMIT; }

session s2
step s2a { SELECT 3 } # a comment
step s2b { SELECT '#not a comment' }
permutation s1a(s2a notices 1) s2a s1b s2b
"""


def test_deadlock_simple_shape() -> None:
    path = SPECS_DIR / "deadlock-simple.spec"
    if not path.is_file():
        pytest.skip("PG18 reference source not present")
    spec = parse_spec(path.read_text(encoding="utf-8"), name="deadlock-simple")
    assert len(spec.sessions) == 2
    assert sum(len(s.steps) for s in spec.sessions) == 6
    assert len(spec.permutations) == 1
    assert [p.name for p in spec.permutations[0]] == [
        "s1as", "s2as", "s1ae", "s2ae", "s1c", "s2c"]
    assert spec.setup and spec.teardown


def test_all_pg18_specs_parse() -> None:
    if not SPECS_DIR.is_dir():
        pytest.skip("PG18 reference source not present")
    files = sorted(SPECS_DIR.glob("*.spec"))
    assert len(files) == 124
    failures = []
    for f in files:
        try:
            parse_spec(f.read_text(encoding="utf-8"), name=f.name)
        except SpecSyntaxError as exc:
            failures.append(str(exc))
    assert failures == []


def test_notices_marker_and_comments() -> None:
    spec = parse_spec(SIMPLE, name="simple")
    assert spec.name == "simple"
    assert spec.setup == ["CREATE TABLE t (i int);"]
    assert spec.teardown == "DROP TABLE t;"
    assert [s.name for s in spec.sessions] == ["s1", "s2"]
    assert spec.sessions[0].setup == "BEGIN;"
    assert spec.sessions[0].teardown == "COMMIT;"
    # spaces right after "{" and right before "}" are dropped; other whitespace is kept
    assert spec.sessions[0].steps[0].sql == "SELECT 1"
    assert spec.sessions[0].steps[1].sql == "\n  SELECT 2;\n"
    assert spec.sessions[1].steps[1].sql == "SELECT '#not a comment'"
    perm = spec.permutations[0]
    assert [p.name for p in perm] == ["s1a", "s2a", "s1b", "s2b"]
    assert perm[0].markers == [Marker(kind="notices", step="s2a", count=1)]
    assert perm[1].markers == []


def test_notices_marker_exact() -> None:
    text = """
session s1
step a { SELECT 1 }
session s2
step b { SELECT 2 }
permutation a b(a notices 1)
"""
    spec = parse_spec(text)
    perm = spec.permutations[0]
    assert perm[1].markers == [Marker(kind="notices", step="a", count=1)]
    assert perm[0].markers == []


def test_star_and_step_markers() -> None:
    text = """
session s1
step a { SELECT 1 }
session s2
step b { SELECT 2 }
permutation a b(*, a)
"""
    spec = parse_spec(text)
    markers = spec.permutations[0][1].markers
    assert markers == [Marker(kind="star"), Marker(kind="step", step="a", count=0)]


def test_quoted_identifiers_and_no_case_folding() -> None:
    text = """
session "Odd""Name"
step "Step One" { SELECT 1 }
session S2
step Bb { SELECT 2 }
permutation "Step One" Bb
"""
    spec = parse_spec(text)
    assert spec.sessions[0].name == 'Odd"Name'
    assert spec.sessions[0].steps[0].name == "Step One"
    assert [p.name for p in spec.permutations[0]] == ["Step One", "Bb"]


def test_all_permutations_two_by_two() -> None:
    text = """
session s1
step a1 { SELECT 1 }
step a2 { SELECT 2 }
session s2
step b1 { SELECT 3 }
step b2 { SELECT 4 }
"""
    spec = parse_spec(text)
    perms = all_permutations(spec)
    assert len(perms) == 6
    seqs = [tuple(p.name for p in perm) for perm in perms]
    assert len(set(seqs)) == 6
    for seq in seqs:
        assert seq.index("a1") < seq.index("a2")
        assert seq.index("b1") < seq.index("b2")
    assert all(p.markers == [] for perm in perms for p in perm)
    # isolationtester order: at each position sessions are tried in declaration order
    assert seqs[0] == ("a1", "a2", "b1", "b2")


@pytest.mark.parametrize("sizes", [(1, 1), (2, 3), (3, 2, 1), (4, 4), (1, 2, 3)])
def test_all_permutations_count_is_multinomial(sizes: tuple[int, ...]) -> None:
    lines = []
    for i, n in enumerate(sizes):
        lines.append(f"session s{i}")
        for j in range(n):
            lines.append(f"step x{i}_{j} {{ SELECT 1 }}")
    spec = parse_spec("\n".join(lines) + "\n")
    from math import factorial
    expected = factorial(sum(sizes))
    for n in sizes:
        expected //= factorial(n)
    assert len(all_permutations(spec)) == expected


@pytest.mark.parametrize("text, message", [
    ("session s1\nstep a { SELECT 1 }\nstep a { SELECT 2 }\n", "duplicate step name: a"),
    ("session s1\nstep a { SELECT 1 }\npermutation a zzz\n",
     'undefined step "zzz" specified in permutation'),
    ("session s1\nstep a { SELECT 1 }\npermutation a(q)\n",
     'undefined blocking step "q"'),
    ("session s1\nstep a { SELECT 1 }\nsession s2\nstep b { SELECT 1 }\n"
     "permutation a(a) b\n", "cannot block on its own session"),
    ("session s1\nstep a { SELECT 1 }\npermutation a(*, b notices 0)\n",
     "notices count must be at least 1"),
    ("setup { SELECT 1 }\n", "syntax error"),
    ("session s1\nstep a { SELECT 1 }\nsetup { SELECT 2 }\n", "syntax error"),
    ("session s1\nstep a { SELECT 1 \n", "unterminated sql block"),
    ('session "abc\n', "unexpected newline in quoted identifier"),
    ('session "abc', "unterminated quoted identifier"),
    ("session s1\nstep a { SELECT 1 }\n@", "unexpected character"),
    ("session s1\n", "syntax error"),
    ("", "syntax error"),
])
def test_syntax_and_semantic_errors(text: str, message: str) -> None:
    with pytest.raises(SpecSyntaxError) as info:
        parse_spec(text, name="bad.spec")
    assert message in str(info.value)
    assert str(info.value).startswith("bad.spec:")


def test_error_reports_line_number() -> None:
    with pytest.raises(SpecSyntaxError) as info:
        parse_spec("session s1\n\nstep a { SELECT 1 }\n\nsetup { x }\n", name="n.spec")
    assert str(info.value).startswith("n.spec:5:")


def test_parse_schedule_flattens_names() -> None:
    text = "# comment\n\ntest: alpha beta\ntest: gamma\n"
    assert parse_schedule(text) == ["alpha", "beta", "gamma"]


def test_parse_schedule_rejects_other_directives() -> None:
    with pytest.raises(SpecSyntaxError):
        parse_schedule("ignore: foo\n")


def test_build_manifest_on_small_tree(tmp_path: Path) -> None:
    (tmp_path / "specs").mkdir()
    (tmp_path / "expected").mkdir()
    (tmp_path / "specs" / "good.spec").write_text(
        "session s1\nstep a { SELECT 1 }\nsession s2\nstep b { SELECT 2 }\n", encoding="utf-8")
    (tmp_path / "specs" / "bad.spec").write_text("session\n", encoding="utf-8")
    (tmp_path / "expected" / "good.out").write_text("x\n", encoding="utf-8")
    (tmp_path / "expected" / "good_1.out").write_text("y\n", encoding="utf-8")
    (tmp_path / "isolation_schedule").write_text(
        "test: good\ntest: bad\ntest: missing\n", encoding="utf-8")
    m = build_manifest(tmp_path)
    assert m["format"] == "pgarbor-isolation-manifest/1"
    assert m["order"] == ["good", "bad", "missing"]
    good = m["tests"]["good"]
    assert good["sessions"] == 2
    assert good["permutations"] == 2
    assert good["parse_error"] is None
    assert [e["path"] for e in good["expected"]] == ["expected/good.out", "expected/good_1.out"]
    assert m["tests"]["bad"]["parse_error"] is not None
    assert m["tests"]["bad"]["spec_sha256"] is not None
    assert m["tests"]["missing"]["parse_error"] is not None
    assert m["tests"]["missing"]["spec_sha256"] is None


def test_build_manifest_pg18_when_present() -> None:
    if not (ISO_DIR / "isolation_schedule").is_file():
        pytest.skip("PG18 reference source not present")
    m = build_manifest(ISO_DIR)
    assert len(m["order"]) == 122
    errors = [n for n, t in m["tests"].items() if t["parse_error"]]
    assert errors == []
    assert m["tests"]["deadlock-simple"]["permutations"] == 1


if __name__ == "__main__":  # pragma: no cover
    sys.exit(pytest.main([__file__]))
