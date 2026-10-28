# `page_paths_under` — batched settled-path guard (Ledger)

Harness: `examples/cms/tests/page_paths_under_batch_profile.rs`.
Artifacts: `baseline.txt` (commit deaedb0, before the change), `after.txt`.

## Reproduce

```sh
AUTUMN_TEST_PG_URL=postgres://postgres@127.0.0.1:5544/postgres \
  cargo test -p cms --test page_paths_under_batch_profile \
  -- --ignored --nocapture --test-threads=1
```

The server must preload `pg_stat_statements`. Without `AUTUMN_TEST_PG_URL` the
harness starts a Postgres 16 testcontainer. Postgres 16.15, `track=all`.

## Workload

`content::update_post_with_revision` (the function the admin save handler
calls) renaming a nested page, which runs `guard_page_path` ->
`page_paths_under` inside the edit transaction. Fixture: 10,500-row `posts`
(10,000 posts at 70/20/10% publish/draft/trash, NULL `published_at` on every
unpublished row, 500 top-level pages), dead tuples from an `UPDATE`, `ANALYZE`,
no `VACUUM`; subtrees of 10 / 50 / 250 descendants, grandchildren skewed onto
few children.

## Mechanism

`page_paths_under` ran `page_path_of` per page: one `SELECT ... WHERE id = $1`
for the page and one per ancestor up to the root, i.e. `(D + 1) * (depth + 1)`
single-row statements for `D` descendants, inside the transaction holding the
hierarchy advisory lock and the edited row's `FOR UPDATE`.

## Result (pg_stat_statements, whole edit transaction)

| descendants | calls before | calls after | buffers before | buffers after | buffers delta |
|---:|---:|---:|---:|---:|---:|
| 10  |  56 | 14 |  416 |  315 | -24% |
| 50  | 208 | 14 |  747 |  275 | -63% |
| 250 | 968 | 14 | 3688 | 1092 | -70% |

Profile before the change: the per-page single-row lookup plus `descendant_ids`
were 84% / 96% / 99% of the transaction's calls and 42% / 93% / 99% of its
buffers. `temp_blks_written` is 0 throughout and WAL bytes are identical
(read-path change). The remaining buffers are `descendant_ids` (inherent: it
uses `idx_posts_parent`) and the edit's own writes.

## Equivalence

The harness keeps a verbatim copy of the old implementation and asserts
`new == legacy` (both sorted) for 844 ids: every page and every row with a
parent in the fixture plus edge cases: top-level page, non-page ancestor,
11-deep chain (past `MAX_PAGE_DEPTH`), a two-page cycle written directly,
duplicate slugs under different parents, trashed and non-page descendants,
and a missing id. No index and no migration.
