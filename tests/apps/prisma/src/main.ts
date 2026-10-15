import { Prisma, PrismaClient, PostStatus, Role } from "@prisma/client";
import { writeFileSync } from "node:fs";
import assert from "node:assert/strict";

const prisma = new PrismaClient();
const results: { name: string; ok: boolean; error?: string }[] = [];

async function step(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    results.push({ name, ok: true });
    console.log(`ok   - ${name}`);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    results.push({ name, ok: false, error: msg });
    console.log(`FAIL - ${name}\n       ${msg.split("\n").join("\n       ")}`);
  }
}

async function main() {
  let aliceId = "";
  let bobId = "";
  let postIds: number[] = [];

  await step("clean tables", async () => {
    await prisma.postTag.deleteMany();
    await prisma.comment.deleteMany();
    await prisma.post.deleteMany();
    await prisma.profile.deleteMany();
    await prisma.tag.deleteMany();
    await prisma.user.deleteMany();
  });

  await step("create user (uuid default, enum, bigint, @updatedAt)", async () => {
    const u = await prisma.user.create({
      data: { email: "alice@example.com", name: "Alice", role: Role.ADMIN, karma: 9007199254740993n },
    });
    aliceId = u.id;
    assert.match(u.id, /^[0-9a-f-]{36}$/);
    assert.equal(u.role, "ADMIN");
    assert.equal(u.active, true);
    assert.equal(u.karma, 9007199254740993n);
    assert.ok(u.createdAt instanceof Date && u.updatedAt instanceof Date);
    const b = await prisma.user.create({ data: { email: "bob@example.com", name: "Bob" } });
    bobId = b.id;
    assert.equal(b.role, "READER");
  });

  await step("createMany + returning posts (serial, array, json, decimal)", async () => {
    const rows = await prisma.post.createManyAndReturn({
      data: [
        { title: "Hello", body: "first", authorId: aliceId, status: PostStatus.PUBLISHED, views: 10,
          rating: new Prisma.Decimal("4.50"), labels: ["intro", "news"], meta: { lang: "en", stats: { likes: 3 } }, publishedAt: new Date("2024-05-01T12:00:00Z") },
        { title: "Second", authorId: aliceId, views: 5, rating: new Prisma.Decimal("3.25"), labels: ["news"], meta: { lang: "de", stats: { likes: 8 } } },
        { title: "Bobs post", authorId: bobId, status: PostStatus.PUBLISHED, views: 50, rating: new Prisma.Decimal("1.00"), labels: [] },
        { title: "Archived", authorId: bobId, status: PostStatus.ARCHIVED, views: 1, labels: ["old"] },
      ],
    });
    postIds = rows.map((r) => r.id);
    assert.equal(rows.length, 4);
    assert.ok(rows[1].id > rows[0].id);
    assert.equal(rows[0].rating.toFixed(2), "4.50");
    assert.deepEqual(rows[0].labels, ["intro", "news"]);
    assert.equal(rows[0].publishedAt?.toISOString(), "2024-05-01T12:00:00.000Z");
    assert.equal(rows[0].subtitle, null);
  });

  await step("select with filter, order, limit, offset", async () => {
    const rows = await prisma.post.findMany({
      where: { status: PostStatus.PUBLISHED, views: { gte: 10 } },
      orderBy: { views: "desc" },
      take: 1,
    });
    assert.equal(rows.length, 1);
    assert.equal(rows[0].title, "Bobs post");
    const second = await prisma.post.findMany({ orderBy: { id: "asc" }, skip: 1, take: 2 });
    assert.deepEqual(second.map((p) => p.title), ["Second", "Bobs post"]);
  });

  await step("select with join (include / relation filter)", async () => {
    await prisma.comment.createMany({
      data: [
        { postId: postIds[0], authorId: bobId, body: "nice" },
        { postId: postIds[0], authorId: aliceId, body: "thanks" },
        { postId: postIds[2], authorId: aliceId, body: "hm" },
      ],
    });
    const posts = await prisma.post.findMany({
      where: { author: { email: "alice@example.com" } },
      include: { author: true, comments: { orderBy: { id: "asc" }, include: { author: true } } },
      orderBy: { id: "asc" },
    });
    assert.equal(posts.length, 2);
    assert.equal(posts[0].author.name, "Alice");
    assert.deepEqual(posts[0].comments.map((c) => `${c.author.name}:${c.body}`), ["Bob:nice", "Alice:thanks"]);
    assert.equal(typeof posts[0].comments[0].id, "bigint");
    const commented = await prisma.user.findMany({ where: { comments: { some: { body: "hm" } } } });
    assert.deepEqual(commented.map((u) => u.name), ["Alice"]);
  });

  await step("array and enum filters", async () => {
    const news = await prisma.post.findMany({ where: { labels: { has: "news" } }, orderBy: { id: "asc" } });
    assert.equal(news.length, 2);
    const both = await prisma.post.findMany({ where: { labels: { hasEvery: ["intro", "news"] } } });
    assert.equal(both.length, 1);
    const inEnum = await prisma.post.findMany({ where: { status: { in: ["DRAFT", "ARCHIVED"] } } });
    assert.equal(inEnum.length, 2);
  });

  await step("json query (path filter, string_contains)", async () => {
    const de = await prisma.post.findMany({ where: { meta: { path: ["lang"], equals: "de" } } });
    assert.deepEqual(de.map((p) => p.title), ["Second"]);
    const likes = await prisma.post.findMany({ where: { meta: { path: ["stats", "likes"], gt: 5 } } });
    assert.deepEqual(likes.map((p) => p.title), ["Second"]);
    const raw = await prisma.$queryRaw<{ title: string }[]>`SELECT title FROM "Post" WHERE meta->'stats'->>'likes' = '3'`;
    assert.deepEqual(raw.map((r) => r.title), ["Hello"]);
  });

  await step("count / aggregate / groupBy", async () => {
    assert.equal(await prisma.post.count(), 4);
    assert.equal(await prisma.post.count({ where: { status: "PUBLISHED" } }), 2);
    const agg = await prisma.post.aggregate({ _sum: { views: true }, _avg: { rating: true }, _max: { views: true }, _min: { views: true }, _count: { _all: true } });
    assert.equal(agg._sum.views, 66);
    assert.equal(agg._max.views, 50);
    assert.equal(agg._min.views, 1);
    assert.equal(agg._count._all, 4);
    const groups = await prisma.post.groupBy({ by: ["authorId"], _count: { _all: true }, _sum: { views: true }, orderBy: { _sum: { views: "desc" } } });
    assert.equal(groups.length, 2);
    assert.equal(groups[0].authorId, bobId);
    assert.equal(groups[0]._sum.views, 51);
  });

  await step("update, updateMany, upsert, @updatedAt bump", async () => {
    const before = await prisma.post.findUniqueOrThrow({ where: { id: postIds[0] } });
    await new Promise((r) => setTimeout(r, 20));
    const after = await prisma.post.update({ where: { id: postIds[0] }, data: { title: "Hello again", views: { increment: 5 }, subtitle: "sub" } });
    assert.equal(after.title, "Hello again");
    assert.equal(after.views, 15);
    assert.equal(after.subtitle, "sub");
    assert.ok(after.updatedAt.getTime() > before.updatedAt.getTime());
    const many = await prisma.post.updateMany({ where: { status: "DRAFT" }, data: { status: "PUBLISHED" } });
    assert.equal(many.count, 1);
    const up = await prisma.tag.upsert({ where: { name: "t1" }, update: {}, create: { name: "t1" } });
    const up2 = await prisma.tag.upsert({ where: { name: "t1" }, update: {}, create: { name: "t1" } });
    assert.equal(up.id, up2.id);
  });

  await step("one-to-one profile + many-to-many tags (nested writes)", async () => {
    await prisma.profile.create({ data: { userId: aliceId, bio: "hi", settings: { theme: "dark", n: [1, 2] } } });
    const p = await prisma.profile.findUniqueOrThrow({ where: { userId: aliceId } });
    assert.deepEqual(p.settings, { theme: "dark", n: [1, 2] });
    const post = await prisma.post.update({
      where: { id: postIds[0] },
      data: { tags: { create: [{ tag: { create: { name: "t2" } } }, { tag: { connect: { name: "t1" } } }] } },
      include: { tags: { include: { tag: true } } },
    });
    assert.deepEqual(post.tags.map((t) => t.tag.name).sort(), ["t1", "t2"]);
    const tagged = await prisma.tag.findMany({ where: { posts: { some: { post: { authorId: aliceId } } } }, orderBy: { name: "asc" } });
    assert.deepEqual(tagged.map((t) => t.name), ["t1", "t2"]);
  });

  await step("transaction commit ($transaction array + interactive)", async () => {
    const [a, b] = await prisma.$transaction([
      prisma.tag.create({ data: { name: "tx-a" } }),
      prisma.tag.create({ data: { name: "tx-b" } }),
    ]);
    assert.ok(a.id !== b.id);
    await prisma.$transaction(async (tx) => {
      const u = await tx.user.create({ data: { email: "carol@example.com", name: "Carol" } });
      await tx.post.create({ data: { title: "Carol post", authorId: u.id } });
    });
    assert.equal(await prisma.post.count({ where: { title: "Carol post" } }), 1);
  });

  await step("transaction rollback", async () => {
    await assert.rejects(
      prisma.$transaction(async (tx) => {
        await tx.user.create({ data: { email: "dave@example.com", name: "Dave" } });
        throw new Error("boom");
      }),
      /boom/,
    );
    assert.equal(await prisma.user.count({ where: { email: "dave@example.com" } }), 0);
    // DB-level failure in a batch rolls back earlier statements too
    await assert.rejects(
      prisma.$transaction([
        prisma.tag.create({ data: { name: "rb-ok" } }),
        prisma.user.create({ data: { email: "alice@example.com", name: "Dup" } }),
      ]),
    );
    assert.equal(await prisma.tag.count({ where: { name: "rb-ok" } }), 0);
  });

  await step("unique violation (P2002) and FK violation (P2003)", async () => {
    try {
      await prisma.user.create({ data: { email: "alice@example.com", name: "Again" } });
      assert.fail("expected unique violation");
    } catch (e) {
      assert.ok(e instanceof Prisma.PrismaClientKnownRequestError, String(e));
      assert.equal(e.code, "P2002");
      assert.deepEqual((e.meta as any)?.target, ["email"]);
    }
    try {
      await prisma.post.create({ data: { title: "orphan", authorId: "00000000-0000-4000-8000-000000000000" } });
      assert.fail("expected fk violation");
    } catch (e) {
      assert.ok(e instanceof Prisma.PrismaClientKnownRequestError, String(e));
      assert.equal(e.code, "P2003");
    }
  });

  await step("enum rejects invalid value at DB level", async () => {
    await assert.rejects(prisma.$executeRaw`INSERT INTO "Post" (title, status, "authorId", "updatedAt") VALUES ('x', 'BOGUS', ${aliceId}::uuid, now())`);
  });

  await step("delete and ON DELETE CASCADE", async () => {
    const del = await prisma.comment.delete({ where: { id: (await prisma.comment.findFirstOrThrow({ where: { body: "hm" } })).id } });
    assert.equal(del.body, "hm");
    const before = await prisma.post.count({ where: { authorId: bobId } });
    assert.equal(before, 2);
    await prisma.user.delete({ where: { id: bobId } });
    assert.equal(await prisma.post.count({ where: { authorId: bobId } }), 0);
    assert.equal(await prisma.comment.count({ where: { authorId: bobId } }), 0);
    await prisma.post.delete({ where: { id: postIds[0] } });
    assert.equal(await prisma.postTag.count({ where: { postId: postIds[0] } }), 0);
    assert.equal(await prisma.comment.count({ where: { postId: postIds[0] } }), 0);
    await prisma.user.delete({ where: { id: aliceId } });
    assert.equal(await prisma.profile.count({ where: { userId: aliceId } }), 0);
    assert.equal((await prisma.post.deleteMany({ where: { title: "Carol post" } })).count, 1);
  });

  await step("raw query round trip (timestamptz, numeric, array)", async () => {
    const r = await prisma.$queryRaw<{ t: Date; n: Prisma.Decimal; a: string[] }[]>`SELECT '2024-01-02T03:04:05Z'::timestamptz AS t, 1.50::numeric(6,2) AS n, ARRAY['a','b']::text[] AS a`;
    assert.equal(r[0].t.toISOString(), "2024-01-02T03:04:05.000Z");
    assert.equal(r[0].n.toFixed(2), "1.50");
    assert.deepEqual(r[0].a, ["a", "b"]);
  });
}

main()
  .catch((e) => {
    console.error("fatal:", e);
    results.push({ name: "fatal", ok: false, error: String(e) });
  })
  .finally(async () => {
    await prisma.$disconnect();
    const failed = results.filter((r) => !r.ok).length;
    const out = { passed: results.length - failed, failed, steps: results.length, failures: results.filter((r) => !r.ok) };
    if (process.env.FIXTURE_OUT) writeFileSync(process.env.FIXTURE_OUT, JSON.stringify(out));
    process.exit(failed ? 1 : 0);
  });
