#!/usr/bin/env bash
# usage: explain.sh <before|after> > explain-<variant>.txt
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of page 1 of a 15,000-comment
# thread (post 10) and page 2 of the 150,000-comment thread (post 1), each
# after five executions so the plan is the cached one the workload runs.
set -euo pipefail
d=$(dirname "$0")
f="$d/statements.sql"; [ "${1:?usage: explain.sh <before|after>}" = after ] && f="$d/statements-after.sql"
c=$(psql -X -qAt -c "SELECT id FROM (SELECT id, row_number() OVER (ORDER BY created_at, id) rn FROM comments
  WHERE commentable_type = 'Post' AND commentable_id = 1 AND parent_id IS NULL AND deleted_at IS NULL) x WHERE rn = 50")
{
  cat "$f"
  for _ in 1 2 3 4 5; do
    echo "EXECUTE page_first('Post', 10, 50, 'Post', 10, 10);"
    echo "EXECUTE page_after('Post', 1, $c, 'Post', 1, 50, 'Post', 1, 1);"
  done
  echo "\\echo '--- page 1, post 10 (15,000 comments)'"
  echo "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) EXECUTE page_first('Post', 10, 50, 'Post', 10, 10);"
  echo "\\echo '--- page 2, post 1 (150,000 comments), after $c'"
  echo "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) EXECUTE page_after('Post', 1, $c, 'Post', 1, 50, 'Post', 1, 1);"
} | psql -X -v ON_ERROR_STOP=1 -qAt | grep -v '^PREPARE$' | grep -v '^[0-9]*|' | awk 'BEGIN{p=0} /^---/{p=1} p'
