# 🗃️ Ledger: batch wiki collection-links insert loop (statements 200→1)

## 🎯 Workload

`examples/wiki` is a worked example of nested (`has_many`) form binding
(`docs/guide/nested-forms.md`): a **collection** (parent) owns many **links**
(children), edited together through one
`NestedChangesetForm<CollectionForm, LinkForm>`. By its own migration
comment, a collection is "a curated set of related external links" — a
resource page, a reading list — so a real one plausibly carries dozens to
hundreds of links.

`routes::collections::create` (`POST /collections`) and
`::update` (`POST /collections/{id}`) both walk the submitted `links: Vec<LinkForm>`
in a `for` loop and issue one `INSERT INTO collection_links ...` per row,
inside the single transaction that also writes the parent `collections` row.
`update` additionally does a bulk `DELETE` of the old set before the same
per-row re-insert loop. Both are real, public HTML routes reachable from the
"New/Edit Collection" forms — not test helpers.

**Fixture** (`examples/wiki/tests/collection_links_batch_profile.rs`): 150
pre-existing `collections` rows with 12 links each (1,800 background
`collection_links` rows), so `idx_collection_links_collection_id` has a
realistic size instead of trivially fitting on one page. A follow-up
`UPDATE` before `ANALYZE`, no `VACUUM`, gives the table real dead tuples —
the same technique every other Ledger fixture in this repo uses. The two
profiled requests each submit 200 links (`create`, then `update` replacing
the set with 200 different links) against a collection outside the
background set, so its own statements are cleanly attributable.

**Reproduce**:
```bash
cargo test -p wiki --test collection_links_batch_profile \
  -- --ignored --nocapture --test-threads=1
```
Requires Docker (`postgres:16-alpine` testcontainer, `pg_stat_statements`
preloaded). `examples/wiki` gained a `src/lib.rs` in this PR (mirroring
`examples/cms`'s split) so a `tests/` crate can mount the same route table
the binary serves — `src/main.rs` is now a thin wrapper around
`wiki::all_routes()`.

## 📈 Profile

Single workload, single statement family — same shape as every other
per-row-loop finding in this repo's history (`ledger_dd_comments`,
`recount_terms`, `set_post_terms`, `mark_dead`, …): the loop *is* the
measured cost, and it is invisible in a buffer ranking because each
individual `INSERT` is cheap. By `calls`, the unbatched `INSERT INTO
collection_links` statement is 200 of the ~201 statements the `create`
request issues (99.5%) and 200 of the ~202 the `update` request issues
(99%) — the parent `collections` INSERT/UPDATE is the only other statement
in `create`, and the one `collection_links` `DELETE` is the only other one
in `update`. By buffers it is smaller relative to total row I/O (the same
bytes move either way — see Measurement) but still the majority of each
request's buffer count.

## 🧭 Plan (before/after `EXPLAIN`)

**Before** (single-row shape, one call per link):
```
INSERT INTO collection_links (collection_id, label, url, position) VALUES (1, 'Explain Demo', 'https://example.org/explain-demo', 999)
Insert on public.collection_links  (cost=0.00..0.02 rows=0 width=0) (actual rows=0 loops=1)
  Buffers: shared hit=6
  ->  Result  (cost=0.00..0.02 rows=1 width=92) (actual rows=1 loops=1)
        Buffers: shared hit=1
```

**After** (batched multi-row shape, one call for the whole set — 3 rows shown):
```
INSERT INTO collection_links (collection_id, label, url, position) VALUES (1, ...), (1, ...), (1, ...)
Insert on public.collection_links  (cost=0.00..0.06 rows=0 width=0) (actual rows=0 loops=1)
  Buffers: shared hit=18
  ->  Values Scan on "*VALUES*"  (cost=0.00..0.06 rows=3 width=92) (actual rows=3 loops=1)
        Buffers: shared hit=3
```
The per-row shape stays identical (a `Result`/`Values Scan` feeding an
`Insert`, one FK-check trigger firing per row inserted); only the statement
count changes — 200 round trips collapse into 1, each carrying 200 value
tuples instead of network round-trip overhead per row. Full output in
`baseline/output.txt` and `after/output.txt`.

