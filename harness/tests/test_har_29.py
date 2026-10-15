"""HAR-29: pgarbor_harness.cli argument parsing, dispatch and exit codes (offline)."""

import pytest

from pgarbor_harness.cli import build_parser, main

LISTED_COMMANDS = [
    ["oracle", "up"],
    ["oracle", "up", "--recreate"],
    ["oracle", "down"],
    ["oracle", "status"],
    ["oracle", "fingerprint"],
    ["cases", "validate"],
    ["cases", "validate", "tests/cases/smoke"],
    ["record"],
    ["record", "--family", "smoke", "--id", "smoke.*", "--rerecord", "--dry-run", "--jobs", "2"],
    ["diff", "--target", "pgarbor"],
    ["diff", "--target", "pg", "--family", "smoke", "--id", "smoke.*", "--jobs", "2",
     "--out", "runs/x"],
    ["diff", "--target", "dsn", "--dsn", "host=127.0.0.1 port=5432 dbname=postgres",
     "--binary", "bin/pgarbor"],
    ["minimize", "--case", "smoke.create-insert-select", "--target", "pgarbor"],
    ["minimize", "--case", "smoke.x", "--target", "pg", "--max-runs", "50"],
    ["regress"],
    ["regress", "--target", "pg", "--only", "boolean,char", "--use-existing"],
    ["regress", "--record-baseline"],
    ["isolation"],
    ["isolation", "--target", "pg", "--only", "deadlock-simple", "--jobs", "1"],
    ["slt", "run", "tests/expected/slt/a.slt"],
    ["slt", "record", "tests/slt/a.slt", "tests/slt/b.slt"],
    ["wire", "record", "--client", "psql", "--listen-port", "0"],
    ["wire", "replay", "tests/expected/wire/psql.jsonl", "--target", "pgarbor"],
    ["generate", "--generator", "operators", "--seed", "1"],
    ["generate", "--generator", "ddl-dml", "--seed", "42", "--out", "tests/cases"],
    ["selftest"],
    ["selftest", "--live"],
    ["manifest", "freeze"],
    ["manifest", "check"],
]


@pytest.mark.parametrize("argv", LISTED_COMMANDS, ids=lambda a: " ".join(a))
def test_build_parser_accepts_listed_command(argv):
    args = build_parser().parse_args(argv)
    assert callable(getattr(args, "func", None))


def test_global_json_and_repo_flags_before_and_after_subcommand():
    parser = build_parser()
    before = parser.parse_args(["--json", "--repo", "/tmp/repo", "selftest"])
    after = parser.parse_args(["selftest", "--json", "--repo", "/tmp/repo"])
    assert before.json is True and after.json is True
    assert before.repo == "/tmp/repo" and after.repo == "/tmp/repo"
    plain = parser.parse_args(["selftest"])
    assert plain.json is False and plain.repo is None


def test_unknown_subcommand_exits_2(capsys):
    with pytest.raises(SystemExit) as excinfo:
        main(["no-such-command"])
    assert excinfo.value.code == 2


def test_missing_subcommand_exits_2():
    with pytest.raises(SystemExit) as excinfo:
        main([])
    assert excinfo.value.code == 2


def test_diff_requires_target():
    with pytest.raises(SystemExit) as excinfo:
        build_parser().parse_args(["diff"])
    assert excinfo.value.code == 2


def test_cases_validate_on_example_corpus_exits_0():
    assert main(["cases", "validate"]) == 0


def test_selftest_offline_exits_0():
    assert main(["selftest"]) == 0
