#!/usr/bin/env bash
# Result equivalence of the expiry `IdempotencyTx::commit` writes: the old
# record write followed by `keep_past_crash_lock`, against the one write with
# the CASE. Each case runs both on a copy of the same row and compares the
# whole row. Cases: a live lock, a lock that ends exactly at `now` (not
# live), an expired lock, a lock held by another owner (no write), a row with
# a later expiry than both (the old write overwrites it too), and a TTL at the
# clamp (`i64::MAX / 16`). Usage: equivalence.sh <postgres-url>
set -euo pipefail
psql "${1:?postgres url}" -X -q -v ON_ERROR_STOP=1 <<'SQL'
BEGIN;
CREATE TEMP TABLE cases (name text, locked_by text, locked_until_ms bigint, expires_at_ms bigint, ttl bigint);
-- now = 1,000,000,000,000 ms; owner 'o'
INSERT INTO cases VALUES
  ('live lock',            'o', 1000000060000, 1000086400000, 86400000),
  ('lock ends at now',     'o', 1000000000000, 1000086400000, 86400000),
  ('expired lock',         'o',  999999990000, 1000086400000, 86400000),
  ('other owner',          'x', 1000000060000, 1000086400000, 86400000),
  ('later expiry kept?',   'o', 1000000060000, 9000000000000, 86400000),
  ('ttl at the clamp',     'o', 1000000060000, 1000086400000, 576460752303423487),
  ('zero ttl, live lock',  'o', 1000000060000, 1000086400000, 0);
CREATE TEMP TABLE a AS SELECT * FROM cases;
CREATE TEMP TABLE b AS SELECT * FROM cases;
-- before: two statements
UPDATE a SET expires_at_ms = 1000000000000 + ttl WHERE locked_by = 'o';
UPDATE a SET expires_at_ms = locked_until_ms + ttl
 WHERE locked_by = 'o' AND locked_until_ms > 1000000000000 AND expires_at_ms < locked_until_ms + ttl;
-- after: one statement
UPDATE b SET expires_at_ms = CASE WHEN locked_until_ms > 1000000000000 THEN locked_until_ms + ttl
                                  ELSE 1000000000000 + ttl END
 WHERE locked_by = 'o';
SELECT a.name, a.expires_at_ms AS before, b.expires_at_ms AS after,
       CASE WHEN row(a.*) IS NOT DISTINCT FROM row(b.*) THEN 'same' ELSE 'DIFFERENT' END AS result
  FROM a JOIN b USING (name) ORDER BY a.name;
SELECT count(*) FILTER (WHERE row(a.*) IS DISTINCT FROM row(b.*)) AS differing_rows
  FROM a JOIN b USING (name);
ROLLBACK;
SQL
