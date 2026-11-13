# ADR 0045: Notifications Feed Index in the Default Sort Order

- Status: Accepted
- Date: 2026-11-10
- Deciders: Autumn maintainers
- Tags: notifications, generator, postgres, performance

## Context

Issue [#3616](https://github.com/autumn-foundation/autumn/issues/3616).

`autumn generate notifications` emitted one feed index on Postgres:
`(tenant_id, recipient_id)`. The feed page sorts by `id DESC` by default,
or by `created_at DESC, id DESC` when the client sends `sort=created_at`.
The index has neither order. So a page reads every row of the recipient and
sorts them. The cost grows with the recipient's history.

Measured on a skewed 2M-row fixture (`docs/perf/notifications-feed/`):

- A recipient with 1,092 rows: 1,083 buffers for one 20-row page.
- A recipient with 49,209 rows: 26 buffers. The planner walks the primary
  key backward. This is cheap only because that recipient owns 2.5% of all
  rows.
- `sort=created_at` for that recipient: 28,129 buffers.

The partial unread index (`idx_notifications_unread`) already serves the
unread count and the unread page.

## Options

1. Replace `(tenant_id, recipient_id)` with
   `(tenant_id, recipient_id, id)`. This is the default sort order.
2. Add `(tenant_id, recipient_id, created_at DESC, id DESC)`, as the issue
   proposed. It does not serve the default sort.
3. Ship both 1 and 2.
4. Keep the index. Bound the count, or move the feed to keyset pagination.
5. Serve `sort=created_at` with the `id` order. Rejected: the two orders
   differ when a test injects the clock or when the clock skews.

| option | mid page | heavy `created_at` page | insert WAL, warm / cold |
| --- | ---: | ---: | ---: |
| before | 1,083 | 28,129 | 43.3 / 66.8 MB |
| 1 | 23 | 28,329 | +30% / +70% |
| 2 | 1,083 | 23 | +63% / +119% |
| 3 | 23 | 23 | +93% / +189% |

## Decision

Option 1 on Postgres. The generator emits
`idx_notifications_feed ON notifications (tenant_id, recipient_id, id)` in
place of `idx_notifications_tenant_recipient`. It keeps
`idx_notifications_unread`.

- SQLite keeps `(tenant_id, recipient_id)`. `id` is the rowid there, so the
  index already reads in `id` order.
- The `created_at` index is opt-in. An app that sends `sort=created_at` adds
  it in its own migration. The guide gives the SQL.
- The count is not bounded. `Page.total_elements` stays exact. The count is
  an index-only scan.

## Consequences

- The first feed page reads about 20 index entries and 20 rows. Page `n`
  reads about `20 × n`. The length of the history has no effect.
- Each insert writes about 30% more WAL (70% just after a checkpoint). The
  index is larger (77 MB against 18 MB at 2M rows), because `id` is unique
  and B-tree deduplication cannot apply.
- The total count of a recipient with 49k rows reads 247 buffers, not 47.
  It stays an index-only scan.
- Existing apps keep their index until they add a migration. The guide
  gives the migrations, one `CONCURRENTLY` statement in each.
- Without the opt-in index, a `sort=created_at` page reads every row of the
  recipient. A `kind` page reads rows until it finds 20 of that kind.
