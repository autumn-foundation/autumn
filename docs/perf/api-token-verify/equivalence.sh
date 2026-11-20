#!/usr/bin/env bash
# Runs the old and the new last_used_at protocol of
# DbApiTokenStore::verify_scoped on two copies of the same rows and compares
# the whole rows afterwards.
#   old: SELECT the live token, then always the guarded UPDATE.
#   new: SELECT the live token with last_used_at; send the same guarded
#        UPDATE only when last_used_at IS NULL OR last_used_at < threshold.
# Cases cover every side of the 5-minute threshold, a stamp from a replica
# whose clock runs ahead, revoked and expired tokens, and a stamp another
# replica writes between the SELECT and the UPDATE. Uses the PG* environment.
set -euo pipefail
psql -X -v ON_ERROR_STOP=1 -q <<'SQL'
DROP TABLE IF EXISTS eq_old, eq_new;
CREATE TABLE eq_old (id int PRIMARY KEY, token_hash text, revoked_at timestamp,
                     expires_at timestamp, last_used_at timestamp, note text);
-- "now" is fixed so the cases sit exactly on the threshold.
CREATE TEMP TABLE clock AS SELECT timestamp '2026-11-20 12:00:00.123456' AS now;
INSERT INTO eq_old SELECT * FROM (VALUES
  (1, 'h1', NULL::timestamp, NULL::timestamp, NULL::timestamp, 'never used'),
  (2, 'h2', NULL, NULL, timestamp '2026-11-20 11:55:00.123455', 'threshold - 1us'),
  (3, 'h3', NULL, NULL, timestamp '2026-11-20 11:55:00.123456', 'exactly threshold'),
  (4, 'h4', NULL, NULL, timestamp '2026-11-20 11:55:00.123457', 'threshold + 1us'),
  (5, 'h5', NULL, NULL, timestamp '2026-11-20 11:59:59', 'used 1s ago'),
  (6, 'h6', NULL, NULL, timestamp '2026-11-20 12:03:00', 'stamped by a clock 3 min ahead'),
  (7, 'h7', NULL, NULL, timestamp '2026-10-01 00:00:00', 'used 7 weeks ago'),
  (8, 'h8', timestamp '2026-11-01', NULL, timestamp '2026-10-01', 'revoked'),
  (9, 'h9', NULL, timestamp '2026-11-20 11:00', timestamp '2026-10-01', 'expired'),
  (10,'h10', NULL, timestamp '2027-01-01', timestamp '2026-11-20 11:58', 'future expiry, fresh'),
  (11,'h11', NULL, NULL, timestamp '2026-11-20 11:00', 'stale; another replica stamps it between SELECT and UPDATE')
) v;
CREATE TABLE eq_new AS SELECT * FROM eq_old;
ALTER TABLE eq_new ADD PRIMARY KEY (id);

CREATE FUNCTION verify(tbl regclass, hash text, protocol text) RETURNS int LANGUAGE plpgsql AS $f$
DECLARE n timestamp := (SELECT now FROM clock); th timestamp := n - interval '5 minutes';
        tid int; used timestamp; touched int := 0;
BEGIN
  EXECUTE format('SELECT id, last_used_at FROM %s WHERE token_hash = $1 AND revoked_at IS NULL
                  AND (expires_at IS NULL OR expires_at > $2) LIMIT 1', tbl)
    INTO tid, used USING hash, n;
  IF tid IS NULL THEN RETURN -1; END IF;            -- 401, no write in either protocol
  IF hash = 'h11' THEN                               -- the concurrent stamp
    EXECUTE format('UPDATE %s SET last_used_at = $1 WHERE id = $2', tbl) USING n - interval '1 second', tid;
  END IF;
  IF protocol = 'old' OR used IS NULL OR used < th THEN
    EXECUTE format('UPDATE %s SET last_used_at = $1 WHERE id = $2
                    AND (last_used_at IS NULL OR last_used_at < $3)', tbl) USING n, tid, th;
    GET DIAGNOSTICS touched = ROW_COUNT;
  END IF;
  RETURN touched;
END $f$;

\echo '--- per case: rows written (old, new); -1 = not a live token'
SELECT id, note, verify('eq_old', token_hash, 'old') AS old, verify('eq_new', token_hash, 'new') AS new
FROM eq_old ORDER BY id;
\echo '--- second request, same instant (old, new)'
SELECT id, verify('eq_old', token_hash, 'old') AS old, verify('eq_new', token_hash, 'new') AS new
FROM eq_old ORDER BY id;
\echo '--- rows that differ between the two tables after both requests'
SELECT count(*) AS differing_rows FROM (
  (SELECT * FROM eq_old EXCEPT SELECT * FROM eq_new)
  UNION ALL
  (SELECT * FROM eq_new EXCEPT SELECT * FROM eq_old)) d;
DROP FUNCTION verify(regclass, text, text);
DROP TABLE eq_old, eq_new;
SQL
