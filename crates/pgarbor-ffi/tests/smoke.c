#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "pgarbor.h"

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL line %d: %s (%s: %s)\n", __LINE__, #c, pga_errcode(), pga_errmsg()); return 1; } } while (0)

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "smoke.pga";
    uint64_t db = 0, s = 0;
    PgaResult *r = NULL;
    CHECK(pga_open(path, PGA_OPEN_CREATE, &db) == 0);
    CHECK(pga_session_open(db, NULL, NULL, NULL, &s) == 0);
    CHECK(pga_exec(s, "CREATE TABLE t(id int primary key, n text)", &r) == 0);
    CHECK(strcmp(pga_result_tag(r, 0), "CREATE TABLE") == 0);
    pga_result_free(r);
    CHECK(pga_exec(s, "INSERT INTO t VALUES (1,'a'),(2,NULL)", &r) == 0);
    CHECK(strcmp(pga_result_tag(r, 0), "INSERT 0 2") == 0);
    pga_result_free(r);
    CHECK(pga_exec(s, "SELECT id, n FROM t ORDER BY id", &r) == 0);
    CHECK(pga_result_nrows(r, 0) == 2 && pga_result_ncols(r, 0) == 2);
    CHECK(strcmp(pga_result_colname(r, 0, 1), "n") == 0);
    CHECK(strcmp(pga_result_value(r, 0, 0, 0), "1") == 0);
    CHECK(strcmp(pga_result_value(r, 0, 0, 1), "a") == 0);
    CHECK(pga_result_value(r, 0, 1, 1) == NULL);
    pga_result_free(r);

    /* error path */
    CHECK(pga_exec(s, "INSERT INTO t VALUES (1,'dup')", &r) == -1);
    CHECK(strcmp(pga_errcode(), "23505") == 0);
    printf("expected error: %s %s\n", pga_errcode(), pga_errmsg());

    /* params + json */
    const char *ps[1] = { "2" };
    CHECK(pga_exec_params(s, "SELECT n FROM t WHERE id = $1", 1, ps, &r) == 0);
    CHECK(pga_result_nrows(r, 0) == 1 && pga_result_value(r, 0, 0, 0) == NULL);
    pga_result_free(r);
    char *j = pga_exec_json(s, "SELECT 1 AS one");
    CHECK(j != NULL && strstr(j, "\"one\"") != NULL);
    pga_string_free(j);

    CHECK(pga_session_close(s) == 0);
    CHECK(pga_close(db) == 0);

    /* reopen persistence */
    CHECK(pga_open(path, 0, &db) == 0);
    CHECK(pga_session_open(db, "main", "postgres", "postgres", &s) == 0);
    CHECK(pga_exec(s, "SELECT count(*) FROM t", &r) == 0);
    CHECK(strcmp(pga_result_value(r, 0, 0, 0), "2") == 0);
    pga_result_free(r);
    pga_session_close(s);
    pga_close(db);
    printf("C smoke OK (engine %s)\n", pga_version());
    return 0;
}
