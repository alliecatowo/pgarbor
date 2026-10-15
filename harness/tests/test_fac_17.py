"""FAC-17: static patch review (harness/pgarbor_factory/review.py).

Each test builds a throwaway git repo under tmp_path with a base commit and a head commit,
then runs review_patch on the two shas. Rules are asserted by name and severity.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

from pgarbor_factory.records import LeaseSpec
from pgarbor_factory.review import contract_items, review_patch

RS = "src/lib.rs"
PY = "harness/pgarbor_harness/thing.py"
LEASE = "FAC-17"


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
         "-c", "commit.gpgsign=false", *args],
        cwd=str(repo), check=True, capture_output=True, text=True,
    ).stdout


def _write(repo: Path, path: str, text: str | None) -> None:
    p = repo / path
    if text is None:
        p.unlink()
        return
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")


def _make(tmp_path: Path, base: dict[str, str], head: dict[str, str | None]) -> tuple[Path, str, str]:
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q")
    for path, text in base.items():
        _write(repo, path, text)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "base")
    base_sha = _git(repo, "rev-parse", "HEAD").strip()
    for path, text in head.items():
        _write(repo, path, text)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", "head")
    head_sha = _git(repo, "rev-parse", "HEAD").strip()
    return repo, base_sha, head_sha


def _spec(files: list[str] | None = None) -> LeaseSpec:
    return LeaseSpec(lease_id=LEASE, crate="harness", task_ids=[], files=files or [RS],
                     model_tier="haiku", summary="", spec="")


def _run(tmp_path: Path, base: dict[str, str], head: dict[str, str | None],
         files: list[str] | None = None) -> list[dict[str, Any]]:
    repo, base_sha, head_sha = _make(tmp_path, base, head)
    return review_patch(repo, _spec(files), base_sha, head_sha)


def _rules(findings: list[dict[str, Any]], severity: str = "reject") -> set[str]:
    return {f["rule"] for f in findings if f["severity"] == severity}


# ---------------------------------------------------------------------------------------------
# extraction

def test_contract_items_maps_marked_rust_items_only() -> None:
    src = (
        "// CONTRACT\n"
        "pub fn add(a: u32) -> u32 {\n"
        "    a + 1\n"
        "}\n"
        "\n"
        "// CONTRACT\n"
        "pub struct Pair {\n"
        "    pub a: u32,\n"
        "}\n"
        "\n"
        "pub fn plain() {}\n"
    )
    items = contract_items(src, "rust")
    assert set(items) == {"fn add", "struct Pair"}
    assert items["fn add"].startswith("pub fn add(a: u32) -> u32 {")
    assert "a + 1" not in items["fn add"]  # body is not part of the frozen signature
    assert "pub a: u32" in items["struct Pair"]


def test_contract_items_python_def_header_only() -> None:
    src = (
        "# CONTRACT\n"
        "def f(x: int) -> int:\n"
        "    return x\n"
        "\n"
        "def g():\n"
        "    return 1\n"
    )
    items = contract_items(src, "python")
    assert set(items) == {"def f"}
    assert items["def f"] == "def f(x: int) -> int:"


# ---------------------------------------------------------------------------------------------
# reject rules

def test_contract_signature_change_rejected(tmp_path: Path) -> None:
    base = {RS: "// CONTRACT\npub fn add(a: u32) -> u32 {\n    a\n}\n"}
    head: dict[str, str | None] = {RS: "// CONTRACT\npub fn add(a: u64) -> u64 {\n    a\n}\n"}
    rejects = _rules(_run(tmp_path, base, head))
    assert "contract_item_changed" in rejects
    assert "signature_changed" in rejects


def test_contract_body_implementation_allowed(tmp_path: Path) -> None:
    base = {RS: ("// CONTRACT\npub fn parse_x(s: &str) -> Result<u8> {\n"
                 "    PgError::unimplemented(\"FAC-17\", \"parse x\")\n}\n")}
    head: dict[str, str | None] = {RS: ("// CONTRACT\npub fn parse_x(s: &str) -> Result<u8> {\n"
                                        "    Ok(s.len() as u8)\n}\n")}
    assert _rules(_run(tmp_path, base, head)) == set()


def test_pub_signature_change_without_marker_rejected(tmp_path: Path) -> None:
    base = {RS: "pub fn helper(x: u8) -> u8 {\n    x\n}\n"}
    head: dict[str, str | None] = {RS: "pub fn helper(x: u16) -> u8 {\n    x as u8\n}\n"}
    assert "signature_changed" in _rules(_run(tmp_path, base, head))


def test_fake_success_rejected(tmp_path: Path) -> None:
    base = {RS: ("pub fn load(path: &str) -> Result<Vec<u8>> {\n"
                 "    PgError::unimplemented(\"FAC-17\", \"load\")\n}\n")}
    head: dict[str, str | None] = {RS: ("pub fn load(path: &str) -> Result<Vec<u8>> {\n"
                                        "    Ok(Vec::new())\n}\n")}
    assert "fake_success" in _rules(_run(tmp_path, base, head))


def test_fake_success_excused_by_doc(tmp_path: Path) -> None:
    base = {RS: ("pub fn load(path: &str) -> Result<Vec<u8>> {\n"
                 "    PgError::unimplemented(\"FAC-17\", \"load\")\n}\n")}
    head: dict[str, str | None] = {RS: ("/// Returns empty when the file is missing (no-op).\n"
                                        "pub fn load(path: &str) -> Result<Vec<u8>> {\n"
                                        "    Ok(Vec::new())\n}\n")}
    assert "fake_success" not in _rules(_run(tmp_path, base, head))


def test_todo_in_production_rejected(tmp_path: Path) -> None:
    base = {RS: "pub fn f() -> u8 {\n    1\n}\n"}
    head: dict[str, str | None] = {RS: "pub fn f() -> u8 {\n    todo!()\n}\n"}
    assert "forbidden_placeholder" in _rules(_run(tmp_path, base, head))


def test_panic_inside_cfg_test_allowed(tmp_path: Path) -> None:
    base = {RS: "pub fn f() -> u8 {\n    1\n}\n"}
    head: dict[str, str | None] = {RS: (
        "pub fn f() -> u8 {\n    1\n}\n"
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        panic!(\"boom\");\n    }\n}\n"
    )}
    assert "forbidden_placeholder" not in _rules(_run(tmp_path, base, head))


def test_unwrap_on_input_rejected(tmp_path: Path) -> None:
    base = {RS: "pub fn n(s: &str) -> u8 {\n    0\n}\n"}
    head: dict[str, str | None] = {RS: "pub fn n(s: &str) -> u8 {\n    s.parse().unwrap()\n}\n"}
    assert "forbidden_placeholder" in _rules(_run(tmp_path, base, head))


def test_python_foreign_lease_id_rejected(tmp_path: Path) -> None:
    base = {PY: "def f():\n    return 1\n"}
    head: dict[str, str | None] = {PY: "def f():\n    raise NotImplementedError(\"FAC-99: other\")\n"}
    assert "forbidden_placeholder" in _rules(_run(tmp_path, base, head, [PY]))


def test_python_own_lease_id_allowed(tmp_path: Path) -> None:
    base = {PY: "def f():\n    return 1\n"}
    head: dict[str, str | None] = {PY: "def f():\n    raise NotImplementedError(\"FAC-17: f\")\n"}
    assert "forbidden_placeholder" not in _rules(_run(tmp_path, base, head, [PY]))


def test_python_pass_over_stub_rejected(tmp_path: Path) -> None:
    base = {PY: "def f():\n    raise NotImplementedError(\"FAC-17: f\")\n"}
    head: dict[str, str | None] = {PY: "def f():\n    pass\n"}
    assert "forbidden_placeholder" in _rules(_run(tmp_path, base, head, [PY]))


def test_python_signature_change_rejected(tmp_path: Path) -> None:
    base = {PY: "def f(x: int) -> int:\n    return x\n"}
    head: dict[str, str | None] = {PY: "def f(x: str) -> int:\n    return x\n"}
    assert "signature_changed" in _rules(_run(tmp_path, base, head, [PY]))


def test_test_weakening_rejected(tmp_path: Path) -> None:
    base = {RS: "#[test]\nfn checks() {\n    assert_eq!(1, 1);\n}\n"}
    head: dict[str, str | None] = {RS: "#[test]\n#[ignore]\nfn checks() {\n}\n"}
    findings = _run(tmp_path, base, head)
    rules = [f["rule"] for f in findings if f["rule"] == "test_weakening"]
    assert len(rules) >= 2  # removed assert_eq! and added #[ignore]


def test_python_skip_marker_rejected(tmp_path: Path) -> None:
    base = {PY: "def test_x():\n    assert 1 == 1\n"}
    head: dict[str, str | None] = {PY: "import pytest\n@pytest.mark.skip\ndef test_x():\n    pass\n"}
    assert "test_weakening" in _rules(_run(tmp_path, base, head, [PY]))


# ---------------------------------------------------------------------------------------------
# warnings

def test_stub_remaining_warns(tmp_path: Path) -> None:
    stub = "pub fn a() -> u8 {\n    PgError::unimplemented(\"FAC-17\", \"a\")\n}\n"
    base = {RS: stub}
    head: dict[str, str | None] = {RS: stub, "notes.txt": "x\n"}
    assert "stub_remaining" in _rules(_run(tmp_path, base, head), "warn")


def test_large_file_warns(tmp_path: Path) -> None:
    base = {RS: "pub fn a() {}\n"}
    head: dict[str, str | None] = {RS: "pub fn a() {}\n" + "// filler\n" * 1600}
    assert "large_file" in _rules(_run(tmp_path, base, head), "warn")
