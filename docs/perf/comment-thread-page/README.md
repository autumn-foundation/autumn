# Paged comment thread: read each comment once

Harness: `fixture.sql` (seeded), `statements.sql` (the shipped SQL),
`statements-after.sql` (the change), `run.sh <before|after>`,
`explain.sh <before|after>`, `equivalence.sh` (+ `equivalence_cases.sql`,
`equivalence_sql.py`), `toast_check.sh`.
Artifacts: `before.txt`, `explain-before.txt` (the baseline commit);
`after.txt`, `explain-after.txt`, `equivalence.txt`, `toast.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded, autovacuum off,
PG* env set. `psql -f fixture.sql` once (about a minute), then
`./run.sh before > before.txt`, `./run.sh after > after.txt`,
`./explain.sh before > explain-before.txt`, `./explain.sh after > explain-after.txt`,
`./equivalence.sh > equivalence.txt`, `./toast_check.sh > toast.txt`.

## Workload

`commentable::router()`'s `GET /{type}/{id}[?after=]`, which calls
`comment_thread_page` with the default page size of 50. A request is three
statements: the parent probe, the page read, and a next-page check when the
page is full. The page read is one recursive statement: up to 50 live
top-level comments in `(created_at, id)` order, then every live reply under
them, then the author names.

`run.sh` runs pages 1 to 3 (where they exist) of seven threads, five times
each: 90 requests, 260 statements.

The SQL is what `autumn/src/commentable.rs` formats for a spec with table
`comments`, parent `posts` (no tenant, no soft delete), a soft-deleting
comments table, and `by = User, author_name = username`, with Postgres
`$n` binds. `pg_stat_statements` normalizes it the same way the framework's
own binds do.

## Fixture

The shared `comments` table exactly as `autumn generate scaffold … comments:commentable`
writes it, with both generated indexes (`(commentable_type, commentable_id, created_at, id)`
and `(parent_id)`). There are 1,655,953 comments on 40,000 posts and 200,000 users.

* Thread size is Zipf: post *p* has ⌊150,000 / *p*⌋ comments. The seven
  measured threads have 150,000, 50,000, 15,000, 5,000, 1,500, 150 and 15.
* 35% of comments are top-level. A reply's parent is an earlier comment in
  the same thread, biased toward recent ones, so threads nest.
* Threads start at random times over two years, and rows go in in global
  time order with ids in insert order. So a thread's rows are spread over
  the heap, as in production.
* About 8% of comments are soft-deleted: 3% of comments with their whole
  subtrees, which is the `delete_comment` rule.
* 5% of rows are edited, then `VACUUM ANALYZE`. Another 1% are edited with no
  vacuum, so there are dead tuples (3,394) and the visibility map is mostly set.

Heap 422 MB. Indexes: thread 130 MB, pkey 36 MB, `parent_id` 35 MB.

## Profile (pg_stat_statements, `before.txt`)

| statement | calls | share of calls | buffers | share of buffers |
| --- | --- | --- | --- | --- |
| page read, first page | 35 | 13.5% | 394,570 | 57.0% |
| page read, later page | 55 | 21.2% | 297,085 | 42.9% |
| next-page check | 80 | 30.8% | 680 | 0.1% |
| parent probe | 90 | 34.6% | 270 | 0.04% |

The page read is one statement with two texts (a cursor or none). It has
99.86% of the buffers and 34.6% of the calls. There are no temp blocks.
There is no N+1: a request issues three statements whatever the thread size.

## Plan (`explain-before.txt`, page 1 of the 15,000-comment thread)

The buffers of one page read (886 rows, 10,642 buffers) split like this:

| node | buffers | share |
| --- | --- | --- |
| top-level scan, `idx_comments_thread` (50 rows, 91 removed by the filter) | 54 | 0.5% |
| reply walk, `idx_comments_parent_id` + heap, 886 loops | 3,497 | 32.9% |
| **join back to `comments_pkey` + heap, 886 loops** | **3,544** | **33.3%** |
| author name, `users_pkey`, 886 loops | 3,544 | 33.3% |
| parent `EXISTS` | 3 | 0.03% |

```
->  Nested Loop  (actual rows=886 loops=1)
      Buffers: shared hit=7095
      ->  CTE Scan on __autumn_cmt_tree t  (actual rows=886 loops=1)
            Buffers: shared hit=3551
      ->  Index Scan using comments_pkey on public.comments c  (actual rows=1 loops=886)
            Index Cond: (c.id = t.id)
            Buffers: shared hit=3544
