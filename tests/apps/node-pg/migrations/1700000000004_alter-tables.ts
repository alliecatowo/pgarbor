import type { MigrationBuilder } from 'node-pg-migrate';

export async function up(pgm: MigrationBuilder): Promise<void> {
  pgm.addColumns('users', {
    bio: { type: 'text' },
    settings: { type: 'jsonb', notNull: true, default: pgm.func("'{}'::jsonb") },
  });
  pgm.alterColumn('posts', 'title', { type: 'varchar(300)' });
  pgm.alterColumn('comments', 'score', { default: 1 });
  pgm.renameColumn('products', 'name', 'title');
  pgm.addColumn('products', { in_stock: { type: 'boolean', notNull: true, default: true } });
  pgm.addConstraint('products', 'products_price_nonneg', { check: 'price >= 0' });
  pgm.createIndex('posts', 'published_at', { where: 'published_at IS NOT NULL', name: 'posts_published_idx' });
  pgm.sql("COMMENT ON TABLE users IS 'Application users'");
}

export async function down(pgm: MigrationBuilder): Promise<void> {
  pgm.dropIndex('posts', 'published_at', { name: 'posts_published_idx' });
  pgm.dropConstraint('products', 'products_price_nonneg');
  pgm.dropColumn('products', 'in_stock');
  pgm.renameColumn('products', 'title', 'name');
  pgm.alterColumn('comments', 'score', { default: 0 });
  pgm.alterColumn('posts', 'title', { type: 'varchar(200)' });
  pgm.dropColumns('users', ['bio', 'settings']);
}
