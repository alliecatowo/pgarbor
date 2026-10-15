import type { MigrationBuilder } from 'node-pg-migrate';

export async function up(pgm: MigrationBuilder): Promise<void> {
  pgm.createType('order_status', ['pending', 'paid', 'shipped', 'cancelled']);
  pgm.createTable('products', {
    id: { type: 'integer', primaryKey: true, sequenceGenerated: { precedence: 'BY DEFAULT' } },
    sku: { type: 'varchar(40)', notNull: true, unique: true },
    name: { type: 'text', notNull: true },
    price: { type: 'numeric(12,2)', notNull: true },
  });
  pgm.createTable('orders', {
    id: 'id',
    user_id: { type: 'integer', notNull: true, references: 'users', onDelete: 'CASCADE' },
    status: { type: 'order_status', notNull: true, default: 'pending' },
    total: { type: 'numeric(12,2)', notNull: true, default: 0 },
    created_at: { type: 'timestamptz', notNull: true, default: pgm.func('now()') },
  });
  pgm.createTable('order_items', {
    order_id: { type: 'integer', notNull: true, references: 'orders', onDelete: 'CASCADE' },
    product_id: { type: 'integer', notNull: true, references: 'products' },
    quantity: { type: 'integer', notNull: true, check: 'quantity > 0' },
    unit_price: { type: 'numeric(12,2)', notNull: true },
  });
  pgm.addConstraint('order_items', 'order_items_pkey', { primaryKey: ['order_id', 'product_id'] });
  pgm.createIndex('orders', ['user_id', 'status']);
}

export async function down(pgm: MigrationBuilder): Promise<void> {
  pgm.dropTable('order_items');
  pgm.dropTable('orders');
  pgm.dropTable('products');
  pgm.dropType('order_status');
}
