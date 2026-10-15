import assert from 'node:assert/strict';
import { createPool, withTransaction } from './db.js';
import { addComment, createPost, createUser, postsWithAuthors } from './repo.js';

const pool = createPool();
let passed = 0;
let failed = 0;
const failures: string[] = [];
const steps: string[] = [];

async function step(name: string, fn: () => Promise<void>): Promise<void> {
  steps.push(name);
  try {
    await fn();
    passed++;
    console.log(`ok   - ${name}`);
  } catch (e) {
    failed++;
    const msg = e instanceof Error ? (e.stack ?? e.message) : String(e);
    failures.push(name);
    console.error(`FAIL - ${name}\n${msg}`);
  }
}

const sqlstate = (e: unknown) => (e as { code?: string }).code;
async function expectCode(code: string, fn: () => Promise<unknown>) {
  try { await fn(); } catch (e) { assert.equal(sqlstate(e), code, `expected SQLSTATE ${code}, got ${String(e)}`); return e as any; }
  assert.fail(`expected SQLSTATE ${code} but no error`);
}

let alice: Awaited<ReturnType<typeof createUser>>;
let bob: Awaited<ReturnType<typeof createUser>>;
let post1: Awaited<ReturnType<typeof createPost>>;
let post2: Awaited<ReturnType<typeof createPost>>;

