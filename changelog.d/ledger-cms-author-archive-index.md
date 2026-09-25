### Performance

- **🗃️ Ledger: cover `examples/cms`'s `/author/{username}` archive lookup
  (buffers 2,367→40 and 7,440→325 on measured author tiers):**
  `content::published_post_count_by_author` and
  `content::published_posts_by_author` filter `posts` by `(author_id,
  status, post_type)`, and the row fetch additionally orders by
  `published_at DESC, id DESC`. Neither existing index — `idx_posts_author`
  (`author_id` alone) nor `idx_posts_status_published` (`status,
  published_at DESC`) — covers that combination, so the row fetch had to
  walk `published_at` order discarding non-matching authors' rows (cheap for
  a prolific author, increasingly expensive the smaller their share of the
  site), while the count — no `LIMIT` to stop early — had to visit every one
  of an author's rows regardless of share, making it the *most* expensive
  for the *most* prolific author (7,426 of one measured tier's 7,440 total
  buffers). A new `CREATE INDEX CONCURRENTLY
  idx_posts_author_status_published ON posts (author_id, status,
  published_at DESC) INCLUDE (post_type)` fixes both: the row fetch gets a
  direct index scan, and the count becomes an Index Only Scan once
  autovacuum sets the visibility map, since `post_type` — the one predicate
  column not otherwise in the key — now rides along in the index tuple.
  Measured through the real route against a 50,000-post, three-author-tier
  fixture: buffers per request drop 54→22 (guest author), 2,367→40 (mid-tier,
  -98.3%) and 7,440→325 (prolific tier, -95.6%); rendered HTML is
  byte-identical before and after for all three tiers. Write cost: WAL for a
  2,000-row insert batch rises 2,053,336→2,398,888 bytes (+16.8%); the index
  is ~2.78MB on this fixture. `CONCURRENTLY` keeps the migration to a `SHARE
  UPDATE EXCLUSIVE` lock, so it does not block reads or writes.
