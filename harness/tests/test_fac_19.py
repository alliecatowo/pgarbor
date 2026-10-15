"""FAC-19: the factory command line (parser coverage, dispatch, exit codes).

Lease FAC-19. Each documented invocation of pgarbor_factory.cli must parse; usage errors exit 2;
``dag validate`` exits 0 on the real task graph; ``guard check`` exits 3 on violations.
"""

from __future__ import annotations

import json

import pytest

from pgarbor_factory import cli, guard
from pgarbor_factory.cli import build_parser, main

DOCUMENTED_INVOCATIONS: list[list[str]] = [
    ["dag", "validate"],
    ["dag", "validate", "--graph", "graph.json"],
    ["dag", "validate", "--leases"],
    ["dag", "waves"],
    ["leases", "sync"],
    ["leases", "list"],
    ["leases", "list", "--state", "SPECULATING"],
    ["leases", "grant", "ARCH-01", "--holder", "worker-1"],
    ["leases", "grant", "ARCH-01", "--holder", "worker-1", "--ttl", "600"],
    ["leases", "release", "ARCH-01"],
    ["leases", "expire"],
    ["inventory"],
    ["inventory", "--json"],
    ["packet", "ARCH-01"],
    ["worktree", "ARCH-01"],
    ["submit", "ARCH-01", "--worker", "w1"],
    ["submit", "ARCH-01", "--worker", "w1", "--assumption", "a", "--behavior", "b",
     "--unresolved", "u", "--assumption", "a2"],
    ["review", "ARCH-01"],
    ["assemble", "--epoch", "1", "--base", "abc123"],
    ["build"],
    ["build", "--worktree", "wt"],
    ["campaign", "run"],
    ["campaign", "run", "--epoch", "1", "--mode", "seams", "--only", "differential",
     "--only", "regress", "--jobs", "2"],
    ["guard", "check"],
    ["report", "runs/example"],
    ["repair", "--from", "runs/example"],
    ["halt", "--budget-epochs", "5", "--window", "2"],
    ["schedule", "--max-authors", "12"],
]

USAGE_ERRORS: list[list[str]] = [
    [],
    ["bogus"],
    ["dag"],
    ["dag", "validate", "--nope"],
    ["leases", "grant", "ARCH-01"],  # --holder is required
    ["campaign", "run", "--mode", "weird"],
    ["halt", "--budget-epochs", "0"],
    ["assemble", "--epoch", "1"],  # --base is required
    ["submit", "ARCH-01"],  # --worker is required
    ["schedule", "--max-authors", "-3"],
]


@pytest.mark.parametrize("argv", DOCUMENTED_INVOCATIONS, ids=lambda a: " ".join(a))
def test_parser_accepts_documented_invocation(argv: list[str]) -> None:
    ns = build_parser().parse_args(argv)
    assert callable(getattr(ns, "func", None))


def test_global_options_accepted_before_and_after_subcommand() -> None:
    before = build_parser().parse_args(["--repo", "/x", "--json", "inventory"])
    after = build_parser().parse_args(["inventory", "--repo", "/x", "--json"])
    assert before.repo == "/x" and before.json is True
    assert after.repo == "/x" and after.json is True


def test_defaults_are_documented_values() -> None:
    ns = build_parser().parse_args(["leases", "grant", "ARCH-01", "--holder", "h"])
    assert ns.ttl == cli.DEFAULT_TTL_S
    ns = build_parser().parse_args(["campaign", "run"])
    assert ns.mode == "cathedral" and ns.jobs == cli.DEFAULT_JOBS and ns.epoch == 0
    ns = build_parser().parse_args(["halt", "--budget-epochs", "5"])
    assert ns.window == 3


@pytest.mark.parametrize("argv", USAGE_ERRORS, ids=lambda a: " ".join(a) or "<none>")
def test_usage_errors_exit_2(argv: list[str], capsys: pytest.CaptureFixture[str]) -> None:
    assert main(argv) == 2
    assert capsys.readouterr().err  # argparse explains the error on stderr


def test_dag_validate_exits_0_on_real_graph() -> None:
    assert main(["dag", "validate"]) == 0


def test_dag_validate_exits_1_on_invalid_graph(tmp_path, capsys: pytest.CaptureFixture[str]) -> None:
    bad = tmp_path / "bad_graph.json"
    bad.write_text(json.dumps({"tasks": [
        {"id": "A", "integration_dependencies": ["ZZ-99"], "contract_dependencies": []},
    ]}), encoding="utf-8")
    assert main(["dag", "validate", "--graph", str(bad)]) == 1
    out = capsys.readouterr().out
    assert "unknown_dependency" in out


def test_guard_check_exits_3_on_violations(monkeypatch: pytest.MonkeyPatch) -> None:
    report = guard.GuardReport(
        ok=False, violations=[guard.GuardViolation(kind="denominator_shrank", path="tests/cases")])
    monkeypatch.setattr(guard, "check_campaign_preconditions",
                        lambda repo_root, run_selftest=True: report)
    assert main(["guard", "check"]) == 3


def test_guard_check_exits_0_when_ok(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(guard, "check_campaign_preconditions",
                        lambda repo_root, run_selftest=True: guard.GuardReport(ok=True))
    assert main(["guard", "check", "--json"]) == 0


def test_internal_error_exits_4(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
    def boom(repo_root, run_selftest=True):
        raise RuntimeError("simulated internal fault")

    monkeypatch.setattr(guard, "check_campaign_preconditions", boom)
    assert main(["guard", "check"]) == 4
    assert "simulated internal fault" in capsys.readouterr().err


def test_exit_code_constants() -> None:
    assert (cli.EXIT_OK, cli.EXIT_INVALID, cli.EXIT_USAGE, cli.EXIT_GUARD, cli.EXIT_INTERNAL) == (
        0, 1, 2, 3, 4)