async function main() {
  await step('migrations applied in order', async () => {
    const r = await pool.query('SELECT name FROM pgmigrations ORDER BY id');
    assert.deepEqual(r.rows.map((x) => x.name), [
      '1700000000001_users-and-enum', '1700000000002_posts-and-comments',
      '1700000000003_orders', '1700000000004_alter-tables',
    ]);
  });

  await step('schema introspection via information_schema/pg_catalog', async () => {
    const t = await pool.query(`SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE' ORDER BY 1`);
    assert.deepEqual(t.rows.map((x) => x.table_name), ['comments', 'order_items', 'orders', 'pgmigrations', 'posts', 'products', 'users']);
    const c = await pool.query(`SELECT column_name, data_type, udt_name, is_nullable, column_default, is_identity, identity_generation
       FROM information_schema.columns WHERE table_schema='public' AND table_name='users' ORDER BY ordinal_position`);
    const by = Object.fromEntries(c.rows.map((x) => [x.column_name, x]));
    assert.equal(by.id.data_type, 'integer');
    assert.match(by.id.column_default, /nextval/);
    assert.equal(by.email.data_type, 'character varying');
    assert.equal(by.role.data_type, 'USER-DEFINED');
    assert.equal(by.role.udt_name, 'user_role');
    assert.equal(by.karma.data_type, 'bigint');
    assert.equal(by.created_at.data_type, 'timestamp with time zone');
    assert.equal(by.settings.data_type, 'jsonb');
    assert.equal(by.bio.is_nullable, 'YES');
    const cm = await pool.query(`SELECT is_identity, identity_generation FROM information_schema.columns WHERE table_name='comments' AND column_name='id'`);
    assert.equal(cm.rows[0].identity_generation, 'ALWAYS');
    const e = await pool.query(`SELECT enumlabel FROM pg_enum e JOIN pg_type t ON t.oid=e.enumtypid WHERE t.typname='user_role' ORDER BY enumsortorder`);
    assert.deepEqual(e.rows.map((x) => x.enumlabel), ['admin', 'editor', 'viewer']);
    const fk = await pool.query(`SELECT conrelid::regclass::text AS tbl, confrelid::regclass::text AS ref, confdeltype FROM pg_constraint WHERE contype='f' AND connamespace='public'::regnamespace ORDER BY 1,2`);
    assert.equal(fk.rows.length, 6);
    assert.ok(fk.rows.filter((x) => x.confdeltype === 'c').length === 5);
    const idx = await pool.query(`SELECT indexname, indexdef FROM pg_indexes WHERE schemaname='public' AND tablename='posts' ORDER BY 1`);
    const names = idx.rows.map((x) => x.indexname);
    assert.ok(names.includes('posts_tags_gin') && names.includes('posts_published_idx') && names.includes('posts_pkey'));
    assert.match(idx.rows.find((x) => x.indexname === 'posts_tags_gin').indexdef, /USING gin/);
    const ck = await pool.query(`SELECT pg_get_constraintdef(oid) AS d FROM pg_constraint WHERE conname='products_price_nonneg'`);
    assert.match(ck.rows[0].d, /price >= \(?0\)?/);
    const cmt = await pool.query(`SELECT obj_description('users'::regclass) AS c`);
    assert.equal(cmt.rows[0].c, 'Application users');
  });

  await step('insert ... returning (serial pk, enum, bigint, timestamptz)', async () => {
    alice = await createUser(pool, 'alice@example.com', 'Alice', 'admin', { theme: 'dark', n: [1, 2] });
    bob = await createUser(pool, 'bob@example.com', 'Bob');
    assert.equal(typeof alice.id, 'number');
    assert.equal(alice.role, 'admin');
    assert.equal(bob.role, 'viewer');
    assert.equal(alice.karma, 0n);
    assert.ok(alice.created_at instanceof Date);
    assert.deepEqual(alice.settings, { theme: 'dark', n: [1, 2] });
    assert.equal(alice.bio, null);
  });

  await step('insert posts (uuid pk, text[], jsonb)', async () => {
    post1 = await createPost(pool, alice.id, 'Hello World', ['intro', 'pg'], { lang: 'en', stats: { likes: 10 } }, new Date('2024-03-01T12:00:00Z'));
    post2 = await createPost(pool, bob.id, 'Second', ['pg', 'sql'], { lang: 'de', stats: { likes: 3 } });
    await createPost(pool, alice.id, 'Draft', [], {});
    assert.match(post1.id, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
    assert.deepEqual(post1.tags, ['intro', 'pg']);
    assert.equal(post1.published_at?.toISOString(), '2024-03-01T12:00:00.000Z');
    assert.equal(post1.views, 0n);
  });

  await step('identity column and comments', async () => {
    const c1 = await addComment(pool, post1.id, bob.id, 'nice');
    const c2 = await addComment(pool, post1.id, alice.id, 'thanks');
    await addComment(pool, post2.id, alice.id, 'welcome');
    assert.equal(typeof c1.id, 'bigint');
    assert.equal(c2.id, c1.id + 1n);
    assert.equal(c1.score, 1); // default changed by ALTER migration
    await expectCode('428C9', () => pool.query('INSERT INTO comments (id, post_id, user_id, body) VALUES (999, $1, $2, $3)', [post1.id, bob.id, 'x']));
  });

  await step('select with joins, filter, order, limit/offset', async () => {
    const rows = await postsWithAuthors(pool, 2);
    assert.equal(rows.length, 2);
    assert.deepEqual(rows.map((r) => [r.title, r.author, r.comment_count]), [['Hello World', 'Alice', 2n], ['Second', 'Bob', 1n]]);
    const next = await postsWithAuthors(pool, 2, 2);
    assert.deepEqual(next.map((r) => r.title), ['Draft']);
    const f = await pool.query('SELECT email FROM users WHERE role = ANY($1::user_role[]) AND name ILIKE $2 ORDER BY id DESC', [['admin', 'editor'], 'al%']);
    assert.deepEqual(f.rows, [{ email: 'alice@example.com' }]);
  });

  await step('json / jsonb / array queries', async () => {
    const a = await pool.query(`SELECT title FROM posts WHERE meta->>'lang' = $1`, ['de']);
    assert.deepEqual(a.rows, [{ title: 'Second' }]);
    const b = await pool.query(`SELECT title, (meta #>> '{stats,likes}')::int AS likes FROM posts WHERE (meta #>> '{stats,likes}')::int > $1`, [5]);
    assert.deepEqual(b.rows, [{ title: 'Hello World', likes: 10 }]);
    const c = await pool.query(`SELECT count(*)::int AS n FROM posts WHERE meta @> $1::jsonb`, [JSON.stringify({ lang: 'en' })]);
    assert.equal(c.rows[0].n, 1);
    const d = await pool.query(`SELECT title FROM posts WHERE tags @> ARRAY['pg']::text[] ORDER BY title`);
    assert.deepEqual(d.rows.map((r) => r.title), ['Hello World', 'Second']);
    const e = await pool.query(`SELECT unnest(tags) AS t, count(*)::int AS n FROM posts GROUP BY t ORDER BY n DESC, t`);
    assert.deepEqual(e.rows[0], { t: 'pg', n: 2 });
    const f = await pool.query(`SELECT jsonb_path_query_first(settings, '$.n[1]') AS v FROM users WHERE id = $1`, [alice.id]);
    assert.equal(f.rows[0].v, 2);
    await pool.query(`UPDATE users SET settings = jsonb_set(settings, '{theme}', '"light"') WHERE id = $1`, [alice.id]);
    const g = await pool.query('SELECT settings FROM users WHERE id=$1', [alice.id]);
    assert.equal(g.rows[0].settings.theme, 'light');
  });

  await step('update ... returning and array append', async () => {
    const r = await pool.query(`UPDATE posts SET views = views + $2, tags = array_append(tags, 'hot') WHERE id = $1 RETURNING views, tags`, [post1.id, 41]);
    assert.equal(r.rows[0].views, 41n);
    assert.deepEqual(r.rows[0].tags, ['intro', 'pg', 'hot']);
    const u = await pool.query('UPDATE users SET karma = karma + 9007199254740993 WHERE id=$1 RETURNING karma', [bob.id]);
    assert.equal(u.rows[0].karma, 9007199254740993n);
    assert.equal(u.rowCount, 1);
  });

  await step('count / aggregate / numeric', async () => {
    const p = await pool.query(`INSERT INTO products (sku, title, price) VALUES ('A-1','Widget',19.99),('B-2','Gadget',5.50),('C-3','Doohickey',100.00) RETURNING id, price`);
    assert.equal(p.rows[0].price, '19.99'); // numeric parsed as string
    const a = await pool.query(`SELECT count(*) AS n, sum(price) AS total, avg(price)::numeric(10,2) AS avg, max(price) AS max, min(title) AS first FROM products`);
    assert.equal(a.rows[0].n, 3n);
    assert.equal(a.rows[0].total, '125.49');
    assert.equal(a.rows[0].avg, '41.83');
    assert.equal(a.rows[0].max, '100.00');
    assert.equal(a.rows[0].first, 'Doohickey');
    const g = await pool.query(`SELECT role, count(*)::int AS n FROM users GROUP BY role HAVING count(*) >= 1 ORDER BY role`);
    assert.deepEqual(g.rows, [{ role: 'admin', n: 1 }, { role: 'viewer', n: 1 }]);
  });

  await step('transaction commit', async () => {
    const order = await withTransaction(pool, async (c) => {
      const o = await c.query(`INSERT INTO orders (user_id) VALUES ($1) RETURNING id`, [alice.id]);
      const prods = await c.query(`SELECT id, price FROM products WHERE sku = ANY($1) ORDER BY sku`, [['A-1', 'B-2']]);
      for (const [i, p] of prods.rows.entries()) {
        await c.query('INSERT INTO order_items (order_id, product_id, quantity, unit_price) VALUES ($1,$2,$3,$4)', [o.rows[0].id, p.id, i + 1, p.price]);
      }
      await c.query(`UPDATE orders SET total = (SELECT sum(quantity*unit_price) FROM order_items WHERE order_id=$1), status='paid' WHERE id=$1`, [o.rows[0].id]);
      return o.rows[0].id as number;
    });
    const r = await pool.query('SELECT status, total FROM orders WHERE id=$1', [order]);
    assert.deepEqual(r.rows[0], { status: 'paid', total: '30.99' });
  });

  await step('transaction rollback', async () => {
    const before = await pool.query('SELECT count(*)::int n FROM orders');
    await assert.rejects(withTransaction(pool, async (c) => {
      await c.query(`INSERT INTO orders (user_id, status) VALUES ($1, 'shipped')`, [alice.id]);
      throw new Error('boom');
    }), /boom/);
    const after = await pool.query('SELECT count(*)::int n FROM orders');
    assert.equal(after.rows[0].n, before.rows[0].n);
  });

  await step('savepoint rollback inside transaction', async () => {
    await withTransaction(pool, async (c) => {
      await c.query(`INSERT INTO products (sku, title, price) VALUES ('S-1','Sp1',1)`);
      await c.query('SAVEPOINT sp');
      await expectCode('23505', () => c.query(`INSERT INTO products (sku, title, price) VALUES ('S-1','dup',1)`));
      await c.query('ROLLBACK TO SAVEPOINT sp');
      await c.query(`INSERT INTO products (sku, title, price) VALUES ('S-2','Sp2',2)`);
    });
    const r = await pool.query(`SELECT sku FROM products WHERE sku LIKE 'S-%' ORDER BY sku`);
    assert.deepEqual(r.rows.map((x) => x.sku), ['S-1', 'S-2']);
  });

  await step('error handling: unique / fk / check / not null / enum / uuid', async () => {
    const e = await expectCode('23505', () => createUser(pool, 'alice@example.com', 'Dup'));
    assert.equal(e.constraint, 'users_email_key');
    assert.equal(e.table, 'users');
    assert.match(e.detail, /already exists/);
    await expectCode('23505', () => createPost(pool, alice.id, 'Hello World', [], {}));
    await expectCode('23503', () => createPost(pool, 987654, 'Orphan', [], {}));
    await expectCode('23514', () => pool.query(`INSERT INTO products (sku,title,price) VALUES ('N','neg',-1)`));
    await expectCode('23514', () => pool.query(`INSERT INTO order_items VALUES (1,1,0,1)`));
    await expectCode('23502', () => pool.query(`INSERT INTO users (email, name) VALUES ('x@y.z', NULL)`));
    await expectCode('22P02', () => pool.query(`INSERT INTO users (email, name, role) VALUES ('q@y.z','Q','root')`));
    await expectCode('22P02', () => pool.query(`SELECT * FROM posts WHERE id = $1`, ['not-a-uuid']));
    await expectCode('42P01', () => pool.query('SELECT 1 FROM nonexistent'));
    await expectCode('23503', () => pool.query('DELETE FROM products WHERE sku = $1', ['A-1'])); // referenced by order_items
    // pool still healthy after errors
    assert.equal((await pool.query('SELECT 1 AS one')).rows[0].one, 1);
  });

  await step('upsert (ON CONFLICT)', async () => {
    const r = await pool.query(`INSERT INTO users (email, name) VALUES ($1,$2)
        ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name, bio = 'upserted' RETURNING id, name, bio, (xmax = 0) AS inserted`, ['bob@example.com', 'Robert']);
    assert.equal(r.rows[0].id, bob.id);
    assert.equal(r.rows[0].name, 'Robert');
    assert.equal(r.rows[0].inserted, false);
    const s = await pool.query(`INSERT INTO users (email, name) VALUES ('bob@example.com','x') ON CONFLICT DO NOTHING`);
    assert.equal(s.rowCount, 0);
  });

  await step('prepared (named) statements, concurrent pool use', async () => {
    const q = { name: 'user-by-email', text: 'SELECT id FROM users WHERE email = $1' };
    const rs = await Promise.all(Array.from({ length: 20 }, (_, i) => pool.query({ ...q, values: [i % 2 ? 'alice@example.com' : 'bob@example.com'] })));
    assert.equal(rs.filter((r) => r.rows[0].id === alice.id).length, 10);
  });

  await step('multi-statement simple query and empty results', async () => {
    const r = await pool.query(`SELECT 1 AS a; SELECT 2 AS b`);
    assert.equal((r as unknown as unknown[]).length, 2);
    const e = await pool.query('SELECT * FROM users WHERE id = -1');
    assert.equal(e.rows.length, 0);
    assert.deepEqual(e.fields.map((f) => f.name).slice(0, 3), ['id', 'email', 'name']);
  });

  await step('timestamptz round trip across time zones', async () => {
    const c = await pool.connect();
    try {
      await c.query(`SET TIME ZONE 'Asia/Tokyo'`);
      const r = await c.query(`SELECT '2024-06-01 09:00:00+09'::timestamptz AS t, now() AS n`);
      assert.equal(r.rows[0].t.toISOString(), '2024-06-01T00:00:00.000Z');
    } finally { c.release(); }
  });

  await step('delete + ON DELETE CASCADE', async () => {
    const before = await pool.query(`SELECT (SELECT count(*) FROM posts WHERE author_id=$1) p, (SELECT count(*) FROM comments WHERE post_id=$2) c`, [alice.id, post1.id]);
    assert.equal(before.rows[0].p, 2n);
    assert.equal(before.rows[0].c, 2n);
    const d = await pool.query('DELETE FROM users WHERE id=$1 RETURNING id', [alice.id]);
    assert.equal(d.rowCount, 1);
    const after = await pool.query(`SELECT (SELECT count(*) FROM posts WHERE author_id=$1) p, (SELECT count(*) FROM comments WHERE post_id=$2) c, (SELECT count(*) FROM orders WHERE user_id=$1) o, (SELECT count(*) FROM order_items) oi`, [alice.id, post1.id]);
    assert.deepEqual(after.rows[0], { p: 0n, c: 0n, o: 0n, oi: 0n });
    const remaining = await pool.query('SELECT count(*)::int n FROM posts');
    assert.equal(remaining.rows[0].n, 1); // Bob's post
  });

  await step('listen/notify', async () => {
    const l = await pool.connect();
    try {
      const got = new Promise<string>((res) => l.once('notification', (m) => res(m.payload ?? '')));
      await l.query('LISTEN fx_chan');
      await pool.query(`SELECT pg_notify('fx_chan', 'hi')`);
      assert.equal(await Promise.race([got, new Promise<string>((_, rej) => setTimeout(() => rej(new Error('notify timeout')), 5000))]), 'hi');
    } finally { l.release(); }
  });
}

main()
  .catch((e) => { failed++; failures.push('harness'); console.error('harness error:', e); })
  .finally(async () => {
    await pool.end().catch(() => {});
    if (failed) console.error(`FAILED: ${failures.join(', ')}`);
    console.log(`FIXTURE_RESULT ${JSON.stringify({ name: 'node-pg', passed, failed, steps: steps.length })}`);
    process.exit(failed ? 1 : 0);
  });