## 💡 Hypothesis

The handler issues one `INSERT` per incoming child row instead of one
batched `INSERT ... VALUES (...), (...), ...` — the textbook N+1-on-write
pattern this repo's own `CLAUDE.md` calls out ("A `.load()` or `.first()`
inside a loop... The fix is one batched query"), applied to `INSERT` instead
of `SELECT`. `position` is already carried as an explicit column and every
read already `ORDER BY position`, so the fix does not depend on the
database's physical insertion order.

## 🔧 Change

`examples/wiki/src/routes/collections.rs`: in both `create` and `update`,
collect the per-row `NewCollectionLink` values into a `Vec` first, then
issue one `diesel::insert_into(collection_links::table).values(&new_links)`
for the whole set — guarded by `if !new_links.is_empty()` (a zero-row
`VALUES` clause is invalid, and an empty submission is a real case: a
collection with no links yet). No schema change, no new index, no migration
— purely a call-site rewrite. `update`'s `DELETE` (already one statement)
is untouched.

## 📊 Measurement

| Scenario | Metric | Before | After | Tool |
|---|---|---:|---:|---|
| `create` (200 new links) | INSERT statements | 200 | **1** | `pg_stat_statements.calls` |
| `create` | INSERT buffers (hit+read) | 2,104 | 2,104 | `pg_stat_statements` |
| `update` (replace w/ 200) | INSERT statements | 200 | **1** | `pg_stat_statements.calls` |
| `update` | INSERT buffers (hit+read) | 2,005 | 2,005 | `pg_stat_statements` |
| `update` | DELETE statements | 1 | 1 | `pg_stat_statements.calls` |
| both | temp blocks written | 0 | 0 | `pg_stat_statements` |

Buffers are unchanged (same rows, same pages touched, same bytes) — this is
a pure round-trip elimination, not an I/O reduction. That is exactly the
impact-floor criterion this counts against: **"Elimination of an N+1 —
statement count per request drops from O(n) to O(1)"**, independent of
buffer delta. `temp_blks_written` stayed 0 in both runs (no spill, not
expected for a 200-row insert).

## ✅ Equivalence

All of the following are asserted in the same test run
(`collection_links_batch_profile.rs`), against the real Postgres fixture:

- **Order preservation**: after `create`, the 200 persisted rows
  (`label`, `url`, `position`), read back `ORDER BY position`, match the
  200 submitted rows exactly, in submitted order.
- **Replace semantics**: after `update`, the persisted set is exactly the
  200 newly-submitted rows — none of the pre-update 200 rows (from the
  `create` step) survive.
- **Empty input**: a submission with zero links still creates the parent
  and persists zero children (the `is_empty()` guard is exercised, not just
  present).
- **Duplicate content**: two link rows with identical `label`+`url` (legal —
  `collection_links` has no uniqueness constraint beyond `id`) both persist;
  the batched insert does not deduplicate.
- **Independent cross-check**: a separate collection is built with a
  standalone one-by-one reference `INSERT` loop (the pre-fix shape,
  reimplemented in the test only for this comparison) against the same
  200-link input used for `update`; its persisted rows are asserted
  byte-identical to the batched route's output.

## 💸 Write cost

No index added or dropped — this changes statement shape only, not schema.
WAL volume is unaffected: the same rows are written either way (a multi-row
`INSERT` produces the same per-tuple WAL records as N single-row inserts;
Postgres does not have a bulk-WAL fast path for a plain `INSERT ... VALUES`
list). The saving is entirely in round-trip / planning overhead, consistent
with buffers being unchanged above.

## 🔬 Reproduce

```bash
# Baseline (checkout the harness-only commit first, or `git stash` the fix):
cargo test -p wiki --test collection_links_batch_profile \
  -- --ignored --nocapture --test-threads=1

# After (with the fix in routes/collections.rs applied):
cargo test -p wiki --test collection_links_batch_profile \
  -- --ignored --nocapture --test-threads=1

# Verification:
cargo fmt --all -- --check
cargo clippy -p wiki --all-targets -- -D warnings
cargo test -p wiki --lib
cargo build -p wiki --bin wiki
```
