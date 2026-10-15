import type { MigrationBuilder } from 'node-pg-migrate';

export async function up(pgm: MigrationBuilder): Promise<void> {
  pgm.createTable('posts', {
    id: { type: 'uuid', primaryKey: true, default: pgm.func('gen_random_uuid()') },
    author_id: { type: 'integer', notNull: true, references: 'users', onDelete: 'CASCADE' },
    title: { type: 'varchar(200)', notNull: true },
    body: { type: 'text' },
    tags: { type: 'text[]', notNull: true, default: pgm.func("'{}'::text[]") },
    meta: { type: 'jsonb', notNull: true, default: pgm.func("'{}'::jsonb") },
    views: { type: 'bigint', notNull: true, default: 0 },
    published_at: { type: 'timestamptz' },
    created_at: { type: 'timestamptz', notNull: true, default: pgm.func('now()') },
  });
  pgm.addConstraint('posts', 'posts_author_title_key', { unique: ['author_id', 'title'] });
  pgm.createIndex('posts', 'author_id');
  pgm.createIndex('posts', 'tags', { method: 'gin', name: 'posts_tags_gin' });

  pgm.createTable('comments', {
    id: { type: 'bigint', primaryKey: true, sequenceGenerated: { precedence: 'ALWAYS' } },
    post_id: { type: 'uuid', notNull: true, references: 'posts', onDelete: 'CASCADE' },
    user_id: { type: 'integer', notNull: true, references: 'users', onDelete: 'CASCADE' },
    body: { type: 'text', notNull: true },
    score: { type: 'integer', notNull: true, default: 0 },
    created_at: { type: 'timestamptz', notNull: true, default: pgm.func('now()') },
  });
  pgm.createIndex('comments', ['post_id', 'created_at']);
}

export async function down(pgm: MigrationBuilder): Promise<void> {
  pgm.dropTable('comments');
  pgm.dropTable('posts');
}
