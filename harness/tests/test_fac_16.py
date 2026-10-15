"""Tests for FAC-16 (harness/pgarbor_factory/packager.py).

Each test builds a small hermetic repository in pytest's tmp_path (contracts, a task DAG, a
lease catalog, source files and a git commit) so the packet layout does not depend on the
live lease catalog.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from pgarbor_factory.packager import build_packet, extract_signatures, write_packet
from pgarbor_harness.config import PROTECTED_GLOBS

LEASE_ID = "HAR-01"
CONTRACT_MARKER = "CONTRACT_MARKER_XYZ"
OWNED_MARKER = "OWNED_FILE_MARKER"
SMALL_MARKER = "SMALL_READ_MARKER"

BIG_LINES = [
    "def big_public(a):",
    '    """Big public API.',
    "",
    "    Details that must not be inlined.",
    '    """',
    "    return a",
] + [f"x_{i} = {i}" for i in range(500)]


def _write(root: Path, rel: str, text: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _make_repo(root: Path) -> None:
    _write(root, "contracts/crates/harness.md", f"# Harness contract\n\n{CONTRACT_MARKER}\n")
    _write(
        root,
        "contracts/dag/task_graph.v1.json",
        json.dumps({
            "tasks": [{
                "id": "SWARM-05",
                "title": "Agent input contract packager",
                "deliverable": "Provide bounded packets",
            }]
        }),
    )
    lease_doc = {
        "crate": "harness",
        "contract_version": "v1",
        "leases": [{
            "lease_id": LEASE_ID,
            "task_ids": ["SWARM-05"],
            "files": ["harness/pkg/owned.py"],
            "model_tier": "haiku",
            "summary": "Demo lease",
            "spec": "Do the thing.",
            "reads": ["harness/pkg/big.py", "harness/pkg/small.py"],
            "depends_on_leases": [],
            "acceptance": ["first criterion", "second criterion"],
        }],
    }
    _write(root, "contracts/leases/harness.json", json.dumps(lease_doc))
    _write(root, "harness/pkg/owned.py", f"# {OWNED_MARKER}\nVALUE = 1\n")
    _write(
        root,
        "harness/pkg/small.py",
        f'# {SMALL_MARKER}\n\ndef public_fn(x):\n    """Does a thing.\n\n    More detail.\n    """\n'
        "    return x\n",
    )
    _write(root, "harness/pkg/big.py", "\n".join(BIG_LINES) + "\n")
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run(
        ["git", "-c", "user.name=test", "-c", "user.email=test@example.invalid",
         "commit", "-q", "-m", "init"],
        cwd=root, check=True,
    )


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    _make_repo(tmp_path)
    return tmp_path


def test_sections_appear_in_documented_order(repo: Path) -> None:
    packet = build_packet(repo, LEASE_ID)
    markers = [
        f"# Lease {LEASE_ID}",
        "## Rules",
        "## Your files",
        "## Spec",
        "## Read for context",
        "## Contract",
    ]
    positions = [packet.index(m) for m in markers]
    assert positions == sorted(positions)
    assert len(set(positions)) == len(positions)


def test_header_lines_carry_metadata_and_tasks(repo: Path) -> None:
    packet = build_packet(repo, LEASE_ID)
    assert f"# Lease {LEASE_ID} — Demo lease" in packet
    assert "- Crate: harness" in packet
    assert "- Model tier: haiku" in packet
    assert "- Contract version: v1" in packet
    assert "Agent input contract packager" in packet
    assert "Provide bounded packets" in packet


def test_owned_file_read_and_contract_are_inlined(repo: Path) -> None:
    packet = build_packet(repo, LEASE_ID)
    assert OWNED_MARKER in packet
    assert SMALL_MARKER in packet  # small read (under 400 lines) is inlined in full
    assert CONTRACT_MARKER in packet  # contracts/crates/harness.md is inlined
    assert "first criterion" in packet and "second criterion" in packet


def test_large_read_is_reduced_to_signatures(repo: Path) -> None:
    packet = build_packet(repo, LEASE_ID)
    assert "def big_public(a):" in packet
    assert "Big public API." in packet
    assert "Details that must not be inlined." not in packet
    assert "x_499 = 499" not in packet


def test_rules_name_every_protected_path(repo: Path) -> None:
    packet = build_packet(repo, LEASE_ID)
    for glob in PROTECTED_GLOBS:
        assert glob in packet
    for name in ("Cargo.toml", "lib.rs", "mod.rs", "__init__.py", "__main__.py"):
        assert name in packet


def test_packet_length_within_default_bound(repo: Path) -> None:
    assert len(build_packet(repo, LEASE_ID)) <= 120_000


def test_size_bound_degrades_then_omits_from_the_end(repo: Path) -> None:
    full = build_packet(repo, LEASE_ID)
    assert "x_499 = 499" not in full  # the big read is already signatures-only
    # Tighten the bound until the packet has to drop reads; keep the first result that does.
    found = None
    for limit in range(len(full), 0, -25):
        try:
            candidate = build_packet(repo, LEASE_ID, max_chars=limit)
        except ValueError:
            continue
        assert len(candidate) <= limit
        if "omitted: " in candidate:
            found = candidate
            break
    assert found is not None
    # Reads drop from the end: the small read goes first, the big one (earlier) survives.
    assert "omitted: harness/pkg/small.py" in found
    assert "omitted: harness/pkg/big.py" not in found
    assert "def big_public(a):" in found


def test_impossible_bound_raises(repo: Path) -> None:
    with pytest.raises(ValueError):
        build_packet(repo, LEASE_ID, max_chars=10)


def test_output_is_deterministic(repo: Path) -> None:
    assert build_packet(repo, LEASE_ID) == build_packet(repo, LEASE_ID)


def test_unknown_lease_raises(repo: Path) -> None:
    with pytest.raises(ValueError):
        build_packet(repo, "HAR-99")


def test_write_packet_stores_under_runs_packets(repo: Path) -> None:
    out = write_packet(repo, LEASE_ID)
    assert out == repo / "runs" / "packets" / f"{LEASE_ID}.md"
    assert out.read_text(encoding="utf-8") == build_packet(repo, LEASE_ID)
    assert not (repo / "runs" / "packets" / f"{LEASE_ID}.md.tmp").exists()


def test_extract_signatures_rust(tmp_path: Path) -> None:
    src = tmp_path / "lib_x.rs"
    src.write_text(
        "use std::fmt;\n"
        "/// Adds two numbers.\n"
        "///\n"
        "/// Wraps around on overflow.\n"
        "#[inline]\n"
        "pub fn add(a: u32, b: u32) -> u32 { a + b }\n"
        "\n"
        "pub(crate) fn helper() {}\n"
        "fn private_fn() {}\n"
        "/// A record.\n"
        "pub struct Rec {\n"
        "    pub id: u64,\n"
        "}\n",
        encoding="utf-8",
    )
    out = extract_signatures(src)
    assert "/// Adds two numbers." in out
    assert "/// Wraps around on overflow." in out
    assert "pub fn add(a: u32, b: u32) -> u32" in out
    assert "{ a + b }" not in out
    assert "pub(crate) fn helper()" in out
    assert "private_fn" not in out
    assert "pub struct Rec" in out
    assert "pub id: u64" not in out


def test_extract_signatures_python(tmp_path: Path) -> None:
    src = tmp_path / "mod_y.py"
    src.write_text(
        "import os\n"
        "\n"
        "def public(a, b):\n"
        '    """First paragraph\n'
        "    continues here.\n"
        "\n"
        '    Second paragraph.\n'
        '    """\n'
        "    return a\n"
        "\n"
        "def _private(x):\n"
        "    return x\n"
        "\n"
        "class Thing:\n"
        '    """A thing."""\n'
        "\n"
        "    def method(self, x: int) -> int:\n"
        "        def nested(y):\n"
        "            return y\n"
        "        return x\n"
        "\n"
        "    def _hidden(self):\n"
        "        pass\n",
        encoding="utf-8",
    )
    out = extract_signatures(src)
    assert "def public(a, b):" in out
    assert "First paragraph continues here." in out
    assert "Second paragraph." not in out
    assert "class Thing:" in out
    assert "A thing." in out
    assert "def method(self, x: int) -> int:" in out
    assert "_private" not in out
    assert "_hidden" not in out
    assert "nested" not in out


def test_extract_signatures_rejects_other_suffixes(tmp_path: Path) -> None:
    src = tmp_path / "notes.md"
    src.write_text("# notes\n", encoding="utf-8")
    with pytest.raises(ValueError):
        extract_signatures(src)
