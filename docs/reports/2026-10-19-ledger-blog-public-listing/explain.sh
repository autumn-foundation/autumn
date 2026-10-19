#!/usr/bin/env bash
# usage: explain.sh <out-dir> ; run against the loaded fixture.
set -eu; P="psql -h 127.0.0.1 -p 5433 -U postgres -d blog"; O=$1
COLS_FULL='id, title, slug, body, published, created_at, updated_at'
COLS_CARD='id, title, slug, published, created_at'
FULL=${COLS_FULL}; CARD=${COLS_CARD}
[ "${2:-full}" = card ] && FULL=$CARD
$P -X -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT $FULL FROM posts WHERE published = true ORDER BY created_at DESC" > $O/explain_home_published.txt
$P -X -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT $FULL FROM posts ORDER BY created_at DESC" > $O/explain_admin_all.txt
