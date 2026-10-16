# Ledger experiment: covering index for the front-page leaderboard (NOT shipped)

`CREATE INDEX idx_votes_post_cover ON votes (post_id) INCLUDE (value)` + `VACUUM (ANALYZE) votes`.
Statement: front-page `SUM(value) GROUP BY post_id ORDER BY 2 DESC LIMIT 5` over 2,991,429 votes.

| counter (EXPLAIN BUFFERS)   | before                              | after (index-only, 0 heap fetches) |
|-----------------------------|-------------------------------------|------------------------------------|
| shared hit+read             | 24,945                              | 12,726 (-49%)                      |
| temp blocks written         | 14,422                              | 0                                  |
| top-5 result (post_id,sum)  | identical (COPY diff, tiebreak id)  |                                    |
| index size                  | idx_votes_post_id 41 MB             | cover index 90 MB (table 201 MB)   |
| WAL / 100k vote inserts     | 332,876,432 B                       | 350,950,064 B (+5.4%)              |

Not shipped: halves a full-table scan that remains O(votes) per front-page view; costs 90 MB and
+5.4% WAL on the vote write path; the existing idx_votes_post_id could be dropped (needs approval).
Real options (rollup/cache/bounded window) change semantics or need an invariant -> human decision.
