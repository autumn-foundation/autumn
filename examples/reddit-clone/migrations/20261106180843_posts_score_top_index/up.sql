-- #3325: the front-page "Top posts by votes" list.
--
-- The list reads `posts.score`. `#[votable(aggregate = sum)]` keeps that
-- column equal to the post's vote sum, in the vote transaction. The list
-- shows posts with a positive score, highest first, ties by id. This index
-- gives that order, so the read stops after five rows. Before, the list
-- summed every row of `votes` on each view.
--
-- The index holds only posts with a positive score. The query must carry
-- `score > 0` as a literal, not a bind, or Postgres cannot use the index.
--
-- `CONCURRENTLY` takes only a `SHARE UPDATE EXCLUSIVE` lock, so reads and
-- votes continue during the build. It cannot run in a transaction:
-- `metadata.toml` sets `run_in_transaction = false`.
--
-- No `IF NOT EXISTS`: a failed concurrent build can leave an INVALID index
-- with this name. A retry must then fail. Drop the index with
-- `DROP INDEX CONCURRENTLY idx_posts_score_top` and run the migration again.
CREATE INDEX CONCURRENTLY idx_posts_score_top
    ON posts (score DESC, id)
    WHERE score > 0;
