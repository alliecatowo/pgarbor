"""Tests for pgarbor_factory.inventory (lease FAC-03).

The sample sources below are built at runtime so that this file itself never contains a literal
marker or forbidden placeholder; otherwise the repository inventory would count the samples.
"""

from __future__ import annotations

import re
from pathlib import Path

from pgarbor_factory.inventory import Marker, delta, inventory, scan_file, scan_markers

U = "unimplemented"  # PgError::unimplemented
NI = "NotImplementedError"
BANG = "!"


def _write(root: Path, rel: str, text: str) -> Path:
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    return p


RUST_SAMPLE = (
    "fn a() -> Result<(), PgError> {\n"
    f"    Err(PgError::{U}(\n"
    '        "FLT-02",\n'
    '        "fsyncgate simulation",\n'
    "    ))\n"
    "}\n"
    f"fn b() -> PgError {{ PgError::{U}(\"STO-03\", \"btree \\\"delete\\\"\") }}\n"
)


def test_rust_marker_spanning_lines_and_escapes(tmp_path: Path) -> None:
    p = _write(tmp_path, "crates/pgarbor-fault/src/a.rs", RUST_SAMPLE)
    markers, forbidden = scan_file(p, "crates/pgarbor-fault/src/a.rs")
    assert forbidden == []
    assert markers == [
        Marker("crates/pgarbor-fault/src/a.rs", 2, "FLT-02", "fsyncgate simulation"),
        Marker("crates/pgarbor-fault/src/a.rs", 7, "STO-03", 'btree "delete"'),
    ]


