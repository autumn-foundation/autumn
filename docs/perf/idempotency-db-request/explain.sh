#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the writes
# `IdempotencyTx::commit` issues for one held key, on the fixture the harness
# `idempotency_db_request_profile` leaves behind. Runs in a transaction that
# rolls back. Usage: explain.sh <postgres-url> [before|after]
set -euo pipefail
url=${1:?postgres url}
mode=${2:-before}
psql "$url" -v ON_ERROR_STOP=1 -X -q <<SQL
BEGIN;
SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint AS now_ms \gset
INSERT INTO autumn_idempotency_keys (storage_key, locked_by, locked_until_ms, expires_at_ms)
VALUES ('v2:explain', 'owner-explain', :now_ms + 60000, :now_ms + 86400000);
SELECT pg_current_wal_insert_lsn() AS lsn0 \gset
\echo '--- commit: write the record'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL)
$(if [ "$mode" = before ]; then cat <<'B'
UPDATE autumn_idempotency_keys
   SET record = decode(repeat('ab', 300), 'hex'), expires_at_ms = :now_ms + 86400000,
       ttl_ms = 86400000, record_owner = 'owner-explain'
 WHERE storage_key = 'v2:explain' AND locked_by = 'owner-explain';
\echo '--- commit: keep_past_crash_lock'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL)
UPDATE autumn_idempotency_keys
   SET expires_at_ms = locked_until_ms + 86400000
 WHERE storage_key = 'v2:explain' AND locked_by = 'owner-explain'
   AND locked_until_ms > :now_ms AND expires_at_ms < locked_until_ms + 86400000;
B
else cat <<'A'
UPDATE autumn_idempotency_keys
   SET record = decode(repeat('ab', 300), 'hex'),
       expires_at_ms = CASE WHEN locked_until_ms > :now_ms
                            THEN locked_until_ms + 86400000
                            ELSE :now_ms + 86400000 END,
       ttl_ms = 86400000, record_owner = 'owner-explain'
 WHERE storage_key = 'v2:explain' AND locked_by = 'owner-explain';
A
fi)
SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(), :'lsn0') AS wal_bytes_for_the_writes;
SELECT expires_at_ms - :now_ms AS expiry_after_now_ms FROM autumn_idempotency_keys WHERE storage_key = 'v2:explain';
ROLLBACK;
SQL
