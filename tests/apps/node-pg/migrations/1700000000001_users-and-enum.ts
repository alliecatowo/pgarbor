import type { MigrationBuilder } from 'node-pg-migrate';

export async function up(pgm: MigrationBuilder): Promise<void> {
  pgm.createExtension('pgcrypto', { ifNotExists: true });
  pgm.createType('user_role', ['admin', 'editor', 'viewer']);
  pgm.createTable('users', {
    id: 'id', // serial primary key
    email: { type: 'varchar(255)', notNull: true, unique: true },
    name: { type: 'text', notNull: true },
    role: { type: 'user_role', notNull: true, default: 'viewer' },
    active: { type: 'boolean', notNull: true, default: true },
    karma: { type: 'bigint', notNull: true, default: 0 },
    created_at: { type: 'timestamptz', notNull: true, default: pgm.func('now()') },
  });
  pgm.createIndex('users', 'role');
}

export async function down(pgm: MigrationBuilder): Promise<void> {
  pgm.dropTable('users');
  pgm.dropType('user_role');
}
