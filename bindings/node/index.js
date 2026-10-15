'use strict';
// Node binding for PGArbor over the C ABI (include/pgarbor.h) using koffi.
// Handles are bound to the creating thread by the engine: use from the main thread only
// (do not use from worker_threads other than the one that opened the database).
const path = require('node:path');
const fs = require('node:fs');
const koffi = require('koffi');

const OPEN_CREATE = 1, OPEN_READONLY = 2, OPEN_MEMORY = 4;

function findLib() {
  if (process.env.PGARBOR_LIB) return process.env.PGARBOR_LIB;
  const name = process.platform === 'darwin' ? 'libpgarbor_ffi.dylib'
    : process.platform === 'win32' ? 'pgarbor_ffi.dll' : 'libpgarbor_ffi.so';
  let dir = __dirname;
  for (let i = 0; i < 6; i++, dir = path.dirname(dir)) {
    for (const profile of ['release', 'debug']) {
      const cand = path.join(dir, 'target', profile, name);
      if (fs.existsSync(cand)) return cand;
    }
  }
  return name;
}

class PgarborError extends Error {
  constructor(sqlstate, message, detail, hint) {
    super(`${sqlstate}: ${message}`);
    this.name = 'PgarborError';
    this.sqlstate = sqlstate;
    this.detail = detail;
    this.hint = hint;
  }
}

let lib;
function load() {
  if (lib) return lib;
  const l = koffi.load(findLib());
  lib = {
    errcode: l.func('const char *pga_errcode()'),
    errmsg: l.func('const char *pga_errmsg()'),
    errdetail: l.func('const char *pga_errdetail()'),
    errhint: l.func('const char *pga_errhint()'),
    version: l.func('const char *pga_version()'),
    open: l.func('int pga_open(const char *path, uint32_t flags, _Out_ uint64_t *out)'),
    close: l.func('int pga_close(uint64_t db)'),
    sessionOpen: l.func('int pga_session_open(uint64_t db, const char *branch, const char *user, const char *dbname, _Out_ uint64_t *out)'),
    sessionClose: l.func('int pga_session_close(uint64_t s)'),
    execJson: l.func('void *pga_exec_json(uint64_t s, const char *sql)'),
    stringFree: l.func('void pga_string_free(void *p)'),
    branchCreate: l.func('int pga_branch_create(uint64_t db, const char *name, const char *from)'),
    branchDelete: l.func('int pga_branch_delete(uint64_t db, const char *name)'),
    branchList: l.func('const char *pga_branch_list(uint64_t db)'),
  };
  return lib;
}

function lastError() {
  const l = load();
  return new PgarborError(l.errcode() || 'XX000', l.errmsg() || 'unknown error', l.errdetail() || null, l.errhint() || null);
}

class Connection {
  constructor(db, session) { this._db = db; this._session = session; }

  /** Execute SQL; returns {columns, rows, tag, rowCount} of the last statement and `all` of every statement. */
  exec(sql) {
    const l = load();
    if (!this._session) throw new PgarborError('08003', 'connection is closed');
    const ptr = l.execJson(this._session, sql);
    if (!ptr) throw lastError();
    let doc;
    try {
      doc = JSON.parse(koffi.decode(ptr, 'char', -1));
    } finally {
      l.stringFree(ptr);
    }
    const last = doc.length ? doc[doc.length - 1] : { columns: [], rows: [], tag: '' };
    const m = /(\d+)$/.exec(last.tag);
    return { columns: last.columns.map((c) => c.name), rows: last.rows, tag: last.tag,
      rowCount: m ? Number(m[1]) : -1, all: doc };
  }

  createBranch(name, from = 'main') { if (load().branchCreate(this._db, name, from) !== 0) throw lastError(); }
  deleteBranch(name) { if (load().branchDelete(this._db, name) !== 0) throw lastError(); }
  branches() {
    const out = load().branchList(this._db);
    if (out === null) throw lastError();
    return out.split('\n').filter(Boolean);
  }

  close() {
    const l = load();
    if (this._session) { l.sessionClose(this._session); this._session = 0n; }
    if (this._db) {
      const rc = l.close(this._db);
      this._db = 0n;
      if (rc !== 0) throw lastError();
    }
  }
}

function connect(file, { create = true, readOnly = false, memory = false, branch = 'main' } = {}) {
  const l = load();
  const flags = (create ? OPEN_CREATE : 0) | (readOnly ? OPEN_READONLY : 0) | (memory ? OPEN_MEMORY : 0);
  const db = [0n];
  if (l.open(file, flags, db) !== 0) throw lastError();
  const sess = [0n];
  if (l.sessionOpen(db[0], branch, null, null, sess) !== 0) {
    const err = lastError();
    l.close(db[0]);
    throw err;
  }
  return new Connection(db[0], sess[0]);
}

module.exports = { connect, PgarborError, version: () => load().version() };
