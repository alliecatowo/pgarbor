use std::process::ExitCode;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

type Res = Result<(), Box<dyn std::error::Error>>;

#[derive(Debug, FromRow)]
struct User {
    #[allow(dead_code)]
    id: i32,
    email: String,
    name: String,
    role: String,
    active: bool,
    age: Option<i32>,
    created_at: DateTime<Utc>,
}

macro_rules! check {
    ($c:expr, $($m:tt)+) => {
        if !$c { return Err(format!($($m)+).into()); }
    };
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code().map(|c| c.to_string()))
}

async fn insert_user(pool: &PgPool, email: &str, name: &str, role: &str, age: Option<i32>) -> Result<i32, sqlx::Error> {
    let row = sqlx::query("INSERT INTO users (email, name, role, age) VALUES ($1, $2, $3::user_role, $4) RETURNING id")
        .bind(email)
        .bind(name)
        .bind(role)
        .bind(age)
        .fetch_one(pool)
        .await?;
    row.try_get::<i32, _>("id")
}

async fn insert_post(pool: &PgPool, author: i32, title: &str, views: i64, price: Option<&str>, tags: &[&str], meta: Value, published: bool) -> Result<i64, sqlx::Error> {
    let price: Option<Decimal> = price.map(|p| p.parse().unwrap());
    let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO posts (author_id, title, views, price, tags, meta, published) VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(author).bind(title).bind(views).bind(price).bind(tags).bind(meta).bind(published)
    .fetch_one(pool)
    .await
}

async fn count(pool: &PgPool, table: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}")).fetch_one(pool).await
}