def test_rust_forbidden_outside_cfg_test_only(tmp_path: Path) -> None:
    src = (
        "fn f() {\n"
        f"    todo{BANG}(1);\n"
        f"    panic{BANG}(\"x\");\n"
        f"    {U}{BANG}();\n"
        "}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        f"    fn t() {{ todo{BANG}(2); panic{BANG}(\"y\"); }}\n"
        "}\n"
    )
    p = _write(tmp_path, "crates/pgarbor-x/src/f.rs", src)
    markers, forbidden = scan_file(p, "crates/pgarbor-x/src/f.rs")
    assert markers == []
    assert [(f["line"], f["kind"]) for f in forbidden] == [
        (2, "rust-todo"),
        (3, "rust-panic"),
        (4, "rust-unimplemented"),
    ]
    assert forbidden[0]["text"] == f"todo{BANG}(1);"


def test_rust_marker_inside_cfg_test_is_not_production_inventory(tmp_path: Path) -> None:
    src = f"#[cfg(test)]\nmod t {{\n    fn x() {{ PgError::{U}(\"CAT-01\", \"probe\"); }}\n}}\n"
    p = _write(tmp_path, "crates/pgarbor-x/src/t.rs", src)
    markers, _ = scan_file(p, "crates/pgarbor-x/src/t.rs")
    assert markers == []


def test_python_markers_and_bare_raise_forbidden(tmp_path: Path) -> None:
    src = (
        "class A:\n"
        "    def f(self):\n"
        f"        raise {NI}(\n"
        '            "FAC-03: scan one file"\n'
        "        )\n"
        f"    def g(self):\n"
        f"        raise {NI}\n"
        f"    def h(self):\n"
        f"        raise {NI}(\"boom\")\n"
        "    def i(self):\n"
        "        pass  # " + "TODO\n"
    )
    p = _write(tmp_path, "harness/pgarbor_factory/a.py", src)
    markers, forbidden = scan_file(p, "harness/pgarbor_factory/a.py")
    assert markers == [Marker("harness/pgarbor_factory/a.py", 3, "FAC-03", "scan one file")]
    assert [(f["line"], f["kind"]) for f in forbidden] == [
        (7, "python-not-implemented"),
        (9, "python-not-implemented"),
        (11, "python-pass-todo"),
    ]


def test_python_marker_regex_requires_lease_prefix() -> None:
    from pgarbor_factory.inventory import _PY_MARKER_RE

    assert _PY_MARKER_RE.search(f'{NI}("ab: x")') is None
    assert _PY_MARKER_RE.search(f'{NI}("FAC-03: x")') is not None


def test_inventory_document_and_pruning(tmp_path: Path) -> None:
    _write(tmp_path, "crates/pgarbor-storage/src/b.rs", RUST_SAMPLE)
    _write(tmp_path, "crates/pgarbor-storage/target/debug/c.rs", RUST_SAMPLE)
    _write(tmp_path, "harness/pgarbor_factory/z.py", f'raise {NI}("FAC-03: z")\n')
    _write(tmp_path, "harness/.venv/lib/y.py", f'{NI}("FAC-03: ignored")\n')
    _write(tmp_path, "harness/pgarbor_factory/__pycache__/q.py", f'{NI}("FAC-03: ignored")\n')
    _write(tmp_path, ".claude/worktrees/w/crates/x/src/d.rs", RUST_SAMPLE)

    doc = inventory(tmp_path)
    assert doc["format"] == "pgarbor-inventory/1"
    assert doc["git_sha"] is None or re.fullmatch(r"[0-9a-f]{40}", doc["git_sha"])
    assert doc["total"] == 3
    assert doc["by_lease"] == {"FAC-03": 1, "FLT-02": 1, "STO-03": 1}
    assert doc["by_crate"] == {"harness": 1, "pgarbor-storage": 2}
    assert doc["by_file"] == {
        "crates/pgarbor-storage/src/b.rs": 2,
        "harness/pgarbor_factory/z.py": 1,
    }
    keys = [(m["file"], m["line"]) for m in doc["markers"]]
    assert keys == sorted(keys)
    assert set(doc["markers"][0]) == {"file", "line", "lease_id", "what"}
    assert doc["forbidden"] == []


def test_bare_raise_is_reported_as_forbidden_in_inventory(tmp_path: Path) -> None:
    _write(tmp_path, "harness/pgarbor_factory/bad.py", f"def f():\n    raise {NI}\n")
    doc = inventory(tmp_path)
    assert doc["total"] == 0
    assert [(f["file"], f["line"], f["kind"]) for f in doc["forbidden"]] == [
        ("harness/pgarbor_factory/bad.py", 2, "python-not-implemented")
    ]


def test_scan_markers_sorted(tmp_path: Path) -> None:
    _write(tmp_path, "crates/pgarbor-b/src/lib.rs", RUST_SAMPLE)
    _write(tmp_path, "crates/pgarbor-a/src/lib.rs", RUST_SAMPLE)
    markers = scan_markers(tmp_path)
    assert [m.file for m in markers] == [
        "crates/pgarbor-a/src/lib.rs",
        "crates/pgarbor-a/src/lib.rs",
        "crates/pgarbor-b/src/lib.rs",
        "crates/pgarbor-b/src/lib.rs",
    ]


def test_delta_closed_and_new(tmp_path: Path) -> None:
    r1 = tmp_path / "before"
    _write(r1, "crates/pgarbor-fault/src/a.rs", RUST_SAMPLE)
    before = inventory(r1)

    r2 = tmp_path / "after"
    after_src = (
        f"fn b() -> PgError {{ PgError::{U}(\"STO-03\", \"btree \\\"delete\\\"\") }}\n"
        f"fn c() -> PgError {{ PgError::{U}(\"FLT-02\", \"new stub\") }}\n"
    )
    _write(r2, "crates/pgarbor-fault/src/a.rs", after_src)
    after = inventory(r2)

    d = delta(before, after)
    assert d["total_before"] == 2
    assert d["total_after"] == 2
    assert d["closed_by_lease"] == {"FLT-02": 1}
    assert [(m["lease_id"], m["what"]) for m in d["new_markers"]] == [("FLT-02", "new stub")]


def test_delta_rejects_foreign_document() -> None:
    import pytest

    with pytest.raises(ValueError):
        delta({"format": "other"}, {"format": "pgarbor-inventory/1", "total": 0})


def test_rust_inventory_keeps_production_after_test_module(tmp_path):
    sample = (f'fn before() {{ PgError::{U}("STO-01", "before"); }}\n'
              '#[cfg(test)]\nmod tests {\n'
              ' const BRACE: &str = "}"; /* nested { /* } */ } */\n'
              ' const RAW: &str = r#"}"#;\n'
              f' fn test() {{ PgError::{U}("CAT-01", "example"); }}\n'
              '}\n'
              f'fn after() {{ PgError::{U}("STO-02", "after"); }}\n')
    path = _write(tmp_path, "crates/probe/src/a.rs", sample)
    markers, _ = scan_file(path, "crates/probe/src/a.rs")
    assert [(m.lease_id, m.line) for m in markers] == [("STO-01", 1), ("STO-02", 8)]
