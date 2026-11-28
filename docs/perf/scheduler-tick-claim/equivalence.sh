#!/usr/bin/env bash
# Result equivalence of the tick claim: every case runs the attempt as BASE
# issues it and as the working tree issues it, each on its own copy of the
# same table, and prints what the attempt returns and the unexpired rows it
# leaves. The two must match on every line.
#
# Usage: equivalence.sh BASE   (a git rev; PG* env names the server)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../../.."
base="${1:?usage: equivalence.sh BASE}"
work="$(mktemp -d)"
trap 'rm -rf "$work"; [ -n "${holder:-}" ] && kill "$holder" 2>/dev/null || true' EXIT
git -C "$root" show "$base:autumn/src/scheduler.rs" > "$work/base.rs"
python3 "$here/extract_sql.py" "$work/base.rs" "$work/old" >/dev/null
python3 "$here/extract_sql.py" "$root/autumn/src/scheduler.rs" "$work/new" >/dev/null

# The binds of one attempt, as psql variables, then the attempt itself. A
# CTE reads the RETURNING of the claim and the row count of the prune, as the
# Rust code does; psql cannot read them otherwise.
bind() { sed -e "s/\\\$1/:'prefix'::text/; s/\\\$2/:'task'::text/; s/\\\$3/:'tick'::text/; s/\\\$4/:'owner'::text/; s/\\\$5/:hold::float8/; s/\\\$6/:akey::int8/" "$1" | tr -d '\n'; }
attempt() {  # $1 = sql dir: the psql script of one try_acquire_for_period
  local d="$1"
  if [ -f "$d/prune_key.sql" ]; then
    cat <<SQL
\\set generation 0
WITH c AS ($(bind "$d/claim.sql")) SELECT coalesce(max(generation), 0) AS generation FROM c \\gset
SELECT :generation > 0 AS won \\gset
\\if :won
\\else
WITH d AS ($(bind "$d/prune_key.sql") RETURNING 1) SELECT count(*) > 0 AS pruned FROM d \\gset
\\if :pruned
WITH c AS ($(bind "$d/claim.sql")) SELECT coalesce(max(generation), 0) AS generation FROM c \\gset
SELECT :generation > 0 AS won \\gset
\\endif
\\endif
\\if :won
$(cat "$d/prune.sql" | tr -d '\n');
\\endif
SQL
  else
    cat <<SQL
\\set generation 0
$(cat "$d/prune.sql" | tr -d '\n');
WITH c AS ($(bind "$d/claim.sql")) SELECT coalesce(max(generation), 0) AS generation FROM c \\gset
SQL
  fi
}
akey() { psql -X -At -c "SELECT ('x' || left(encode(sha256(convert_to('$1', 'UTF8') || '\\x00'::bytea || convert_to('$2', 'UTF8') || '\\x00'::bytea || convert_to('$3', 'UTF8')), 'hex'), 16))::bit(64)::bigint"; }

# The table each case starts from: this app's live and expired rows, another
# app's expired row, generations 1..5.
seed="
DROP TABLE IF EXISTS autumn_scheduler_ticks;
$(cat "$work/old/ddl.sql")
INSERT INTO autumn_scheduler_ticks (key_prefix, task_name, tick_key, owner, expires_at) VALUES
  ('autumn:scheduler', 'digest', 'digest:100', 'replica-b', now() + interval '1 hour'),
  ('autumn:scheduler', 'digest', 'digest:99',  'replica-b', now() - interval '1 second'),
  ('autumn:scheduler', 'acme',   'acme',       'replica-b', now() - interval '1 second'),
  ('autumn:scheduler', 'digest', 'digest:98',  'replica-c', now() - interval '1 hour'),
  ('other:scheduler',  'digest', 'digest:100', 'replica-z', now() - interval '1 second');"

run_case() {  # $1 = label, $2 = prefix, $3 = task, $4 = tick, $5 = hold the advisory key?
  local label="$1" prefix="$2" task="$3" tick="$4" hold="$5" k out
  k="$(akey "$prefix" "$task" "$tick")"
  for side in old new; do
    psql -X -q -v ON_ERROR_STOP=1 -c "$seed" >/dev/null
    if [ "$hold" = yes ]; then
      psql -X -q -c "SELECT pg_advisory_lock($k)" -c "SELECT pg_sleep(30)" >/dev/null 2>&1 &
      holder=$!
      until [ "$(psql -X -At -c "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted")" = 1 ]; do sleep 0.05; done
    fi
    out="$( { echo "\\set prefix '$prefix'"; echo "\\set task '$task'"; echo "\\set tick '$tick'";
             echo "\\set owner 'replica-a'"; echo "\\set hold 300"; echo "\\set akey $k";
             attempt "$work/$side";
             echo "SELECT CASE WHEN :generation > 5 THEN 'won, generation above every earlier one' WHEN :generation = 0 THEN 'lost' ELSE 'BAD generation ' || :generation END AS result;";
             echo "SELECT key_prefix || '/' || tick_key || ' ' || owner FROM autumn_scheduler_ticks WHERE expires_at >= now() ORDER BY key_prefix, task_name, tick_key;";
           } | psql -X -q -At -v ON_ERROR_STOP=1 2>&1 | tr '\n' ';')"
    expired="$(psql -X -At -c "SELECT count(*) FROM autumn_scheduler_ticks WHERE expires_at < now()")"
    if [ "$hold" = yes ]; then kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null || true; holder=; fi
    eval "out_$side=\$out; expired_$side=\$expired"
  done
  if [ "$out_old" = "$out_new" ]; then verdict=same; else verdict=DIFFERENT; fi
  printf '%-58s %-9s %s\n' "$label" "$verdict" "$out_new"
  printf '%-58s %-9s expired rows left: base %s, new %s\n' "" "" "$expired_old" "$expired_new"
  [ "$verdict" = same ]
}

status=0
run_case "free key" autumn:scheduler digest digest:101 no || status=1
run_case "key holds a live row" autumn:scheduler digest digest:100 no || status=1
run_case "key holds an expired row (late replica)" autumn:scheduler digest digest:99 no || status=1
run_case "constant key, expired row (ACME mutex)" autumn:scheduler acme acme no || status=1
run_case "free key, advisory key held (pre-#3052 replica)" autumn:scheduler digest digest:101 yes || status=1
run_case "expired row, advisory key held" autumn:scheduler digest digest:99 yes || status=1
run_case "another prefix, key live here, expired there" other:scheduler digest digest:100 no || status=1
run_case "same key in another prefix" other:scheduler digest digest:101 no || status=1
exit $status
