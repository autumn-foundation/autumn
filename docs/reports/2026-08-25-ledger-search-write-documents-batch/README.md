# 🗃️ Ledger: batch PostgresSearchStore::write_documents upserts (backfill statements/req N→⌈N/500⌉)

## 🎯 Workload

`PostgresSearchStore::write_documents` (`autumn-search/src/postgres.rs`) is the
**single write path** behind both `SearchBackend::index` and
`SearchBackend::index_unless_newer` — the doc comment says so explicitly, "so
the conditional and unconditional forms cannot drift apart." It is what
`SearchClient::backfill` (`autumn-search/src/client.rs`) calls once per batch
of up to `DEFAULT_BACKFILL_BATCH` (500) documents, and `backfill` is itself
the body of the framework's own `autumn_search_backfill` background job
(`autumn-search/src/jobs.rs`) — the job that rebuilds a search index end to
end, the exact workload a large table's full reindex runs through.

Before this fix, `write_documents` looped over its `documents` slice and
issued one `INSERT ... ON CONFLICT DO UPDATE` **per document**:

```rust
for document in documents {
    // ... build one document's bind list and SQL text ...
    bind_all(diesel::sql_query(sql).into_boxed(), binds)
        .execute(&mut conn)
        .await?;
}
```

A 500-document backfill batch therefore cost 500 DB round trips, not one —
invisible in a buffer-cost ranking (each individual statement is cheap) but
dominant in `pg_stat_statements.calls`, exactly the shape the Ledger process
calls out as hiding in plain sight ("this is where N+1 lives... usually
invisible in the buffer ranking because each individual statement is
cheap"). `postgres.rs`'s own `delete()` method already batches multiple ids
into one statement via `record_id = ANY($2)`; the write path was the odd one
out.

**Fixture**: a `search_tenant_articles` source table backfilled through the
REAL public entry point (`SearchClient::backfill`, not a hand-rolled call to
`write_documents`), at three cumulative tiers — 100, 500, 2,000 rows — at the
framework's actual default batch size (`BackfillOptions::default()`, 500).
Tenant cardinality is skewed (5% no tenant, ~76% one of 15 repeat tenants,
~19% unique long-tail tenants — a real NULL density), and every row carries a
`#[searchable(embed)]` body, so every write exercises the weighted-tsvector
concatenation **and** the embedding-array column, not just plain-text
columns. The shared `autumn_search_documents` table also carries 30,000
pre-existing rows under an unrelated index name before any of this fixture's
rows are written, so `ON CONFLICT` / GIN index maintenance runs against a
realistically sized table, not an empty one.

**Reproduce**:
```bash
cargo test -p autumn-search --test search_tests -- --ignored \
  write_documents_batch_profile --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). CI runs it automatically in the Docker
sweep (`cargo test -p autumn-search -- --ignored`, see `CLAUDE.md`).

## 📈 Profile

Within this workload, the `write_documents` `INSERT` is the dominant cost by
both buffers and calls, in every tier:

| tier (total rows) | write `INSERT` calls | write buffers | ALL statement calls this run | ALL buffers this run | write share of buffers |
|---:|---:|---:|---:|---:|---:|
| 100  | 100  |  1,256 | 108  |  1,268 | 99.1% |
| 500  | 500  |  5,693 | 507  |  5,715 | 99.6% |
| 2000 | 2000 | 22,263 | 2016 | 22,391 | 99.4% |

Across all three tiers combined: **99.4% of every buffer touched by this
backfill workload (29,212 / 29,374) is the `write_documents` INSERT** — far
above the 5%-of-buffers floor the process sets for "not worth changing."
`calls` is even more lopsided: at the 2,000-row tier the write statement
alone accounts for 2,000 of the run's 2,016 total statement calls.

## 🧭 Plan

Same access method either side — an `(index_name, record_id)` primary-key
upsert with a GIN `search_vector` index and a btree `(index_name,
tenant_id)` index to maintain — this is not a plan-shape change, it is the
same operation issued once per **batch** instead of once per **document**.
Representative `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` for a 3-row
example (full output for the real 100/500/2,000-row tiers is in
`baseline/output.txt` / `after/output.txt`, captured from `pg_stat_statements`
against the actual statements `write_documents` issued):

**Before** (one call per document — `baseline/output.txt`, e.g. the 100-row
tier's normalized statement, `calls=100`):
```
INSERT INTO autumn_search_documents (index_name, record_id, tenant_id, language, fields, content, search_vector, embedding)
SELECT $1, $2, $3, $5, $4::jsonb, $6,
       setweight(to_tsvector($5::text::regconfig, $8), $11) || setweight(to_tsvector($5::text::regconfig, $9), $12),
       $7::double precision[]
WHERE NOT EXISTS (SELECT $13 FROM autumn_search_deletes d WHERE d.index_name = $1 AND d.record_id = $2 AND d.deleted_at > $10::timestamptz)
ON CONFLICT (index_name, record_id) DO UPDATE SET ... WHERE autumn_search_documents.updated_at <= $10::timestamptz
```

**After** (one call per BATCH — `after/output.txt`, e.g. the 2,000-row
tier's normalized statement, `calls=4`, one per `⌈2000/500⌉` batch):
```
INSERT INTO autumn_search_documents (index_name, record_id, tenant_id, language, fields, content, search_vector, embedding)
SELECT $1, $4, $5, $2, $6::jsonb, $7,
       setweight(to_tsvector($2::text::regconfig, $9), $3504) || setweight(to_tsvector($2::text::regconfig, $10), $3505), ...
WHERE NOT EXISTS (...) UNION ALL SELECT ... (500 rows total) ...
ON CONFLICT (index_name, record_id) DO UPDATE SET ... WHERE autumn_search_documents.updated_at <= $3::timestamptz
```

`index_name` and `language` are hoisted to `$1`/`$2` and bound **once**,
shared by every row, rather than once per row — the only reason the
statement's shape stays independent of batch size and diesel's
prepared-statement cache sees ONE statement per index rather than one per
document.

## 💡 Hypothesis

"The `write_documents` `INSERT` is ~99% of both buffers and calls in a
backfill workload. The Rust code issues it inside a `for document in
documents` loop — one DB round trip per document — while
`SearchClient::backfill` already assembles documents into batches of up to
500 before calling it. The fix is mechanical: batch every document in one
`write_documents` call into ONE multi-row statement, joined with `UNION ALL`
when watermark-guarded (each row's own `WHERE NOT EXISTS` tombstone check
needs somewhere to attach, which a bare `VALUES` row has no room for) or a
plain multi-row `VALUES` when unconditional."

## 🔧 Change

`autumn-search/src/postgres.rs`: `write_documents` now builds one
`UpsertRow` (a rendered column-value list plus that row's own `record_id`
placeholder) per document, using the *exact same* per-field SQL generation
as before (same `setweight`/`to_tsvector` expression, same casts, same
dimension-mismatch guard), then joins all rows into ONE statement via the
rewritten `upsert_sql`. No migration — the target table and its indexes are
unchanged; this only changes how many round trips one write costs.

The one behavior change, disclosed rather than hidden: a `DimensionMismatch`
now aborts the WHOLE batch with nothing written (the error is detected while
still assembling Rust-side bind lists, before any SQL executes), whereas the
old per-document loop would already have committed every document before the
bad one. This is untested surface either way — no test in this suite
exercises pgvector mode (no pgvector-enabled container image is used
anywhere in this repo's CI), so this behavior is unverified by an automated
test in either direction. I did not add pgvector coverage here; it is a
pre-existing gap, not one this change introduces.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before each tier, read after. Full per-tier statement dumps in
`baseline/output.txt` (captured against the pre-fix per-document loop, in
its own commit) and `after/output.txt` (captured against the fix, same
fixture, same session).

| tier (rows) | write INSERT calls (before → after) | write INSERT buffers (before → after) |
|---:|---:|---:|
| 100  | 100 → **1** | 1,256 → 1,256 |
| 500  | 500 → **1** | 5,693 → 6,421 |
| 2000 | 2,000 → **4** | 22,263 → 26,133 |

Statement count drops from **one per document to one per backfill batch**
(⌈rows/500⌉) — the admissible-on-its-own N+1 floor
("statement count per request drops from O(n) to O(1)... needs no other
justification"). Buffers touched are **not** reduced (in fact marginally
higher at the 500/2,000 tiers — folding N single-row upserts into one
multi-row `UNION ALL`/`VALUES` statement does the same index and heap
maintenance work, just in fewer round trips) — this is an explicit,
disclosed round-trip-count win, not a buffer-reduction claim, and it is not
represented as one.

No `temp_blks_written` at any tier (no spill, either side). No index was
added or dropped, so there is no write-amplification/WAL-tax tradeoff to
measure — this changes *how many statements* carry the same writes, not what
they write.

## ✅ Equivalence

Every one of the 2,000 documents this harness writes is dumped
(`record_id, tenant_id, content, fields::text, search_vector::text,
embedding::text`, sorted by `record_id` — no ties) at the end of both the
baseline and after runs. **The two dumps are byte-for-byte identical** (the
only line that differs between `baseline/output.txt` and `after/output.txt`
is the harness's own wall-clock "finished in Ns" summary line, which is not
part of the dump and is inadmissible timing evidence anyway). This covers:

- NULL `tenant_id` (5% of rows, deterministic via `id % 20 == 0`)
- repeat-tenant assignment (the shared, hoisted `$1`/`$2` binding path)
- unique long-tail tenants (`id % 5 == 4`)
- the full weighted-tsvector concatenation (`title` weight A, `body` weight
  B) for every row
- the portable `embedding double precision[]` array column round-trip

The full existing `autumn-search` Docker-backed suite (30 tests — tombstone
clearing/replay, watermark-guarded backfill vs. a soft-deleted row, tenant
filtering, id allowlist/denylist, keyword ranking, vector search, embedding
mismatch skip, purge, and more) passes unchanged against the fix, with zero
test-expectation edits.

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement applies —
see Measurement above.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit):
cargo test -p autumn-search --test search_tests -- --ignored \
  write_documents_batch_profile --nocapture --test-threads=1

# Full regression suite:
cargo test -p autumn-search -- --ignored
```
