-- Requires pg_stat_statements in shared_preload_libraries (see
-- ../README.md's "Reproduce" section) -- without it, pg_stat_statements_reset()
-- below errors and ON_ERROR_STOP stops the script rather than silently
-- continuing past a profile that was never collected.
\set ON_ERROR_STOP on

-- Capture (not profiled -- these run before the reset below, and their
-- array_agg wrapper is a different statement shape than what front_page
-- actually sends, so they must not appear in the profiled window). Needed
-- because the fixture is seeded (not hand-picked ids), so the hot-posts
-- page, the ids `preload()` batches, and the leaderboard's actual top-5
-- winners are only known once the seeded data exists.
SELECT
    array_agg(DISTINCT author_id) AS author_ids,
    array_agg(DISTINCT subreddit_id) AS subreddit_ids
FROM (SELECT author_id, subreddit_id FROM posts ORDER BY hot_rank DESC LIMIT 25) hp \gset hp_

SELECT array_agg(post_id ORDER BY s DESC NULLS LAST, post_id ASC) AS winners
FROM (
    SELECT post_id, SUM(value) AS s FROM votes
    WHERE post_id IS NOT NULL GROUP BY post_id ORDER BY s DESC NULLS LAST, post_id ASC LIMIT 5
) w \gset lb_

-- Scoped reset: the bare pg_stat_statements_reset() clears stats for every
-- database and role in the cluster, which on a shared/reused server would
-- disrupt unrelated monitoring even though the reads above are scoped.
-- Passing userid/dbid resets only this session's statements.
SELECT pg_stat_statements_reset(
    (SELECT oid FROM pg_roles WHERE rolname = current_user),
    (SELECT oid FROM pg_database WHERE datname = current_database())
);

-- 1. front_page's hot_posts listing
SELECT id, title, slug, body, url, author_id, subreddit_id, score, hot_rank, comment_count, created_at, updated_at
FROM posts ORDER BY hot_rank DESC LIMIT 25;

-- 2 & 3. front_page's `repo.on_primary().preload(hot_posts, Post::preload().author().subreddit())`
-- (posts.rs:176-179) -- the batched belongs_to lookups for the page of posts.
-- Approximated from the schema (author/subreddit are plain belongs_to, no
-- soft-delete/tenant guard on either table) -- not verified byte-identical
-- against the preload macro's codegen the way the leaderboard query below is.
SELECT * FROM users WHERE id = ANY(:'hp_author_ids'::bigint[]);
SELECT * FROM subreddits WHERE id = ANY(:'hp_subreddit_ids'::bigint[]);

-- 4. front_page's top-by-votes leaderboard
-- The codegen (autumn-macros-repository/src/repository.rs:14354-14364,
-- 14396-14398; real trait call verified in
-- examples/reddit-clone/tests/votable_pg_integration.rs:443-449) binds
-- eq/low/high as three REUSED parameters -- each appears twice in the SQL
-- text (`($1 IS NULL OR post_id = $1)`, etc.) but is bound once -- and
-- leaves `LIMIT 5` a literal (`__lim` is `format!`-interpolated into the
-- SQL string, never bound). A plain literal `NULL::bigint` written three
-- times, as an earlier version of this harness did, is textually THREE
-- separate constants to Postgres's query jumbler, not one reused twice --
-- pg_stat_statements then normalizes it to 6 distinct placeholders instead
-- of 3, which doesn't match the shape a real trace of this app would show.
-- PREPARE/EXECUTE reproduces the real reuse structure (verified: the
-- executed plan/buffers are unaffected either way -- a freshly prepared
-- statement's first execution costs itself using the actual bound values,
-- identical to a literal query, so this is a query-*text* fidelity fix,
-- not a plan or buffer-count fix). The one artifact this leaves: the row
-- pg_stat_statements records for this statement carries a
-- `PREPARE leaderboard_lookup (...) AS` prefix that a driver-level bind
-- (as the app's tokio-postgres/diesel-async stack actually sends, via the
-- wire protocol rather than textual PREPARE) would not have -- psql has no
-- clean way to reproduce that without one (its `\bind` meta-command can't
-- pass a typed SQL NULL, only literal text). Cosmetic difference in the
-- recorded query text only.
PREPARE leaderboard_lookup (bigint, bigint, bigint) AS
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
  AND ($1 IS NULL OR post_id = $1)
  AND ($2 IS NULL OR post_id >= $2)
  AND ($3 IS NULL OR post_id <= $3)
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;
EXECUTE leaderboard_lookup(NULL, NULL, NULL);
DEALLOCATE leaderboard_lookup;

-- 5. front_page's title resolution for the leaderboard's actual top ids
-- (captured above, not hard-coded -- this fixture is seeded but the winners
-- still depend on the full vote distribution, so pinning literal ids here
-- would silently stop matching the data on any fixture change).
SELECT id, title FROM posts WHERE id = ANY(:'lb_winners'::bigint[]);

-- 6. front_page's `flags.enabled("new_ui_preview")` (posts.rs:195). With a
-- real primary database configured, `build_store` resolves to `PgFlagStore`
-- (examples/reddit-clone/src/feature_flags.rs:15-18), whose 1-second cache
-- (autumn/src/feature_flags.rs:688-706) means a request landing on a cold
-- cache issues this lookup. This profile represents that cold-cache case --
-- a request within the same second as a prior one for this flag would skip
-- it. Not wrapped in PREPARE/EXECUTE like statement 4: this one has a
-- single non-reused parameter, so a literal produces the same normalized
-- shape either way.
SELECT key, description, enabled, rollout_pct, actor_allowlist, group_allowlist
FROM autumn_feature_flags WHERE key = 'new_ui_preview';

\echo '--- pg_stat_statements profile (6 front-page statements) ---'
-- pg_stat_statements is cluster-wide, not scoped to this database: on a
-- reused/shared Postgres instance with other databases active, an
-- unfiltered scan would count their concurrent statements too, corrupting
-- both the total and the "6 statements" claim. Filter to this database (and
-- this session's role, since the fixture/profile run as one user) so
-- unrelated cluster traffic can't leak in.
SELECT query, calls, shared_blks_hit, shared_blks_read,
       (shared_blks_hit + shared_blks_read) AS total_buffers,
       round(100.0 * (shared_blks_hit + shared_blks_read) /
             sum(shared_blks_hit + shared_blks_read) OVER (), 2) AS pct_of_page_buffers
FROM pg_stat_statements
WHERE query NOT ILIKE '%pg_stat_statements%'
  AND dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
  AND userid = (SELECT oid FROM pg_roles WHERE rolname = current_user)
ORDER BY total_buffers DESC;

\echo '--- EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) for the leaderboard query ---'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;
