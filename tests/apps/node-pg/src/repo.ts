import type pg from 'pg';

export type Role = 'admin' | 'editor' | 'viewer';
export interface User { id: number; email: string; name: string; role: Role; active: boolean; karma: bigint; created_at: Date; bio: string | null; settings: Record<string, unknown>; }
export interface Post { id: string; author_id: number; title: string; body: string | null; tags: string[]; meta: Record<string, any>; views: bigint; published_at: Date | null; created_at: Date; }

type Q = pg.Pool | pg.PoolClient;

export async function createUser(q: Q, email: string, name: string, role: Role = 'viewer', settings: object = {}): Promise<User> {
  const r = await q.query<User>(
    'INSERT INTO users (email, name, role, settings) VALUES ($1, $2, $3, $4) RETURNING *',
    [email, name, role, JSON.stringify(settings)],
  );
  return r.rows[0];
}

export async function createPost(q: Q, authorId: number, title: string, tags: string[], meta: object, publishedAt: Date | null = null): Promise<Post> {
  const r = await q.query<Post>(
    'INSERT INTO posts (author_id, title, body, tags, meta, published_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *',
    [authorId, title, `Body of ${title}`, tags, JSON.stringify(meta), publishedAt],
  );
  return r.rows[0];
}

export async function addComment(q: Q, postId: string, userId: number, body: string): Promise<{ id: bigint; score: number }> {
  const r = await q.query('INSERT INTO comments (post_id, user_id, body) VALUES ($1, $2, $3) RETURNING id, score', [postId, userId, body]);
  return r.rows[0];
}

export async function postsWithAuthors(q: Q, limit: number, offset = 0) {
  const r = await q.query<{ title: string; author: string; comment_count: bigint }>(
    `SELECT p.title, u.name AS author, count(c.id) AS comment_count
       FROM posts p
       JOIN users u ON u.id = p.author_id
       LEFT JOIN comments c ON c.post_id = p.id
      GROUP BY p.id, u.name
      ORDER BY comment_count DESC, p.title ASC
      LIMIT $1 OFFSET $2`,
    [limit, offset],
  );
  return r.rows;
}