```

## Hypothesis

The recursive CTE carries only `id`. The walk already reads each comment's
heap row: it filters on `commentable_type`, `commentable_id` and
`deleted_at`. The outer query then reads the same row again through
`comments_pkey` to get `parent_id`, `author_id`, `body` and `created_at`.
That is a second index descent and heap visit per returned comment, about
four buffers each, which is a third of the statement.

Two other ideas were ruled out by the profile:

* A partial top-level index (`WHERE parent_id IS NULL AND deleted_at IS NULL`,
  as the guide suggests) serves only the top-level scan. That scan is 0.5% of
  the statement, below the 5% floor, so it was not tried. It would also tax
  every comment insert.
* The author-name join is a third of the statement. It is one probe per
  comment, and it is needed: each row has its own author. The probes could be
  collapsed per distinct author, but that changes the statement's shape.

## Change

`page_statement` (`autumn/src/commentable.rs`): both CTEs carry `id`,
`parent_id`, `author_id`, `body` and `created_at`. The outer query reads
them from the CTE and joins only the author table. Bind order, row order
(`created_at, id`), the parent-visibility `EXISTS`, and the recursion guard
do not change. The same text serves SQLite. There is no schema change, no
migration, and no new index.

## Measurement

Same harness, fixture and session. Tool: `pg_stat_statements` (reset after a
warm-up execution, `track_utility` off).

Whole workload (90 requests):

| | before | after | delta |
| --- | --- | --- | --- |
| page read buffers | 691,655 | 462,215 | −33.2% |
| total buffers | 692,605 | 463,165 | −33.1% |
| rows returned by the page read | 57,360 | 57,360 | 0 |
| statements | 260 | 260 | 0 |
| temp blocks read / written | 0 / 0 | 0 / 0 | 0 |
| WAL | read-only | read-only | — |

Page read buffers per call, by thread size:

| post | comments | page | rows | before | after | delta |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 150,000 | 1 | 3,037 | 36,483 | 24,335 | −33.3% |
| 1 | 150,000 | 2 | 1,019 | 12,251 | 8,175 | −33.3% |
| 1 | 150,000 | 3 | 816 | 9,824 | 6,560 | −33.2% |
| 3 | 50,000 | 1 | 1,545 | 18,556 | 12,376 | −33.3% |
| 3 | 50,000 | 2 | 876 | 10,517 | 7,013 | −33.3% |
| 3 | 50,000 | 3 | 568 | 6,824 | 4,552 | −33.3% |
| 10 | 15,000 | 1 | 886 | 10,642 | 7,098 | −33.3% |
| 10 | 15,000 | 2 | 565 | 6,787 | 4,527 | −33.3% |
| 10 | 15,000 | 3 | 287 | 3,468 | 2,320 | −33.1% |
| 30 | 5,000 | 1 | 609 | 7,356 | 4,920 | −33.1% |
| 30 | 5,000 | 2 | 268 | 3,251 | 2,179 | −33.0% |
| 30 | 5,000 | 3 | 277 | 3,364 | 2,256 | −32.9% |
| 100 | 1,500 | 1 | 339 | 4,145 | 2,789 | −32.7% |
| 100 | 1,500 | 2 | 131 | 1,647 | 1,123 | −31.8% |
| 100 | 1,500 | 3 | 109 | 1,405 | 969 | −31.0% |
| 1,000 | 150 | 1 | 121 | 1,815 | 1,331 | −26.7% |
| 1,000 | 150 | 2 | 5 | 313 | 293 | −6.4% |
| 10,000 | 15 | 1 | 14 | 215 | 159 | −26.0% |

The saving is four buffers per returned comment, so it is a third of any page
that returns more than a few rows. Only a near-empty page has a smaller
share. That five-row page also costs more than its rows suggest in both
variants: the figure averages five executions, and those run before the
plan cache switches to the generic plan. `explain-*.txt` shows the generic
plan at 79 buffers before and 59 after.

Page 1 of the 15,000-comment thread (`explain-after.txt`): 10,642 → 7,098
buffers. The `comments_pkey` node is gone. Every other node reads the same
number of buffers.

## Equivalence (`equivalence.txt`)

`equivalence.sh` walks every page of 14 threads with both statements: page
size 50, and also 2 for the small threads. Each page's full result (every
column, with `md5(body)`, in the statement's own row order) is compared by
md5, along with its row count and top-level count. That is 2,068 pages and
273,047 rows, all identical. Next-page cursors come from each variant's own
output.

Edge cases were added in a transaction that is rolled back:

* a post with no comments
* a post whose every comment is deleted
* a dangling author (`author_name` NULL)
* a TOASTed 200 kB body
* five top-level comments with one `created_at`, ordered by id
* a nested reply whose `created_at` ties top-level rows
* a live reply under a dead parent, which is on no page in both variants
* a deleted reply under a live parent
* a reply in another thread whose parent is in this one
* a cursor from another thread, a cursor that names no row, a cursor on a
  deleted comment, a cursor inside a tie, and a parent that does not exist

All are identical. Row-for-row `EXCEPT ALL` is empty both ways.

The read is one statement under one snapshot before and after. The outer
query read the same row versions the walk had read, so the columns cannot
differ. Isolation does not change.

TOAST (`toast.txt`, five reads of the edge-case thread): TOAST blocks 5 → 5,
TOAST index blocks 6 → 6. Carrying `body` through the CTE does not detoast it
early. Heap blocks fall 183 → 120 and index blocks 199 → 63.

Tests, with no test edited: `sqlite_commentable` (8 passed) and the
Postgres `commentable::` suite of `integration_tests` (67 passed, run against
a local Postgres 16 with `ledger-capture` on).

The page read's text changed, so its seven Ledger catalog profiles (one per
spec shape the suite uses) have new fingerprints. The new profiles come from
that capture. The seven old ones are deleted. `check-perf-profiles.py ledger`
on the capture reports 0 failures.

## Write cost

None. There is no index, no migration and no write-path change.
