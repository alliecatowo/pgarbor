"""Wire smoke test: start `pgarbor serve` on a fresh .pga, drive it with psycopg (real PG driver)."""
import os, subprocess, sys, tempfile, time
import psycopg

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target/debug/pgarbor")
QUERIES = [
    "SELECT 1", "SELECT version()", "SHOW server_version",
    "CREATE TABLE t(a int primary key, b text, c numeric, d timestamptz default now())",
    "INSERT INTO t(a,b,c) VALUES (1,'x',1.5),(2,'y',NULL)",
    "SELECT a,b,c FROM t ORDER BY a", "UPDATE t SET b = b || '!' WHERE a = 1",
    "SELECT count(*), sum(c) FROM t", "DELETE FROM t WHERE a = 2",
    "SELECT current_schema(), current_user",
    "SELECT table_name FROM information_schema.tables WHERE table_schema='public'",
    "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relname='t'",
    "BEGIN", "INSERT INTO t(a,b) VALUES (3,'z')", "ROLLBACK", "SELECT count(*) FROM t",
    "INSERT INTO t(a) VALUES (1)",
]

def main():
    d = tempfile.mkdtemp(prefix="pgarbor-wire-")
    db = os.path.join(d, "w.pga")
    subprocess.run([BIN, "init", db], check=True, capture_output=True)
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 55433
    srv = subprocess.Popen([BIN, "serve", db, "--port", str(port), "--auth", "trust"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    ok = fail = 0
    try:
        time.sleep(0.5)
        c = psycopg.connect(f"postgres://postgres@127.0.0.1:{port}/postgres?sslmode=disable",
                            autocommit=True, connect_timeout=5)
        print("connected; server_version", c.info.parameter_status("server_version"))
        for q in QUERIES:
            try:
                cur = c.execute(q)
                rows = cur.fetchall() if cur.description else ""
                print(f"OK   {q} -> {cur.statusmessage} {rows}"); ok += 1
            except Exception as e:
                print(f"ERR  {q} -> {type(e).__name__}: {str(e).splitlines()[0]}"); fail += 1
        def check(name, got, want):
            nonlocal ok, fail
            if got == want:
                print(f"OK   check {name} -> {got!r}"); ok += 1
            else:
                print(f"ERR  check {name} -> got {got!r}, want {want!r}"); fail += 1
        try:
            n = lambda: c.execute("SELECT count(*) FROM t").fetchone()[0]
            check("UPDATE changed value", c.execute("SELECT b FROM t WHERE a = 1").fetchone()[0], "x!")
            check("information_schema filter",
                  [r[0] for r in c.execute("SELECT table_name FROM information_schema.tables WHERE table_schema='public'")], ["t"])
            before = n()
            c.execute("BEGIN"); c.execute("INSERT INTO t(a,b) VALUES (100,'tmp')")
            check("count inside txn", n(), before + 1)
            c.execute("ROLLBACK")
            check("ROLLBACK restores count", n(), before)
        except Exception as e:
            print("ERR  checks ->", type(e).__name__, str(e).splitlines()[0]); fail += 1
        try:
            cur = c.execute("SELECT a, b FROM t WHERE a = %s", (1,))
            print("OK   extended/params ->", cur.fetchall()); ok += 1
        except Exception as e:
            print("ERR  extended/params ->", type(e).__name__, str(e).splitlines()[0]); fail += 1
    except Exception as e:
        print("CONNECT FAIL", type(e).__name__, e); fail += 1
    finally:
        srv.terminate()
        out = srv.communicate(timeout=5)[0]
        if fail: print("--- server log ---\n" + out[-2000:])
    print(f"summary: {ok} ok, {fail} err")

if __name__ == "__main__":
    main()
