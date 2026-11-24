#!/usr/bin/env bash
# usage: equivalence.sh > equivalence.txt
# Walks every page of sampled threads with the shipped page read
# (statements.sql) and the rewrite (statements-after.sql), and compares each
# page's full result (every column, in the statement's own row order) by md5.
# Edge-case rows are added inside a transaction that is rolled back.
# Needs PG env vars and the database fixture.sql seeds.
set -euo pipefail
d=$(dirname "$0")
{
  echo "BEGIN;"
  python3 "$d/equivalence_sql.py" "$d/statements.sql" "$d/statements-after.sql"
  cat "$d/equivalence_cases.sql"
  echo "\\echo '--- every page, both statements: post | page size | pages | rows | identical'"
  for post in 1 2 3 10 30 100 1000 5000 10000 40000 40001 40002 40003 40004; do
    lims="50 2"; [ "$post" -lt 1000 ] && lims=50
    for lim in $lims; do
      echo "SELECT '$post | $lim | ' || count(*) || ' | ' || sum(o.n) || ' | '
              || (bool_and(o.digest = n.digest AND o.n = n.n AND o.top = n.top)
                  AND count(*) = (SELECT count(*) FROM pg_temp.walk_new($post, $lim)))
              FROM pg_temp.walk_old($post, $lim) o JOIN pg_temp.walk_new($post, $lim) n USING (page);"
    done
  done
  cat <<'SQL'
\echo '--- single pages: case | old | new'
SELECT 'cursor from another thread (a 40004 row, on 40003) | ' || pg_temp.after_old(40003, 9000030, 50) || ' | ' || pg_temp.after_new(40003, 9000030, 50);
SELECT 'cursor that names no row | ' || pg_temp.after_old(40003, 123456789, 50) || ' | ' || pg_temp.after_new(40003, 123456789, 50);
SELECT 'cursor on a deleted top-level comment | ' || pg_temp.after_old(40003, 9000020, 50) || ' | ' || pg_temp.after_new(40003, 9000020, 50);
SELECT 'cursor inside a created_at tie, page size 2 | ' || pg_temp.after_old(40003, 9000013, 2) || ' | ' || pg_temp.after_new(40003, 9000013, 2);
SELECT 'parent that does not exist (40005) | ' || pg_temp.after_old(40005, 9000010, 50) || ' | ' || pg_temp.after_new(40005, 9000010, 50);
\echo '--- post 40003, first page: rows only in old'
SELECT * FROM pg_temp.rows_old(40003) EXCEPT ALL SELECT * FROM pg_temp.rows_new(40003);
\echo '--- post 40003, first page: rows only in new'
SELECT * FROM pg_temp.rows_new(40003) EXCEPT ALL SELECT * FROM pg_temp.rows_old(40003);
\echo '--- post 40003, first page, new statement'
SELECT id, parent_id, author_id, left(body, 32) AS body, length(body) AS len, created_at, author_name, depth
  FROM pg_temp.rows_new(40003);
ROLLBACK;
SQL
} > "$d/.equivalence-run.sql"
psql -X -v ON_ERROR_STOP=1 -qAt -f "$d/.equivalence-run.sql"
rm -f "$d/.equivalence-run.sql"
