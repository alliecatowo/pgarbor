'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const root = path.resolve(__dirname, '..', '..', '..');
execFileSync('cargo', ['build', '-p', 'pgarbor-ffi'], { cwd: root, stdio: 'inherit' });
const ext = process.platform === 'darwin' ? 'dylib' : 'so';
process.env.PGARBOR_LIB = path.join(root, 'target', 'debug', `libpgarbor_ffi.${ext}`);
const pgarbor = require('..');

test('create/insert/select, duplicate key, reopen', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pga-node-'));
  const file = path.join(dir, 'n.pga');
  try {
    let c = pgarbor.connect(file);
    assert.deepStrictEqual(c.exec('SELECT 1 AS one').rows, [['1']]);
    c.exec('CREATE TABLE t(id int primary key, n text)');
    assert.strictEqual(c.exec("INSERT INTO t VALUES (1,'a'),(2,NULL)").rowCount, 2);
    const r = c.exec('SELECT id, n FROM t ORDER BY id');
    assert.deepStrictEqual(r.columns, ['id', 'n']);
    assert.deepStrictEqual(r.rows, [['1', 'a'], ['2', null]]);
    assert.throws(() => c.exec("INSERT INTO t VALUES (1,'dup')"), (e) => e.sqlstate === '23505');
    assert.deepStrictEqual(c.exec('SELECT count(*) FROM t').rows, [['2']]);
    c.close();

    c = pgarbor.connect(file, { create: false });
    assert.deepStrictEqual(c.exec('SELECT n FROM t WHERE id = 1').rows, [['a']]);
    c.close();
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
