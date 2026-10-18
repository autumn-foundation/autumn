# Revision history — batched editor names (Ledger)

Harness: `examples/cms/tests/revisions_author_batch_profile.rs`.
Artifacts: `baseline.txt` (RED commit, before the change), `after.txt`.

## Reproduce

```sh
AUTUMN_TEST_PG_URL=postgres://postgres@127.0.0.1:5433/postgres \
  cargo test -p cms --test revisions_author_batch_profile \
  -- --ignored --nocapture --test-threads=1
```

Server must preload `pg_stat_statements` (`track=all`); Postgres 16, local, same
session for both runs. Without the env var the harness starts a testcontainer.

## Workload / fixture

`GET /admin/content/post/{id}/revisions` as the site owner. 50,000 posts
(70/20/10 publish/draft/trash, dead tuples, no VACUUM), 2,000 users (2% with an
empty `display_name`), ~150,000 background revisions (5% NULL `author_id`,
squared skew onto few editors). Three measured posts, 60 revisions each, by
1 / 4 / 12 distinct editors, with NULL-author revisions mixed in.

## Mechanism

After one revisions query the handler called `users.find_by_id` once per
distinct `revisions.author_id` (`k` statements); now one `users.id = ANY($1)`.

## Result (pg_stat_statements, whole request, per request)

| editors k | statements before → after | buffers before → after | author-resolve stmts |
|---:|---:|---:|---:|
| 1  | 11 → 10 | 15 → 15 (0%)   | 2 → 2  |
| 4  | 20 → 10 | 23 → 20 (-13%) | 5 → 2  |
| 12 | 44 → 10 | 47 → 37 (-21%) | 13 → 2 |

(The `2` includes the session user's own lookup.) Before the change the
per-editor lookup was 29.5% of calls and 83% of buffers at k=12. Statements per
request go from O(k) to O(1). `temp_blks_written` 0 and `wal_bytes` 0 in both
(read path). No index, no migration. Remaining buffers are inherent (revisions
scan, one index probe per user id).

## Equivalence

Rendered response body hash is identical before and after for all three posts
(`31e4dded…`, `23537ce1…`, `64ea7e61…`), and the harness asserts every editor's
public name appears, including editors with an empty `display_name` (falls back
to username, via the unchanged `User::public_name`) and NULL-author revisions
(no lookup, no name). A deleted editor's id is simply absent from the map, as
`find_by_id` returning `None` was before.
