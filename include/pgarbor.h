/* PGArbor C API. Hand-written; mirrors crates/pgarbor-ffi/src/lib.rs.
 *
 * Handles are uint64_t (0 is never valid) and are bound to the thread that created them.
 * Functions return 0 on success, -1 on failure; the per-thread last error is then available via
 * pga_errcode (SQLSTATE) / pga_errmsg / pga_errdetail / pga_errhint (pointers valid until the next
 * pga_* call on the same thread). All strings are UTF-8.
 */
#ifndef PGARBOR_H
#define PGARBOR_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define PGA_OPEN_CREATE   1u
#define PGA_OPEN_READONLY 2u
#define PGA_OPEN_MEMORY   4u

typedef struct PgaResult PgaResult;

const char *pga_version(void);

const char *pga_errcode(void);
const char *pga_errmsg(void);
const char *pga_errdetail(void); /* may be NULL */
const char *pga_errhint(void);   /* may be NULL */

int pga_open(const char *path, uint32_t flags, uint64_t *out_db);
int pga_close(uint64_t db);

/* branch/user/dbname may be NULL (defaults "main"/"postgres"/"postgres"). */
int pga_session_open(uint64_t db, const char *branch, const char *user, const char *dbname, uint64_t *out_session);
int pga_session_close(uint64_t session);

/* Simple-query execution; *out must be released with pga_result_free. */
int pga_exec(uint64_t session, const char *sql, PgaResult **out);
/* One statement with text parameters; params[i] == NULL is SQL NULL. */
int pga_exec_params(uint64_t session, const char *sql, uint32_t nparams, const char *const *params, PgaResult **out);
/* JSON: [{"columns":[{"name":..,"type_oid":..}],"rows":[[..]],"tag":".."}]; free with pga_string_free; NULL on error. */
char *pga_exec_json(uint64_t session, const char *sql);
void pga_string_free(char *s);

void pga_result_free(PgaResult *r);
uint32_t pga_result_nstmts(const PgaResult *r);
const char *pga_result_tag(const PgaResult *r, uint32_t stmt);
uint32_t pga_result_ncols(const PgaResult *r, uint32_t stmt);
uint32_t pga_result_nrows(const PgaResult *r, uint32_t stmt);
const char *pga_result_colname(const PgaResult *r, uint32_t stmt, uint32_t col);
uint32_t pga_result_coltype(const PgaResult *r, uint32_t stmt, uint32_t col);
/* NULL for SQL NULL (or out of range). */
const char *pga_result_value(const PgaResult *r, uint32_t stmt, uint32_t row, uint32_t col);

/* 'I', 'T' or 'E'; 0 on error. */
int pga_txn_status(uint64_t session);
int pga_cancel(uint64_t session);

int pga_branch_create(uint64_t db, const char *name, const char *from);
int pga_branch_delete(uint64_t db, const char *name);
/* newline-separated names; valid until next pga_branch_list on this thread; NULL on error. */
const char *pga_branch_list(uint64_t db);

#ifdef __cplusplus
}
#endif
#endif
