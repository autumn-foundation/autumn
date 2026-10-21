#!/usr/bin/env bash
# usage: explain.sh <out-dir> [listing]
# Home/feed/API (`published`) always selects every column. `listing` narrows only
# the admin list to the five columns Post::all_listings selects.
set -eu; P="psql -h 127.0.0.1 -p 5433 -U postgres -d blog"; O=$1
FULL='id, title, slug, body, published, created_at, updated_at'
ADMIN=$FULL; [ "${2:-full}" = listing ] && ADMIN='id, title, slug, published, created_at'
$P -X -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT $FULL FROM posts WHERE published = true ORDER BY created_at DESC" > $O/explain_home_published.txt
$P -X -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT $ADMIN FROM posts ORDER BY created_at DESC" > $O/explain_admin_all.txt
