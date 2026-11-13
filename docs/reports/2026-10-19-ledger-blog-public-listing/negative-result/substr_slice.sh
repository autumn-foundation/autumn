#!/usr/bin/env bash
# Does substr(body,1,200) avoid detoasting the whole body on the home-card query? (No.)
P="psql -h 127.0.0.1 -p 5433 -U postgres -d blog -X"
$P -qAt -c "select pg_stat_statements_reset()" >/dev/null
for q in "select sum(length(substr(body,1,200))) from posts where published" "select sum(octet_length(body)) from posts where published" "select sum(length(body)) from posts where published" "select count(id) from posts where published"; do $P -qAt -c "$q" >/dev/null; done
$P -c "select left(query,70) q, shared_blks_hit+shared_blks_read buffers from pg_stat_statements where query like 'select sum%' or query like 'select count%' order by 1"
$P -Atc "show server_encoding" -c "select attstorage, attcompression from pg_attribute where attrelid='posts'::regclass and attname='body'" -c "show default_toast_compression"