async fn crud(pool: &PgPool) -> Vec<(&'static str, Res)> {
    let mut out: Vec<(&'static str, Res)> = Vec::new();
    macro_rules! step {
        ($name:expr, $body:expr) => {{
            let r: Res = async { $body }.await;
            out.push(($name, r));
        }};
    }

    let mut alice = 0;
    let mut bob = 0;
    let mut carol = 0;
    step!("crud.insert_returning_users", {
        alice = insert_user(pool, "alice@example.com", "Alice", "admin", Some(34)).await?;
        bob = insert_user(pool, "bob@example.com", "Bob", "member", Some(27)).await?;
        carol = insert_user(pool, "carol@example.com", "Carol", "guest", None).await?;
        check!(alice > 0 && bob > alice && carol > bob, "serial ids not increasing: {alice} {bob} {carol}");
        let u: User = sqlx::query_as("SELECT id, email, name, role::text AS role, active, age, created_at FROM users WHERE id = $1")
            .bind(alice).fetch_one(pool).await?;
        check!(u.email == "alice@example.com" && u.name == "Alice" && u.role == "admin" && u.active && u.age == Some(34), "bad row {u:?}");
        check!((Utc::now() - u.created_at).num_seconds().abs() < 300, "created_at default off: {}", u.created_at);
        Ok(())
    });

    let mut p1 = 0;
    let mut p2 = 0;
    let mut p3 = 0;
    step!("crud.insert_returning_posts", {
        p1 = insert_post(pool, alice, "Hello", 10, Some("9.99"), &["intro", "rust"], json!({"lang":"en","stars":5,"nested":{"k":"v"}}), true).await?;
        p2 = insert_post(pool, alice, "Second", 100, Some("19.50"), &["rust"], json!({"lang":"en","stars":3}), true).await?;
        p3 = insert_post(pool, bob, "Bob's post", 5, None, &[], json!({"lang":"de","stars":4}), false).await?;
        check!(p1 > 0 && p2 > p1 && p3 > p2, "identity ids not increasing");
        let c = sqlx::query("INSERT INTO comments (post_id, user_id, body, scores) VALUES ($1,$2,$3,$4) RETURNING id, scores")
            .bind(p1).bind(bob).bind("nice").bind(vec![1i32, 2, 3]).fetch_one(pool).await?;
        let id: Uuid = c.try_get("id")?;
        let scores: Vec<i32> = c.try_get("scores")?;
        check!(!id.is_nil() && scores == vec![1, 2, 3], "comment returning mismatch");
        sqlx::query("INSERT INTO comments (post_id, user_id, body) VALUES ($1,$2,$3)").bind(p1).bind(carol).bind("meh").execute(pool).await?;
        sqlx::query("INSERT INTO comments (id, post_id, user_id, body) VALUES ($1,$2,$3,$4)").bind(Uuid::new_v4()).bind(p2).bind(bob).bind("second!").execute(pool).await?;
        Ok(())
    });

    step!("query.join_filter_order_limit", {
        let rows = sqlx::query(
            "SELECT p.title, u.name, p.views FROM posts p JOIN users u ON u.id = p.author_id \
             WHERE p.published AND p.views >= $1 ORDER BY p.views DESC LIMIT 1",
        ).bind(10i64).fetch_all(pool).await?;
        check!(rows.len() == 1, "expected 1 row, got {}", rows.len());
        check!(rows[0].get::<String, _>("title") == "Second" && rows[0].get::<String, _>("name") == "Alice", "wrong join result");
        let left = sqlx::query("SELECT u.name, count(c.id) AS n FROM users u LEFT JOIN comments c ON c.user_id = u.id GROUP BY u.name ORDER BY n DESC, u.name")
            .fetch_all(pool).await?;
        check!(left.len() == 3 && left[0].get::<String, _>("name") == "Bob" && left[0].get::<i64, _>("n") == 2, "left join result wrong");
        Ok(())
    });

    step!("query.numeric_array_types", {
        let p: Decimal = sqlx::query_scalar("SELECT price FROM posts WHERE id = $1").bind(p2).fetch_one(pool).await?;
        check!(p == "19.50".parse::<Decimal>()?, "numeric roundtrip: {p}");
        let tags: Vec<String> = sqlx::query_scalar("SELECT tags FROM posts WHERE id = $1").bind(p1).fetch_one(pool).await?;
        check!(tags == ["intro", "rust"], "tags: {tags:?}");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM posts WHERE $1 = ANY(tags)").bind("rust").fetch_one(pool).await?;
        check!(n == 2, "ANY(tags) count {n}");
        let none: Option<Decimal> = sqlx::query_scalar("SELECT price FROM posts WHERE id = $1").bind(p3).fetch_one(pool).await?;
        check!(none.is_none(), "null numeric");
        Ok(())
    });

    step!("query.json", {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM posts WHERE meta @> $1::jsonb").bind(json!({"lang":"en"})).fetch_one(pool).await?;
        check!(n == 2, "jsonb @> count {n}");
        let v: String = sqlx::query_scalar("SELECT meta->'nested'->>'k' FROM posts WHERE id = $1").bind(p1).fetch_one(pool).await?;
        check!(v == "v", "nested json {v}");
        let rows = sqlx::query_scalar::<_, i64>("SELECT id FROM posts WHERE (meta->>'stars')::int >= 4 ORDER BY id").fetch_all(pool).await?;
        check!(rows == vec![p1, p3], "stars filter {rows:?}");
        let m: Value = sqlx::query_scalar("SELECT meta FROM posts WHERE id = $1").bind(p2).fetch_one(pool).await?;
        check!(m == json!({"lang":"en","stars":3}), "jsonb value {m}");
        Ok(())
    });

    step!("query.aggregate", {
        let r = sqlx::query("SELECT count(*) AS c, sum(views)::bigint AS s, max(views) AS mx, avg(price)::numeric(10,2) AS a FROM posts").fetch_one(pool).await?;
        check!(r.get::<i64, _>("c") == 3 && r.get::<i64, _>("s") == 115 && r.get::<i64, _>("mx") == 100, "aggregate basics");
        let a: Decimal = r.get("a");
        check!(a == "14.75".parse::<Decimal>()?, "avg(price) = {a}");
        let g = sqlx::query("SELECT author_id, count(*) AS n FROM posts GROUP BY author_id HAVING count(*) > 1").fetch_all(pool).await?;
        check!(g.len() == 1 && g[0].get::<i32, _>("author_id") == alice, "group by having");
        Ok(())
    });

    step!("crud.update", {
        let r = sqlx::query("UPDATE posts SET views = views + 1, slug = $2 WHERE id = $1").bind(p1).bind("hello").execute(pool).await?;
        check!(r.rows_affected() == 1, "rows_affected");
        let (v, s): (i64, Option<String>) = sqlx::query_as("SELECT views, slug FROM posts WHERE id = $1").bind(p1).fetch_one(pool).await?;
        check!(v == 11 && s.as_deref() == Some("hello"), "update result {v} {s:?}");
        let name: String = sqlx::query_scalar("UPDATE users SET name = $2, settings = settings || $3 WHERE id = $1 RETURNING name").bind(carol).bind("Carol II").bind(json!({"theme":"dark"})).fetch_one(pool).await?;
        check!(name == "Carol II", "update returning");
        Ok(())
    });

    step!("tx.commit", {
        let before = count(pool, "users").await?;
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO users (email, name) VALUES ('tx1@example.com','Tx1')").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO users (email, name) VALUES ('tx2@example.com','Tx2')").execute(&mut *tx).await?;
        tx.commit().await?;
        check!(count(pool, "users").await? == before + 2, "commit not visible");
        Ok(())
    });

    step!("tx.rollback", {
        let before = count(pool, "users").await?;
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO users (email, name) VALUES ('rb@example.com','Rb')").execute(&mut *tx).await?;
        tx.rollback().await?;
        check!(count(pool, "users").await? == before, "rollback leaked rows");
        // failed statement inside a transaction aborts it; dropping rolls back
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO users (email, name) VALUES ('rb2@example.com','Rb2')").execute(&mut *tx).await?;
        let e = sqlx::query("INSERT INTO users (email, name) VALUES ('alice@example.com','Dup')").execute(&mut *tx).await;
        check!(e.is_err(), "expected dup error in tx");
        drop(tx);
        check!(count(pool, "users").await? == before, "implicit rollback leaked rows");
        Ok(())
    });

    step!("error.unique_violation", {
        let e = insert_user(pool, "alice@example.com", "Dup", "member", None).await.err().ok_or("expected unique violation")?;
        check!(sqlstate(&e).as_deref() == Some("23505"), "expected 23505, got {e}");
        let d = e.as_database_error().unwrap();
        check!(d.constraint() == Some("users_email_key"), "constraint name {:?}", d.constraint());
        let e = insert_post(pool, alice, "Hello", 0, None, &[], json!({}), false).await.err().ok_or("expected unique violation 2")?;
        check!(sqlstate(&e).as_deref() == Some("23505") && e.as_database_error().unwrap().constraint() == Some("posts_author_title_key"), "composite unique: {e}");
        let e = sqlx::query("UPDATE posts SET slug = 'hello' WHERE id = $1").bind(p2).execute(pool).await.err().ok_or("expected partial unique violation")?;
        check!(sqlstate(&e).as_deref() == Some("23505"), "partial unique: {e}");
        Ok(())
    });

    step!("error.fk_check_enum", {
        let e = insert_post(pool, 999_999, "Orphan", 0, None, &[], json!({}), false).await.err().ok_or("expected fk violation")?;
        check!(sqlstate(&e).as_deref() == Some("23503"), "expected 23503, got {e}");
        let e = sqlx::query("INSERT INTO comments (post_id, body) VALUES ($1, '')").bind(p1).execute(pool).await.err().ok_or("expected check violation")?;
        check!(sqlstate(&e).as_deref() == Some("23514"), "expected 23514, got {e}");
        let e = insert_user(pool, "x@example.com", "X", "root", None).await.err().ok_or("expected enum error")?;
        check!(sqlstate(&e).as_deref() == Some("22P02"), "expected 22P02, got {e}");
        let e = sqlx::query("INSERT INTO users (email, name) VALUES (NULL, 'n')").execute(pool).await.err().ok_or("expected not-null")?;
        check!(sqlstate(&e).as_deref() == Some("23502"), "expected 23502, got {e}");
        Ok(())
    });

    step!("crud.delete_cascade", {
        let r = sqlx::query("DELETE FROM comments WHERE body = 'meh'").execute(pool).await?;
        check!(r.rows_affected() == 1, "delete comment");
        // user delete: posts cascade, comments.user_id set null
        let cc: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE user_id = $1").bind(bob).fetch_one(pool).await?;
        check!(cc == 2, "bob comments before = {cc}");
        sqlx::query("DELETE FROM users WHERE id = $1").bind(bob).execute(pool).await?;
        check!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM posts WHERE author_id = $1").bind(bob).fetch_one(pool).await? == 0, "post cascade");
        let nulls: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE user_id IS NULL").fetch_one(pool).await?;
        check!(nulls == 2, "set null count {nulls}");
        // post delete cascades comments
        sqlx::query("DELETE FROM users WHERE id = $1").bind(alice).execute(pool).await?;
        check!(count(pool, "posts").await? == 0 && count(pool, "comments").await? == 0, "cascade to posts/comments");
        Ok(())
    });
    out
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Sig(Vec<String>);

/// Introspect `schema` into a normalized, schema-independent signature.
async fn signature(pool: &PgPool, schema: &str) -> Result<Sig, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    sqlx::query(&format!("SET search_path TO {schema}, public")).execute(&mut *conn).await?;
    let mut lines = Vec::new();
    let cols = sqlx::query(
        "SELECT c.relname, a.attnum, a.attname, format_type(a.atttypid, a.atttypmod) AS typ, a.attnotnull, a.attidentity::text AS ident \
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped ORDER BY c.relname, a.attnum",
    ).bind(schema).fetch_all(&mut *conn).await?;
    for r in cols {
        lines.push(format!("col {}.{} {} nn={} ident={}", r.get::<String, _>("relname"), r.get::<String, _>("attname"), r.get::<String, _>("typ"), r.get::<bool, _>("attnotnull"), r.get::<String, _>("ident")));
    }
    let cons = sqlx::query(
        "SELECT c.relname, k.conname, k.contype::text AS ct, pg_get_constraintdef(k.oid) AS def \
         FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 ORDER BY 1, 2",
    ).bind(schema).fetch_all(&mut *conn).await?;
    for r in cons {
        lines.push(format!("con {} {} {} {}", r.get::<String, _>("relname"), r.get::<String, _>("conname"), r.get::<String, _>("ct"), r.get::<String, _>("def")));
    }
    let idx = sqlx::query("SELECT tablename, indexname, indexdef FROM pg_indexes WHERE schemaname = $1 ORDER BY 1, 2").bind(schema).fetch_all(&mut *conn).await?;
    for r in idx {
        let def: String = r.get("indexdef");
        let def = def.replace(&format!("{schema}."), "");
        lines.push(format!("idx {} {} {}", r.get::<String, _>("tablename"), r.get::<String, _>("indexname"), def));
    }
    sqlx::query("RESET search_path").execute(&mut *conn).await?;
    Ok(Sig(lines))
}

