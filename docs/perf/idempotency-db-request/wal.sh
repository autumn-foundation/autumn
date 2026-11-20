#!/usr/bin/env bash
# WAL of the key-row writes of one `IdempotencyTx` request, before and after
# folding `keep_past_crash_lock` into the record write. Replays the
# statements the store issues (lock INSERT, record write, release) for 1,024
# keys per variant, one transaction per key as on the request path, on the
# fixture `idempotency_db_request_profile` leaves behind. Reports
# `pg_stat_wal` deltas (records, full-page images, bytes). Run it with
# `full_page_writes = off` for record bytes alone, and on for the total.
# Usage: wal.sh <postgres-url>
set -euo pipefail
url=${1:?postgres url}
run() {
  local variant=$1
  psql "$url" -X -q -v ON_ERROR_STOP=1 -At <<SQL
CHECKPOINT;
SELECT pg_stat_force_next_flush();
SELECT wal_records, wal_fpi, wal_bytes FROM pg_stat_wal \gset b_
DO \$\$
DECLARE i int; k text; now_ms bigint;
BEGIN
  FOR i IN 1..1024 LOOP
    k := 'v2:' || encode(sha256(('$variant-' || i)::bytea), 'hex');
    now_ms := (extract(epoch FROM clock_timestamp()) * 1000)::bigint;
    INSERT INTO autumn_idempotency_keys (storage_key, locked_by, locked_until_ms, expires_at_ms)
    VALUES (k, 'o', now_ms + 60000, now_ms + 86400000);
    COMMIT;
    IF '$variant' = 'before' THEN
      UPDATE autumn_idempotency_keys SET record = decode(repeat('ab', 300), 'hex'),
             expires_at_ms = now_ms + 86400000, ttl_ms = 86400000, record_owner = 'o'
       WHERE storage_key = k AND locked_by = 'o';
      UPDATE autumn_idempotency_keys SET expires_at_ms = locked_until_ms + 86400000
       WHERE storage_key = k AND locked_by = 'o' AND locked_until_ms > now_ms
         AND expires_at_ms < locked_until_ms + 86400000;
    ELSE
      UPDATE autumn_idempotency_keys SET record = decode(repeat('ab', 300), 'hex'),
             expires_at_ms = CASE WHEN locked_until_ms > now_ms THEN locked_until_ms + 86400000
                                  ELSE now_ms + 86400000 END,
             ttl_ms = 86400000, record_owner = 'o'
       WHERE storage_key = k AND locked_by = 'o';
    END IF;
    COMMIT;
    UPDATE autumn_idempotency_keys SET expires_at_ms = ttl_ms + now_ms, locked_by = NULL, locked_until_ms = 0
     WHERE storage_key = k AND locked_by = 'o';
    COMMIT;
  END LOOP;
END \$\$;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT format('%-6s fpw=%s  records=%s  fpi=%s  bytes=%s  bytes/request=%s', '$variant',
              current_setting('full_page_writes'), wal_records - :b_wal_records,
              wal_fpi - :b_wal_fpi, wal_bytes - :b_wal_bytes,
              (wal_bytes - :b_wal_bytes) / 1024)
  FROM pg_stat_wal;
DELETE FROM autumn_idempotency_keys WHERE locked_by IS NULL AND record_owner = 'o';
SQL
}
run before
run after
