// Loads the wasm build in Node and runs real SQL. Usage: node demo/test-node.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import assert from 'node:assert';

const dir = join(dirname(fileURLToPath(import.meta.url)), 'web', 'pkg');
const { default: init, exec, version } = await import(join(dir, 'pgarbor_wasm.js'));
await init({ module_or_path: readFileSync(join(dir, 'pgarbor_wasm_bg.wasm')) });

const run = (sql) => JSON.parse(exec(sql));
console.log('engine', version());
let r = run('SELECT 1 AS one');
assert.deepStrictEqual(r.results[0].rows, [['1']]);
assert.ok(run('CREATE TABLE t(id int primary key, n text)').ok);
assert.strictEqual(run("INSERT INTO t VALUES (1,'a'),(2,NULL)").results[0].tag, 'INSERT 0 2');
r = run('SELECT id, n FROM t ORDER BY id');
assert.deepStrictEqual(r.results[0].rows, [['1', 'a'], ['2', null]]);
r = run("INSERT INTO t VALUES (1,'dup')");
assert.strictEqual(r.ok, false);
assert.strictEqual(r.error.sqlstate, '23505');
assert.strictEqual(run('SELECT count(*) FROM t').results[0].rows[0][0], '2');
console.log('wasm node smoke OK');
