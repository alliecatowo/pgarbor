import { and, asc, avg, count, desc, eq, gt, ilike, inArray, sql, sum } from "drizzle-orm";
import { drizzle } from "drizzle-orm/node-postgres";
import pg from "pg";
import * as schema from "./schema.js";

const { organizations, users, posts, comments, postLikes } = schema;

let passed = 0;
let failed = 0;
const failures: string[] = [];

async function step(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    passed++;
    console.log(`ok   - ${name}`);
  } catch (e) {
    failed++;
    const msg = e instanceof Error ? `${e.message}${e.cause ? ` (cause: ${(e.cause as Error).message})` : ""}` : String(e);
    failures.push(`${name}: ${msg}`);
    console.log(`FAIL - ${name}: ${msg}`);
  }
}

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}
function eq_<T>(a: T, b: T, msg: string) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`${msg}: expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}

async function main() {
  const url = process.env.DATABASE_URL;
  if (!url) throw new Error("DATABASE_URL is not set");
  const pool = new pg.Pool({ connectionString: url, max: 4 });
  const db = drizzle(pool, { schema });

  let orgId = 0;
  let aliceId = "";
  let bobId = "";
  const postIds: number[] = [];

  await step("insert returning (serial pk)", async () => {
    const [org] = await db.insert(organizations).values({ name: "Acme" }).returning();
    assert(org.id > 0 && org.name === "Acme" && org.createdAt instanceof Date, "org row");
    orgId = org.id;
    const rows = await db.insert(organizations).values([{ name: "Globex" }, { name: "Initech" }]).returning({ id: organizations.id });
    eq_(rows.length, 2, "multi insert");
  });

  await step("insert returning (uuid pk, enum, jsonb defaults)", async () => {
    const [alice, bob] = await db.insert(users).values([
      { orgId, email: "alice@example.com", displayName: "Alice", role: "admin", settings: { theme: "dark", notifications: { email: true } } },
      { orgId, email: "bob@example.com", displayName: "Bob" },
    ]).returning();
    assert(/^[0-9a-f-]{36}$/.test(alice.id), "uuid generated");
    eq_(bob.role, "viewer", "default enum");
    eq_(bob.settings, {}, "default jsonb");
    eq_(bob.karma, 0, "bigint default");
    assert(bob.active === true && bob.lastLoginAt === null, "defaults");
    aliceId = alice.id;
    bobId = bob.id;
  });

  await step("insert posts (identity bigint, arrays, numeric, jsonb)", async () => {
    const rows = await db.insert(posts).values([
      { authorId: aliceId, title: "Hello World", body: "first", tags: ["intro", "news"], scores: [1, 2, 3], price: "9.99", published: true, meta: { source: "web", n: 1 } },
      { authorId: aliceId, title: "Second Post", tags: ["news"], price: "19.50", published: true, meta: { source: "api", n: 2 } },
      { authorId: bobId, title: "Bob's Draft", tags: [], meta: { source: "web", n: 3 } },
      { authorId: bobId, title: "Bob on Databases", tags: ["db", "intro"], published: true, price: "0.00" },
    ]).returning({ id: posts.id });
    for (const r of rows) postIds.push(r.id);
    assert(postIds.length === 4 && postIds[0] < postIds[1], "identity ids ascend");
    const [p] = await db.select().from(posts).where(eq(posts.id, postIds[0]));
    eq_(p.tags, ["intro", "news"], "text[]");
    eq_(p.scores, [1, 2, 3], "int[]");
    eq_(p.price, "9.99", "numeric");
    eq_(p.viewCount, 0, "added column default");
  });

  await step("select with join, filter, order, limit, offset", async () => {
    const rows = await db
      .select({ title: posts.title, author: users.displayName, org: organizations.name })
      .from(posts)
      .innerJoin(users, eq(posts.authorId, users.id))
      .innerJoin(organizations, eq(users.orgId, organizations.id))
      .where(and(eq(posts.published, true), ilike(posts.title, "%o%")))
      .orderBy(desc(posts.createdAt), asc(posts.id))
      .limit(2)
      .offset(0);
    assert(rows.length === 2, "limit");
    assert(rows.every((r) => r.org === "Acme"), "join org");
    const left = await db.select({ t: posts.title, c: comments.id }).from(posts).leftJoin(comments, eq(comments.postId, posts.id)).orderBy(asc(posts.id));
    eq_(left.length, 4, "left join keeps posts");
    assert(left.every((r) => r.c === null), "no comments yet");
  });

  await step("array and json queries", async () => {
    const byTag = await db.select({ id: posts.id }).from(posts).where(sql`${posts.tags} @> ARRAY['intro']::text[]`).orderBy(asc(posts.id));
    eq_(byTag.length, 2, "array contains (gin)");
    const anyTag = await db.select({ id: posts.id }).from(posts).where(sql`'db' = ANY(${posts.tags})`);
    eq_(anyTag.length, 1, "= ANY");
    const web = await db.select({ id: posts.id, n: sql<number>`(${posts.meta}->>'n')::int` }).from(posts)
      .where(sql`${posts.meta}->>'source' = 'web'`).orderBy(asc(posts.id));
    eq_(web.map((r) => r.n), [1, 3], "jsonb ->> filter");
    const contains = await db.select({ id: posts.id }).from(posts).where(sql`${posts.meta} @> '{"source":"api"}'::jsonb`);
    eq_(contains.length, 1, "jsonb @>");
    const nested = await db.select({ e: sql<boolean>`(${users.settings}->'notifications'->>'email')::boolean` }).from(users).where(eq(users.id, aliceId));
    eq_(nested[0].e, true, "nested jsonb path");
  });

  await step("update returning, jsonb merge, increment", async () => {
    const [u] = await db.update(users).set({ role: "editor", lastLoginAt: new Date(), karma: sql`${users.karma} + 5` }).where(eq(users.id, bobId)).returning();
    eq_(u.role, "editor", "role");
    eq_(u.karma, 5, "karma");
    assert(u.lastLoginAt instanceof Date, "lastLoginAt");
    await db.update(users).set({ settings: sql`${users.settings} || '{"theme":"light"}'::jsonb` }).where(eq(users.id, bobId));
    const [again] = await db.select({ s: users.settings }).from(users).where(eq(users.id, bobId));
    eq_(again.s, { theme: "light" }, "jsonb ||");
    const bumped = await db.update(posts).set({ viewCount: sql`${posts.viewCount} + 1` }).where(inArray(posts.id, postIds)).returning({ v: posts.viewCount });
    eq_(bumped.map((b) => b.v), [1, 1, 1, 1], "bulk update");
  });

  await step("composite pk table and comments", async () => {
    await db.insert(postLikes).values([
      { postId: postIds[0], userId: aliceId },
      { postId: postIds[0], userId: bobId },
      { postId: postIds[1], userId: bobId },
    ]);
    await db.insert(comments).values([
      { postId: postIds[0], authorId: bobId, body: "nice" },
      { postId: postIds[0], authorId: aliceId, body: "thanks" },
      { postId: postIds[1], authorId: bobId, body: "ok" },
    ]);
    const [c] = await db.select({ c: count() }).from(postLikes);
    eq_(c.c, 3, "likes count");
  });

  await step("count and aggregates with group by / having", async () => {
    const [tot] = await db.select({ n: count(), published: count(sql`nullif(${posts.published}, false)`) }).from(posts);
    eq_([tot.n, tot.published], [4, 3], "count");
    const agg = await db
      .select({ author: users.displayName, posts: count(posts.id), total: sum(posts.price), mean: avg(posts.price), maxId: sql<number>`max(${posts.id})` })
      .from(users)
      .innerJoin(posts, eq(posts.authorId, users.id))
      .groupBy(users.displayName)
      .having(gt(count(posts.id), 1))
      .orderBy(asc(users.displayName));
    eq_(agg.length, 2, "groups");
    eq_(agg[0].total, "29.49", "sum numeric");
    eq_(agg[1].total, "0.00", "sum numeric 2");
    const grouped = await db.execute(sql`select unnest(tags) as tag, count(*)::int as n from posts group by 1 order by n desc, tag`);
    eq_(grouped.rows.map((r) => r.tag), ["intro", "news", "db"], "unnest group");
  });

  await step("relational query API (with/one/many)", async () => {
    const result = await db.query.users.findMany({
      where: eq(users.orgId, orgId),
      orderBy: asc(users.email),
      with: {
        organization: true,
        posts: { orderBy: asc(posts.id), with: { comments: { orderBy: asc(comments.id) } } },
      },
    });
    eq_(result.length, 2, "users");
    eq_(result[0].organization.name, "Acme", "org");
    eq_(result[0].posts.length, 2, "alice posts");
    eq_(result[0].posts[0].comments.map((c) => c.body), ["nice", "thanks"], "nested comments");
    const one = await db.query.posts.findFirst({ where: eq(posts.id, postIds[2]), with: { author: { columns: { displayName: true } } } });
    eq_(one?.author.displayName, "Bob", "findFirst with one");
  });

  await step("transaction commit", async () => {
    await db.transaction(async (tx) => {
      const [org] = await tx.insert(organizations).values({ name: "TxCommitOrg" }).returning();
      await tx.insert(users).values({ orgId: org.id, email: "tx@example.com", displayName: "Tx" });
    });
    const rows = await db.select().from(users).where(eq(users.email, "tx@example.com"));
    eq_(rows.length, 1, "committed");
  });

  await step("transaction rollback (thrown error)", async () => {
    let threw = false;
    try {
      await db.transaction(async (tx) => {
        await tx.insert(organizations).values({ name: "RolledBack" });
        throw new Error("boom");
      });
    } catch (e) {
      threw = (e as Error).message === "boom";
    }
    assert(threw, "error propagated");
    eq_((await db.select().from(organizations).where(eq(organizations.name, "RolledBack"))).length, 0, "rolled back");
  });

  await step("transaction explicit rollback + savepoint", async () => {
    await db.transaction(async (tx) => {
      await tx.insert(organizations).values({ name: "SavepointOuter" });
      try {
        await tx.transaction(async (inner) => {
          await inner.insert(organizations).values({ name: "SavepointInner" });
          inner.rollback();
        });
      } catch { /* TransactionRollbackError expected */ }
    });
    const names = (await db.select({ n: organizations.name }).from(organizations).where(inArray(organizations.name, ["SavepointOuter", "SavepointInner"]))).map((r) => r.n);
    eq_(names, ["SavepointOuter"], "only outer kept");
    await db.transaction(async (tx) => {
      await tx.insert(organizations).values({ name: "ExplicitRollback" });
      tx.rollback();
    }).catch(() => undefined);
    eq_((await db.select().from(organizations).where(eq(organizations.name, "ExplicitRollback"))).length, 0, "explicit rollback");
  });

  const pgCode = (e: unknown): string | undefined => {
    const err = e as { code?: string; cause?: { code?: string } };
    return err.code ?? err.cause?.code;
  };

  await step("unique violation (23505)", async () => {
    try {
      await db.insert(users).values({ orgId, email: "alice@example.com", displayName: "Dup" });
    } catch (e) {
      eq_(pgCode(e), "23505", "sqlstate");
      const c = (e as { cause?: { constraint?: string }; constraint?: string });
      eq_(c.constraint ?? c.cause?.constraint, "users_email_unique", "constraint name");
      return;
    }
    throw new Error("expected unique violation");
    });

  await step("upsert (on conflict do update / do nothing)", async () => {
    const [u] = await db.insert(users).values({ orgId, email: "alice@example.com", displayName: "Alice Updated" })
      .onConflictDoUpdate({ target: users.email, set: { displayName: "Alice Updated" } }).returning();
    eq_(u.id, aliceId, "same row");
    const none = await db.insert(organizations).values({ name: "Acme" }).onConflictDoNothing().returning();
    eq_(none.length, 0, "do nothing");
  });

  await step("foreign key violation (23503) and check violation (23514)", async () => {
    try {
      await db.insert(posts).values({ authorId: "00000000-0000-0000-0000-000000000000", title: "orphan" });
      throw new Error("expected fk violation");
    } catch (e) {
      eq_(pgCode(e), "23503", "fk sqlstate");
    }
    try {
      await db.update(posts).set({ price: "-1.00" }).where(eq(posts.id, postIds[0]));
      throw new Error("expected check violation");
    } catch (e) {
      eq_(pgCode(e), "23514", "check sqlstate");
    }
  });

  await step("invalid enum value (22P02)", async () => {
    try {
      await db.execute(sql`insert into users (org_id, email, display_name, role) values (${orgId}, 'x@example.com', 'X', 'superuser')`);
      throw new Error("expected enum error");
    } catch (e) {
      eq_(pgCode(e), "22P02", "enum sqlstate");
    }
  });

  await step("delete returning and ON DELETE CASCADE / SET NULL", async () => {
    const [deleted] = await db.delete(posts).where(eq(posts.id, postIds[3])).returning({ id: posts.id });
    eq_(deleted.id, postIds[3], "deleted id");
    // deleting a post cascades to its comments and likes
    await db.delete(posts).where(eq(posts.id, postIds[1]));
    eq_((await db.select().from(comments).where(eq(comments.postId, postIds[1]))).length, 0, "comments cascaded");
    eq_((await db.select().from(postLikes).where(eq(postLikes.postId, postIds[1]))).length, 0, "likes cascaded");
    // deleting a user sets comment author to null and cascades to posts
    await db.delete(users).where(eq(users.id, bobId));
    const rest = await db.select().from(comments).where(eq(comments.postId, postIds[0])).orderBy(asc(comments.id));
    eq_(rest.map((c) => c.authorId), [null, aliceId], "set null");
    eq_((await db.select().from(posts).where(eq(posts.authorId, bobId))).length, 0, "bob posts cascaded");
    // deleting an org cascades to users
    await db.delete(organizations).where(eq(organizations.id, orgId));
    const [left] = await db.select({ c: count() }).from(users).where(eq(users.orgId, orgId));
    eq_(left.c, 0, "users cascaded");
    const [cc] = await db.select({ c: count() }).from(comments);
    eq_(cc.c, 0, "all comments gone");
  });

  await step("catalog sanity (information_schema)", async () => {
    const r = await db.execute(sql`select count(*)::int as n from information_schema.columns where table_schema = 'public' and table_name in ('users','posts','comments','organizations','post_likes')`);
    eq_(r.rows[0].n, 10 + 11 + 5 + 3 + 3, "column count");
    const idx = await db.execute(sql`select indexname from pg_indexes where schemaname = 'public' and tablename = 'posts' order by 1`);
    eq_(idx.rows.map((x) => x.indexname), ["posts_author_created_idx", "posts_pkey", "posts_published_partial_idx", "posts_tags_gin_idx"], "post indexes");
  });

  await pool.end();
  const result = { name: "drizzle", passed, failed, steps: passed + failed, failures };
  console.log(`TEST_RESULT ${JSON.stringify(result)}`);
  if (failed > 0) process.exit(1);
}

main().catch((e) => {
  console.error("FATAL:", e instanceof Error ? e.stack : e);
  process.exit(2);
});
