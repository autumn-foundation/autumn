#!/usr/bin/env bash
# usage: run.sh <before|after> [fixture] > <variant>.txt
# Drives the router's GET /comments/{type}/{id}[?after=] read
# (commentable::comment_thread_page, page size 50) for threads of seven sizes,
# pages 1-3 of each, five times each, and prints pg_stat_statements totals.
# `before` is the shipped SQL (statements.sql), `after` the rewrite
# (statements-after.sql). `fixture` seeds the database first (fixture.sql).
# Needs PG env vars and a server that preloads pg_stat_statements.
set -euo pipefail
variant="${1:?usage: run.sh <before|after> [fixture]}"
d=$(dirname "$0")
case "$variant" in
  before) stmts="$d/statements.sql" ;;
  after) stmts="$d/statements-after.sql" ;;
  *) echo "unknown variant $variant" >&2; exit 2 ;;
esac
p() { psql -X -v ON_ERROR_STOP=1 "$@"; }
[ "${2:-}" = fixture ] && p -q -f "$d/fixture.sql" >/dev/null
p -qAtc "CREATE EXTENSION IF NOT EXISTS pg_stat_statements" >/dev/null
posts="1 3 10 30 100 1000 10000"
limit=50
reps=5

# Page cursors: the ids of the 50th and 100th live top-level comment, i.e. the
# `next` of pages 1 and 2. Read outside the measured window.
work=$(mktemp)
trap 'rm -f "$work"' EXIT
{
  cat "$stmts"
  # Warm the backend's catalog caches outside the measured window.
  echo "EXECUTE probe(10000);"
  echo "EXECUTE page_first('Post', 10000, $limit, 'Post', 10000, 10000);"
  echo "EXECUTE page_after('Post', 10000, 0, 'Post', 10000, $limit, 'Post', 10000, 10000);"
  echo "EXECUTE next_top('Post', 10000, 0, 'Post', 10000);"
  echo "SET pg_stat_statements.track_utility = off;"
  echo "SELECT pg_stat_statements_reset();"
  for post in $posts; do
    cursors=$(p -qAt -c "SELECT string_agg(id::text, ' ' ORDER BY rn) FROM (
        SELECT id, row_number() OVER (ORDER BY created_at, id) rn FROM comments
         WHERE commentable_type = 'Post' AND commentable_id = $post
           AND parent_id IS NULL AND deleted_at IS NULL) x WHERE rn IN ($limit, 2 * $limit)")
    live=$(p -qAt -c "SELECT count(*) FROM comments WHERE commentable_type = 'Post'
        AND commentable_id = $post AND parent_id IS NULL AND deleted_at IS NULL")
    for _ in $(seq $reps); do
      echo "EXECUTE probe($post);"
      echo "EXECUTE page_first('Post', $post, $limit, 'Post', $post, $post);"
      set -- $cursors
      [ "$live" -gt "$limit" ] && echo "EXECUTE next_top('Post', $post, $1, 'Post', $post);"
      page=1
      for c in $cursors; do
        page=$((page + 1))
        echo "EXECUTE probe($post);"
        echo "EXECUTE page_after('Post', $post, $c, 'Post', $post, $limit, 'Post', $post, $post);"
        [ "$live" -gt $((page * limit)) ] && echo "EXECUTE next_top('Post', $post, $(p -qAt -c "SELECT id FROM (
            SELECT id, row_number() OVER (ORDER BY created_at, id) rn FROM comments
             WHERE commentable_type = 'Post' AND commentable_id = $post
               AND parent_id IS NULL AND deleted_at IS NULL) x WHERE rn = $((page * limit))"), 'Post', $post);"
      done
    done
  done
  echo "SET pg_stat_statements.track_utility = on;"
} > "$work"
p -q -f "$work" >/dev/null

echo "variant=$variant"
p -qAtc "SELECT version()"
p -qAtc "SELECT 'fixture comments=' || count(*) || ' posts=' || count(DISTINCT commentable_id) FROM comments"
echo "--- pg_stat_statements, the whole workload (${reps}x pages 1-3 of posts $posts)"
p -qAtF ' | ' -c "
  WITH s AS (SELECT substring(query from 'PREPARE ([a-z_]+)') AS stmt,
                    calls, rows, shared_blks_hit + shared_blks_read AS buffers,
                    shared_blks_hit, shared_blks_read, temp_blks_read, temp_blks_written
               FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
                AND query LIKE 'PREPARE %')
  SELECT stmt, 'calls=' || sum(calls), 'rows=' || sum(rows), 'buffers=' || sum(buffers),
         'hit=' || sum(shared_blks_hit), 'read=' || sum(shared_blks_read),
         'temp_r=' || sum(temp_blks_read), 'temp_w=' || sum(temp_blks_written),
         'buffers%=' || round(100.0 * sum(buffers) / sum(sum(buffers)) OVER (), 1),
         'calls%=' || round(100.0 * sum(calls) / sum(sum(calls)) OVER (), 1)
    FROM s GROUP BY stmt ORDER BY sum(buffers) DESC"

echo "--- page statement buffers per call, by thread size and page"
printf '%-6s %-7s %-5s %8s %6s\n' post comments page buffers rows
for post in $posts; do
  n=$(p -qAt -c "SELECT count(*) FROM comments WHERE commentable_id = $post")
  cursors=$(p -qAt -c "SELECT string_agg(id::text, ' ' ORDER BY rn) FROM (
      SELECT id, row_number() OVER (ORDER BY created_at, id) rn FROM comments
       WHERE commentable_type = 'Post' AND commentable_id = $post
         AND parent_id IS NULL AND deleted_at IS NULL) x WHERE rn IN ($limit, 2 * $limit)")
  page=1
  for c in "" $cursors; do
    if [ -z "$c" ]; then q="EXECUTE page_first('Post', $post, $limit, 'Post', $post, $post)"
    else q="EXECUTE page_after('Post', $post, $c, 'Post', $post, $limit, 'Post', $post, $post)"; fi
    # One unmeasured run first: a fresh backend's first execution also loads
    # catalog caches, and that would be charged to the statement.
    { cat "$stmts"; echo "$q;"; echo "SET pg_stat_statements.track_utility = off; SELECT pg_stat_statements_reset();";
      for _ in $(seq $reps); do echo "$q;"; done; } | p -q >/dev/null
    p -qAtF ' ' -c "SELECT round(sum(shared_blks_hit + shared_blks_read)::numeric / sum(calls), 1), sum(rows) / sum(calls)
      FROM pg_stat_statements WHERE query LIKE 'PREPARE page_%'" |
      while read -r b r; do printf '%-6s %-7s %-5s %8s %6s\n' "$post" "$n" "$page" "$b" "$r"; done
    page=$((page + 1))
  done
done
