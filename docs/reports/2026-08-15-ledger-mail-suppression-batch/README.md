# 🗃️ Ledger: batch list-mail suppression checks (statements/send N→1, buffers -87.8% at 20k recipients)

> **Correction (post-review):** an earlier version of this report measured the
> "after" workload with one un-chunked statement covering the whole recipient
> batch, regardless of size. Review caught that this didn't match the shipped
> `DbSuppressionStore::is_suppressed_many`, which chunks at a fixed
> `CHUNK_SIZE`. Reproducing the actual chunk loop surfaced a real finding: the
> original `CHUNK_SIZE = 5_000` sat right past a planner cost crossover (see
> 🧭 Plan below) where `subscriber = ANY(...)` stops using the
> `(subscriber, list_id)` index and falls back to a `Parallel Seq Scan` of the
> whole table — a plan whose cost is roughly *fixed* per statement, so
> splitting one large send into many 5,000-sized chunks was paying that fixed
> cost multiple times for no benefit. `CHUNK_SIZE` is now `50_000` (chunking
> exists only as a backstop against a truly pathological single send, not to
> shape ordinary sends), and every number below is measured against the code
> as shipped, including the chunk loop.

## 🎯 Workload

`Mailer::send_list_mail` (autumn/src/mail.rs) — the path `Mailer::send` takes
whenever a message has `list_unsubscribe` set (`#[mailer(list_unsubscribe)]`
or `MailBuilder::list_unsubscribe`, e.g. a weekly digest or product-update
newsletter). Before delivering anything it resolves, for every address in
`mail.to`, whether that subscriber has opted out of the list — backed in
production by `db_suppression::DbSuppressionStore` against the
`mail_unsubscribes` table that `autumn generate mailer --list-unsubscribe`
provisions (schema pinned by
`autumn/tests/integration/mail_unsubscribe.rs::newsletter_unsubscribe_end_to_end_db_backed`,
one of the testcontainer-managed DB tests CI sweeps automatically per
CLAUDE.md's Docker-dependent-test rule).

**Fixture**: `mail_unsubscribes`, seeded deterministically (`setseed(0.7213)`)
to match a multi-year SaaS's unsubscribe history — 800,000 rows, skewed
across 40 lists (`weekly_digest` 40%, `product_updates` 20%,
`security_alerts` 15%, 37 smaller per-team/per-feature lists sharing the
remaining 25%) — plus a fixed recipient batch for one simulated
`send_list_mail` call to `weekly_digest`: 30% real unsubscribes drawn from
the corpus (a realistic suppressed fraction for an old, high-volume list),
70% addresses that never unsubscribed. `VACUUM ANALYZE` after load. Three
batch sizes, same shape: 200 / 2,000 / 20,000 recipients — a manual send to a
segment, a mid-size digest, and a full-list newsletter blast. All three are
under `CHUNK_SIZE` (50,000), so each is one statement in production; the
harness (`fixture/workload_after.sql`) still reproduces the chunk loop
generically so it stays correct if `CHUNK_SIZE` or the batch sizes tested
here ever change.

**Reproduce**:
```
createdb ledger_bench
psql -d ledger_bench -c "CREATE EXTENSION pg_stat_statements;"
# shared_preload_libraries = 'pg_stat_statements', pg_stat_statements.track = 'all'

psql -d ledger_bench -f fixture/schema.sql
fixture/run_size.sh 2000   # seeds + runs before/after + prints calls,buffers CSV
fixture/run_size.sh 200
fixture/run_size.sh 20000
```

## 📈 Profile

`send_list_mail` issues exactly one kind of database statement in its
suppression-resolution step; nothing else in the function touches the
database (delivery is `self.transport.send(...)` per recipient — SMTP/log/
preview, not SQL). So within this workload the suppression-check statement
is, by construction, 100% of both calls and buffers — the target is not a
slice of a bigger workload to weigh against a 5% floor, it *is* the
workload. The `calls` ranking is where this shows up: one call per
recipient, invisible in a buffer-only ranking of a single send (each
individual lookup is ~3–4 buffers) but linear in recipient count across a
list send.

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` per size. Full output in
`baseline/explain_before_*.txt` / `after/explain_after_*.txt`.

**Before** (one call, medium size, representative "hit" case — this runs once
per recipient, at every size):
```
Aggregate  (actual time=0.057..0.058 rows=1 loops=1)
  Buffers: shared hit=4
  ->  Index Only Scan using mail_unsubscribes_subscriber_list_id_key
        Index Cond: (subscriber = 'user1@example.com' AND list_id = 'weekly_digest')
        Buffers: shared hit=4
Planning: Buffers: shared hit=60
```

**After, 200 / 2,000 recipients** (one call, whole batch — same shape as the
per-row lookup):
```
Index Only Scan using mail_unsubscribes_subscriber_list_id_key
  (actual time=4.647..8.939 rows=600 loops=1)
  Index Cond: (subscriber = ANY('{user1@example.com,...2000 addresses...}')
               AND list_id = 'weekly_digest')
  Buffers: shared hit=5940 read=64
Planning: Buffers: shared hit=110 read=1
```
Same index, same access method — the planner serves `subscriber = ANY(...)`
as a multi-probe scan over the same unique btree it was already using per
row.

**After, 20,000 recipients** — a genuine plan-shape change, not the same
Index Only Scan at bigger scale:
```
Gather  (actual time=22.710..59.107 rows=6000 loops=1)
  Buffers: shared hit=6616 read=1454
  ->  Parallel Seq Scan on mail_unsubscribes  (actual rows=2000 loops=3)
        Filter: (list_id = 'weekly_digest' AND subscriber = ANY('{...20,000 addresses...}'))
        Rows Removed by Filter: 264667
        Buffers: shared hit=6616 read=1454
Planning: Buffers: shared hit=110 read=1
```
Between 2,000 and 20,000 array elements the planner crosses over from probing
the index per element to a `Parallel Seq Scan` of the whole 800k-row table
with the array as a `Filter` — cheaper once the array is large enough that
paying for the scan once beats thousands of individual index descents. This
crossover is what the harness correction above found: it sits (for this
fixture) somewhere between 4,000 and 5,000 array elements — probed directly
in `fixture/` while investigating the review (not part of the committed
scripts, since the exact number is fixture-specific), which is why the
original `CHUNK_SIZE = 5_000` was a bad choice — it landed statements right
past the crossover into the expensive plan, then paid that ~fixed cost again
per chunk instead of once.

## 💡 Hypothesis

"The handler issues one `SELECT` per parent row instead of one batched
load." `send_list_mail`'s suppression loop (autumn/src/mail.rs, before this
change) called `SuppressionStore::is_suppressed(subscriber, list_id)` once
per entry in `mail.to`, sequentially, awaiting each round trip before
starting the next. `DbSuppressionStore::is_suppressed` is a single-row
`SELECT COUNT(*) ... WHERE subscriber = $1 AND list_id = $2` — cheap per
call (the table's `UNIQUE (subscriber, list_id)` constraint already backs
it with an index-only lookup) but paid N times for an N-recipient send, each
carrying its own connection-pool checkout, parse, plan, and network round
trip that a single batched query does not.

## 🔧 Change

One change: `autumn/src/mail.rs`.

- `SuppressionStore` (the list-unsubscribe trait, not the separate
  bounce/complaint `suppression::SuppressionStore`) gained
  `is_suppressed_many(subscribers: &[&str], list_id: &str) -> HashSet<String>`
  — the subset of `subscribers` that are suppressed. Its **default
  implementation loops over `is_suppressed`** in `subscribers` order,
  stopping at the first error — byte-for-byte the same sequential behavior
  the old code had — so `InMemorySuppressionStore` and any external/test
  implementor that doesn't override it keep working unchanged with zero call
  sites to update.
- `DbSuppressionStore::is_suppressed_many` overrides the default with one
  `SELECT subscriber FROM mail_unsubscribes WHERE list_id = $1 AND
  subscriber = ANY($2)` per chunk of up to `CHUNK_SIZE = 50_000` recipients.
  Chunking exists purely as a backstop against an unbounded array bind on a
  pathological single send, not to shape ordinary sends — see the
  correction note above and 🧭 Plan for why a small chunk size is actively
  counterproductive here.
- `send_list_mail` now validates every recipient's address format first (in
  original order, same fail-fast behavior), then calls
  `is_suppressed_many` **once** for the whole batch instead of
  `is_suppressed` once per recipient inside the validation loop.

No migration, no new index, no lock — `mail_unsubscribes` and its existing
`UNIQUE (subscriber, list_id)` index are untouched.

## 📊 Measurement

`pg_stat_statements`, one simulated `send_list_mail` call per size
(`fixture/run_size.sh`, resetting stats between the before/after run at each
size; the "after" run executes `fixture/workload_after.sql`'s chunk loop,
which for these three sizes is one chunk each since all are under
`CHUNK_SIZE`):

| recipients | before: calls | before: buffers | after: calls | after: buffers | buffers Δ |
|-----------:|---------------:|------------------:|---------------:|------------------:|----------:|
| 200        | 200            | 660               | 1              | 604               | −8.5%     |
| 2,000      | 2,000          | 6,600             | 1              | 6,004             | −9.0%     |
| 20,000     | 20,000         | 66,000            | 1              | 8,070             | **−87.8%**|

`temp_blks_read`/`temp_blks_written`: 0 in every run, both variants — no spill
either side.

Clears the impact floor on **statement count**: N→1 at every size tested
(**elimination of an N+1**, which per the process needs no other
justification), and additionally clears the ≥20% buffer-reduction floor at
the largest, most realistic size (a full-list send) — via the
index-scan-to-seq-scan crossover described in 🧭 Plan: a single
`Parallel Seq Scan` over the whole table costs about the same whether it's
serving an array of 6,000 or 20,000 elements, so at 20,000 the fixed scan
cost is amortized over far more recipients than the 20,000 independent
index descents it replaces. At small batches the per-statement
planning/execution floor dominates the buffer count and the win shows
almost entirely as calls, not buffers — consistent with the mechanism, not
a discrepancy. **This floor-clearing number is size- and chunk-size
dependent** — see the correction note: a batch just past `CHUNK_SIZE`
would add a second statement paying the ~8,070-buffer scan cost again,
which is exactly why `CHUNK_SIZE` was raised well above any realistic
single-send recipient count instead of left tight.

## ✅ Equivalence

- **Result set**: the batched query returns exactly the set of subscribers
  for which the per-row `SELECT COUNT(*) ... WHERE subscriber = $1 AND
  list_id = $2 → count > 0` would have been true — it's the same equality
  predicate applied set-wise (`subscriber = ANY(...)`) instead of row-wise,
  no `NOT IN`/`NOT EXISTS` NULL-semantics hazard (this is a positive
  membership test, not a negated one), and `list_id` is a single bound
  scalar in both forms. Unaffected by which physical plan the planner picks.
- **Rust-level proof of the N+1 elimination**: a new unit test,
  `mail::tests::send_list_mail_resolves_suppression_in_one_batched_call`,
  uses a counting `SuppressionStore` that instruments both methods. It
  asserts `is_suppressed_many` is called **exactly once** for a 3-recipient
  batch, `is_suppressed` is called **zero** times, and the correct 2 of 3
  (non-suppressed) recipients are delivered.
- **Error-ordering edge case**: `send_list_mail_suppression_error_fails_before_any_delivery`
  (pre-existing test, unmodified) uses a store that only overrides
  `is_suppressed` (not the new method) and errors for a specific subscriber.
  It still passes unmodified: the default `is_suppressed_many` loops in
  `subscribers` order and propagates the first error exactly as the old
  per-recipient loop did, so "fails before any delivery" holds for stores
  that haven't adopted batching.
- **Invalid-address edge case**: `send_list_mail_rejects_invalid_recipient_before_delivery`
  (pre-existing, unmodified) passes unmodified — address-format validation
  still runs over every recipient, in order, before any suppression lookup.
- **Empty batch**: `DbSuppressionStore::is_suppressed_many` short-circuits to
  an empty `HashSet` before touching the pool when `subscribers` is empty,
  matching the old loop's zero-iteration behavior (no round trip either way).
- **Duplicates in `mail.to`**: unioning into a `HashSet<String>` for the
  membership test doesn't affect delivery multiplicity — `deliveries` is
  still built by iterating the original (possibly duplicate-containing)
  candidate list once, same as before.
- **Chunking correctness**: each `CHUNK_SIZE`-recipient chunk is queried
  independently and the hit sets are unioned (`HashSet::extend`); membership
  in the union is equivalent to membership in any one chunk's result, which
  is equivalent to the un-chunked query's result restricted to that chunk —
  chunking changes statement count (and, as this report's correction found,
  can change which physical plan each statement gets), never the predicate
  or the result.
- **Isolation/transactions**: unchanged — no transaction was opened by the
  old loop or the new batched call; each is autocommit reads via a
  pool-checked-out connection, same as before.
- Existing tests pass **unchanged**: full `mail::` unit suite (130 tests,
  `cargo test -p autumn-web --lib --features db,mail mail::`) and the
  Docker-backed `newsletter_unsubscribe_end_to_end_db_backed` integration
  test (`autumn/tests/integration/mail_unsubscribe.rs`, real
  `DbSuppressionStore` against a testcontainers Postgres) both pass with no
  expectation changes.

## 💸 Write cost

No index added or dropped; `DbSuppressionStore::suppress` (the `INSERT ...
ON CONFLICT DO NOTHING` write path) is untouched by this change, so there is
no write-path or WAL impact to measure.

## 🔬 Reproduce

SQL-level:
```bash
psql -d ledger_bench -f docs/reports/2026-08-15-ledger-mail-suppression-batch/fixture/schema.sql
docs/reports/2026-08-15-ledger-mail-suppression-batch/fixture/run_size.sh 200
docs/reports/2026-08-15-ledger-mail-suppression-batch/fixture/run_size.sh 2000
docs/reports/2026-08-15-ledger-mail-suppression-batch/fixture/run_size.sh 20000
```

Rust-side:
```bash
cargo fmt --all
cargo clippy -p autumn-web --features db,mail --all-targets -- -D warnings
cargo test -p autumn-web --lib --features db,mail mail::
cargo test -p autumn-web --test integration_tests --features "test-support,offline-sync,db,mail" \
    mail_unsubscribe -- --ignored --test-threads=1
```
