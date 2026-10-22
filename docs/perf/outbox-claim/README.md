# Outbox claim: partial index on pending `seq`

Harness: `autumn/tests/outbox_claim_profile.rs` (fixture `fixture.sql`).
Artifacts: `baseline.txt` (commit 27a774e), `after.txt`, `write_cost.sql`, `write_cost.txt`.

Reproduce (Docker, or any Postgres 16 with `pg_stat_statements` preloaded):

```text
OUTBOX_PROFILE_DATABASE_URL=postgres://... \
cargo test -p autumn-web --features test-support --test outbox_claim_profile \
  -- --ignored --nocapture --test-threads=1
psql -v with_claim_index=true -f write_cost.sql   # then false
```

| history (sent rows) | claim buffers before | after | workload buffers before | after |
| --- | --- | --- | --- | --- |
| 100,000 | 19,104 | 7,501 | 25,654 | 14,163 |
| 300,000 | 44,474 | 7,505 | 50,722 | 13,868 |
| 950,000 | 126,813 | 7,499 | 132,976 | 13,771 |

Rows removed by filter per claim: 100,034 / 300,034 / 950,034 -> 34.
Ordered handled-id hashes are identical before and after at every tier.
