#!/usr/bin/env bash
# Drain the edge-case fixture with the shipped claim (claim.sql) and the rewrite
# (claim_after.sql); print each claimed-id sequence per scenario, then diff them.
# Scenarios: deferred set x ack cadence (0 = never ack, so slots fill; 1 = ack each).
set -euo pipefail
cd "$(dirname "$0")"
DB=ledger_jobclaim_eq
dropdb --if-exists "$DB"; createdb "$DB"
psql -qX -v ON_ERROR_STOP=1 -d "$DB" -f schema.sql >/dev/null
sql_of() { sed -n '/^PREPARE claim/,/;$/p' "$1" | sed '1s/^PREPARE claim(text, text, text\[\]) AS//' | tr '\n' ' ' | sed 's/; *$//'; }
psql -qX -v ON_ERROR_STOP=1 -d "$DB" <<'SQL' >/dev/null
CREATE OR REPLACE FUNCTION drain(claim_sql text, deferred text[], ack_every int) RETURNS text AS $$
DECLARE got text; seq text[] := '{}'; n int := 0;
BEGIN
  LOOP
    EXECUTE 'WITH c AS (' || claim_sql || ') SELECT id FROM c' INTO got USING 'w', 'default', deferred;
    EXIT WHEN got IS NULL OR n > 100;
    seq := seq || got; n := n + 1;
    IF ack_every > 0 AND n % ack_every = 0 THEN
      UPDATE autumn_jobs SET status = 'completed', finished_at = now(), claimed_by = NULL, claimed_at = NULL
      WHERE id = got AND claimed_by = 'w' AND status = 'running';
    END IF;
  END LOOP;
  RETURN array_to_string(seq, ' ');
END $$ LANGUAGE plpgsql;
SQL
for variant in claim.sql claim_after.sql; do
  SQLTEXT=$(sql_of "$variant")
  for deferred in '{}' '{parked}' '{parked,no.such.job}' '{grp,mix,plain,parked,sat,xq,z}'; do
    for ack in 0 1 2; do
      psql -qX -v ON_ERROR_STOP=1 -d "$DB" -f equivalence_fixture.sql >/dev/null
      seq=$(psql -qXAt -v ON_ERROR_STOP=1 -d "$DB" -v q="$SQLTEXT" -v d="$deferred" -v a="$ack" \
            <<<"SELECT drain(:'q', :'d'::text[], :a)")
      echo "deferred=$deferred ack=$ack: $seq"
    done
  done > "eq-$variant.out"
done
echo "--- shipped claim"; cat eq-claim.sql.out
if diff eq-claim.sql.out eq-claim_after.sql.out; then echo "IDENTICAL: 12 scenarios"; else echo "DIFFERENT"; fi
rm -f eq-claim.sql.out eq-claim_after.sql.out
