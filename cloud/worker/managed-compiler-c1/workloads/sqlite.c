#include <sqlite3.h>
#include <stdio.h>
int main(void) {
  sqlite3 *db = 0;
  if (sqlite3_open(":memory:", &db) != SQLITE_OK) return 1;
  if (sqlite3_exec(db, "CREATE TABLE c1(n); INSERT INTO c1 VALUES(42);", 0, 0, 0) != SQLITE_OK) return 2;
  sqlite3_stmt *statement = 0;
  if (sqlite3_prepare_v2(db, "SELECT n FROM c1", -1, &statement, 0) != SQLITE_OK) return 3;
  if (sqlite3_step(statement) != SQLITE_ROW || sqlite3_column_int(statement, 0) != 42) return 4;
  sqlite3_finalize(statement); sqlite3_close(db); puts("c1-sqlite-ok"); return 0;
}
