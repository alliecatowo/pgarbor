"""Actual CLI/wire acceptance gate; fake target helper tests remain separate.

PGARBOR_REAL_BINARY=/absolute/path/to/pgarbor uv run pytest tests/test_real_pgarbor_target.py
The live gate fails on unsupported SQL; it never counts an unavailable binary as coverage.
"""
import os
from pathlib import Path
import pytest
from pgarbor_harness.runner.targets import PgarborCommand, PgarborTarget


def test_default_commands_match_frozen_real_cli(tmp_path):
    target = PgarborTarget(tmp_path, PgarborCommand(binary="/bin/pgarbor"))
    file = tmp_path / "fresh.pga"
    assert target._argv(target.command.create, file, None) == ["/bin/pgarbor", "create", str(file)]
    assert target._argv(target.command.serve, file, 5440) == [
        "/bin/pgarbor", "serve", str(file), "--host", "127.0.0.1", "--port", "5440", "--branch", "main"]


@pytest.mark.skipif(not os.environ.get("PGARBOR_REAL_BINARY"),
                    reason="opt in with PGARBOR_REAL_BINARY; no live SQL coverage without it")
def test_real_target_select_two_sessions_and_crash_reopen(tmp_path):
    import psycopg
    target = PgarborTarget(tmp_path, PgarborCommand(binary=os.environ["PGARBOR_REAL_BINARY"]))
    target.start()
    try:
        env = target.provision("acceptance.real-target")
        try:
            with psycopg.connect(env.endpoint.conninfo(), autocommit=True) as first:
                assert first.execute("SELECT 1").fetchone() == (1,)
                first.execute("CREATE TABLE gate_rows (id int PRIMARY KEY, value text)")
                first.execute("INSERT INTO gate_rows VALUES (1, 'durable')")
                with psycopg.connect(env.endpoint.conninfo(), autocommit=True) as second:
                    assert second.execute("SELECT id, value FROM gate_rows").fetchall() == [(1, "durable")]
            target.restart(env, "kill")
            with psycopg.connect(env.endpoint.conninfo(), autocommit=True) as reopened:
                assert reopened.execute("SELECT id, value FROM gate_rows").fetchall() == [(1, "durable")]
        finally:
            target.release(env)
            assert not target.is_alive(env)
            assert not any(p.suffix == ".pga" for p in tmp_path.iterdir())
    finally:
        target.stop()
