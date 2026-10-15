import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "bindings" / "python"))


@pytest.fixture(scope="session", autouse=True)
def build_lib():
    subprocess.run(["cargo", "build", "-p", "pgarbor-ffi"], cwd=ROOT, check=True)
    os.environ.setdefault("PGARBOR_LIB", str(ROOT / "target" / "debug" / "libpgarbor_ffi.so"))
    if sys.platform == "darwin":
        os.environ["PGARBOR_LIB"] = str(ROOT / "target" / "debug" / "libpgarbor_ffi.dylib")


def test_create_insert_select_error_reopen(tmp_path):
    import pgarbor

    path = str(tmp_path / "py.pga")
    conn = pgarbor.connect(path)
    assert conn.execute("SELECT 1 AS one").fetchall() == [("1",)]
    conn.execute("CREATE TABLE t(id int primary key, n text)")
    cur = conn.execute("INSERT INTO t VALUES (1,'a'),(2,NULL)")
    assert cur.rowcount == 2
    cur = conn.execute("SELECT id, n FROM t ORDER BY id")
    assert [d[0] for d in cur.description] == ["id", "n"]
    assert cur.fetchall() == [("1", "a"), ("2", None)]
    with pytest.raises(pgarbor.Error) as ei:
        conn.execute("INSERT INTO t VALUES (1,'dup')")
    assert ei.value.sqlstate == "23505"
    # session is still usable after an error
    assert conn.execute("SELECT count(*) FROM t").fetchone() == ("2",)
    conn.close()

    conn = pgarbor.connect(path, create=False)
    assert conn.execute("SELECT n FROM t WHERE id = 1").fetchall() == [("a",)]
    conn.close()


def test_branches(tmp_path):
    import pgarbor

    with pgarbor.connect(str(tmp_path / "b.pga")) as conn:
        conn.execute("CREATE TABLE t(id int)")
        conn.create_branch("feature", "main")
        assert "feature" in conn.branches()
