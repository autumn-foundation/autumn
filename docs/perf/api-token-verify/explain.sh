#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the statements
# DbApiTokenStore::verify_scoped sends for one request, on the database the
# harness (autumn/tests/integration/api_token_verify_profile.rs) left behind.
# The token was last used a minute ago, so its last_used_at stamp is not due.
# Writes run in a transaction that rolls back. Uses the PG* environment.
set -euo pipefail
ID=${HOT_ID:-1000}
HASH=$(printf 'fixture-token-%s' "$ID" | sha256sum | cut -d' ' -f1)
psql -X -v ON_ERROR_STOP=1 -q <<SQL
UPDATE api_tokens SET last_used_at = (now() AT TIME ZONE 'UTC') - interval '1 minute'
WHERE token_hash = '$HASH';
SELECT id AS token_id FROM api_tokens WHERE token_hash = '$HASH' \\gset
CHECKPOINT;
\echo '--- verify_scoped: look up the live token (before: no last_used_at)'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
SELECT "api_tokens"."id", "api_tokens"."principal_id", "api_tokens"."name",
       "api_tokens"."expires_at", "api_tokens"."scopes"
FROM "api_tokens"
WHERE ((("api_tokens"."token_hash" = '$HASH') AND ("api_tokens"."revoked_at" IS NULL))
  AND (("api_tokens"."expires_at" IS NULL) OR ("api_tokens"."expires_at" > (now() AT TIME ZONE 'UTC'))))
LIMIT 1;
\echo '--- verify_scoped: look up the live token (after: with last_used_at)'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
SELECT "api_tokens"."id", "api_tokens"."principal_id", "api_tokens"."name",
       "api_tokens"."expires_at", "api_tokens"."scopes", "api_tokens"."last_used_at"
FROM "api_tokens"
WHERE ((("api_tokens"."token_hash" = '$HASH') AND ("api_tokens"."revoked_at" IS NULL))
  AND (("api_tokens"."expires_at" IS NULL) OR ("api_tokens"."expires_at" > (now() AT TIME ZONE 'UTC'))))
LIMIT 1;
\echo '--- verify_scoped: stamp last_used_at (not due: matches no row)'
BEGIN;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
UPDATE "api_tokens" SET "last_used_at" = (now() AT TIME ZONE 'UTC')
WHERE (("api_tokens"."id" = :token_id)
  AND (("api_tokens"."last_used_at" IS NULL)
    OR ("api_tokens"."last_used_at" < (now() AT TIME ZONE 'UTC') - interval '5 minutes')));
ROLLBACK;
SQL