async fn introspect(pool: &PgPool) -> Vec<(&'static str, Res)> {
    let mut out: Vec<(&'static str, Res)> = Vec::new();
    macro_rules! step {
        ($name:expr, $body:expr) => {{
            let r: Res = async { $body }.await;
            out.push(($name, r));
        }};
    }
    step!("introspect.expected_schema", {
        let sig = signature(pool, "public").await?;
        let text = sig.0.join("\n");
        for want in [
            "col users.id integer nn=true ident=",
            "col users.email character varying(255) nn=true",
            "col users.role user_role nn=true",
            "col users.settings jsonb nn=true",
            "col posts.id bigint nn=true ident=a",
            "col posts.title character varying(300) nn=true",
            "col posts.price numeric(10,2) nn=false",
            "col posts.tags text[] nn=true",
            "col comments.id uuid nn=true",
            "col comments.scores integer[] nn=true",
            "FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE CASCADE",
            "FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE SET NULL",
            "idx posts posts_author_id_idx",
            "WHERE (slug IS NOT NULL)",
        ] {
            check!(text.contains(want), "introspection missing `{want}` in:\n{text}");
        }
        let enums: Vec<String> = sqlx::query_scalar("SELECT e.enumlabel::text FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'user_role' ORDER BY e.enumsortorder").fetch_all(pool).await?;
        check!(enums == ["admin", "member", "guest"], "enum labels {enums:?}");
        let migs: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(pool).await?;
        check!(migs == 4, "applied migrations = {migs}");
        Ok(())
    });
    step!("introspect.roundtrip_ddl", {
        // Rebuild the introspected tables as DDL in a scratch schema, re-introspect, compare.
        let orig = signature(pool, "public").await?;
        sqlx::query("DROP SCHEMA IF EXISTS rt_copy CASCADE").execute(pool).await?;
        sqlx::query("CREATE SCHEMA rt_copy").execute(pool).await?;
        let mut conn = pool.acquire().await?;
        sqlx::query("SET search_path TO rt_copy, public").execute(&mut *conn).await?;
        let tables: Vec<String> = sqlx::query_scalar("SELECT relname::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND relkind='r' AND relname <> '_sqlx_migrations' ORDER BY (relname='users') DESC, (relname='posts') DESC, relname")
            .fetch_all(&mut *conn).await?;
        for t in &tables {
            let cols = sqlx::query(
                "SELECT a.attname::text AS n, format_type(a.atttypid, a.atttypmod) AS typ, a.attnotnull, a.attidentity::text AS ident, \
                 pg_get_expr(d.adbin, d.adrelid) AS dflt FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum \
                 WHERE a.attrelid = ('public.' || quote_ident($1))::regclass AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum",
            ).bind(t).fetch_all(&mut *conn).await?;
            let mut defs = Vec::new();
            for c in cols {
                let mut s = format!("\"{}\" {}", c.get::<String, _>("n"), c.get::<String, _>("typ"));
                let ident: String = c.get("ident");
                if ident == "a" { s.push_str(" GENERATED ALWAYS AS IDENTITY"); }
                else if ident == "d" { s.push_str(" GENERATED BY DEFAULT AS IDENTITY"); }
                else if let Some(d) = c.get::<Option<String>, _>("dflt") { if !d.starts_with("nextval(") { s.push_str(&format!(" DEFAULT {d}")); } }
                if c.get::<bool, _>("attnotnull") { s.push_str(" NOT NULL"); }
                defs.push(s);
            }
            let cons = sqlx::query("SELECT conname::text AS n, pg_get_constraintdef(oid) AS d FROM pg_constraint WHERE conrelid = ('public.' || quote_ident($1))::regclass AND contype IN ('p','u','c') ORDER BY conname").bind(t).fetch_all(&mut *conn).await?;
            for c in cons { defs.push(format!("CONSTRAINT \"{}\" {}", c.get::<String, _>("n"), c.get::<String, _>("d"))); }
            let ddl = format!("CREATE TABLE rt_copy.\"{t}\" (\n  {}\n)", defs.join(",\n  "));
            sqlx::query(&ddl).execute(&mut *conn).await.map_err(|e| format!("replaying DDL failed: {e}\n{ddl}"))?;
        }
        for t in &tables {
            let fks = sqlx::query("SELECT conname::text AS n, pg_get_constraintdef(oid) AS d FROM pg_constraint WHERE conrelid = ('public.' || quote_ident($1))::regclass AND contype='f' ORDER BY conname").bind(t).fetch_all(&mut *conn).await?;
            for c in fks {
                let ddl = format!("ALTER TABLE rt_copy.\"{t}\" ADD CONSTRAINT \"{}\" {}", c.get::<String, _>("n"), c.get::<String, _>("d").replace("REFERENCES public.", "REFERENCES "));
                sqlx::query(&ddl).execute(&mut *conn).await.map_err(|e| format!("replaying FK failed: {e}\n{ddl}"))?;
            }
        }
        let idx = sqlx::query("SELECT indexdef FROM pg_indexes WHERE schemaname='public' AND tablename <> '_sqlx_migrations' AND indexname NOT IN (SELECT conname FROM pg_constraint WHERE contype IN ('p','u')) ORDER BY indexname").fetch_all(&mut *conn).await?;
        for r in idx {
            let def: String = r.get("indexdef");
            let ddl = def.replace(" public.", " rt_copy.");
            sqlx::query(&ddl).execute(&mut *conn).await.map_err(|e| format!("replaying index failed: {e}\n{ddl}"))?;
        }
        drop(conn);
        let copy = signature(pool, "rt_copy").await?;
        // The original signature also lists _sqlx_migrations; drop it for comparison.
        let strip = |s: &Sig| -> Vec<String> { s.0.iter().filter(|l| !l.contains("_sqlx_migrations")).cloned().collect() };
        let (a, b) = (strip(&orig), strip(&copy));
        if a != b {
            let mut diff = String::new();
            for l in a.iter().filter(|l| !b.contains(l)) { diff.push_str(&format!("- {l}\n")); }
            for l in b.iter().filter(|l| !a.contains(l)) { diff.push_str(&format!("+ {l}\n")); }
            return Err(format!("round-trip signature mismatch:\n{diff}").into());
        }
        check!(a.len() > 20, "suspiciously small signature ({})", a.len());
        sqlx::query("DROP SCHEMA rt_copy CASCADE").execute(pool).await?;
        Ok(())
    });
    out
}

async fn run() -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let opts: PgConnectOptions = url.parse()?;
    let pool = PgPoolOptions::new().max_connections(5).connect_with(opts).await?;

    let mut results: Vec<(&'static str, Res)> = Vec::new();

    // Migrations, from scratch (twice: second run must be a no-op).
    let m = sqlx::migrate!("./migrations");
    let r1: Res = async { m.run(&pool).await?; Ok(()) }.await;
    let ok = r1.is_ok();
    results.push(("migrate.run", r1));
    if ok {
        let r2: Res = async {
            m.run(&pool).await?;
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations").fetch_one(&pool).await?;
            check!(n == 4, "expected 4 migration rows, got {n}");
            Ok(())
        }.await;
        results.push(("migrate.idempotent", r2));
        results.extend(crud(&pool).await);
        results.extend(introspect(&pool).await);
    }

    let (mut passed, mut failed) = (0, 0);
    for (name, r) in &results {
        match r {
            Ok(()) => { passed += 1; println!("ok   {name}"); }
            Err(e) => { failed += 1; eprintln!("FAIL {name}: {e}"); }
        }
    }
    Ok((passed, failed, results.len()))
}

#[tokio::main]
async fn main() -> ExitCode {
    let (passed, failed, steps) = match run().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FATAL: {e}");
            println!("FIXTURE_RESULT {}", json!({"name":"sqlx","passed":0,"failed":1,"steps":1}));
            return ExitCode::FAILURE;
        }
    };
    println!("FIXTURE_RESULT {}", json!({"name":"sqlx","passed":passed,"failed":failed,"steps":steps}));
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
